use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rampire::attestation::AttestedCompatibilitySource;
use rampire::compatibility_source::CompatibilitySource;
use rampire::guest_channel::{GuestChannel, GuestLink};
use rampire::qga::VirshGuestFiles;
use rampire::virsh::Virsh;
use rampire_core::settings::GuestOs;
use rampire_core::{
    parse_virtio_mem_xml_for_alias, MemoryRequest, RawTelemetryEnvelope, VirtioMemState,
};
use serde::Serialize;
use serde_json::Value;

use crate::{process, windows};

#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    vm: String,
    alias: String,
    unit: String,
    telemetry_path: String,
    service: String,
    attestation: PathBuf,
    host_deployment_evidence: PathBuf,
    windows_evidence: PathBuf,
    output: PathBuf,
    host_headroom_bytes: u64,
    sample_interval: Duration,
    telemetry_max_age: Duration,
    future_tolerance: Duration,
    preflight_timeout: Duration,
    command_timeout: Duration,
    connection: String,
    apply_service_restart: bool,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    vm_name: String,
    device_alias: String,
    controller_unit: String,
    controller_active_state: String,
    controller_unit_file_state: String,
    host_mem_available_bytes: u64,
    required_host_headroom_bytes: u64,
    requested_before_bytes: u64,
    current_before_bytes: u64,
    requested_after_bytes: u64,
    current_after_bytes: u64,
    initial_session_id: String,
    initial_sequence: u64,
    initial_request: Option<MemoryRequest>,
    new_session_id: String,
    new_session_sequence: u64,
    new_session_request: Option<MemoryRequest>,
    windows_service_restart_applied: bool,
    attestation_current: bool,
    rollback_path: String,
    no_memory_mutation_observed: bool,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.len() < 2 {
        return Err("preflight requires VM_NAME ALIAS and explicit options".to_owned());
    }
    validate_identity(&args[0], "VM_NAME")?;
    validate_identity(&args[1], "ALIAS")?;
    let mut values = std::collections::BTreeMap::new();
    let mut apply_service_restart = false;
    let mut index = 2;
    while index < args.len() {
        let option = args[index].clone();
        if option == "--apply-service-restart" && !apply_service_restart {
            apply_service_restart = true;
            index += 1;
            continue;
        }
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{option} requires a value"))?;
        if !matches!(
            option.as_str(),
            "--unit"
                | "--telemetry-path"
                | "--service"
                | "--attestation"
                | "--host-deployment-evidence"
                | "--windows-evidence"
                | "--output"
                | "--host-headroom-bytes"
                | "--sample-interval-seconds"
                | "--telemetry-max-age-seconds"
                | "--future-tolerance-seconds"
                | "--preflight-timeout-seconds"
                | "--command-timeout-seconds"
                | "--connect"
        ) || values.insert(option.clone(), value.clone()).is_some()
        {
            return Err(format!("unknown or duplicate preflight option: {option}"));
        }
        index += 1;
    }
    let required = |name: &str| {
        values
            .get(name)
            .cloned()
            .ok_or_else(|| format!("preflight requires {name}"))
    };
    let positive = |name: &str| -> Result<u64, String> {
        required(name)?
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} requires a positive integer"))
    };
    let unit = required("--unit")?;
    crate::host_deploy::controller_instance(&unit)?;
    let telemetry_path = required("--telemetry-path")?;
    if !telemetry_path
        .as_bytes()
        .get(1)
        .is_some_and(|byte| *byte == b':')
        || !telemetry_path.contains('\\')
        || telemetry_path.contains("..")
    {
        return Err("--telemetry-path must be an absolute safe Windows path".to_owned());
    }
    Ok(Command {
        vm: args[0].clone(),
        alias: args[1].clone(),
        unit,
        telemetry_path,
        service: required("--service")?,
        attestation: PathBuf::from(required("--attestation")?),
        host_deployment_evidence: PathBuf::from(required("--host-deployment-evidence")?),
        windows_evidence: PathBuf::from(required("--windows-evidence")?),
        output: PathBuf::from(required("--output")?),
        host_headroom_bytes: positive("--host-headroom-bytes")?,
        sample_interval: Duration::from_secs(positive("--sample-interval-seconds")?),
        telemetry_max_age: Duration::from_secs(positive("--telemetry-max-age-seconds")?),
        future_tolerance: Duration::from_secs(positive("--future-tolerance-seconds")?),
        preflight_timeout: Duration::from_secs(positive("--preflight-timeout-seconds")?),
        command_timeout: Duration::from_secs(positive("--command-timeout-seconds")?),
        connection: values
            .get("--connect")
            .cloned()
            .unwrap_or_else(|| "qemu:///system".to_owned()),
        apply_service_restart,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    if !command.apply_service_restart {
        return Err(
            "QA-T008 requires --apply-service-restart; no memory mutation is performed".to_owned(),
        );
    }
    let unit_state = controller_state(command, repo)?;
    let before = live_state(command, repo)?;
    require_converged(before)?;
    let host_mem_available_bytes = host_mem_available()?;
    if host_mem_available_bytes < command.host_headroom_bytes {
        return Err(format!(
            "host headroom is below the explicit bound (available={host_mem_available_bytes}, required={})",
            command.host_headroom_bytes
        ));
    }
    let attestation = resolve(repo, &command.attestation);
    AttestedCompatibilitySource::new(
        Virsh::with_connection("virsh", command.command_timeout, &command.connection),
        &command.vm,
        &command.alias,
        // Preflight checks a Windows service install; Linux guests have none.
        GuestOs::Windows,
        attestation,
    )
    .compatibility()?
    .validate_for_resize()
    .map_err(|error| format!("preflight attestation failed: {error}"))?;
    let rollback_path = applied_rollback_path(&resolve(repo, &command.host_deployment_evidence))?;
    // With the controller held inactive nothing can resolve a guest request,
    // so a request pending before the service restart must survive it with
    // its identity unchanged.
    let initial = read_telemetry(command)?;

    windows::run(
        windows::Command::ServiceCycle {
            service: command.service.clone(),
            output: command.windows_evidence.clone(),
            apply: true,
        },
        repo,
    )?;

    let deadline = Instant::now() + command.preflight_timeout;
    let mut previous = initial.clone();
    let new_session = loop {
        let envelope = read_telemetry(command)?;
        if envelope != previous {
            envelope
                .validate_successor(&previous)
                .map_err(|error| format!("Windows publication ordering failed: {error}"))?;
            if envelope.session_id != initial.session_id {
                break envelope;
            }
            previous = envelope;
        }
        if Instant::now() >= deadline {
            return Err(
                "Windows telemetry did not roll to a new session before the preflight deadline"
                    .to_owned(),
            );
        }
        std::thread::sleep(command.sample_interval);
    };
    if initial.request.is_some() && new_session.request != initial.request {
        return Err(format!(
            "the unresolved guest request did not survive the service restart: before={:?}, after={:?}",
            initial.request, new_session.request
        ));
    }
    let after = live_state(command, repo)?;
    require_converged(after)?;
    if before != after {
        return Err(format!("memory state changed during no-actuation preflight: before={before:?}, after={after:?}"));
    }
    let final_unit_state = controller_state(command, repo)?;
    if final_unit_state != unit_state {
        return Err("controller unit state changed during preflight".to_owned());
    }
    persist(
        &resolve(repo, &command.output),
        &Evidence {
            schema_version: 2,
            vm_name: command.vm.clone(),
            device_alias: command.alias.clone(),
            controller_unit: command.unit.clone(),
            controller_active_state: unit_state.0,
            controller_unit_file_state: unit_state.1,
            host_mem_available_bytes,
            required_host_headroom_bytes: command.host_headroom_bytes,
            requested_before_bytes: before.requested_bytes,
            current_before_bytes: before.current_bytes,
            requested_after_bytes: after.requested_bytes,
            current_after_bytes: after.current_bytes,
            initial_session_id: initial.session_id,
            initial_sequence: initial.sequence,
            initial_request: initial.request,
            new_session_id: new_session.session_id,
            new_session_sequence: new_session.sequence,
            new_session_request: new_session.request,
            windows_service_restart_applied: true,
            attestation_current: true,
            rollback_path,
            no_memory_mutation_observed: true,
        },
    )?;
    println!(
        "No-actuation preflight evidence written to {}",
        resolve(repo, &command.output).display()
    );
    Ok(())
}

fn read_telemetry(command: &Command) -> Result<RawTelemetryEnvelope, String> {
    GuestChannel::new(
        VirshGuestFiles::new(
            Virsh::with_connection("virsh", command.command_timeout, &command.connection),
            &command.vm,
        ),
        &command.telemetry_path,
        &command.vm,
        &command.service,
        command.telemetry_max_age,
        command.future_tolerance,
    )
    .read()
}

fn live_state(command: &Command, repo: &Path) -> Result<VirtioMemState, String> {
    let xml = process::bounded_text(
        "virsh",
        &[
            process::os("--connect"),
            process::os(&command.connection),
            process::os("dumpxml"),
            process::os(&command.vm),
        ],
        repo,
        command.command_timeout,
    )?;
    parse_virtio_mem_xml_for_alias(&xml, &command.alias)
        .map(|state| state.memory)
        .map_err(|error| format!("preflight live state: {error}"))
}
fn require_converged(state: VirtioMemState) -> Result<(), String> {
    if state.requested_bytes == state.current_bytes {
        Ok(())
    } else {
        Err(format!(
            "preflight requires requested == current: {state:?}"
        ))
    }
}

fn controller_state(command: &Command, repo: &Path) -> Result<(String, String), String> {
    let text = process::bounded_text(
        "systemctl",
        &[
            process::os("show"),
            process::os(&command.unit),
            process::os("--property=ActiveState,UnitFileState,MainPID"),
            process::os("--no-pager"),
        ],
        repo,
        command.command_timeout,
    )?;
    let fields = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<std::collections::BTreeMap<_, _>>();
    let active = fields.get("ActiveState").copied().unwrap_or("");
    let enabled = fields.get("UnitFileState").copied().unwrap_or("");
    let pid = fields.get("MainPID").copied().unwrap_or("");
    if active != "inactive" || enabled != "disabled" || pid != "0" {
        return Err(format!(
            "controller is not in disabled inactive safe hold: {text:?}"
        ));
    }
    Ok((active.to_owned(), enabled.to_owned()))
}

fn host_mem_available() -> Result<u64, String> {
    use rampire::host_memory::{HostMemorySource, ProcMeminfoSource};
    ProcMeminfoSource::new().available_bytes()
}

fn applied_rollback_path(path: &Path) -> Result<String, String> {
    let document: Value = serde_json::from_str(
        &std::fs::read_to_string(path)
            .map_err(|error| format!("read host deployment evidence: {error}"))?,
    )
    .map_err(|error| format!("parse host deployment evidence: {error}"))?;
    if document.get("applied").and_then(Value::as_bool) != Some(true) {
        return Err("host deployment evidence is not applied".to_owned());
    }
    document
        .get("backup_path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "host deployment evidence lacks rollback path".to_owned())
}

fn persist(path: &Path, evidence: &Evidence) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "preflight output has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create preflight evidence directory: {error}"))?;
    let mut json = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("serialize preflight evidence: {error}"))?;
    json.push(b'\n');
    let temporary = parent.join(format!(".preflight-{}.tmp", std::process::id()));
    std::fs::write(&temporary, json)
        .map_err(|error| format!("write preflight evidence: {error}"))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("commit preflight evidence: {error}"))
}

fn resolve(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}
fn validate_identity(value: &str, name: &str) -> Result<(), String> {
    if !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        Ok(())
    } else {
        Err(format!("{name} contains unsafe characters"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_implicit_preflight_scope() {
        assert!(parse(&["guest".to_owned(), "alias".to_owned()]).is_err());
    }
}
