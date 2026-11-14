use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use rampire_host::config::HostConfig;

use rampire_ops::process;

const EVIDENCE_VERSION: u16 = 1;
const DEFAULT_CONNECTION: &str = "qemu:///system";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Required when the configuration holds more than one VM.
    vm: Option<String>,
    host_cli: PathBuf,
    output: PathBuf,
    timeout: Duration,
    connection: String,
    elevate: bool,
    elevated_child: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    version: u16,
    unit: String,
    active_state: String,
    sub_state: String,
    unit_file_state: String,
    status: rampire_core::ControllerStatusSnapshot,
}

pub fn parse(arguments: &[String]) -> Result<Command, String> {
    let mut host_cli = None;
    let mut vm = None;
    let mut output = None;
    let mut timeout = None;
    let mut connection = DEFAULT_CONNECTION.to_owned();
    let mut connection_seen = false;
    let mut elevate = false;
    let mut elevated_child = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--elevate" if !elevate => elevate = true,
            "--elevated-child" if !elevated_child => elevated_child = true,
            "--host-cli" | "--vm" | "--output" | "--command-timeout-seconds" | "--connect" => {
                let option = arguments[index].as_str();
                index += 1;
                let value = arguments
                    .get(index)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| format!("{option} requires a value"))?;
                match option {
                    "--host-cli" if host_cli.is_none() => host_cli = Some(PathBuf::from(value)),
                    "--vm" if vm.is_none() => {
                        validate_identity(value)?;
                        vm = Some(value.clone());
                    }
                    "--output" if output.is_none() => output = Some(PathBuf::from(value)),
                    "--command-timeout-seconds" if timeout.is_none() => {
                        timeout = Some(Duration::from_secs(
                            value
                                .parse::<u64>()
                                .ok()
                                .filter(|value| *value > 0)
                                .ok_or_else(|| {
                                    "--command-timeout-seconds requires a positive integer"
                                        .to_owned()
                                })?,
                        ));
                    }
                    "--connect" if !connection_seen => {
                        connection = value.clone();
                        connection_seen = true;
                    }
                    _ => return Err(format!("duplicate controller-status option: {option}")),
                }
            }
            option => {
                return Err(format!(
                    "unknown or duplicate controller-status option: {option}"
                ));
            }
        }
        index += 1;
    }
    if elevate && elevated_child {
        return Err("--elevate and --elevated-child are mutually exclusive".to_owned());
    }
    let host_cli = host_cli.ok_or_else(|| "controller-status requires --host-cli".to_owned())?;
    if !host_cli.is_absolute() {
        return Err("controller-status --host-cli must be an absolute path".to_owned());
    }
    Ok(Command {
        vm,
        host_cli,
        output: output.ok_or_else(|| "controller-status requires --output".to_owned())?,
        timeout: timeout
            .ok_or_else(|| "controller-status requires --command-timeout-seconds".to_owned())?,
        connection,
        elevate,
        elevated_child,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    validate_command(command, repo)?;
    if command.elevate {
        let json = run_elevated(command, repo)?;
        let evidence: Evidence = serde_json::from_str(&json)
            .map_err(|error| format!("elevated controller status is invalid: {error}"))?;
        evidence
            .status
            .validate()
            .map_err(|error| error.to_string())?;
        persist(&resolve(repo, &command.output), json.as_bytes())?;
        println!(
            "Controller status evidence written to {}",
            resolve(repo, &command.output).display()
        );
        return Ok(());
    }
    if command.elevated_child && process::effective_uid() != 0 {
        return Err("controller-status --elevated-child requires effective uid 0".to_owned());
    }
    if !command.elevated_child {
        return Err("controller-status requires --elevate for protected live state".to_owned());
    }
    let evidence = collect(command, repo)?;
    let json = serde_json::to_string_pretty(&evidence)
        .map(|json| format!("{json}\n"))
        .map_err(|error| format!("serialize controller status evidence: {error}"))?;
    print!("{json}");
    Ok(())
}

fn validate_command(command: &Command, repo: &Path) -> Result<(), String> {
    let candidate = &command.host_cli;
    let expected = repo.join("target/release/rampire");
    if candidate != &expected {
        return Err(format!(
            "controller-status --host-cli must select the current prebuilt candidate {}",
            expected.display()
        ));
    }
    let metadata = std::fs::metadata(candidate)
        .map_err(|error| format!("inspect controller-status host candidate: {error}"))?;
    if !metadata.is_file() {
        return Err("controller-status host candidate is not a file".to_owned());
    }
    if command.connection.trim().is_empty() || command.connection.chars().any(char::is_control) {
        return Err("controller-status connection is invalid".to_owned());
    }
    Ok(())
}

fn collect(command: &Command, repo: &Path) -> Result<Evidence, String> {
    let unit = rampire_ops::units::CONTROLLER_UNIT.to_owned();
    let properties = process::bounded_text(
        "systemctl",
        &[
            process::os("show"),
            process::os(&unit),
            process::os("--property=ActiveState,SubState,UnitFileState"),
            process::os("--no-pager"),
        ],
        repo,
        command.timeout,
    )?;
    let properties = parse_properties(&properties)?;
    let config_path = PathBuf::from("/etc/rampire");
    let configured = HostConfig::load_vm(&config_path, command.vm.as_deref())
        .map_err(|error| format!("protected host configuration: {error}"))?;
    let environment = [
        ("LIBVIRT_DEFAULT_URI", command.connection.as_str()),
        ("HOME", "/run/rampire"),
        ("XDG_CACHE_HOME", "/run/rampire/.cache"),
    ];
    let output = process::bounded_output_with_env(
        &command.host_cli,
        &[
            process::os("--config"),
            config_path.as_os_str().to_owned(),
            process::os("status"),
            process::os(&configured.vm_name),
            process::os("--connect"),
            process::os(&command.connection),
        ],
        repo,
        command.timeout,
        environment,
    )?;
    if !output.status.success() {
        return Err(format!(
            "controller status candidate failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let status = rampire_core::parse_controller_status(&output.stdout)
        .map_err(|error| format!("candidate controller status is invalid: {error}"))?;
    if status.vm_name != configured.vm_name {
        return Err("controller status returned the wrong VM identity".to_owned());
    }
    Ok(Evidence {
        version: EVIDENCE_VERSION,
        unit,
        active_state: one(&properties, "ActiveState")?.to_owned(),
        sub_state: one(&properties, "SubState")?.to_owned(),
        unit_file_state: one(&properties, "UnitFileState")?.to_owned(),
        status,
    })
}

fn run_elevated(command: &Command, repo: &Path) -> Result<String, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("resolve xtask executable: {error}"))?;
    let mut args = vec![process::os("--"), executable.into_os_string()];
    args.extend(elevated_arguments(command));
    process::elevated_text(
        &args,
        repo,
        command
            .timeout
            .checked_mul(4)
            .ok_or("elevated workflow timeout overflowed")?,
    )
}

fn elevated_arguments(command: &Command) -> Vec<OsString> {
    let mut arguments = vec![process::os("controller-status")];
    if let Some(vm) = &command.vm {
        arguments.extend([process::os("--vm"), process::os(vm)]);
    }
    arguments.extend([
        process::os("--host-cli"),
        command.host_cli.as_os_str().to_owned(),
        process::os("--output"),
        command.output.as_os_str().to_owned(),
        process::os("--command-timeout-seconds"),
        process::os(command.timeout.as_secs().to_string()),
        process::os("--connect"),
        process::os(&command.connection),
        process::os("--elevated-child"),
    ]);
    arguments
}

fn parse_properties(text: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed systemd property: {line}"))?;
        if values.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(format!("duplicate systemd property: {name}"));
        }
    }
    Ok(values)
}

fn one<'a>(
    values: &'a std::collections::BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, String> {
    values
        .get(name)
        .filter(|value| !value.is_empty())
        .map(String::as_str)
        .ok_or_else(|| format!("systemd output lacks {name}"))
}

fn persist(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "controller status output requires a parent directory".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create controller status evidence directory: {error}"))?;
    let temporary = parent.join(format!(".controller-status-{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("create controller status temporary file: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("write controller status evidence: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("flush controller status evidence: {error}"))?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("publish controller status evidence: {error}")
    })?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("flush controller status evidence directory: {error}"))
}

fn resolve(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

fn validate_identity(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        Ok(())
    } else {
        Err("controller-status VM contains unsafe characters".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_a_fixed_read_only_elevation_scope() {
        let command = parse(&args(&[
            "--vm",
            "guest",
            "--host-cli",
            "/workspace/target/release/rampire",
            "--output",
            ".artifacts/status.json",
            "--command-timeout-seconds",
            "10",
            "--connect",
            "qemu:///system",
            "--elevate",
        ]))
        .expect("valid command");
        assert_eq!(command.vm.as_deref(), Some("guest"));
        assert!(elevated_arguments(&command).contains(&process::os("guest")));
        assert_eq!(command.timeout, Duration::from_secs(10));
        assert!(command.elevate);
        assert!(!command.elevated_child);
    }

    #[test]
    fn rejects_implicit_or_unsafe_scope() {
        assert!(parse(&args(&["guest"])).is_err());
        assert!(parse(&args(&[
            "bad/guest",
            "--host-cli",
            "/tmp/host",
            "--output",
            "status.json",
            "--command-timeout-seconds",
            "10"
        ]))
        .is_err());
        assert!(parse(&args(&[
            "guest",
            "--host-cli",
            "relative/host",
            "--output",
            "status.json",
            "--command-timeout-seconds",
            "10"
        ]))
        .is_err());
    }

    #[test]
    fn parses_systemd_properties_strictly() {
        let parsed =
            parse_properties("ActiveState=inactive\nSubState=dead\nUnitFileState=disabled\n")
                .expect("properties");
        assert_eq!(one(&parsed, "ActiveState"), Ok("inactive"));
        assert!(parse_properties("ActiveState=inactive\nActiveState=active\n").is_err());
    }
}
