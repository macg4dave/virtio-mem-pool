//! Capture provenance and explanations without adding guest-agent polling.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::health::{performance_incident, Observed};
use super::{create, iso_utc, write_error, Command, JournalEvent, Sample};
use crate::process;
use crate::watch::COMMAND_TIMEOUT;

const HEARTBEAT_MS: u64 = 30_000;

fn observation(result: Result<String, String>) -> Value {
    match result {
        Ok(value) => json!({"value": value.trim_end()}),
        Err(error) => json!({"error": error}),
    }
}

fn read(path: &Path) -> Value {
    observation(std::fs::read_to_string(path).map_err(|error| error.to_string()))
}

pub(super) fn write_manifest(
    command: &Command,
    repo: &Path,
    directory: &Path,
    started: u64,
) -> Result<(), String> {
    let run = |program: &str, args: &[&str]| {
        observation(process::bounded_text(
            program,
            &args.iter().map(process::os).collect::<Vec<_>>(),
            repo,
            COMMAND_TIMEOUT,
        ))
    };
    let settings = virtio_mem_core::settings::load_configuration(&command.config_path)
        .map_err(|error| error.to_string())
        .map(|(global, hosts, vms)| json!({"global": global, "hosts": hosts, "vms": vms}));
    let config =
        virtio_mem_host::config::HostConfig::load_vm(&command.config_path, Some(&command.vm))
            .map_err(|error| error.to_string());
    let controller_pid = process::bounded_text(
        "systemctl",
        &[
            process::os("show"),
            process::os(&command.unit),
            process::os("--property=MainPID"),
            process::os("--value"),
        ],
        repo,
        COMMAND_TIMEOUT,
    );
    let executable = controller_pid
        .as_ref()
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .map(|pid| format!("/proc/{pid}/exe"));
    let installed_hash = executable.map_or_else(
        || json!({"error": "controller PID unavailable"}),
        |path| run("sha256sum", &[&path]),
    );
    let state = match config {
        Ok(config) => {
            json!({"checkpoint": read(Path::new(&config.policy_state_path)), "attestation": read(Path::new(&config.compatibility_attestation_path))})
        }
        Err(error) => json!({"error": error}),
    };
    let manifest = json!({
        "schema_version": 1, "started_utc": iso_utc(started), "finished_utc": null,
        "status": "recording", "vm": command.vm, "alias": command.alias, "unit": command.unit,
        "recorder": {"executable_sha256": std::env::current_exe().map_or_else(|error| json!({"error": error.to_string()}), |path| run("sha256sum", &[&path.to_string_lossy()])),
            "checkout_commit": run("git", &["rev-parse", "HEAD"]), "checkout_changes": run("git", &["status", "--short"]),
            "sample_interval_ms": command.interval.as_millis(), "guest_interval_ms": command.guest_interval.as_millis()},
        "configured_settings": settings.unwrap_or_else(|error| json!({"error": error})),
        "installed_controller_sha256": installed_hash,
        "controller_unit": run("systemctl", &["show", &command.unit, "--property=ExecStart,FragmentPath,MainPID,InvocationID"]),
        "installed_settings": {"global": read(Path::new("/etc/virtio-mem-host/global-settings.toml")), "host_list": read(Path::new("/etc/virtio-mem-host/host_list.toml")),
            "vm": read(&PathBuf::from(format!("/etc/virtio-mem-host/{}_settings.toml", command.vm)))},
        "initial_control_state": state,
        "host_boot_id": read(Path::new("/proc/sys/kernel/random/boot_id")),
        "live_xml": run("virsh", &["-c", &command.connect, "--readonly", "dumpxml", &command.vm]),
        "saved_xml": run("virsh", &["-c", &command.connect, "--readonly", "dumpxml", &command.vm, "--inactive"]),
        "guest_metadata": {"status": "awaiting first readable publication"},
        "unavailable_metadata": "Permission failures are recorded explicitly; the recorder never elevates privileges. Checkout identity describes the recorder checkout, not the installed host binary."
    });
    write_json(&directory.join("run.json"), &manifest)
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(temporary, path))
        .map_err(write_error)
}

pub(super) fn finish_manifest(directory: &Path, result: &Result<(), String>) -> Result<(), String> {
    let path = directory.join("run.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).map_err(write_error)?)
        .map_err(|error| error.to_string())?;
    value["finished_utc"] = json!(iso_utc(super::unix_millis()));
    value["status"] = json!(if result.is_ok() { "finished" } else { "failed" });
    value["error"] = json!(result.as_ref().err());
    write_json(&path, &value)
}

pub(super) struct Diagnostics {
    publications: BufWriter<File>,
    directory: PathBuf,
    last_key: Option<Value>,
    last_snapshot: u64,
    last_consumed: Option<Value>,
    last_uptime: Option<(String, u64)>,
    metadata_written: bool,
    last_current: Option<(u64, u64)>,
    conditions: BTreeMap<&'static str, u64>,
    reported: BTreeSet<&'static str>,
    zero_grants: VecDeque<u64>,
}

impl Diagnostics {
    pub(super) fn new(directory: &Path) -> Result<Self, String> {
        Ok(Self {
            publications: create(&directory.join("guest-publications.jsonl"))?,
            directory: directory.to_owned(),
            last_key: None,
            last_snapshot: 0,
            last_consumed: None,
            last_uptime: None,
            metadata_written: false,
            last_current: None,
            conditions: BTreeMap::new(),
            reported: BTreeSet::new(),
            zero_grants: VecDeque::new(),
        })
    }

    pub(super) fn observe(
        &mut self,
        sample: &Sample,
        events: &[JournalEvent],
        command: &Command,
        observed: &mut Observed,
        record_publication: bool,
    ) -> Result<(), String> {
        let now = sample.unix_millis;
        let elapsed = u64::try_from(sample.elapsed.as_millis()).unwrap_or(u64::MAX);
        if let Some(guest) = sample.telemetry() {
            let diagnostic = guest.diagnostics.as_ref();
            let key = json!([
                guest.session_id,
                guest.native_pressure,
                guest.request.as_ref().map(|request| &request.identity),
                diagnostic.map(|value| value.policy.reason)
            ]);
            let changed = self.last_key.as_ref() != Some(&key);
            if record_publication
                && (changed || elapsed.saturating_sub(self.last_snapshot) >= HEARTBEAT_MS)
            {
                writeln!(self.publications, "{}", json!({"recorded_utc": iso_utc(now), "trigger": if changed {"state_change"} else {"heartbeat"}, "publication": guest})).map_err(write_error)?;
                self.publications.flush().map_err(write_error)?;
                self.last_snapshot = elapsed;
                self.last_key = Some(key);
                if changed {
                    observed.event(
                        now,
                        "metrics_guest_decision",
                        &[
                            ("session_id", guest.session_id.clone()),
                            (
                                "details",
                                serde_json::to_string(&diagnostic)
                                    .map_err(|error| error.to_string())?,
                            ),
                        ],
                        "guest decision changed; full publication in guest-publications.jsonl"
                            .to_owned(),
                    );
                }
            }
            if !self.metadata_written {
                let path = self.directory.join("run.json");
                let mut manifest: Value =
                    serde_json::from_slice(&std::fs::read(&path).map_err(write_error)?)
                        .map_err(|error| error.to_string())?;
                manifest["guest_metadata"] = json!({"publication_version": guest.version, "initial_session": guest.session_id, "diagnostics": diagnostic, "initial_pending_request": guest.request});
                write_json(&path, &manifest)?;
                self.metadata_written = true;
            }
            if let Some(diagnostic) = diagnostic {
                if let Some(uptime) = diagnostic.boot_uptime_millis {
                    if self
                        .last_uptime
                        .as_ref()
                        .is_some_and(|(session, previous)| {
                            guest.session_id != *session && uptime < *previous
                        })
                    {
                        observed.event(now, "metrics_guest_reboot_detected", &[("uptime_ms", uptime.to_string())], "guest boot uptime decreased; this is distinct from a service session change".to_owned());
                    }
                    self.last_uptime = Some((guest.session_id.clone(), uptime));
                }
                if let Some(result) = &diagnostic.last_consumed_result {
                    let identity = json!([
                        result.request.identity,
                        diagnostic.result_consumed_unix_millis
                    ]);
                    if self.last_consumed.as_ref() != Some(&identity) {
                        observed.event(
                            now,
                            "metrics_guest_result_consumed",
                            &[
                                ("session_id", result.request.identity.session_id.clone()),
                                ("sequence", result.request.identity.sequence.to_string()),
                                (
                                    "issue_to_consumed_ms",
                                    diagnostic.result_consumed_unix_millis.map_or_else(
                                        String::new,
                                        |consumed| {
                                            consumed
                                                .saturating_sub(result.request.issued_unix_millis)
                                                .to_string()
                                        },
                                    ),
                                ),
                            ],
                            "guest consumed the host result".to_owned(),
                        );
                        self.last_consumed = Some(identity);
                    }
                }
            }
            self.problem(
                "stale_publication",
                now.saturating_sub(guest.observed_unix_millis)
                    > command.telemetry_max_age.as_millis() as u64,
                sample,
                observed,
            );
        }
        if let Some(device) = sample.device {
            if self
                .last_current
                .is_none_or(|(current, _)| current != device.current_bytes)
            {
                self.last_current = Some((device.current_bytes, elapsed));
            }
            self.problem(
                "device_stalled",
                device.current_bytes != device.requested_bytes
                    && self
                        .last_current
                        .is_some_and(|(_, last)| elapsed.saturating_sub(last) >= HEARTBEAT_MS),
                sample,
                observed,
            );
        }
        self.problem(
            "host_below_reserve",
            sample.host.available < command.host_floor_bytes,
            sample,
            observed,
        );
        for event in events {
            if (event.name.as_deref() == Some("controller_decision")
                && event
                    .fields
                    .get("grant_bytes")
                    .is_some_and(|bytes| bytes == "0"))
                || (event.name.as_deref() == Some("resize_requested")
                    && event
                        .fields
                        .get("command_result")
                        .is_some_and(|result| result.starts_with("Err(Rejected")))
            {
                self.zero_grants.push_back(elapsed);
            }
        }
        while self
            .zero_grants
            .front()
            .is_some_and(|time| elapsed.saturating_sub(*time) > 60_000)
        {
            self.zero_grants.pop_front();
        }
        self.problem(
            "repeated_capacity_rejections",
            self.zero_grants.len() >= 5,
            sample,
            observed,
        );
        Ok(())
    }

    fn problem(
        &mut self,
        reason: &'static str,
        active: bool,
        sample: &Sample,
        observed: &mut Observed,
    ) {
        if !active {
            self.conditions.remove(reason);
            self.reported.remove(reason);
            return;
        }
        let elapsed = u64::try_from(sample.elapsed.as_millis()).unwrap_or(u64::MAX);
        let since = *self.conditions.entry(reason).or_insert(elapsed);
        if elapsed.saturating_sub(since) >= HEARTBEAT_MS && self.reported.insert(reason) {
            observed.event(
                sample.unix_millis,
                "metrics_performance_problem",
                &[("reason", reason.to_owned())],
                format!("{reason} persisted for at least 30 seconds"),
            );
            observed
                .incidents
                .push(performance_incident(sample, reason));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use virtio_mem_core::demand::GuestDiagnostics;
    use virtio_mem_core::{MemoryTelemetrySnapshot, RawTelemetryEnvelope, VirtioMemState};

    fn fixture(name: &str) -> (PathBuf, Diagnostics, Command) {
        let directory =
            std::env::temp_dir().join(format!("metrics-diagnostics-{name}-{}", std::process::id()));
        if directory.exists() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("run.json"), "{}").unwrap();
        let command = super::super::parse(&[
            "--config".to_owned(),
            crate::repo_root()
                .unwrap()
                .join("fixtures/config/host_list.toml")
                .display()
                .to_string(),
        ])
        .unwrap();
        let recorder = Diagnostics::new(&directory).unwrap();
        (directory, recorder, command)
    }

    fn sample(now: u64, session: &str, uptime: u64) -> Sample {
        let mut guest = RawTelemetryEnvelope::new(
            "windows-guest",
            "VirtioMemService",
            session,
            now,
            now,
            now,
            MemoryTelemetrySnapshot {
                physical_total_bytes: 8 << 30,
                physical_available_bytes: 4 << 30,
                memory_load_percent: 50,
                commit_total_bytes: 2 << 30,
                commit_limit_bytes: 8 << 30,
                commit_peak_bytes: 2 << 30,
                system_cache_bytes: 0,
                kernel_paged_bytes: 0,
                kernel_nonpaged_bytes: 0,
            },
        );
        guest.diagnostics = Some(GuestDiagnostics {
            build_id: "test-build".to_owned(),
            driver_file_version: Ok("1.2.3.4".to_owned()),
            environment: std::collections::BTreeMap::new(),
            boot_uptime_millis: Some(uptime),
            policy: Default::default(),
            last_consumed_result: None,
            result_consumed_unix_millis: None,
        });
        Sample {
            unix_millis: now,
            elapsed: Duration::from_millis(now),
            host: crate::watch::HostMemory {
                available: 16 << 30,
                ..Default::default()
            },
            guest: Some(super::super::GuestRead {
                elapsed: Duration::ZERO,
                telemetry: Ok(guest),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn snapshots_are_bounded_pause_respected_and_uptime_distinguishes_reboot() {
        let (directory, mut recorder, command) = fixture("snapshots");
        for (now, session, uptime, paused, reboot) in [
            (1000, "one", 100_000, false, false),
            (2000, "one", 101_000, false, false),
            (3000, "two", 102_000, false, false),
            (4000, "three", 1000, true, true),
            (5000, "three", 2000, false, false),
            (35_000, "three", 32_000, false, false),
        ] {
            let mut observed = Observed::default();
            recorder
                .observe(
                    &sample(now, session, uptime),
                    &[],
                    &command,
                    &mut observed,
                    !paused,
                )
                .unwrap();
            assert_eq!(
                observed
                    .events
                    .iter()
                    .any(|event| event.name.as_deref() == Some("metrics_guest_reboot_detected")),
                reboot
            );
        }
        let records = std::fs::read_to_string(directory.join("guest-publications.jsonl")).unwrap();
        assert_eq!(
            records.lines().count(),
            4,
            "initial, service change, resumed, heartbeat"
        );
        let first: Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
        assert_eq!(
            first["publication"]["diagnostics"]["build_id"],
            "test-build"
        );
        assert_eq!(
            first["publication"]["memory"]["physical_total_bytes"],
            8u64 << 30
        );
        drop(recorder);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sustained_stalls_bundle_once_and_rearm_after_progress() {
        let (directory, mut recorder, command) = fixture("stalls");
        let mut total = 0;
        for (now, current) in [
            (1000, 1024),
            (31_000, 1024),
            (61_000, 1024),
            (62_000, 1024),
            (63_000, 512),
            (93_000, 512),
            (123_000, 512),
        ] {
            let mut sample = Sample {
                unix_millis: now,
                elapsed: Duration::from_millis(now),
                host: crate::watch::HostMemory {
                    available: 16 << 30,
                    ..Default::default()
                },
                ..Default::default()
            };
            sample.device = Some(VirtioMemState {
                size_bytes: 2048,
                block_size_bytes: 2,
                requested_bytes: 0,
                current_bytes: current,
            });
            let mut observed = Observed::default();
            recorder
                .observe(&sample, &[], &command, &mut observed, true)
                .unwrap();
            total += observed.incidents.len();
        }
        assert_eq!(total, 2);
        drop(recorder);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn manifest_records_failure_without_overwriting_initial_evidence() {
        let (directory, recorder, _) = fixture("finish");
        std::fs::write(
            directory.join("run.json"),
            r#"{"started_utc":"original","status":"recording"}"#,
        )
        .unwrap();
        finish_manifest(&directory, &Err("journal unavailable".to_owned())).unwrap();
        let value: Value =
            serde_json::from_slice(&std::fs::read(directory.join("run.json")).unwrap()).unwrap();
        assert_eq!(value["started_utc"], "original");
        assert_eq!(value["status"], "failed");
        assert_eq!(value["error"], "journal unavailable");
        drop(recorder);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
