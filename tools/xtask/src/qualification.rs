use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rampire::config::HostConfig;
use rampire::guest_channel::PublicationCopyFirst;
use rampire::host_memory::meminfo_bytes;
use rampire::qga::{GuestFileReader, VirshGuestFiles};
use rampire::virsh::Virsh;
use rampire_core::settings::GuestOs;
use rampire_core::{parse_virtio_mem_xml_for_alias, RawTelemetryEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::process;

const SCHEMA_VERSION: u32 = 4;
const CONTROLLER_GUARD_ROOT: &str = "/run/rampire-qualification";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WorkloadMode {
    Committed,
    Resident,
    /// Linux only: files read through repeatedly, so page cache is the
    /// working set.
    FileCache,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Config {
    version: u32,
    run_id: String,
    vm_name: String,
    device_alias: String,
    ssh_target: String,
    connect_uri: String,
    controller_unit: String,
    /// Runs before Linux support name no guest and were Windows.
    #[serde(default)]
    guest_os: GuestOs,
    guest_service: String,
    #[serde(default)]
    apply_service_restart: bool,
    #[serde(default)]
    guest_service_cycle_timeout_seconds: Option<u64>,
    remote_workload: String,
    telemetry_path: String,
    telemetry_max_age_seconds: u64,
    telemetry_future_tolerance_seconds: u64,
    mode: WorkloadMode,
    /// Where the file-cache workload writes its files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_dir: Option<String>,
    peak_bytes: u64,
    retained_bytes: u64,
    max_allocation_bytes: u64,
    peak_hold_seconds: u64,
    settled_hold_seconds: u64,
    renewed_hold_seconds: u64,
    resident_refresh_seconds: u64,
    post_hold_seconds: u64,
    interval_seconds: u64,
    command_timeout_seconds: u64,
    controller_timeout_seconds: u64,
    expect_growth_bytes: u64,
    expect_reclaim_bytes: u64,
    expect_renewed_growth_bytes: u64,
    #[serde(default)]
    expect_no_resize: bool,
    #[serde(default)]
    expect_no_reclaim: bool,
    #[serde(default)]
    require_renewed_during_pending_shrink: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Status {
    version: u32,
    run_id: String,
    state: String,
    pid: Option<u32>,
    started_unix_millis: Option<u64>,
    finished_unix_millis: Option<u64>,
    final_result: Option<String>,
    message: String,
}

#[derive(Debug)]
struct StartOptions {
    config: Config,
    output_root: PathBuf,
    apply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControllerGuardStatus {
    version: u32,
    run_id: String,
    state: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiveStateEvidence {
    version: u32,
    run_id: String,
    stage: String,
    unix_millis: u64,
    vm_state: String,
    controller_active_state: String,
    controller_unit_file_state: String,
    requested_bytes: u64,
    current_bytes: u64,
    device_size_bytes: u64,
    block_size_bytes: u64,
    host_mem_available_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrivilegedSample {
    version: u32,
    run_id: String,
    unix_millis: u64,
    vm_state: String,
    controller_active_state: String,
    requested_bytes: u64,
    current_bytes: u64,
    device_size_bytes: u64,
    block_size_bytes: u64,
    host_mem_available_bytes: Option<u64>,
    guest_dommemstat_kib: serde_json::Map<String, Value>,
    #[serde(alias = "windows_raw_telemetry")]
    guest_raw_telemetry: RawTelemetryEnvelope,
    qga_reply: String,
}

#[derive(Debug, Default)]
struct Observations {
    initial_current: Option<u64>,
    maximum_current: u64,
    maximum_peak_current: u64,
    minimum_settled_current: Option<u64>,
    minimum_settled_phase_current: Option<u64>,
    maximum_renewed_current: Option<u64>,
    workload_phases: Vec<String>,
    sample_count: u64,
    warnings: u64,
    last_requested: Option<u64>,
    last_privileged_sample_millis: Option<u64>,
    memory_samples: Vec<MemorySample>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct MemorySample {
    phase: String,
    requested_bytes: u64,
    current_bytes: u64,
    telemetry_session_id: String,
    telemetry_sequence: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct QualificationAnalysis {
    initial_current_bytes: Option<u64>,
    maximum_peak_current_bytes: u64,
    minimum_settled_current_bytes: Option<u64>,
    minimum_settled_phase_current_bytes: Option<u64>,
    maximum_renewed_current_bytes: Option<u64>,
    observed_growth_bytes: u64,
    observed_reclaim_bytes: u64,
    observed_renewed_growth_bytes: u64,
    renewed_during_observed_pending_shrink: bool,
    lower_request_while_pending_violations: u64,
    lower_request_violations: u64,
    telemetry_session_changes: u64,
    telemetry_sequence_regressions: u64,
    requested_change_violations: u64,
    current_change_violations: u64,
    unconverged_sample_violations: u64,
}

pub fn execute(arguments: &[String], repo: &Path) -> Result<(), String> {
    match arguments.first().map(String::as_str) {
        Some("start") => start(parse_start(&arguments[1..], repo)?, repo),
        Some("run") => run_internal(&arguments[1..], repo),
        Some("guard") => guard_internal(&arguments[1..], repo),
        Some("status") => show_status(&arguments[1..], repo),
        Some("review") => review(&arguments[1..], repo),
        _ => Err("qualification requires start, status, or review".to_owned()),
    }
}

fn parse_start(args: &[String], repo: &Path) -> Result<StartOptions, String> {
    if args.len() < 2 {
        return Err(
            "qualification start requires VM_NAME ALIAS and --ssh-target TARGET".to_owned(),
        );
    }
    let vm_name = scope(&args[0], "VM_NAME")?;
    let device_alias = identifier(&args[1], "ALIAS")?;
    let mut ssh_target = None;
    let mut guest_os = GuestOs::Windows;
    let mut cache_dir = None;
    let mut mode = None;
    let mut peak_bytes = None;
    let mut retained_bytes = None;
    let mut max_allocation_bytes = None;
    let mut peak_hold_seconds = None;
    let mut settled_hold_seconds = None;
    let mut renewed_hold_seconds = None;
    let mut resident_refresh_seconds = None;
    let mut post_hold_seconds = None;
    let mut interval_seconds = None;
    let mut command_timeout_seconds = None;
    let mut controller_timeout_seconds = None;
    let mut expect_growth_bytes = None;
    let mut expect_reclaim_bytes = None;
    let mut expect_renewed_growth_bytes = None;
    let mut expect_no_resize = false;
    let mut expect_no_reclaim = false;
    let mut require_renewed_during_pending_shrink = false;
    let mut remote_workload = None;
    let mut controller_unit = None;
    let mut guest_service = None;
    let mut apply_service_restart = false;
    let mut guest_service_cycle_timeout_seconds = None;
    let mut telemetry_path = None;
    let mut telemetry_max_age_seconds = None;
    let mut telemetry_future_tolerance_seconds = None;
    let mut connect_uri = "qemu:///system".to_owned();
    let mut output_root = repo.join(".artifacts/qualification");
    let mut apply = false;
    let mut elevate = false;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--apply" => apply = true,
            "--elevate" => elevate = true,
            "--ssh-target" => ssh_target = Some(value(args, &mut index, "--ssh-target")?),
            "--mode" => {
                mode = Some(match value(args, &mut index, "--mode")?.as_str() {
                    "resident" => WorkloadMode::Resident,
                    "committed" => WorkloadMode::Committed,
                    "file-cache" => WorkloadMode::FileCache,
                    _ => return Err("--mode must be resident, committed or file-cache".to_owned()),
                })
            }
            "--guest-os" => {
                guest_os = match value(args, &mut index, "--guest-os")?.as_str() {
                    "windows" => GuestOs::Windows,
                    "linux" => GuestOs::Linux,
                    _ => return Err("--guest-os must be windows or linux".to_owned()),
                }
            }
            "--cache-dir" => cache_dir = Some(value(args, &mut index, "--cache-dir")?),
            "--peak-bytes" => peak_bytes = Some(number(args, &mut index, "--peak-bytes")?),
            "--retained-bytes" => {
                retained_bytes = Some(number(args, &mut index, "--retained-bytes")?)
            }
            "--max-allocation-bytes" => {
                max_allocation_bytes = Some(number(args, &mut index, "--max-allocation-bytes")?)
            }
            "--peak-hold-seconds" => {
                peak_hold_seconds = Some(number(args, &mut index, "--peak-hold-seconds")?)
            }
            "--settled-hold-seconds" => {
                settled_hold_seconds = Some(number(args, &mut index, "--settled-hold-seconds")?)
            }
            "--renewed-hold-seconds" => {
                renewed_hold_seconds = Some(number(args, &mut index, "--renewed-hold-seconds")?)
            }
            "--resident-refresh-seconds" => {
                resident_refresh_seconds =
                    Some(number(args, &mut index, "--resident-refresh-seconds")?)
            }
            "--post-hold-seconds" => {
                post_hold_seconds = Some(number(args, &mut index, "--post-hold-seconds")?)
            }
            "--interval-seconds" => {
                interval_seconds = Some(number(args, &mut index, "--interval-seconds")?)
            }
            "--command-timeout-seconds" => {
                command_timeout_seconds =
                    Some(number(args, &mut index, "--command-timeout-seconds")?)
            }
            "--controller-timeout-seconds" => {
                controller_timeout_seconds =
                    Some(number(args, &mut index, "--controller-timeout-seconds")?)
            }
            "--expect-growth-bytes" => {
                expect_growth_bytes = Some(number(args, &mut index, "--expect-growth-bytes")?)
            }
            "--expect-reclaim-bytes" => {
                expect_reclaim_bytes = Some(number(args, &mut index, "--expect-reclaim-bytes")?)
            }
            "--expect-renewed-growth-bytes" => {
                expect_renewed_growth_bytes =
                    Some(number(args, &mut index, "--expect-renewed-growth-bytes")?)
            }
            "--expect-no-resize" if !expect_no_resize => expect_no_resize = true,
            "--expect-no-reclaim" if !expect_no_reclaim => expect_no_reclaim = true,
            "--require-renewed-during-pending-shrink" => {
                require_renewed_during_pending_shrink = true
            }
            "--remote-workload" => {
                remote_workload = Some(value(args, &mut index, "--remote-workload")?)
            }
            "--controller-unit" => {
                controller_unit = Some(value(args, &mut index, "--controller-unit")?)
            }
            "--guest-service" => guest_service = Some(value(args, &mut index, "--guest-service")?),
            "--apply-service-restart" => apply_service_restart = true,
            "--guest-service-cycle-timeout-seconds" => {
                guest_service_cycle_timeout_seconds = Some(number(
                    args,
                    &mut index,
                    "--guest-service-cycle-timeout-seconds",
                )?)
            }
            "--telemetry-path" => {
                telemetry_path = Some(value(args, &mut index, "--telemetry-path")?)
            }
            "--telemetry-max-age-seconds" => {
                telemetry_max_age_seconds =
                    Some(number(args, &mut index, "--telemetry-max-age-seconds")?)
            }
            "--telemetry-future-tolerance-seconds" => {
                telemetry_future_tolerance_seconds = Some(number(
                    args,
                    &mut index,
                    "--telemetry-future-tolerance-seconds",
                )?)
            }
            "--connect" => connect_uri = value(args, &mut index, "--connect")?,
            "--output-root" => {
                output_root = absolute_or_repo(repo, &value(args, &mut index, "--output-root")?)
            }
            option => return Err(format!("unknown qualification option: {option}")),
        }
        index += 1;
    }
    let ssh_target = scope(
        &ssh_target.ok_or_else(|| {
            "--ssh-target is required; refusing to infer a guest endpoint".to_owned()
        })?,
        "--ssh-target",
    )?;
    let mode = required(mode, "--mode")?;
    let peak_bytes = required(peak_bytes, "--peak-bytes")?;
    let retained_bytes = required(retained_bytes, "--retained-bytes")?;
    let max_allocation_bytes = required(max_allocation_bytes, "--max-allocation-bytes")?;
    let peak_hold_seconds = required(peak_hold_seconds, "--peak-hold-seconds")?;
    let settled_hold_seconds = required(settled_hold_seconds, "--settled-hold-seconds")?;
    let renewed_hold_seconds = required(renewed_hold_seconds, "--renewed-hold-seconds")?;
    let resident_refresh_seconds =
        required(resident_refresh_seconds, "--resident-refresh-seconds")?;
    let post_hold_seconds = required(post_hold_seconds, "--post-hold-seconds")?;
    let interval_seconds = required(interval_seconds, "--interval-seconds")?;
    let command_timeout_seconds = required(command_timeout_seconds, "--command-timeout-seconds")?;
    let controller_timeout_seconds =
        required(controller_timeout_seconds, "--controller-timeout-seconds")?;
    let (expect_growth_bytes, expect_reclaim_bytes, expect_renewed_growth_bytes) =
        if expect_no_resize {
            if expect_growth_bytes.is_some()
                || expect_reclaim_bytes.is_some()
                || expect_renewed_growth_bytes.is_some()
            {
                return Err(
                    "--expect-no-resize conflicts with resize expectation options".to_owned(),
                );
            }
            (0, 0, 0)
        } else if expect_no_reclaim {
            if expect_reclaim_bytes.is_some() {
                return Err("--expect-no-reclaim conflicts with --expect-reclaim-bytes".to_owned());
            }
            (
                required(expect_growth_bytes, "--expect-growth-bytes")?,
                0,
                required(expect_renewed_growth_bytes, "--expect-renewed-growth-bytes")?,
            )
        } else {
            (
                required(expect_growth_bytes, "--expect-growth-bytes")?,
                required(expect_reclaim_bytes, "--expect-reclaim-bytes")?,
                required(expect_renewed_growth_bytes, "--expect-renewed-growth-bytes")?,
            )
        };
    let remote_workload = required(remote_workload, "--remote-workload")?;
    let controller_unit = required(controller_unit, "--controller-unit")?;
    let guest_service = required(guest_service, "--guest-service")?;
    let telemetry_path = required(telemetry_path, "--telemetry-path")?;
    let telemetry_max_age_seconds =
        required(telemetry_max_age_seconds, "--telemetry-max-age-seconds")?;
    let telemetry_future_tolerance_seconds = required(
        telemetry_future_tolerance_seconds,
        "--telemetry-future-tolerance-seconds",
    )?;
    match guest_os {
        GuestOs::Windows => {
            if remote_workload.is_empty()
                || remote_workload.chars().any(char::is_control)
                || remote_workload.chars().any(|c| "&|<>^%!\"".contains(c))
            {
                return Err("--remote-workload contains characters unsafe for cmd.exe".to_owned());
            }
            rampire_core::windows_service_config::normalize_windows_path(&telemetry_path)?;
            if mode == WorkloadMode::FileCache || cache_dir.is_some() {
                return Err("the file-cache workload is for Linux guests".to_owned());
            }
        }
        GuestOs::Linux => {
            for (option, path) in [
                ("--remote-workload", Some(&remote_workload)),
                ("--telemetry-path", Some(&telemetry_path)),
                ("--cache-dir", cache_dir.as_ref()),
            ] {
                if let Some(path) = path {
                    posix_path(path, option)?;
                }
            }
            if (mode == WorkloadMode::FileCache) != cache_dir.is_some() {
                return Err("--mode file-cache and --cache-dir go together".to_owned());
            }
            if apply_service_restart {
                return Err(
                    "--apply-service-restart restarts a Windows service; it is not available for Linux guests"
                        .to_owned(),
                );
            }
        }
    }
    scope(&connect_uri, "--connect")?;
    identifier(&controller_unit, "--controller-unit")?;
    identifier(&guest_service, "--guest-service")?;
    // The unit may run a shared config; the running config must contain the VM.
    crate::host_deploy::controller_instance(&controller_unit)?;
    if max_allocation_bytes == 0
        || peak_bytes == 0
        || peak_bytes > max_allocation_bytes
        || retained_bytes == 0
        || retained_bytes >= peak_bytes
    {
        return Err("workload bytes require 0 < retained < peak <= max allocation".to_owned());
    }
    for (name, seconds) in [
        ("peak", peak_hold_seconds),
        ("settled", settled_hold_seconds),
        ("renewed", renewed_hold_seconds),
    ] {
        if seconds == 0 {
            return Err(format!("{name} hold must be positive"));
        }
    }
    if resident_refresh_seconds == 0
        || interval_seconds == 0
        || command_timeout_seconds == 0
        || controller_timeout_seconds == 0
        || telemetry_max_age_seconds == 0
    {
        return Err("refresh, sampling, and command timeout values must be positive".to_owned());
    }
    if !expect_no_resize
        && (expect_growth_bytes == 0
            || (!expect_no_reclaim && expect_reclaim_bytes == 0)
            || expect_renewed_growth_bytes == 0)
    {
        return Err("resize expectations must be positive".to_owned());
    }
    if (expect_no_resize || expect_no_reclaim) && require_renewed_during_pending_shrink {
        return Err(
            "no-resize/no-reclaim qualification conflicts with --require-renewed-during-pending-shrink"
                .to_owned(),
        );
    }
    if expect_no_resize && expect_no_reclaim {
        return Err("--expect-no-resize conflicts with --expect-no-reclaim".to_owned());
    }
    match (apply_service_restart, guest_service_cycle_timeout_seconds) {
        (true, Some(0)) => {
            return Err("--guest-service-cycle-timeout-seconds must be positive".to_owned())
        }
        (true, None) => {
            return Err(
                "--apply-service-restart requires --guest-service-cycle-timeout-seconds".to_owned(),
            )
        }
        (false, Some(_)) => {
            return Err(
                "--guest-service-cycle-timeout-seconds requires --apply-service-restart".to_owned(),
            )
        }
        _ => {}
    }
    let minimum_controller_seconds = peak_hold_seconds
        .checked_add(settled_hold_seconds)
        .and_then(|value| value.checked_add(renewed_hold_seconds))
        .and_then(|value| value.checked_add(post_hold_seconds))
        .and_then(|value| {
            value.checked_add(guest_service_cycle_timeout_seconds.unwrap_or_default())
        })
        .and_then(|value| value.checked_add(command_timeout_seconds.checked_mul(10)?))
        .ok_or_else(|| "configured qualification duration overflows".to_owned())?;
    if controller_timeout_seconds < minimum_controller_seconds {
        return Err(format!(
            "--controller-timeout-seconds must be at least {minimum_controller_seconds} for the configured run"
        ));
    }
    if apply && !elevate {
        return Err(
            "qualification --apply requires --elevate for bounded controller ownership".to_owned(),
        );
    }
    if elevate && !apply {
        return Err("qualification --elevate requires --apply".to_owned());
    }
    let run_id = new_run_id()?;
    Ok(StartOptions {
        config: Config {
            version: SCHEMA_VERSION,
            run_id,
            vm_name,
            device_alias,
            ssh_target,
            connect_uri,
            controller_unit,
            guest_os,
            guest_service,
            apply_service_restart,
            guest_service_cycle_timeout_seconds,
            remote_workload,
            telemetry_path,
            telemetry_max_age_seconds,
            telemetry_future_tolerance_seconds,
            mode,
            cache_dir,
            peak_bytes,
            retained_bytes,
            max_allocation_bytes,
            peak_hold_seconds,
            settled_hold_seconds,
            renewed_hold_seconds,
            resident_refresh_seconds,
            post_hold_seconds,
            interval_seconds,
            command_timeout_seconds,
            controller_timeout_seconds,
            expect_growth_bytes,
            expect_reclaim_bytes,
            expect_renewed_growth_bytes,
            expect_no_resize,
            expect_no_reclaim,
            require_renewed_during_pending_shrink,
        },
        output_root,
        apply,
    })
}

fn start(options: StartOptions, repo: &Path) -> Result<(), String> {
    if !options.apply {
        println!(
            "DRY RUN: qualification configuration is valid; add --apply to start it.\n{}",
            serde_json::to_string_pretty(&options.config).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if !process::command_exists("setsid") {
        return Err("setsid is required for an unattended qualification run".to_owned());
    }
    let run_dir = options.output_root.join(&options.config.run_id);
    std::fs::create_dir_all(&run_dir).map_err(|e| format!("create {}: {e}", run_dir.display()))?;
    write_json(&run_dir.join("config.json"), &options.config)?;
    let initial_status = Status {
        version: SCHEMA_VERSION,
        run_id: options.config.run_id.clone(),
        state: "starting".to_owned(),
        pid: None,
        started_unix_millis: None,
        finished_unix_millis: None,
        final_result: None,
        message: "detached supervisor is starting".to_owned(),
    };
    write_json(&run_dir.join("status.json"), &initial_status)?;
    let preparation = remote_guest_health(&options.config, repo, "before_controller");
    if let Err(error) = preparation {
        finish_start_failure(&run_dir, initial_status, &error)?;
        return Err(error);
    }
    if let Err(error) =
        launch_controller_guard(&run_dir, repo, options.config.command_timeout_seconds)
    {
        finish_start_failure(&run_dir, initial_status, &error)?;
        return Err(error);
    }
    let preparation = persist_guard_initial_state(&options.config, &run_dir).and_then(|initial| {
        preflight_guest_health(
            &options.config,
            &run_dir,
            repo,
            "before_controller",
            &initial.qga_reply,
        )
    });
    if let Err(error) = preparation {
        let release = write_release(&run_dir);
        let cleanup = wait_for_guard_stopped(
            &run_dir,
            &options.config,
            Duration::from_secs(options.config.command_timeout_seconds)
                .checked_mul(3)
                .ok_or_else(|| "controller cleanup timeout overflowed".to_owned())?,
            Duration::from_secs(options.config.interval_seconds),
        );
        let combined = format!("{error}; controller release={release:?}; cleanup={cleanup:?}");
        finish_start_failure(&run_dir, initial_status, &combined)?;
        return Err(combined);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("supervisor.log"))
        .map_err(|e| format!("open supervisor log: {e}"))?;
    let error_log = log
        .try_clone()
        .map_err(|e| format!("clone supervisor log: {e}"))?;
    let executable =
        std::env::current_exe().map_err(|e| format!("locate xtask executable: {e}"))?;
    let child = match ProcessCommand::new("setsid")
        .arg(executable)
        .args(["qualification", "run", "--run-dir"])
        .arg(&run_dir)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let launch_error = format!("start detached supervisor: {error}");
            let release = write_release(&run_dir);
            let cleanup = wait_for_guard_stopped(
                &run_dir,
                &options.config,
                Duration::from_secs(options.config.command_timeout_seconds)
                    .checked_mul(3)
                    .ok_or_else(|| "controller cleanup timeout overflowed".to_owned())?,
                Duration::from_secs(options.config.interval_seconds),
            );
            let combined =
                format!("{launch_error}; controller release={release:?}; cleanup={cleanup:?}");
            finish_start_failure(&run_dir, initial_status, &combined)?;
            return Err(combined);
        }
    };
    let mut status = initial_status;
    status.pid = Some(child.id());
    status.message = "detached supervisor launched".to_owned();
    write_json(&run_dir.join("status.json"), &status)?;
    println!(
        "qualification run started\nrun_id={}\noutput_dir={}\npid={}",
        options.config.run_id,
        run_dir.display(),
        child.id()
    );
    Ok(())
}

fn launch_controller_guard(
    run_dir: &Path,
    repo: &Path,
    timeout_seconds: u64,
) -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("locate xtask executable: {error}"))?;
    let mut child = ProcessCommand::new("sudo")
        .arg("--")
        .arg(executable)
        .args(["qualification", "guard", "--run-dir"])
        .arg(run_dir)
        .arg("--launch")
        .current_dir(repo)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start elevated qualification controller guard: {error}"))?;
    let timeout = Duration::from_secs(timeout_seconds)
        .checked_mul(4)
        .ok_or_else(|| "controller guard launch timeout overflowed".to_owned())?;
    let status = child
        .wait_timeout(timeout)
        .map_err(|error| format!("wait for elevated qualification controller guard: {error}"))?
        .ok_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            format!("elevated qualification controller guard timed out after {timeout:?}")
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "elevated qualification controller guard failed with {status}"
        ))
    }
}

fn guard_internal(args: &[String], repo: &Path) -> Result<(), String> {
    if args.len() != 3
        || args[0] != "--run-dir"
        || !matches!(args[2].as_str(), "--launch" | "--serve")
    {
        return Err(
            "qualification guard is internal and requires --run-dir PATH --launch|--serve"
                .to_owned(),
        );
    }
    if unsafe { libc_geteuid() } != 0 {
        return Err("qualification controller guard requires effective uid 0".to_owned());
    }
    let run_dir = PathBuf::from(&args[1]);
    let config: Config = serde_json::from_str(&process::read_file(&run_dir.join("config.json"))?)
        .map_err(|error| format!("parse qualification config: {error}"))?;
    if run_dir.file_name().and_then(|value| value.to_str()) != Some(config.run_id.as_str()) {
        return Err("qualification run directory does not match the configured run ID".to_owned());
    }
    if args[2] == "--serve" {
        return serve_controller_guard(&config, &run_dir, repo);
    }
    launch_controller_guard_daemon(&config, &run_dir, repo)
}

fn launch_controller_guard_daemon(
    config: &Config,
    run_dir: &Path,
    repo: &Path,
) -> Result<(), String> {
    ensure_guard_root()?;
    let properties = systemd_properties(
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )?;
    if properties.get("ActiveState").map(String::as_str) != Some("inactive")
        || properties.get("UnitFileState").map(String::as_str) != Some("disabled")
    {
        return Err(format!(
            "qualification requires disabled/inactive {}; got UnitFileState={} ActiveState={}",
            config.controller_unit,
            properties
                .get("UnitFileState")
                .map_or("missing", String::as_str),
            properties
                .get("ActiveState")
                .map_or("missing", String::as_str)
        ));
    }
    let initial = capture_privileged_sample(config, repo)?;
    if initial.requested_bytes != initial.current_bytes {
        return Err(format!(
            "qualification guard found divergent live state; requested={} current={}",
            initial.requested_bytes, initial.current_bytes
        ));
    }
    write_json(&guard_initial_path(config), &initial)?;
    write_root_guard_status(
        run_dir,
        config,
        "starting",
        "starting the exclusive controller",
    )?;
    if let Err(error) = systemctl_action(
        "start",
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )
    .and_then(|_| require_controller_active_and_shrink(config, repo))
    {
        let cleanup = systemctl_action(
            "stop",
            &config.controller_unit,
            repo,
            config.command_timeout_seconds,
        );
        write_root_guard_status(
            run_dir,
            config,
            "failed",
            &format!("{error}; cleanup={cleanup:?}"),
        )?;
        return Err(error);
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("locate xtask executable: {error}"))?;
    if let Err(error) = ProcessCommand::new("setsid")
        .arg(executable)
        .args(["qualification", "guard", "--run-dir"])
        .arg(run_dir)
        .arg("--serve")
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let _ = systemctl_action(
            "stop",
            &config.controller_unit,
            repo,
            config.command_timeout_seconds,
        );
        return Err(format!("start detached controller guard: {error}"));
    }
    write_root_guard_status(
        run_dir,
        config,
        "running",
        "exclusive controller is active under the bounded guard",
    )
}

fn serve_controller_guard(config: &Config, run_dir: &Path, repo: &Path) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(config.controller_timeout_seconds))
        .ok_or_else(|| "controller guard deadline exceeds the platform clock range".to_owned())?;
    let release = run_dir.join("controller-release");
    let sample_interval = Duration::from_secs(config.interval_seconds);
    let mut sampling_error = None;
    let mut first_sampling_failure = None;
    let sampling_failure_bound = Duration::from_secs(config.command_timeout_seconds)
        .checked_mul(3)
        .ok_or_else(|| "sampling failure bound overflowed".to_owned())?;
    while !release.is_file() && Instant::now() < deadline {
        match capture_privileged_sample(config, repo) {
            Ok(sample) => {
                if let Err(error) = write_json(&guard_sample_path(config), &sample) {
                    sampling_error = Some(error);
                    break;
                }
                first_sampling_failure = None;
            }
            Err(error) => {
                let first_failure = first_sampling_failure.get_or_insert_with(Instant::now);
                if first_failure.elapsed() >= sampling_failure_bound {
                    sampling_error = Some(error);
                    break;
                }
            }
        }
        thread::sleep(sample_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    let timed_out = !release.is_file();
    let cleanup = systemctl_action(
        "stop",
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    );
    let reset_failed = systemd_properties(
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )
    .and_then(|values| {
        if values.get("ActiveState").map(String::as_str) == Some("failed") {
            systemctl_action(
                "reset-failed",
                &config.controller_unit,
                repo,
                config.command_timeout_seconds,
            )
        } else {
            Ok(())
        }
    });
    let inactive = systemd_properties(
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )
    .and_then(|values| {
        (values.get("ActiveState").map(String::as_str) == Some("inactive"))
            .then_some(())
            .ok_or_else(|| "controller did not return to inactive state".to_owned())
    });
    let final_state = capture_live_state_direct(config, repo, "after_controller")
        .and_then(|state| write_json(&guard_final_path(config), &state));
    match (
        timed_out,
        sampling_error,
        cleanup,
        reset_failed,
        inactive,
        final_state,
    ) {
        (false, None, Ok(()), Ok(()), Ok(()), Ok(())) => write_root_guard_status(
            run_dir,
            config,
            "stopped",
            "controller stopped and the pre-run inactive state was restored",
        ),
        (deadline_elapsed, sampling, stop, reset, state, final_state) => {
            let message = format!(
                "controller guard failed: deadline_elapsed={deadline_elapsed} sampling={sampling:?} stop={stop:?} reset_failed={reset:?} inactive={state:?} final_state={final_state:?}"
            );
            write_root_guard_status(run_dir, config, "failed", &message)?;
            Err(message)
        }
    }
}

fn run_internal(args: &[String], repo: &Path) -> Result<(), String> {
    if args.len() != 2 || args[0] != "--run-dir" {
        return Err("qualification run is an internal command requiring --run-dir PATH".to_owned());
    }
    let run_dir = PathBuf::from(&args[1]);
    let config: Config = serde_json::from_str(&process::read_file(&run_dir.join("config.json"))?)
        .map_err(|e| format!("parse qualification config: {e}"))?;
    wait_for_launcher_status(
        &run_dir.join("status.json"),
        Duration::from_secs(config.command_timeout_seconds),
        Duration::from_secs(config.interval_seconds),
    )?;
    let started = now_millis()?;
    let mut status = Status {
        version: SCHEMA_VERSION,
        run_id: config.run_id.clone(),
        state: "running".to_owned(),
        pid: Some(std::process::id()),
        started_unix_millis: Some(started),
        finished_unix_millis: None,
        final_result: None,
        message: "preflight".to_owned(),
    };
    write_json(&run_dir.join("status.json"), &status)?;
    event(
        &run_dir,
        "info",
        "run_started",
        json!({"configuration": config}),
    )?;
    let result = supervise(&config, &run_dir, repo);
    let finished = now_millis()?;
    status.state = "finished".to_owned();
    status.finished_unix_millis = Some(finished);
    status.final_result = Some(if result.is_ok() { "pass" } else { "fail" }.to_owned());
    status.message = result.as_ref().map_or_else(
        |e| e.clone(),
        |_| "qualification criteria passed".to_owned(),
    );
    write_json(&run_dir.join("status.json"), &status)?;
    let observations = process::read_file(&run_dir.join("result.json"))
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok());
    let artifacts = available_artifacts(&run_dir);
    let summary = json!({"version": SCHEMA_VERSION, "run_id": config.run_id, "test": "automatic_controller_workload_resize", "started_unix_millis": started, "finished_unix_millis": finished, "duration_millis": finished.saturating_sub(started), "status": status.final_result, "message": status.message, "observations": observations, "artifacts": artifacts});
    write_json(&run_dir.join("summary.json"), &summary)?;
    event(
        &run_dir,
        if result.is_ok() { "info" } else { "failure" },
        "run_finished",
        summary,
    )?;
    result
}

fn supervise(config: &Config, run_dir: &Path, repo: &Path) -> Result<(), String> {
    let result = supervise_inner(config, run_dir, repo);
    let release = write_release(run_dir);
    let cleanup = wait_for_guard_stopped(
        run_dir,
        config,
        Duration::from_secs(config.command_timeout_seconds)
            .checked_mul(3)
            .ok_or_else(|| "controller cleanup timeout overflowed".to_owned())?,
        Duration::from_secs(config.interval_seconds),
    );
    archive_controller_log(config, run_dir, repo);
    let final_state = persist_guard_final_state(config, run_dir);
    let final_no_resize = validate_final_no_resize(config, run_dir);
    match (result, release, cleanup, final_state, final_no_resize) {
        (Ok(()), Ok(()), Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(error), _, Ok(()), Ok(()), Ok(())) => Err(error),
        (result, release, cleanup, final_state, final_no_resize) => Err(format!(
            "qualification result={result:?}; controller release={release:?}; cleanup={cleanup:?}; final_state={final_state:?}; final_no_resize={final_no_resize:?}"
        )),
    }
}

fn supervise_inner(config: &Config, run_dir: &Path, repo: &Path) -> Result<(), String> {
    for command in ["ssh", "virsh"] {
        if !process::command_exists(command) {
            return Err(format!("missing prerequisite: {command}"));
        }
    }
    if config.apply_service_restart {
        let started = now_millis()?;
        let timeout = config
            .guest_service_cycle_timeout_seconds
            .ok_or_else(|| "guest service-cycle timeout is missing".to_owned())?;
        let service_cycle = crate::windows::apply_service_cycle_for_qualification(
            repo,
            &config.ssh_target,
            &config.guest_service,
            config.command_timeout_seconds,
            timeout,
        )?;
        event(run_dir, "info", "guest_service_restarted", service_cycle)?;
        let decision = wait_for_controller_decision(config, repo, started, timeout)?;
        event(
            run_dir,
            "info",
            "controller_telemetry_handoff",
            json!({"controller_log": decision}),
        )?;
    }
    preflight_health(config, run_dir, repo, "before_workload")?;
    let mut observations = Observations::default();
    sample_host(config, run_dir, repo, "baseline", &mut observations)?;
    let remote = workload_command(config);
    event(
        run_dir,
        "info",
        "workload_start",
        json!({"ssh_target": config.ssh_target, "command": remote}),
    )?;
    let connect_timeout = format!("ConnectTimeout={}", config.command_timeout_seconds);
    let mut child = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &connect_timeout,
            "--",
            &config.ssh_target,
            &remote,
        ])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start remote workload: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture workload stdout".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture workload stderr".to_owned())?;
    let (sender, receiver) = mpsc::channel();
    spawn_reader(stdout, false, sender.clone());
    spawn_reader(stderr, true, sender);
    let started = Instant::now();
    let max_duration_seconds = config
        .peak_hold_seconds
        .checked_add(config.settled_hold_seconds)
        .and_then(|value| value.checked_add(config.renewed_hold_seconds))
        .and_then(|value| value.checked_add(config.command_timeout_seconds))
        .ok_or_else(|| "configured workload deadline overflows".to_owned())?;
    let max_duration = Duration::from_secs(max_duration_seconds);
    let interval = Duration::from_secs(config.interval_seconds);
    let mut next_sample = Instant::now() + interval;
    let mut phase = "baseline".to_owned();
    let exit_status;
    loop {
        while let Ok((is_error, line)) = receiver.try_recv() {
            if is_error {
                observations.warnings += 1;
                event(
                    run_dir,
                    "warning",
                    "workload_stderr",
                    json!({"message": line}),
                )?;
            } else {
                append_line(&run_dir.join("workload.jsonl"), &line)?;
                let parsed: Value = serde_json::from_str(&line)
                    .map_err(|e| format!("workload emitted invalid JSON: {e}: {line}"))?;
                phase = parsed
                    .get("phase")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "workload record has no phase".to_owned())?
                    .to_owned();
                observations.workload_phases.push(phase.clone());
                event(run_dir, "info", "workload_phase", parsed)?;
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("poll workload: {e}"))?
        {
            exit_status = status;
            break;
        }
        if started.elapsed() > max_duration {
            let _ = child.kill();
            return Err("workload exceeded its bounded deadline".to_owned());
        }
        if Instant::now() >= next_sample {
            if let Err(error) = sample_host(config, run_dir, repo, &phase, &mut observations) {
                observations.warnings += 1;
                event(
                    run_dir,
                    "warning",
                    "host_sample_failed",
                    json!({"error": error}),
                )?;
            }
            next_sample = Instant::now() + interval;
        }
        let wait = next_sample.saturating_duration_since(Instant::now());
        if let Ok((is_error, line)) = receiver.recv_timeout(wait) {
            if is_error {
                observations.warnings += 1;
                event(
                    run_dir,
                    "warning",
                    "workload_stderr",
                    json!({"message": line}),
                )?;
            } else {
                append_line(&run_dir.join("workload.jsonl"), &line)?;
                let parsed: Value = serde_json::from_str(&line)
                    .map_err(|e| format!("workload emitted invalid JSON: {e}: {line}"))?;
                phase = parsed
                    .get("phase")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "workload record has no phase".to_owned())?
                    .to_owned();
                observations.workload_phases.push(phase.clone());
                event(run_dir, "info", "workload_phase", parsed)?;
            }
        }
    }
    while let Ok((is_error, line)) = receiver.try_recv() {
        if is_error {
            observations.warnings += 1;
            event(
                run_dir,
                "warning",
                "workload_stderr",
                json!({"message": line}),
            )?;
        } else {
            append_line(&run_dir.join("workload.jsonl"), &line)?;
            if let Ok(parsed) = serde_json::from_str::<Value>(&line) {
                if let Some(value) = parsed.get("phase").and_then(Value::as_str) {
                    observations.workload_phases.push(value.to_owned());
                }
                event(run_dir, "info", "workload_phase", parsed)?;
            }
        }
    }
    if !exit_status.success() {
        return Err(format!("remote workload exited with {exit_status}"));
    }
    let post_deadline = Instant::now() + Duration::from_secs(config.post_hold_seconds);
    while Instant::now() < post_deadline {
        sample_host(config, run_dir, repo, "post_workload", &mut observations)?;
        thread::sleep(interval.min(post_deadline.saturating_duration_since(Instant::now())));
    }
    preflight_health(config, run_dir, repo, "after_workload")?;
    let analysis = analyze(&observations);
    write_json(
        &run_dir.join("result.json"),
        &json!({
            "version": SCHEMA_VERSION,
            "sample_count": observations.sample_count,
            "warning_count": observations.warnings,
            "initial_current_bytes": analysis.initial_current_bytes,
            "maximum_current_bytes": observations.maximum_current,
            "maximum_peak_current_bytes": analysis.maximum_peak_current_bytes,
            "minimum_settled_current_bytes": analysis.minimum_settled_current_bytes,
            "observed_growth_bytes": analysis.observed_growth_bytes,
            "required_growth_bytes": config.expect_growth_bytes,
            "observed_reclaim_bytes": analysis.observed_reclaim_bytes,
            "required_reclaim_bytes": config.expect_reclaim_bytes,
            "observed_renewed_growth_bytes": analysis.observed_renewed_growth_bytes,
            "required_renewed_growth_bytes": config.expect_renewed_growth_bytes,
            "expect_no_resize": config.expect_no_resize,
            "require_renewed_during_pending_shrink": config.require_renewed_during_pending_shrink,
            "analysis": analysis,
            "workload_phases": observations.workload_phases,
        }),
    )?;
    classify(config, &observations, &analysis)
}

fn wait_for_controller_decision(
    config: &Config,
    repo: &Path,
    since_unix_millis: u64,
    timeout_seconds: u64,
) -> Result<String, String> {
    let timeout = Duration::from_secs(timeout_seconds);
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "controller handoff deadline exceeds the platform clock range".to_owned())?;
    let since = format!("@{}", since_unix_millis / 1000);
    let vm = format!("vm={}", config.vm_name);
    let alias = format!("alias={}", config.device_alias);
    loop {
        let output = process::bounded_text(
            "journalctl",
            &[
                OsString::from("--no-pager"),
                OsString::from("--output=cat"),
                OsString::from("--unit"),
                OsString::from(&config.controller_unit),
                OsString::from("--since"),
                OsString::from(&since),
            ],
            repo,
            Duration::from_secs(config.command_timeout_seconds),
        )?;
        if let Some(line) = output.lines().rev().find(|line| {
            line.contains("event=controller_decision")
                && line.contains(&vm)
                && line.contains(&alias)
        }) {
            return Ok(line.to_owned());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "controller did not accept the restarted telemetry session within {timeout:?}"
            ));
        }
        thread::sleep(
            Duration::from_secs(config.interval_seconds)
                .min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn sample_host(
    config: &Config,
    run_dir: &Path,
    _repo: &Path,
    phase: &str,
    observations: &mut Observations,
) -> Result<(), String> {
    let sample = read_privileged_sample(config, observations.last_privileged_sample_millis)?;
    observations.last_privileged_sample_millis = Some(sample.unix_millis);
    if !sample.vm_state.to_ascii_lowercase().contains("running") {
        return Err(format!("VM is not running: {}", sample.vm_state.trim()));
    }
    let now = now_millis()?;
    sample
        .guest_raw_telemetry
        .validate_for(
            &config.vm_name,
            &config.guest_service,
            now,
            seconds_millis(config.telemetry_max_age_seconds)?,
            seconds_millis(config.telemetry_future_tolerance_seconds)?,
        )
        .map_err(|error| format!("guest publication failed validation: {error}"))?;
    let evidence = json!({"version": SCHEMA_VERSION, "run_id": config.run_id, "unix_millis": sample.unix_millis, "workload_phase": phase, "vm_state": sample.vm_state.trim(), "requested_bytes": sample.requested_bytes, "current_bytes": sample.current_bytes, "device_size_bytes": sample.device_size_bytes, "block_size_bytes": sample.block_size_bytes, "host_mem_available_bytes": sample.host_mem_available_bytes, "guest_dommemstat_kib": sample.guest_dommemstat_kib, "guest_raw_telemetry": sample.guest_raw_telemetry});
    append_json(&run_dir.join("host-metrics.jsonl"), &evidence)?;
    if let Some(previous) = observations.last_requested {
        if previous != sample.requested_bytes {
            event(
                run_dir,
                "info",
                "resize_request_observed",
                json!({
                    "previous_requested_bytes": previous,
                    "requested_bytes": sample.requested_bytes,
                    "current_bytes": sample.current_bytes,
                    "workload_phase": phase,
                }),
            )?;
        }
    }
    observations.last_requested = Some(sample.requested_bytes);
    observations
        .initial_current
        .get_or_insert(sample.current_bytes);
    observations.maximum_current = observations.maximum_current.max(sample.current_bytes);
    if phase == "peak" {
        observations.maximum_peak_current =
            observations.maximum_peak_current.max(sample.current_bytes);
    }
    if phase == "renewed" {
        observations.maximum_renewed_current = Some(
            observations
                .maximum_renewed_current
                .map_or(sample.current_bytes, |value| {
                    value.max(sample.current_bytes)
                }),
        );
    }
    if phase == "settled" {
        observations.minimum_settled_phase_current = Some(
            observations
                .minimum_settled_phase_current
                .map_or(sample.current_bytes, |value| {
                    value.min(sample.current_bytes)
                }),
        );
    }
    if matches!(phase, "settled" | "post_workload" | "complete") {
        observations.minimum_settled_current = Some(
            observations
                .minimum_settled_current
                .map_or(sample.current_bytes, |value| {
                    value.min(sample.current_bytes)
                }),
        );
    }
    observations.sample_count += 1;
    observations.memory_samples.push(MemorySample {
        phase: phase.to_owned(),
        requested_bytes: sample.requested_bytes,
        current_bytes: sample.current_bytes,
        telemetry_session_id: sample.guest_raw_telemetry.session_id.clone(),
        telemetry_sequence: sample.guest_raw_telemetry.sequence,
    });
    Ok(())
}

fn preflight_health(
    config: &Config,
    run_dir: &Path,
    repo: &Path,
    stage: &str,
) -> Result<(), String> {
    let privileged = read_privileged_sample(config, None)?;
    if privileged.controller_active_state != "active" {
        return Err(format!(
            "controller {} is not active: {}",
            config.controller_unit, privileged.controller_active_state
        ));
    }
    let guest = remote_guest_health(config, repo, stage)?;
    event(
        run_dir,
        "info",
        "preflight_health_passed",
        json!({"controller": privileged.controller_active_state, "qga_reply": privileged.qga_reply, "guest": guest}),
    )
}

fn preflight_guest_health(
    config: &Config,
    run_dir: &Path,
    repo: &Path,
    stage: &str,
    qga_reply: &str,
) -> Result<(), String> {
    let guest = remote_guest_health(config, repo, stage)?;
    event(
        run_dir,
        "info",
        "guest_health_passed",
        json!({"qga_reply": qga_reply, "guest": guest}),
    )
}

fn remote_guest_health(config: &Config, repo: &Path, stage: &str) -> Result<Value, String> {
    match config.guest_os {
        GuestOs::Windows => remote_windows_health(config, repo, stage),
        GuestOs::Linux => remote_linux_health(config, repo, stage),
    }
}

/// The guest service runs, no package manager is changing the guest and no
/// reboot is pending.
fn remote_linux_health(config: &Config, repo: &Path, stage: &str) -> Result<Value, String> {
    let report = remote_text(config, repo, &linux_health_script(&config.guest_service))?;
    let facts = key_values(&report);
    let fact = |key: &str| facts.get(key).map_or("", String::as_str);
    if fact("service") != "active" {
        return Err(format!(
            "guest service {} is {:?}, not active",
            config.guest_service,
            fact("service")
        ));
    }
    if fact("package_manager") != "idle" {
        return Err(
            "a package manager is running in the guest; refusing to overlap qualification"
                .to_owned(),
        );
    }
    if fact("reboot_required") != "no" {
        return Err("the guest has a pending reboot (/run/reboot-required)".to_owned());
    }
    Ok(json!({
        "stage": stage,
        "guest_service": config.guest_service,
        "authenticated_guest_command": true,
        "installer_running": false,
        "pending_reboot": false,
    }))
}

/// `key=value` lines; anything else is ignored.
fn key_values(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn linux_health_script(service: &str) -> String {
    format!(
        "printf 'service=%s\\n' \"$(systemctl is-active {service} 2>/dev/null)\"; \
         if pgrep -x 'apt|apt-get|dpkg|unattended-upgr' >/dev/null; then echo package_manager=busy; else echo package_manager=idle; fi; \
         if [ -e /run/reboot-required ]; then echo reboot_required=yes; else echo reboot_required=no; fi"
    )
}

fn remote_windows_health(config: &Config, repo: &Path, stage: &str) -> Result<Value, String> {
    let service = remote_text(config, repo, &format!("sc query {}", config.guest_service))?;
    if !service.contains("RUNNING") {
        return Err(format!(
            "guest service {} is not RUNNING",
            config.guest_service
        ));
    }
    let installers = remote_text(config, repo, r#"tasklist /FI "IMAGENAME eq msiexec.exe""#)?;
    if installers.to_ascii_lowercase().contains("msiexec.exe") {
        return Err("Windows Installer is running; refusing to overlap qualification".to_owned());
    }
    for key in [
        r#"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending"#,
        r#"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired"#,
    ] {
        let result = remote_text(
            config,
            repo,
            &format!(r#"reg query "{key}" 2>nul || echo NOT_PENDING"#),
        )?;
        if !result.contains("NOT_PENDING") {
            return Err(format!("Windows pending-reboot marker exists: {key}"));
        }
    }
    Ok(json!({
        "stage": stage,
        "guest_service": config.guest_service,
        "authenticated_guest_command": true,
        "installer_running": false,
        "pending_reboot": false,
    }))
}

fn remote_text(config: &Config, repo: &Path, command: &str) -> Result<String, String> {
    let connect_timeout = format!("ConnectTimeout={}", config.command_timeout_seconds);
    process::bounded_text(
        "ssh",
        &[
            OsString::from("-o"),
            OsString::from("BatchMode=yes"),
            OsString::from("-o"),
            OsString::from(connect_timeout),
            OsString::from("--"),
            OsString::from(&config.ssh_target),
            OsString::from(command),
        ],
        repo,
        Duration::from_secs(config.command_timeout_seconds),
    )
}

fn analyze(observations: &Observations) -> QualificationAnalysis {
    let initial = observations.initial_current.unwrap_or(0);
    let observed_growth_bytes = observations.maximum_peak_current.saturating_sub(initial);
    let observed_reclaim_bytes = observations.minimum_settled_current.map_or(0, |minimum| {
        observations.maximum_peak_current.saturating_sub(minimum)
    });
    let observed_renewed_growth_bytes = observations
        .maximum_renewed_current
        .zip(observations.minimum_settled_phase_current)
        .map_or(0, |(maximum, minimum)| maximum.saturating_sub(minimum));
    let renewed_during_observed_pending_shrink = observations
        .memory_samples
        .iter()
        .position(|sample| sample.phase == "renewed")
        .and_then(|renewed| {
            observations.memory_samples[..renewed]
                .iter()
                .rev()
                .find(|sample| sample.phase == "settled")
        })
        .is_some_and(|sample| sample.requested_bytes < sample.current_bytes);
    let mut lower_request_while_pending_violations = 0;
    let mut lower_request_violations = 0;
    let mut telemetry_session_changes = 0;
    let mut telemetry_sequence_regressions = 0;
    let mut requested_change_violations = 0;
    let mut current_change_violations = 0;
    let mut unconverged_sample_violations = 0;
    for sample in &observations.memory_samples {
        if sample.requested_bytes != initial {
            requested_change_violations += 1;
        }
        if sample.current_bytes != initial {
            current_change_violations += 1;
        }
        if sample.requested_bytes != sample.current_bytes {
            unconverged_sample_violations += 1;
        }
    }
    for pair in observations.memory_samples.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        if next.requested_bytes < previous.requested_bytes {
            lower_request_violations += 1;
        }
        if previous.requested_bytes != previous.current_bytes
            && next.requested_bytes < previous.requested_bytes
            && next.current_bytes != previous.requested_bytes
        {
            lower_request_while_pending_violations += 1;
        }
        if next.telemetry_session_id != previous.telemetry_session_id {
            telemetry_session_changes += 1;
        } else if next.telemetry_sequence < previous.telemetry_sequence {
            telemetry_sequence_regressions += 1;
        }
    }
    QualificationAnalysis {
        initial_current_bytes: observations.initial_current,
        maximum_peak_current_bytes: observations.maximum_peak_current,
        minimum_settled_current_bytes: observations.minimum_settled_current,
        minimum_settled_phase_current_bytes: observations.minimum_settled_phase_current,
        maximum_renewed_current_bytes: observations.maximum_renewed_current,
        observed_growth_bytes,
        observed_reclaim_bytes,
        observed_renewed_growth_bytes,
        renewed_during_observed_pending_shrink,
        lower_request_while_pending_violations,
        lower_request_violations,
        telemetry_session_changes,
        telemetry_sequence_regressions,
        requested_change_violations,
        current_change_violations,
        unconverged_sample_violations,
    }
}

fn classify(
    config: &Config,
    observations: &Observations,
    analysis: &QualificationAnalysis,
) -> Result<(), String> {
    let required = ["baseline", "peak", "settled", "renewed", "complete"];
    for phase in required {
        if !observations
            .workload_phases
            .iter()
            .any(|value| value == phase)
        {
            return Err(format!("missing workload phase: {phase}"));
        }
    }
    if analysis.initial_current_bytes.is_none() {
        return Err("no initial memory sample".to_owned());
    }
    if config.expect_no_resize
        && (analysis.requested_change_violations != 0
            || analysis.current_change_violations != 0
            || analysis.unconverged_sample_violations != 0)
    {
        return Err(format!(
            "observe-only no-resize invariant failed: requested_changes={} current_changes={} unconverged_samples={}",
            analysis.requested_change_violations,
            analysis.current_change_violations,
            analysis.unconverged_sample_violations
        ));
    }
    if config.expect_no_reclaim && analysis.lower_request_violations != 0 {
        return Err(format!(
            "grow-only invariant failed: observed {} lower request(s)",
            analysis.lower_request_violations
        ));
    }
    if !config.expect_no_resize && analysis.observed_growth_bytes < config.expect_growth_bytes {
        return Err(format!(
            "observed growth {} bytes is below required {}",
            analysis.observed_growth_bytes, config.expect_growth_bytes
        ));
    }
    if !config.expect_no_resize && analysis.observed_reclaim_bytes < config.expect_reclaim_bytes {
        return Err(format!(
            "observed reclaim {} bytes is below required {}",
            analysis.observed_reclaim_bytes, config.expect_reclaim_bytes
        ));
    }
    if !config.expect_no_resize
        && analysis.observed_renewed_growth_bytes < config.expect_renewed_growth_bytes
    {
        return Err(format!(
            "observed renewed growth {} bytes is below required {}",
            analysis.observed_renewed_growth_bytes, config.expect_renewed_growth_bytes
        ));
    }
    if config.require_renewed_during_pending_shrink
        && !analysis.renewed_during_observed_pending_shrink
    {
        return Err("renewed pressure did not overlap an observed pending shrink".to_owned());
    }
    if analysis.lower_request_while_pending_violations != 0 {
        return Err(format!(
            "observed {} lower request(s) while a prior request was pending",
            analysis.lower_request_while_pending_violations
        ));
    }
    if analysis.telemetry_session_changes != 0 || analysis.telemetry_sequence_regressions != 0 {
        return Err(format!(
            "telemetry continuity failed: session_changes={} sequence_regressions={}",
            analysis.telemetry_session_changes, analysis.telemetry_sequence_regressions
        ));
    }
    Ok(())
}

fn workload_command(config: &Config) -> String {
    let mode = match config.mode {
        WorkloadMode::Committed => "committed",
        WorkloadMode::Resident => "resident",
        WorkloadMode::FileCache => "file-cache",
    };
    // Windows runs it under cmd.exe, Linux under sh; both paths were checked
    // for characters either shell would interpret.
    let program = match config.guest_os {
        GuestOs::Windows => format!("\"{}\"", config.remote_workload),
        GuestOs::Linux => format!("'{}'", config.remote_workload),
    };
    let mut command = format!("{program} --workload-id {} --mode {mode} --peak-bytes {} --retained-bytes {} --max-allocation-bytes {} --peak-hold-seconds {} --settled-hold-seconds {} --renewed-hold-seconds {} --refresh-interval-seconds {}", config.run_id, config.peak_bytes, config.retained_bytes, config.max_allocation_bytes, config.peak_hold_seconds, config.settled_hold_seconds, config.renewed_hold_seconds, config.resident_refresh_seconds);
    if let Some(directory) = &config.cache_dir {
        command.push_str(&format!(" --cache-dir '{directory}'"));
    }
    command
}

/// An absolute guest path a POSIX shell takes literally inside single quotes.
fn posix_path(path: &str, option: &str) -> Result<(), String> {
    if !path.starts_with('/')
        || path.chars().any(char::is_control)
        || path.contains(['\'', '\\'])
        || path.split('/').any(|part| part == "..")
    {
        return Err(format!(
            "{option} must be an absolute guest path without quotes or .."
        ));
    }
    Ok(())
}

fn archive_controller_log(config: &Config, run_dir: &Path, repo: &Path) {
    let status_path = run_dir.join("status.json");
    let Some(since) = process::read_file(&status_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Status>(&s).ok())
        .and_then(|s| s.started_unix_millis)
        .map(|v| format!("@{}", v / 1000))
    else {
        let _ = event(
            run_dir,
            "warning",
            "controller_log_failed",
            json!({"error": "run start time is unavailable; refusing to guess a journal window"}),
        );
        return;
    };
    let args = [
        OsString::from("--no-pager"),
        OsString::from("--output=short-iso"),
        OsString::from("--unit"),
        OsString::from(&config.controller_unit),
        OsString::from("--since"),
        OsString::from(since),
    ];
    match process::bounded_text(
        "journalctl",
        &args,
        repo,
        Duration::from_secs(config.command_timeout_seconds),
    ) {
        Ok(value) => {
            let _ = std::fs::write(run_dir.join("controller.log"), value);
        }
        Err(error) => {
            let _ = event(
                run_dir,
                "warning",
                "controller_log_failed",
                json!({"error": error}),
            );
        }
    }
}

fn show_status(args: &[String], repo: &Path) -> Result<(), String> {
    let run_dir = locate_run(args, repo)?;
    let status = process::read_file(&run_dir.join("status.json"))?;
    let parsed: Status =
        serde_json::from_str(&status).map_err(|e| format!("parse run status: {e}"))?;
    println!("{status}\noutput_dir={}", run_dir.display());
    if parsed.state != "finished"
        && parsed
            .pid
            .is_some_and(|pid| !Path::new("/proc").join(pid.to_string()).exists())
    {
        println!("warning=supervisor process is no longer present; inspect supervisor.log for an interrupted run");
    }
    Ok(())
}

fn review(args: &[String], repo: &Path) -> Result<(), String> {
    let run_dir = locate_run(args, repo)?;
    let summary = process::read_file(&run_dir.join("summary.json")).unwrap_or_else(|_| {
        process::read_file(&run_dir.join("status.json"))
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    });
    println!("{summary}\noutput_dir={}", run_dir.display());
    for name in [
        "result.json",
        "initial-state.json",
        "final-state.json",
        "events.jsonl",
        "host-metrics.jsonl",
        "pressure-shadow.jsonl",
        "workload.jsonl",
        "controller.log",
        "controller-guard.json",
        "supervisor.log",
    ] {
        let path = run_dir.join(name);
        if path.is_file() {
            println!("artifact={}", path.display());
        }
    }
    Ok(())
}

fn locate_run(args: &[String], repo: &Path) -> Result<PathBuf, String> {
    if args.is_empty() {
        return Err("RUN_ID is required".to_owned());
    }
    let run_id = identifier(&args[0], "RUN_ID")?;
    let mut root = repo.join(".artifacts/qualification");
    if args.len() == 3 && args[1] == "--output-root" {
        root = absolute_or_repo(repo, &args[2]);
    } else if args.len() != 1 {
        return Err("expected RUN_ID [--output-root PATH]".to_owned());
    }
    Ok(root.join(run_id))
}

fn virsh(config: &Config, repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut values = vec![OsString::from("-c"), OsString::from(&config.connect_uri)];
    values.extend(args.iter().map(OsString::from));
    process::bounded_text(
        "virsh",
        &values,
        repo,
        Duration::from_secs(config.command_timeout_seconds),
    )
}

fn systemctl_action(
    action: &str,
    unit: &str,
    repo: &Path,
    timeout_seconds: u64,
) -> Result<(), String> {
    process::bounded_text(
        "systemctl",
        &[OsString::from(action), OsString::from(unit)],
        repo,
        Duration::from_secs(timeout_seconds),
    )
    .map(|_| ())
}

fn capture_live_state_direct(
    config: &Config,
    repo: &Path,
    stage: &str,
) -> Result<LiveStateEvidence, String> {
    let xml = virsh(config, repo, &["dumpxml", &config.vm_name])?;
    let memory = parse_virtio_mem_xml_for_alias(&xml, &config.device_alias)
        .map_err(|error| format!("parse virtio-mem state: {error}"))?
        .memory;
    let vm_state = virsh(config, repo, &["domstate", &config.vm_name])?;
    if !vm_state.to_ascii_lowercase().contains("running") {
        return Err(format!("VM is not running: {}", vm_state.trim()));
    }
    let properties = systemd_properties(
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )?;
    let host = process::read_file(Path::new("/proc/meminfo"))?;
    Ok(LiveStateEvidence {
        version: SCHEMA_VERSION,
        run_id: config.run_id.clone(),
        stage: stage.to_owned(),
        unix_millis: now_millis()?,
        vm_state: vm_state.trim().to_owned(),
        controller_active_state: properties
            .get("ActiveState")
            .cloned()
            .ok_or_else(|| "systemctl did not report ActiveState".to_owned())?,
        controller_unit_file_state: properties
            .get("UnitFileState")
            .cloned()
            .ok_or_else(|| "systemctl did not report UnitFileState".to_owned())?,
        requested_bytes: memory.requested_bytes,
        current_bytes: memory.current_bytes,
        device_size_bytes: memory.size_bytes,
        block_size_bytes: memory.block_size_bytes,
        host_mem_available_bytes: meminfo_bytes(&host, "MemAvailable").ok(),
    })
}

fn capture_privileged_sample(config: &Config, repo: &Path) -> Result<PrivilegedSample, String> {
    let live = capture_live_state_direct(config, repo, "privileged_sample")?;
    let dommemstat = virsh(config, repo, &["dommemstat", &config.vm_name])?;
    let ping = virsh(
        config,
        repo,
        &[
            "qemu-agent-command",
            &config.vm_name,
            r#"{"execute":"guest-ping"}"#,
        ],
    );
    // A Windows guest is reached through its agent; a Linux guest on the
    // control channel needs none, so its absence is recorded, not fatal.
    let qga_reply = match (ping, config.guest_os) {
        (Ok(reply), _) => reply,
        (Err(error), GuestOs::Linux) => format!("unavailable: {error}"),
        (Err(error), GuestOs::Windows) => return Err(error),
    };
    let reader = PublicationCopyFirst::new(
        &config.vm_name,
        VirshGuestFiles::new(
            Virsh::with_connection(
                "virsh",
                Duration::from_secs(config.command_timeout_seconds),
                &config.connect_uri,
            ),
            &config.vm_name,
        ),
    );
    let bytes = reader.read_file(
        &config.telemetry_path,
        rampire::guest_channel::MAX_RAW_TELEMETRY_FILE_BYTES,
    )?;
    let contents = String::from_utf8(bytes)
        .map_err(|error| format!("QGA raw telemetry is not valid UTF-8: {error}"))?;
    let line = last_complete_record(&contents)
        .ok_or_else(|| "the guest publication has no complete record".to_owned())?;
    let guest_raw_telemetry: RawTelemetryEnvelope = serde_json::from_str(line)
        .map_err(|error| format!("the guest publication is invalid JSON: {error}"))?;
    Ok(PrivilegedSample {
        version: SCHEMA_VERSION,
        run_id: config.run_id.clone(),
        unix_millis: live.unix_millis,
        vm_state: live.vm_state,
        controller_active_state: live.controller_active_state,
        requested_bytes: live.requested_bytes,
        current_bytes: live.current_bytes,
        device_size_bytes: live.device_size_bytes,
        block_size_bytes: live.block_size_bytes,
        host_mem_available_bytes: live.host_mem_available_bytes,
        guest_dommemstat_kib: whitespace_pairs(&dommemstat),
        guest_raw_telemetry,
        qga_reply: qga_reply.trim().to_owned(),
    })
}

fn read_privileged_sample(
    config: &Config,
    after_millis: Option<u64>,
) -> Result<PrivilegedSample, String> {
    let wait = Duration::from_secs(config.command_timeout_seconds)
        .checked_mul(8)
        .and_then(|value| value.checked_add(Duration::from_secs(config.interval_seconds)))
        .ok_or_else(|| "privileged sample wait overflows".to_owned())?;
    let deadline = Instant::now()
        .checked_add(wait)
        .ok_or_else(|| "privileged sample deadline exceeds the platform clock range".to_owned())?;
    loop {
        let guard = read_root_guard_status(config)?;
        if guard.state != "running" {
            return Err(format!(
                "privileged controller guard is {}: {}",
                guard.state, guard.message
            ));
        }
        if let Ok(value) = process::read_file(&guard_sample_path(config)) {
            if let Ok(sample) = serde_json::from_str::<PrivilegedSample>(&value) {
                let is_new = after_millis.is_none_or(|previous| sample.unix_millis > previous);
                let is_fresh = now_millis()?
                    .checked_sub(sample.unix_millis)
                    .is_some_and(|age| u128::from(age) <= wait.as_millis());
                if sample.version == SCHEMA_VERSION
                    && sample.run_id == config.run_id
                    && is_new
                    && is_fresh
                {
                    return Ok(sample);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(
                "privileged controller guard did not publish a new sample before the bound"
                    .to_owned(),
            );
        }
        thread::sleep(
            Duration::from_secs(config.interval_seconds)
                .min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn persist_guard_initial_state(
    config: &Config,
    run_dir: &Path,
) -> Result<PrivilegedSample, String> {
    let value = process::read_file(&guard_initial_path(config))?;
    let sample: PrivilegedSample = serde_json::from_str(&value)
        .map_err(|error| format!("parse privileged initial state: {error}"))?;
    if sample.version != SCHEMA_VERSION || sample.run_id != config.run_id {
        return Err("privileged initial state identity does not match the run".to_owned());
    }
    let state = live_state_from_sample(config, &sample, "before_controller", "inactive")?;
    write_json(&run_dir.join("initial-state.json"), &state)?;
    Ok(sample)
}

fn persist_guard_final_state(config: &Config, run_dir: &Path) -> Result<(), String> {
    let value = process::read_file(&guard_final_path(config))?;
    let state: LiveStateEvidence = serde_json::from_str(&value)
        .map_err(|error| format!("parse privileged final state: {error}"))?;
    if state.version != SCHEMA_VERSION || state.run_id != config.run_id {
        return Err("privileged final state identity does not match the run".to_owned());
    }
    write_json(&run_dir.join("final-state.json"), &state)
}

fn live_state_from_sample(
    config: &Config,
    sample: &PrivilegedSample,
    stage: &str,
    expected_controller_state: &str,
) -> Result<LiveStateEvidence, String> {
    if sample.controller_active_state != expected_controller_state {
        return Err(format!(
            "controller state is {}; expected {expected_controller_state}",
            sample.controller_active_state
        ));
    }
    Ok(LiveStateEvidence {
        version: SCHEMA_VERSION,
        run_id: config.run_id.clone(),
        stage: stage.to_owned(),
        unix_millis: sample.unix_millis,
        vm_state: sample.vm_state.clone(),
        controller_active_state: sample.controller_active_state.clone(),
        controller_unit_file_state: "disabled".to_owned(),
        requested_bytes: sample.requested_bytes,
        current_bytes: sample.current_bytes,
        device_size_bytes: sample.device_size_bytes,
        block_size_bytes: sample.block_size_bytes,
        host_mem_available_bytes: sample.host_mem_available_bytes,
    })
}

fn ensure_guard_root() -> Result<(), String> {
    std::fs::create_dir_all(CONTROLLER_GUARD_ROOT)
        .map_err(|error| format!("create controller guard state directory: {error}"))
}

fn read_root_guard_status(config: &Config) -> Result<ControllerGuardStatus, String> {
    let value = process::read_file(
        &Path::new(CONTROLLER_GUARD_ROOT).join(format!("{}.json", config.run_id)),
    )?;
    let status: ControllerGuardStatus = serde_json::from_str(&value)
        .map_err(|error| format!("parse privileged controller guard status: {error}"))?;
    if status.version != SCHEMA_VERSION || status.run_id != config.run_id {
        return Err(
            "privileged controller guard status identity does not match the run".to_owned(),
        );
    }
    Ok(status)
}

fn guard_initial_path(config: &Config) -> PathBuf {
    Path::new(CONTROLLER_GUARD_ROOT).join(format!("{}-initial.json", config.run_id))
}

fn guard_sample_path(config: &Config) -> PathBuf {
    Path::new(CONTROLLER_GUARD_ROOT).join(format!("{}-sample.json", config.run_id))
}

fn guard_final_path(config: &Config) -> PathBuf {
    Path::new(CONTROLLER_GUARD_ROOT).join(format!("{}-final.json", config.run_id))
}

fn validate_final_no_resize(config: &Config, run_dir: &Path) -> Result<(), String> {
    if !config.expect_no_resize {
        return Ok(());
    }
    let initial: LiveStateEvidence =
        serde_json::from_str(&process::read_file(&run_dir.join("initial-state.json"))?)
            .map_err(|error| format!("parse initial live state: {error}"))?;
    let final_state: LiveStateEvidence =
        serde_json::from_str(&process::read_file(&run_dir.join("final-state.json"))?)
            .map_err(|error| format!("parse final live state: {error}"))?;
    if initial.requested_bytes != initial.current_bytes
        || final_state.requested_bytes != initial.requested_bytes
        || final_state.current_bytes != initial.current_bytes
    {
        return Err(format!(
            "observe-only run changed allocation: initial requested/current={}/{} final requested/current={}/{}",
            initial.requested_bytes,
            initial.current_bytes,
            final_state.requested_bytes,
            final_state.current_bytes
        ));
    }
    Ok(())
}

fn systemd_properties(
    unit: &str,
    repo: &Path,
    timeout_seconds: u64,
) -> Result<BTreeMap<String, String>, String> {
    let output = process::bounded_text(
        "systemctl",
        &[
            OsString::from("show"),
            OsString::from(unit),
            OsString::from("--property=ActiveState,UnitFileState,MainPID"),
            OsString::from("--no-pager"),
        ],
        repo,
        Duration::from_secs(timeout_seconds),
    )?;
    let mut properties = BTreeMap::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("systemctl returned malformed property: {line}"))?;
        if properties
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(format!("systemctl returned repeated property: {key}"));
        }
    }
    Ok(properties)
}

fn require_controller_active_and_shrink(config: &Config, repo: &Path) -> Result<(), String> {
    let properties = systemd_properties(
        &config.controller_unit,
        repo,
        config.command_timeout_seconds,
    )?;
    if properties.get("ActiveState").map(String::as_str) != Some("active") {
        return Err(format!(
            "controller {} did not become active",
            config.controller_unit
        ));
    }
    let pid = properties
        .get("MainPID")
        .ok_or_else(|| "systemctl did not report MainPID".to_owned())?
        .parse::<u32>()
        .map_err(|_| "systemctl returned an invalid MainPID".to_owned())?;
    if pid == 0 {
        return Err("controller has no running MainPID".to_owned());
    }
    let command_line = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|error| format!("read controller command line: {error}"))?;
    let running = HostConfig::load_vm(
        &controller_config_path(&command_line)?,
        Some(&config.vm_name),
    )
    .map_err(|error| format!("running controller configuration: {error}"))?;
    if config.expect_no_resize && !running.observe_only {
        return Err(
            "--expect-no-resize requires the running controller with observe_only = true"
                .to_owned(),
        );
    }
    Ok(())
}

/// The controller's `--config PATH` argument from a NUL-separated command line.
fn controller_config_path(command_line: &[u8]) -> Result<PathBuf, String> {
    let arguments = command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    match arguments.as_slice() {
        [_, b"--config", path] => Ok(PathBuf::from(String::from_utf8_lossy(path).into_owned())),
        _ => Err("running controller was not started with exactly --config PATH".to_owned()),
    }
}

fn write_root_guard_status(
    _run_dir: &Path,
    config: &Config,
    state: &str,
    message: &str,
) -> Result<(), String> {
    let root = Path::new(CONTROLLER_GUARD_ROOT);
    std::fs::create_dir_all(root)
        .map_err(|error| format!("create controller guard state directory: {error}"))?;
    write_json(
        &root.join(format!("{}.json", config.run_id)),
        &ControllerGuardStatus {
            version: SCHEMA_VERSION,
            run_id: config.run_id.clone(),
            state: state.to_owned(),
            message: message.to_owned(),
        },
    )
}

fn write_release(run_dir: &Path) -> Result<(), String> {
    let mut file = File::create(run_dir.join("controller-release"))
        .map_err(|error| format!("create controller release marker: {error}"))?;
    file.write_all(b"release\n")
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("publish controller release marker: {error}"))
}

fn finish_start_failure(run_dir: &Path, mut status: Status, error: &str) -> Result<(), String> {
    status.state = "finished".to_owned();
    status.finished_unix_millis = Some(now_millis()?);
    status.final_result = Some("fail".to_owned());
    status.message = error.to_owned();
    write_json(&run_dir.join("status.json"), &status)
}

fn wait_for_guard_stopped(
    run_dir: &Path,
    config: &Config,
    timeout: Duration,
    interval: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "controller cleanup wait exceeds the platform clock range".to_owned())?;
    loop {
        if let Ok(value) = process::read_file(
            &Path::new(CONTROLLER_GUARD_ROOT).join(format!("{}.json", config.run_id)),
        ) {
            if let Ok(status) = serde_json::from_str::<ControllerGuardStatus>(&value) {
                if let Some(result) = guard_status_result(&status) {
                    write_json(&run_dir.join("controller-guard.json"), &status)?;
                    return result;
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("controller guard did not confirm cleanup before the bound".to_owned());
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn guard_status_result(status: &ControllerGuardStatus) -> Option<Result<(), String>> {
    match status.state.as_str() {
        "stopped" => Some(Ok(())),
        "failed" => Some(Err(status.message.clone())),
        _ => None,
    }
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

unsafe fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

fn spawn_reader(
    input: impl std::io::Read + Send + 'static,
    is_error: bool,
    sender: mpsc::Sender<(bool, String)>,
) {
    thread::spawn(move || {
        for line in BufReader::new(input).lines().map_while(Result::ok) {
            let _ = sender.send((is_error, line));
        }
    });
}

fn event(run_dir: &Path, level: &str, kind: &str, detail: Value) -> Result<(), String> {
    append_json(
        &run_dir.join("events.jsonl"),
        &json!({"version": SCHEMA_VERSION, "unix_millis": now_millis()?, "level": level, "event": kind, "detail": detail}),
    )
}

fn append_json(path: &Path, value: &Value) -> Result<(), String> {
    append_line(
        path,
        &serde_json::to_string(value).map_err(|e| format!("serialize evidence: {e}"))?,
    )
}
fn append_line(path: &Path, line: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    writeln!(file, "{line}")
        .and_then(|_| file.flush())
        .map_err(|e| format!("write {}: {e}", path.display()))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    let mut file =
        File::create(&temporary).map_err(|e| format!("create {}: {e}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|e| format!("encode {}: {e}", path.display()))?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("flush {}: {e}", path.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("publish {}: {e}", path.display()))
}
use crate::process::unix_millis as now_millis;
fn new_run_id() -> Result<String, String> {
    Ok(format!(
        "qualification-{}-{}",
        now_millis()?,
        std::process::id()
    ))
}
fn value(args: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| format!("{option} requires a value"))
}
fn number(args: &[String], index: &mut usize, option: &str) -> Result<u64, String> {
    value(args, index, option)?
        .parse()
        .map_err(|_| format!("{option} requires an unsigned integer"))
}
fn required<T>(value: Option<T>, option: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("{option} is required"))
}
fn scope(value: &str, name: &str) -> Result<String, String> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_control) {
        Err(format!("{name} is invalid"))
    } else {
        Ok(value.to_owned())
    }
}
fn identifier(value: &str, name: &str) -> Result<String, String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
    {
        Err(format!("{name} contains unsafe characters"))
    } else {
        Ok(value.to_owned())
    }
}
fn absolute_or_repo(repo: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}
fn whitespace_pairs(input: &str) -> serde_json::Map<String, Value> {
    input
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((
                parts.next()?.to_owned(),
                Value::from(parts.next()?.parse::<u64>().ok()?),
            ))
        })
        .collect()
}
fn last_complete_record(value: &str) -> Option<&str> {
    if !value.ends_with('\n') {
        return None;
    }
    value.lines().rev().find(|line| !line.trim().is_empty())
}

fn seconds_millis(seconds: u64) -> Result<u64, String> {
    seconds
        .checked_mul(1000)
        .ok_or_else(|| "telemetry time bound overflows milliseconds".to_owned())
}

fn available_artifacts(run_dir: &Path) -> Vec<&'static str> {
    let mut names = vec!["summary.json"];
    names.extend(
        [
            "config.json",
            "status.json",
            "result.json",
            "initial-state.json",
            "final-state.json",
            "events.jsonl",
            "host-metrics.jsonl",
            "pressure-shadow.jsonl",
            "workload.jsonl",
            "controller.log",
            "controller-guard.json",
            "supervisor.log",
        ]
        .into_iter()
        .filter(|name| run_dir.join(name).is_file()),
    );
    names
}

fn wait_for_launcher_status(
    path: &Path,
    timeout: Duration,
    interval: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "launcher timeout exceeds the platform clock range".to_owned())?;
    loop {
        if process::read_file(path)
            .ok()
            .and_then(|value| serde_json::from_str::<Status>(&value).ok())
            .and_then(|status| status.pid)
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("launcher did not publish the detached supervisor PID".to_owned());
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn valid_start_arguments() -> Vec<String> {
        strings(&[
            "vm",
            "memory0",
            "--ssh-target",
            "guest",
            "--mode",
            "resident",
            "--peak-bytes",
            "8192",
            "--retained-bytes",
            "4096",
            "--max-allocation-bytes",
            "16384",
            "--peak-hold-seconds",
            "2",
            "--settled-hold-seconds",
            "3",
            "--renewed-hold-seconds",
            "2",
            "--resident-refresh-seconds",
            "1",
            "--post-hold-seconds",
            "1",
            "--interval-seconds",
            "1",
            "--command-timeout-seconds",
            "4",
            "--controller-timeout-seconds",
            "60",
            "--expect-growth-bytes",
            "2048",
            "--expect-reclaim-bytes",
            "1024",
            "--expect-renewed-growth-bytes",
            "50",
            "--remote-workload",
            r"C:\qualification\rampire-workload.exe",
            "--controller-unit",
            "rampire@vm.service",
            "--guest-service",
            "ConfiguredService",
            "--telemetry-path",
            r"C:\ProgramData\RampireGuest\telemetry.json",
            "--telemetry-max-age-seconds",
            "30",
            "--telemetry-future-tolerance-seconds",
            "5",
        ])
    }

    #[test]
    fn explicit_configuration_is_dry_run_until_apply() {
        let repo = Path::new("/repo");
        let parsed = parse_start(&valid_start_arguments(), repo).expect("configuration");
        assert!(!parsed.apply);
        assert_eq!(parsed.config.mode, WorkloadMode::Resident);
        assert_eq!(parsed.config.peak_bytes, 8192);
        assert_eq!(parsed.config.expect_reclaim_bytes, 1024);
        assert_eq!(parsed.config.expect_renewed_growth_bytes, 50);
    }

    #[test]
    fn shadow_configuration_requires_no_resize_instead_of_resize_thresholds() {
        let mut arguments = valid_start_arguments();
        for option in [
            "--expect-growth-bytes",
            "--expect-reclaim-bytes",
            "--expect-renewed-growth-bytes",
        ] {
            let position = arguments
                .iter()
                .position(|value| value == option)
                .expect("resize expectation option");
            arguments.drain(position..=position + 1);
        }
        arguments.push("--expect-no-resize".to_owned());
        let options = parse_start(&arguments, Path::new("/repo")).expect("shadow options");
        assert!(options.config.expect_no_resize);
        assert_eq!(options.config.expect_growth_bytes, 0);

        arguments.extend(strings(&["--expect-growth-bytes", "1"]));
        let error = parse_start(&arguments, Path::new("/repo"))
            .expect_err("shadow and resize expectations conflict");
        assert!(error.contains("conflicts"));
    }

    #[test]
    fn rejects_implicit_or_zero_operational_settings() {
        assert!(parse_start(&strings(&["vm", "memory0"]), Path::new("/repo")).is_err());
        let mut invalid = valid_start_arguments();
        let position = invalid
            .iter()
            .position(|value| value == "--command-timeout-seconds")
            .expect("timeout option");
        invalid[position + 1] = "0".to_owned();
        assert!(parse_start(&invalid, Path::new("/repo")).is_err());
    }

    #[test]
    fn apply_requires_the_typed_elevation_boundary() {
        let mut arguments = valid_start_arguments();
        arguments.push("--apply".to_owned());
        let error = parse_start(&arguments, Path::new("/repo")).expect_err("missing elevation");
        assert!(error.contains("requires --elevate"));
        arguments.push("--elevate".to_owned());
        assert!(parse_start(&arguments, Path::new("/repo")).is_ok());
    }

    #[test]
    fn explicit_service_restart_requires_its_own_bound() {
        let mut arguments = valid_start_arguments();
        arguments.push("--apply-service-restart".to_owned());
        let error = parse_start(&arguments, Path::new("/repo"))
            .expect_err("service restart without a bound");
        assert!(error.contains("requires --guest-service-cycle-timeout-seconds"));

        arguments.extend(strings(&["--guest-service-cycle-timeout-seconds", "90"]));
        let controller_timeout = arguments
            .iter()
            .position(|value| value == "--controller-timeout-seconds")
            .expect("controller timeout");
        arguments[controller_timeout + 1] = "180".to_owned();
        let options = parse_start(&arguments, Path::new("/repo")).expect("bounded restart");
        assert!(options.config.apply_service_restart);
        assert_eq!(options.config.guest_service_cycle_timeout_seconds, Some(90));
    }

    #[test]
    fn rejects_short_controller_ownership_bound_and_non_windows_telemetry_path() {
        let mut arguments = valid_start_arguments();
        let timeout = arguments
            .iter()
            .position(|value| value == "--controller-timeout-seconds")
            .expect("controller timeout");
        arguments[timeout + 1] = "47".to_owned();
        assert!(parse_start(&arguments, Path::new("/repo")).is_err());

        let mut arguments = valid_start_arguments();
        let telemetry = arguments
            .iter()
            .position(|value| value == "--telemetry-path")
            .expect("telemetry path");
        arguments[telemetry + 1] = "/run/telemetry.jsonl".to_owned();
        assert!(parse_start(&arguments, Path::new("/repo")).is_err());
    }

    fn linux_start_arguments() -> Vec<String> {
        let mut arguments = valid_start_arguments();
        for (option, value) in [
            ("--remote-workload", "/usr/local/bin/rampire-workload"),
            ("--guest-service", "rampire-guest"),
            ("--telemetry-path", "/var/lib/rampire-guest/control.json"),
        ] {
            let position = arguments.iter().position(|item| item == option).unwrap();
            arguments[position + 1] = value.to_owned();
        }
        arguments.extend(strings(&["--guest-os", "linux"]));
        arguments
    }

    #[test]
    fn a_linux_guest_runs_the_linux_workload_under_sh() {
        let options = parse_start(&linux_start_arguments(), Path::new("/repo")).expect("linux");
        assert_eq!(options.config.guest_os, GuestOs::Linux);
        let command = workload_command(&options.config);
        assert!(command.starts_with("'/usr/local/bin/rampire-workload' --workload-id "));
        assert!(!command.contains("--cache-dir"));

        let mut cache = linux_start_arguments();
        let mode = cache.iter().position(|item| item == "--mode").unwrap();
        cache[mode + 1] = "file-cache".to_owned();
        assert!(parse_start(&cache, Path::new("/repo"))
            .unwrap_err()
            .contains("--cache-dir"));
        cache.extend(strings(&["--cache-dir", "/var/tmp/qualification-cache"]));
        let options = parse_start(&cache, Path::new("/repo")).expect("file cache");
        let command = workload_command(&options.config);
        assert!(command.contains(" --mode file-cache "));
        assert!(command.ends_with(" --cache-dir '/var/tmp/qualification-cache'"));

        for (option, unsafe_value) in [
            ("--remote-workload", "/usr/bin/x'; reboot; '"),
            ("--telemetry-path", "relative/control.json"),
            ("--remote-workload", "/usr/../bin/x"),
        ] {
            let mut arguments = linux_start_arguments();
            let position = arguments.iter().position(|item| item == option).unwrap();
            arguments[position + 1] = unsafe_value.to_owned();
            assert!(
                parse_start(&arguments, Path::new("/repo")).is_err(),
                "{unsafe_value}"
            );
        }
        let mut windows_cache = valid_start_arguments();
        windows_cache.extend(strings(&["--cache-dir", "/tmp/x"]));
        assert!(parse_start(&windows_cache, Path::new("/repo")).is_err());
        let mut restart = linux_start_arguments();
        restart.extend(strings(&[
            "--apply-service-restart",
            "--guest-service-cycle-timeout-seconds",
            "30",
        ]));
        assert!(parse_start(&restart, Path::new("/repo")).is_err());
    }

    #[test]
    fn linux_health_reads_service_package_manager_and_reboot_state() {
        let script = linux_health_script("rampire-guest");
        assert!(script.contains("systemctl is-active rampire-guest"));
        let facts = key_values("service=active\npackage_manager=idle\nreboot_required=no\nnoise\n");
        assert_eq!(facts["service"], "active");
        assert_eq!(facts.len(), 3);
    }

    #[test]
    fn a_run_recorded_before_linux_support_reads_as_windows() {
        let mut config = parse_start(&valid_start_arguments(), Path::new("/repo"))
            .expect("windows")
            .config;
        config.cache_dir = None;
        let mut value = serde_json::to_value(&config).unwrap();
        value.as_object_mut().unwrap().remove("guest_os");
        let old: Config = serde_json::from_value(value).unwrap();
        assert_eq!(old.guest_os, GuestOs::Windows);
    }

    #[test]
    fn guard_completion_is_a_required_cleanup_signal() {
        let status = |state: &str, message: &str| ControllerGuardStatus {
            version: SCHEMA_VERSION,
            run_id: "run".to_owned(),
            state: state.to_owned(),
            message: message.to_owned(),
        };
        assert_eq!(guard_status_result(&status("running", "active")), None);
        assert_eq!(
            guard_status_result(&status("stopped", "restored")),
            Some(Ok(()))
        );
        assert_eq!(
            guard_status_result(&status("failed", "stop failed")),
            Some(Err("stop failed".to_owned()))
        );
    }

    #[test]
    fn privileged_snapshot_paths_are_scoped_to_the_run() {
        let options = parse_start(&valid_start_arguments(), Path::new("/repo"))
            .expect("qualification configuration");
        let run_id = &options.config.run_id;
        assert_eq!(
            guard_initial_path(&options.config),
            Path::new(CONTROLLER_GUARD_ROOT).join(format!("{run_id}-initial.json"))
        );
        assert_eq!(
            guard_sample_path(&options.config),
            Path::new(CONTROLLER_GUARD_ROOT).join(format!("{run_id}-sample.json"))
        );
        assert_eq!(
            guard_final_path(&options.config),
            Path::new(CONTROLLER_GUARD_ROOT).join(format!("{run_id}-final.json"))
        );
    }

    #[test]
    fn result_requires_all_phases_and_resize_thresholds() {
        let mut options =
            parse_start(&valid_start_arguments(), Path::new("/repo")).expect("options");
        options.config.expect_growth_bytes = 100;
        options.config.expect_reclaim_bytes = 50;
        let observations = Observations {
            initial_current: Some(1_000),
            maximum_current: 1_200,
            maximum_peak_current: 1_200,
            minimum_settled_current: Some(1_100),
            minimum_settled_phase_current: Some(1_100),
            maximum_renewed_current: Some(1_175),
            workload_phases: ["baseline", "peak", "settled", "renewed", "complete"]
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            sample_count: 5,
            warnings: 0,
            last_requested: Some(1_100),
            last_privileged_sample_millis: None,
            memory_samples: vec![
                MemorySample {
                    phase: "settled".to_owned(),
                    requested_bytes: 1_100,
                    current_bytes: 1_150,
                    telemetry_session_id: "session-a".to_owned(),
                    telemetry_sequence: 1,
                },
                MemorySample {
                    phase: "renewed".to_owned(),
                    requested_bytes: 1_200,
                    current_bytes: 1_175,
                    telemetry_session_id: "session-a".to_owned(),
                    telemetry_sequence: 2,
                },
            ],
        };
        let analysis = analyze(&observations);
        assert!(classify(&options.config, &observations, &analysis).is_ok());
        assert!(analysis.renewed_during_observed_pending_shrink);
        options.config.expect_growth_bytes = 201;
        assert!(classify(&options.config, &observations, &analysis).is_err());
    }

    #[test]
    fn grow_only_result_rejects_a_new_shrink_from_a_converged_state() {
        let mut arguments = valid_start_arguments();
        let option = arguments
            .iter()
            .position(|arg| arg == "--expect-reclaim-bytes")
            .expect("reclaim expectation");
        arguments.drain(option..=option + 1);
        arguments.push("--expect-no-reclaim".to_owned());
        let mut options = parse_start(&arguments, Path::new("/repo")).expect("grow-only options");
        options.config.expect_growth_bytes = 100;
        options.config.expect_renewed_growth_bytes = 50;
        let mut observations = Observations {
            initial_current: Some(1_000),
            maximum_current: 1_300,
            maximum_peak_current: 1_200,
            minimum_settled_current: Some(1_200),
            minimum_settled_phase_current: Some(1_200),
            maximum_renewed_current: Some(1_300),
            workload_phases: ["baseline", "peak", "settled", "renewed", "complete"]
                .iter()
                .map(|phase| (*phase).to_owned())
                .collect(),
            sample_count: 5,
            memory_samples: [
                ("baseline", 1_000),
                ("peak", 1_200),
                ("settled", 1_200),
                ("renewed", 1_300),
                ("complete", 1_300),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (phase, bytes))| MemorySample {
                phase: phase.to_owned(),
                requested_bytes: bytes,
                current_bytes: bytes,
                telemetry_session_id: "session-a".to_owned(),
                telemetry_sequence: index as u64,
            })
            .collect(),
            ..Observations::default()
        };
        assert!(classify(&options.config, &observations, &analyze(&observations)).is_ok());

        observations.memory_samples[2].requested_bytes = 1_100;
        let analysis = analyze(&observations);
        assert_eq!(analysis.lower_request_while_pending_violations, 0);
        assert_eq!(analysis.lower_request_violations, 1);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("a grow-only run must reject even the first shrink request");
        assert!(error.contains("grow-only invariant"));
    }

    #[test]
    fn shadow_result_requires_every_sample_to_preserve_allocation() {
        let mut options =
            parse_start(&valid_start_arguments(), Path::new("/repo")).expect("options");
        options.config.expect_no_resize = true;
        let samples = ["baseline", "peak", "settled", "renewed", "complete"]
            .iter()
            .enumerate()
            .map(|(index, phase)| MemorySample {
                phase: (*phase).to_owned(),
                requested_bytes: 1_000,
                current_bytes: 1_000,
                telemetry_session_id: "session-a".to_owned(),
                telemetry_sequence: index as u64,
            })
            .collect::<Vec<_>>();
        let mut observations = Observations {
            initial_current: Some(1_000),
            maximum_current: 1_000,
            maximum_peak_current: 1_000,
            minimum_settled_current: Some(1_000),
            minimum_settled_phase_current: Some(1_000),
            maximum_renewed_current: Some(1_000),
            workload_phases: ["baseline", "peak", "settled", "renewed", "complete"]
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            sample_count: 5,
            warnings: 0,
            last_requested: Some(1_000),
            last_privileged_sample_millis: None,
            memory_samples: samples,
        };
        let analysis = analyze(&observations);
        assert!(classify(&options.config, &observations, &analysis).is_ok());

        observations.memory_samples[2].requested_bytes = 999;
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("shadow allocation change");
        assert!(error.contains("no-resize invariant"));
    }

    #[test]
    fn result_rejects_renewed_growth_and_continuity_failures() {
        let mut options =
            parse_start(&valid_start_arguments(), Path::new("/repo")).expect("options");
        options.config.expect_growth_bytes = 100;
        options.config.expect_reclaim_bytes = 50;
        options.config.expect_renewed_growth_bytes = 50;
        let mut observations = Observations {
            initial_current: Some(1_000),
            maximum_current: 1_200,
            maximum_peak_current: 1_200,
            minimum_settled_current: Some(1_100),
            minimum_settled_phase_current: Some(1_100),
            maximum_renewed_current: Some(1_125),
            workload_phases: ["baseline", "peak", "settled", "renewed", "complete"]
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            sample_count: 2,
            warnings: 0,
            last_requested: Some(1_200),
            last_privileged_sample_millis: None,
            memory_samples: vec![
                MemorySample {
                    phase: "settled".to_owned(),
                    requested_bytes: 1_100,
                    current_bytes: 1_150,
                    telemetry_session_id: "session-a".to_owned(),
                    telemetry_sequence: 2,
                },
                MemorySample {
                    phase: "renewed".to_owned(),
                    requested_bytes: 1_050,
                    current_bytes: 1_125,
                    telemetry_session_id: "session-b".to_owned(),
                    telemetry_sequence: 1,
                },
            ],
        };
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("renewed growth is below its declared threshold");
        assert!(error.contains("renewed growth"));

        observations.maximum_renewed_current = Some(1_175);
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("lower pending request is unsafe");
        assert!(error.contains("lower request"));
        assert_eq!(analysis.telemetry_session_changes, 1);

        observations.memory_samples[1].requested_bytes = 1_200;
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("session change breaks telemetry continuity");
        assert!(error.contains("telemetry continuity"));

        observations.memory_samples[1].telemetry_session_id = "session-a".to_owned();
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("sequence regression breaks telemetry continuity");
        assert!(error.contains("sequence_regressions=1"));
    }

    #[test]
    fn pending_shrink_overlap_is_an_explicit_acceptance_gate() {
        let mut arguments = valid_start_arguments();
        arguments.push("--require-renewed-during-pending-shrink".to_owned());
        let options = parse_start(&arguments, Path::new("/repo")).expect("options");
        assert!(options.config.require_renewed_during_pending_shrink);

        let observations = Observations {
            initial_current: Some(1_000),
            maximum_current: 3_100,
            maximum_peak_current: 3_100,
            minimum_settled_current: Some(2_000),
            minimum_settled_phase_current: Some(2_000),
            maximum_renewed_current: Some(3_100),
            workload_phases: ["baseline", "peak", "settled", "renewed", "complete"]
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            sample_count: 2,
            warnings: 0,
            last_requested: Some(3_100),
            last_privileged_sample_millis: None,
            memory_samples: vec![
                MemorySample {
                    phase: "settled".to_owned(),
                    requested_bytes: 2_000,
                    current_bytes: 2_000,
                    telemetry_session_id: "session-a".to_owned(),
                    telemetry_sequence: 1,
                },
                MemorySample {
                    phase: "renewed".to_owned(),
                    requested_bytes: 3_100,
                    current_bytes: 3_100,
                    telemetry_session_id: "session-a".to_owned(),
                    telemetry_sequence: 2,
                },
            ],
        };
        let analysis = analyze(&observations);
        let error = classify(&options.config, &observations, &analysis)
            .expect_err("renewed pressure did not begin during a pending shrink");
        assert!(error.contains("did not overlap"));
    }

    #[test]
    fn parses_host_memory_and_dommemstat_without_guessing_units() {
        assert_eq!(
            meminfo_bytes("MemAvailable: 1024 kB\n", "MemAvailable"),
            Ok(1 << 20)
        );
        let values = whitespace_pairs("unused 10\navailable 20\nlast-update 30\n");
        assert_eq!(values["available"], 20);
    }

    #[test]
    fn workload_command_preserves_mode_bounds_and_run_identity() {
        let mut arguments = valid_start_arguments();
        let position = arguments
            .iter()
            .position(|value| value == "--mode")
            .expect("mode option");
        arguments[position + 1] = "committed".to_owned();
        let options = parse_start(&arguments, Path::new("/repo")).expect("options");
        let command = workload_command(&options.config);
        assert!(command.contains("--mode committed"));
        assert!(command.contains(&format!("--workload-id {}", options.config.run_id)));
        assert!(command.contains("--max-allocation-bytes 16384"));
        assert!(command.contains("--refresh-interval-seconds 1"));
    }

    #[test]
    fn artifact_index_only_claims_files_that_exist() {
        let directory = std::env::temp_dir().join(format!(
            "rampire-qualification-artifacts-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(directory.join("status.json"), b"{}\n").expect("write status");
        let artifacts = available_artifacts(&directory);
        assert!(artifacts.contains(&"summary.json"));
        assert!(artifacts.contains(&"status.json"));
        assert!(!artifacts.contains(&"result.json"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn controller_config_comes_only_from_its_exact_command_line() {
        assert_eq!(
            controller_config_path(b"/usr/local/bin/rampire\0--config\0/etc/rampire/guest.toml\0"),
            Ok(PathBuf::from("/etc/rampire/guest.toml"))
        );
        for unexpected in [
            &b"/usr/local/bin/rampire\0"[..],
            b"/usr/local/bin/rampire\0status\0--config\0a.toml\0",
            b"/usr/local/bin/rampire\0--config\0a.toml\0--config\0b.toml\0",
        ] {
            assert!(controller_config_path(unexpected).is_err());
        }
    }
}
