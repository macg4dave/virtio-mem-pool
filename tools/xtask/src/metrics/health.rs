//! Service and VM lifecycle for `metrics`. Each sample reads the controller
//! unit with `systemctl show` and the domain with `virsh domstate --reason`;
//! [`Lifecycle`] turns changes in those, in the QEMU process, in the guest
//! service's publication session and in guest-agent reachability into
//! `metrics_*` events. A controller failure, a new guest service session, a
//! QEMU exit and a crashed domain each also become an [`Incident`]: a record
//! in `incidents.jsonl` and a text bundle under `incidents/` holding the
//! evidence still readable at that moment (journal, exit status, panic text,
//! core dump, the publications either side, the samples before it).
//!
//! Windows keeps its own record of a service crash (System log, Service
//! Control Manager 7031/7034; Application log, Application Error 1000); the
//! host cannot read it without guest-exec, so guest bundles point there.

use std::collections::VecDeque;
use std::path::Path;

use rampire::virsh::{Virsh, VirshCommand};
use rampire_core::RawTelemetryEnvelope;
use serde_json::{json, Map, Value};

use super::{iso_utc, JournalEvent, Sample};
use crate::process;
use crate::watch::COMMAND_TIMEOUT;

const UNIT_PROPERTIES: &str = "ActiveState,SubState,Result,MainPID,ExecMainPID,\
ExecMainCode,ExecMainStatus,NRestarts,MemoryCurrent,CPUUsageNSec,InvocationID";
/// Controller journal lines kept for an incident's last words.
const RECENT_LINES: usize = 60;
const LAST_WORDS: usize = 12;
/// Lines after `panicked at` kept as the panic text.
const PANIC_LINES: usize = 8;
const BUNDLE_JOURNAL_LINES: &str = "400";
const QEMU_LOG_TAIL_BYTES: usize = 32 * 1024;

// ---- Controller unit --------------------------------------------------------

/// The controller unit as `systemctl show` reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct UnitStatus {
    pub(super) active_state: String,
    pub(super) sub_state: String,
    /// `success`, or why it stopped: `exit-code`, `signal`, `core-dump`,
    /// `timeout`, `watchdog`, `oom-kill`, ...
    pub(super) result: String,
    pub(super) main_pid: Option<u32>,
    /// The newest main process, kept after it exits.
    pub(super) exec_pid: Option<u32>,
    /// `CLD_*`: 0 still running, 1 exited, 2 killed, 3 dumped.
    exit_code: u32,
    exit_status: i32,
    pub(super) restarts: u64,
    pub(super) memory_bytes: Option<u64>,
    cpu_nanos: Option<u64>,
    invocation: String,
}

impl UnitStatus {
    pub(super) fn read(unit: &str, repo: &Path) -> Option<Self> {
        let property = format!("--property={UNIT_PROPERTIES}");
        let arguments = ["show", unit, "--no-pager", property.as_str()].map(process::os);
        process::bounded_text("systemctl", &arguments, repo, COMMAND_TIMEOUT)
            .ok()
            .map(|text| Self::parse(&text))
    }

    pub(super) fn parse(text: &str) -> Self {
        let mut status = Self::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let pid = || value.parse::<u32>().ok().filter(|pid| *pid != 0);
            // `[not set]` while stopped; u64::MAX when accounting is off.
            let counter = || value.parse::<u64>().ok().filter(|value| *value != u64::MAX);
            match key {
                "ActiveState" => status.active_state = value.to_owned(),
                "SubState" => status.sub_state = value.to_owned(),
                "Result" => status.result = value.to_owned(),
                "MainPID" => status.main_pid = pid(),
                "ExecMainPID" => status.exec_pid = pid(),
                "ExecMainCode" => status.exit_code = value.parse().unwrap_or(0),
                "ExecMainStatus" => status.exit_status = value.parse().unwrap_or(0),
                "NRestarts" => status.restarts = value.parse().unwrap_or(0),
                "MemoryCurrent" => status.memory_bytes = counter(),
                "CPUUsageNSec" => status.cpu_nanos = counter(),
                "InvocationID" => status.invocation = value.to_owned(),
                _ => {}
            }
        }
        status
    }

    pub(super) fn active(&self) -> bool {
        matches!(self.active_state.as_str(), "active" | "reloading")
    }

    /// Stopped other than cleanly. A `systemctl stop` is `success` even when
    /// the process ends on SIGTERM.
    pub(super) fn failed(&self) -> bool {
        !self.active() && !self.result.is_empty() && self.result != "success"
    }

    /// How the newest main process ended: `exit status 101`,
    /// `killed by SIGKILL`, `SIGABRT, core dumped`.
    pub(super) fn exit(&self) -> String {
        match self.exit_code {
            1 => format!("exit status {}", self.exit_status),
            2 => format!("killed by {}", signal_name(self.exit_status)),
            3 => format!("{}, core dumped", signal_name(self.exit_status)),
            _ => "no exit recorded".to_owned(),
        }
    }

    pub(super) fn state(&self) -> String {
        format!("{}/{}", self.active_state, self.sub_state)
    }

    fn dumped(&self) -> bool {
        self.exit_code == 3
    }
}

fn signal_name(number: i32) -> String {
    const NAMES: [&str; 15] = [
        "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2",
        "PIPE", "ALRM", "TERM",
    ];
    usize::try_from(number - 1)
        .ok()
        .and_then(|index| NAMES.get(index))
        .map_or_else(|| format!("signal {number}"), |name| format!("SIG{name}"))
}

/// Controller CPU use between samples, as a percentage of one core.
#[derive(Default)]
pub(super) struct CpuMeter {
    /// Invocation, CPU nanoseconds and wall time of the previous reading.
    last: Option<(String, u64, u64)>,
}

impl CpuMeter {
    pub(super) fn percent(&mut self, status: Option<&UnitStatus>, unix_millis: u64) -> Option<f64> {
        let Some((status, nanos)) = status.and_then(|status| Some((status, status.cpu_nanos?)))
        else {
            self.last = None;
            return None;
        };
        let (invocation, last_nanos, last_millis) =
            self.last
                .replace((status.invocation.clone(), nanos, unix_millis))?;
        (invocation == status.invocation && unix_millis > last_millis).then(|| {
            nanos.saturating_sub(last_nanos) as f64 / ((unix_millis - last_millis) as f64 * 1e4)
        })
    }
}

/// `running (booted)`, `paused (user)`, `crashed (panicked)`,
/// `shut off (crashed)`, ...
pub(super) fn vm_state(virsh: &Virsh, vm: &str) -> Option<String> {
    virsh
        .run(&["domstate".to_owned(), "--reason".to_owned(), vm.to_owned()])
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|state| !state.is_empty())
}

fn vm_crashed(state: &str) -> bool {
    state.starts_with("crashed") || state.contains("(crashed)") || state.contains("(panicked)")
}

// ---- Lifecycle --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Component {
    Controller,
    GuestService,
    Vm,
    Performance,
}

impl Component {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Controller => "controller",
            Self::GuestService => "guest_service",
            Self::Vm => "vm",
        }
    }
}

/// Something that stopped or restarted, with the evidence already in hand.
#[derive(Debug, Clone)]
pub(super) struct Incident {
    pub(super) component: Component,
    pub(super) unix_millis: u64,
    /// One line, e.g. `controller failed: exit status 101 (result exit-code)`.
    pub(super) summary: String,
    pub(super) pid: Option<u32>,
    /// Copied into the `incidents.jsonl` record.
    details: Map<String, Value>,
    /// Titled text written at the top of the bundle.
    sections: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub(super) struct Observed {
    pub(super) events: Vec<JournalEvent>,
    pub(super) incidents: Vec<Incident>,
}

impl Observed {
    pub(super) fn event(
        &mut self,
        unix_millis: u64,
        name: &str,
        fields: &[(&str, String)],
        message: String,
    ) {
        self.events.push(JournalEvent {
            unix_micros: unix_millis * 1000,
            pid: None,
            name: Some(name.to_owned()),
            fields: fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
            message,
        });
    }
}

/// Follows the controller, QEMU, the domain and the guest service between
/// samples. The first sample only sets the baseline, except that a
/// controller already failed or a domain already crashed is reported.
pub(super) struct Lifecycle {
    telemetry_max_age_millis: u64,
    started: bool,
    controller: Option<UnitStatus>,
    /// Recent controller journal lines (not systemd's), newest last.
    controller_lines: VecDeque<JournalEvent>,
    qemu_pid: Option<u32>,
    vm_state: Option<String>,
    /// The newest publication read.
    guest: Option<RawTelemetryEnvelope>,
    /// When the guest last published before the current stall.
    stalled_since: Option<u64>,
    agent_lost_since: Option<(u64, bool)>,
}

impl Lifecycle {
    pub(super) fn new(telemetry_max_age_millis: u64) -> Self {
        Self {
            telemetry_max_age_millis,
            started: false,
            controller: None,
            controller_lines: VecDeque::new(),
            qemu_pid: None,
            vm_state: None,
            guest: None,
            stalled_since: None,
            agent_lost_since: None,
        }
    }

    pub(super) fn observe(&mut self, sample: &Sample, journal: &[JournalEvent]) -> Observed {
        for event in journal
            .iter()
            .filter(|event| event.pid.is_some_and(|pid| pid != 1))
        {
            self.controller_lines.push_back(event.clone());
            if self.controller_lines.len() > RECENT_LINES {
                self.controller_lines.pop_front();
            }
        }
        let mut observed = Observed::default();
        self.observe_controller(sample, &mut observed);
        self.observe_vm(sample, &mut observed);
        self.observe_guest(sample, &mut observed);
        self.started = true;
        observed
    }

    fn observe_controller(&mut self, sample: &Sample, observed: &mut Observed) {
        let now = sample.unix_millis;
        // An unreadable unit keeps the previous state rather than inventing a stop.
        let Some(status) = sample.controller.clone() else {
            return;
        };
        let Some(previous) = self.controller.replace(status.clone()) else {
            if status.failed() {
                observed.incidents.push(self.controller_failure(
                    &status,
                    status.exec_pid,
                    now,
                    "controller already failed when recording started",
                ));
            }
            return;
        };
        let pid_field = |pid: Option<u32>| pid.map_or_else(String::new, |pid| pid.to_string());
        if status.failed() && (!previous.failed() || previous.invocation != status.invocation) {
            let incident =
                self.controller_failure(&status, status.exec_pid, now, "controller failed");
            observed.event(
                now,
                "metrics_controller_failed",
                &[
                    ("pid", pid_field(incident.pid)),
                    ("result", status.result.clone()),
                    ("exit", status.exit()),
                ],
                incident.summary.clone(),
            );
            observed.incidents.push(incident);
        } else if previous.active() && !status.active() {
            observed.event(
                now,
                "metrics_controller_stopped",
                &[("pid", pid_field(previous.main_pid))],
                format!(
                    "controller stopped cleanly ({}), pid {}",
                    status.exit(),
                    pid_field(previous.main_pid)
                ),
            );
        } else if previous.active() && status.active() && previous.main_pid != status.main_pid {
            let fields = [
                ("previous_pid", pid_field(previous.main_pid)),
                ("pid", pid_field(status.main_pid)),
                ("restarts", status.restarts.to_string()),
            ];
            if status.restarts > previous.restarts {
                // A Restart= policy restarted it, so the old process failed.
                let incident = self.controller_failure(
                    &status,
                    previous.main_pid,
                    now,
                    "controller restarted by systemd after a failure",
                );
                observed.event(
                    now,
                    "metrics_controller_restarted",
                    &fields,
                    incident.summary.clone(),
                );
                observed.incidents.push(incident);
            } else {
                observed.event(
                    now,
                    "metrics_controller_restarted",
                    &fields,
                    format!(
                        "controller restarted: pid {} -> {}",
                        pid_field(previous.main_pid),
                        pid_field(status.main_pid)
                    ),
                );
            }
        } else if !previous.active() && status.active() {
            observed.event(
                now,
                "metrics_controller_started",
                &[("pid", pid_field(status.main_pid))],
                format!("controller started, pid {}", pid_field(status.main_pid)),
            );
        }
    }

    /// `pid` is the process that ended; its journal lines are its last words.
    fn controller_failure(
        &self,
        status: &UnitStatus,
        pid: Option<u32>,
        now: u64,
        what: &str,
    ) -> Incident {
        let lines: Vec<&JournalEvent> = self
            .controller_lines
            .iter()
            .filter(|event| pid.is_none() || event.pid == pid)
            .collect();
        let last_words: Vec<String> = lines
            .iter()
            .skip(lines.len().saturating_sub(LAST_WORDS))
            .map(|event| event.message.clone())
            .collect();
        let panic = lines
            .iter()
            .rposition(|event| event.message.contains("panicked at"))
            .map(|start| {
                lines[start..]
                    .iter()
                    .take(PANIC_LINES + 1)
                    .map(|event| event.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        let mut summary = what.to_owned();
        // After a systemd restart the exit fields already describe the new process.
        if !status.active() {
            summary.push_str(&format!(": {} (result {})", status.exit(), status.result));
        }
        if let Some(panic) = &panic {
            let message = panic.lines().nth(1).unwrap_or(panic.as_str());
            summary.push_str(&format!(
                ", panic: {}",
                message.chars().take(120).collect::<String>()
            ));
        }
        let mut details = Map::new();
        details.insert("unit_state".to_owned(), json!(status.state()));
        details.insert("result".to_owned(), json!(status.result));
        details.insert("exit".to_owned(), json!(status.exit()));
        details.insert("restarts".to_owned(), json!(status.restarts));
        details.insert("core_dumped".to_owned(), json!(status.dumped()));
        details.insert("panic".to_owned(), json!(panic));
        details.insert("last_words".to_owned(), json!(last_words));
        let mut sections = Vec::new();
        if let Some(panic) = panic {
            sections.push(("panic".to_owned(), panic));
        }
        sections.push((
            "last controller lines seen by metrics".to_owned(),
            if last_words.is_empty() {
                "(none during this run; see the journal below)".to_owned()
            } else {
                last_words.join("\n")
            },
        ));
        Incident {
            component: Component::Controller,
            unix_millis: now,
            summary,
            pid,
            details,
            sections,
        }
    }

    fn observe_vm(&mut self, sample: &Sample, observed: &mut Observed) {
        let now = sample.unix_millis;
        let state = sample.vm_state.clone();
        let state_text = state.as_deref().unwrap_or("unknown");
        let vm_incident = |summary: String, pid: Option<u32>| {
            let mut details = Map::new();
            details.insert("vm_state".to_owned(), json!(state));
            Incident {
                component: Component::Vm,
                unix_millis: now,
                summary,
                pid,
                details,
                sections: Vec::new(),
            }
        };
        let mut incident = None;
        match (self.qemu_pid, sample.qemu_pid) {
            (Some(old), None) => {
                let summary = format!("QEMU process {old} exited; VM {state_text}");
                observed.event(
                    now,
                    "metrics_qemu_exited",
                    &[
                        ("pid", old.to_string()),
                        ("vm_state", state_text.to_owned()),
                    ],
                    summary.clone(),
                );
                incident = Some(vm_incident(summary, Some(old)));
            }
            (Some(old), Some(new)) if old != new => {
                let summary = format!("QEMU process replaced: {old} -> {new}; VM {state_text}");
                observed.event(
                    now,
                    "metrics_qemu_restarted",
                    &[("previous_pid", old.to_string()), ("pid", new.to_string())],
                    summary.clone(),
                );
                incident = Some(vm_incident(summary, Some(old)));
            }
            (None, Some(new)) if self.started => observed.event(
                now,
                "metrics_qemu_started",
                &[("pid", new.to_string())],
                format!("QEMU process {new} started"),
            ),
            _ => {}
        }
        self.qemu_pid = sample.qemu_pid;
        if let Some(state) = &state {
            if self.vm_state.as_ref() != Some(state) {
                if let Some(previous) = self.vm_state.as_deref() {
                    observed.event(
                        now,
                        "metrics_vm_state",
                        &[("previous", previous.to_owned()), ("state", state.clone())],
                        format!("VM {previous} -> {state}"),
                    );
                }
                if incident.is_none() && vm_crashed(state) {
                    incident = Some(vm_incident(format!("VM {state}"), sample.qemu_pid));
                }
                self.vm_state = Some(state.clone());
            }
        }
        observed.incidents.extend(incident);
    }

    fn observe_guest(&mut self, sample: &Sample, observed: &mut Observed) {
        let now = sample.unix_millis;
        let Some(read) = &sample.guest else {
            return;
        };
        let telemetry = match &read.telemetry {
            Err(error) => {
                if self.agent_lost_since.is_none() {
                    let disconnected =
                        error.contains("not connected") || error.contains("not responding");
                    self.agent_lost_since = Some((now, disconnected));
                    let error = error.clone();
                    observed.event(
                        now,
                        if disconnected {
                            "metrics_guest_agent_lost"
                        } else {
                            "metrics_guest_read_failed"
                        },
                        &[("error", error.clone())],
                        format!("guest publication unreadable: {error}"),
                    );
                }
                return;
            }
            Ok(telemetry) => telemetry,
        };
        if let Some((since, disconnected)) = self.agent_lost_since.take() {
            let seconds = seconds_between(since, now);
            observed.event(
                now,
                if disconnected {
                    "metrics_guest_agent_restored"
                } else {
                    "metrics_guest_read_restored"
                },
                &[("outage_seconds", seconds.clone())],
                format!("guest publication readable again after {seconds}s"),
            );
        }
        let previous = self.guest.replace(telemetry.clone());
        if let Some(previous) =
            previous.filter(|previous| previous.session_id != telemetry.session_id)
        {
            let gap = seconds_between(
                previous.observed_unix_millis,
                telemetry.observed_unix_millis,
            );
            let summary = format!(
                "guest service session changed (service restart or guest reboot), \
                 {gap}s between publications"
            );
            observed.event(
                now,
                "metrics_guest_session_changed",
                &[
                    ("previous_session", previous.session_id.clone()),
                    ("session", telemetry.session_id.clone()),
                    ("gap_seconds", gap.clone()),
                ],
                summary.clone(),
            );
            if let Some(since) = self.stalled_since.take() {
                self.resume(since, telemetry, observed, now);
            }
            observed
                .incidents
                .push(guest_incident(summary, &previous, telemetry, now, gap));
        }
        let age = now.saturating_sub(telemetry.observed_unix_millis);
        match self.stalled_since {
            None if age > self.telemetry_max_age_millis => {
                self.stalled_since = Some(telemetry.observed_unix_millis);
                let seconds = format!("{:.1}", age as f64 / 1000.0);
                observed.event(
                    now,
                    "metrics_guest_publication_stalled",
                    &[
                        ("age_seconds", seconds.clone()),
                        ("sequence", telemetry.sequence.to_string()),
                    ],
                    format!("guest service stopped publishing: newest publication {seconds}s old"),
                );
            }
            Some(since) if age <= self.telemetry_max_age_millis => {
                self.stalled_since = None;
                self.resume(since, telemetry, observed, now);
            }
            _ => {}
        }
    }

    fn resume(
        &self,
        since: u64,
        telemetry: &RawTelemetryEnvelope,
        observed: &mut Observed,
        now: u64,
    ) {
        let seconds = seconds_between(since, telemetry.observed_unix_millis);
        observed.event(
            now,
            "metrics_guest_publication_resumed",
            &[("stalled_seconds", seconds.clone())],
            format!("guest service publishing again; {seconds}s between publications"),
        );
    }
}

pub(super) fn performance_incident(sample: &Sample, reason: &str) -> Incident {
    Incident {
        component: Component::Performance,
        unix_millis: sample.unix_millis,
        summary: format!("sustained performance problem: {reason}"),
        pid: sample
            .controller
            .as_ref()
            .and_then(|status| status.main_pid),
        details: [("reason".to_owned(), json!(reason))].into_iter().collect(),
        sections: vec![(
            "guest publication at detection".to_owned(),
            sample.telemetry().map_or_else(
                || "unavailable".to_owned(),
                |publication| {
                    serde_json::to_string_pretty(publication)
                        .unwrap_or_else(|error| error.to_string())
                },
            ),
        )],
    }
}

fn seconds_between(earlier: u64, later: u64) -> String {
    format!("{:.1}", later.saturating_sub(earlier) as f64 / 1000.0)
}

fn guest_incident(
    summary: String,
    previous: &RawTelemetryEnvelope,
    telemetry: &RawTelemetryEnvelope,
    now: u64,
    gap: String,
) -> Incident {
    let pretty = |envelope: &RawTelemetryEnvelope| {
        serde_json::to_string_pretty(envelope).unwrap_or_else(|error| error.to_string())
    };
    let mut details = Map::new();
    details.insert("previous_session".to_owned(), json!(previous.session_id));
    details.insert("session".to_owned(), json!(telemetry.session_id));
    details.insert("previous_sequence".to_owned(), json!(previous.sequence));
    details.insert(
        "last_publication_utc".to_owned(),
        json!(iso_utc(previous.observed_unix_millis)),
    );
    details.insert("gap_seconds".to_owned(), json!(gap));
    details.insert(
        "request_pending_at_restart".to_owned(),
        json!(previous.request.is_some()),
    );
    Incident {
        component: Component::GuestService,
        unix_millis: now,
        summary,
        pid: None,
        details,
        sections: vec![
            (
                "guest evidence (read inside the guest)".to_owned(),
                guest_evidence_hint(
                    &previous.service_name,
                    &iso_utc(previous.observed_unix_millis.saturating_sub(60_000)),
                ),
            ),
            (
                "last publication of the previous session".to_owned(),
                pretty(previous),
            ),
            (
                "first publication of the new session".to_owned(),
                pretty(telemetry),
            ),
        ],
    }
}

// ---- Bundles ----------------------------------------------------------------

/// What a bundle needs beyond the incident.
pub(super) struct BundleContext<'a> {
    pub(super) vm: &'a str,
    pub(super) unit: &'a str,
    pub(super) repo: &'a Path,
    /// `samples.csv` header and the newest rows, oldest first.
    pub(super) recent_rows: Vec<String>,
}

/// The bundle text: the incident's own evidence, then what is collected now.
pub(super) fn bundle_text(number: u32, incident: &Incident, context: &BundleContext) -> String {
    let mut text = format!(
        "# RAMpire metrics incident {number:03}\n\
         component: {}\ndetected:  {}\nsummary:   {}\nvm:        {}\nunit:      {}\n",
        incident.component.name(),
        iso_utc(incident.unix_millis),
        incident.summary,
        context.vm,
        context.unit,
    );
    let mut section = |title: &str, body: &str| {
        text.push_str(&format!("\n## {title}\n{}\n", body.trim_end()));
    };
    for (title, body) in &incident.sections {
        section(title, body);
    }
    let run = |program: &str, arguments: &[&str]| {
        let arguments: Vec<_> = arguments.iter().map(process::os).collect();
        process::bounded_text(program, &arguments, context.repo, COMMAND_TIMEOUT)
            .unwrap_or_else(|error| format!("(unavailable: {error})"))
    };
    let title = |program: &str, arguments: &[&str]| format!("{program} {}", arguments.join(" "));
    let mut commands: Vec<(&str, Vec<String>)> = vec![(
        "systemctl",
        ["status", context.unit, "--no-pager", "--full", "--lines=0"]
            .map(str::to_owned)
            .to_vec(),
    )];
    if incident.component == Component::Controller {
        let dumped = incident.details.get("core_dumped").and_then(Value::as_bool) == Some(true);
        if let Some(pid) = incident.pid.filter(|_| dumped) {
            commands.push((
                "coredumpctl",
                vec!["info".to_owned(), "--no-pager".to_owned(), pid.to_string()],
            ));
        }
    }
    commands.push((
        "journalctl",
        [
            "-u",
            context.unit,
            "-n",
            BUNDLE_JOURNAL_LINES,
            "-o",
            "short-iso-precise",
            "--no-pager",
        ]
        .map(str::to_owned)
        .to_vec(),
    ));
    for (program, arguments) in &commands {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        section(&title(program, &arguments), &run(program, &arguments));
    }
    if incident.component == Component::Vm {
        let path = format!("/var/log/libvirt/qemu/{}.log", context.vm);
        section(&format!("tail of {path}"), &file_tail(&path));
    }
    section(
        "samples before detection (samples.csv)",
        &context.recent_rows.join("\n"),
    );
    text
}

/// The incident's `incidents.jsonl` record.
pub(super) fn record(number: u32, incident: &Incident, bundle: &str) -> Value {
    let mut record = Map::new();
    record.insert("number".to_owned(), json!(number));
    record.insert("time_utc".to_owned(), json!(iso_utc(incident.unix_millis)));
    record.insert("component".to_owned(), json!(incident.component.name()));
    record.insert("summary".to_owned(), json!(incident.summary));
    record.insert("pid".to_owned(), json!(incident.pid));
    record.insert("bundle".to_owned(), json!(bundle));
    record.extend(incident.details.clone());
    Value::Object(record)
}

fn file_tail(path: &str) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(QEMU_LOG_TAIL_BYTES);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(error) => format!("(unavailable: {error}; read it as root)"),
    }
}

/// Where the guest keeps its own record of a service stop, for the guest OS
/// that publishes under `service_name`. The host cannot read it.
fn guest_evidence_hint(service_name: &str, since: &str) -> String {
    if service_name == rampire_core::settings::GuestOs::Linux.service_name() {
        format!(
            "systemd journal: journalctl -u {service_name} --utc --since '{since}'\n\
             Service state and last exit: systemctl status {service_name}\n\
             Kernel out-of-memory kills: journalctl -k --utc --since '{since}' \
             --grep 'Out of memory'"
        )
    } else {
        format!(
            "Event Viewer > Windows Logs > System: Service Control Manager 7031/7034 \
             (unexpected stop), 7036 (state change).\nWindows Logs > Application: \
             Application Error 1000 and Windows Error Reporting 1001 for {service_name}.\n\
             PowerShell: Get-WinEvent -FilterHashtable @{{LogName='System'; \
             ProviderName='Service Control Manager'; StartTime='{since}'}}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::GuestRead;
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn guest_service_incidents_point_at_the_guest_os_own_logs() {
        let linux = guest_evidence_hint("rampire-guest", "2026-11-13T02:00:00Z");
        assert!(linux.contains("journalctl -u rampire-guest"));
        assert!(!linux.contains("Event Viewer"));
        let windows = guest_evidence_hint("RampireGuest", "2026-11-13T02:00:00Z");
        assert!(windows.contains("Get-WinEvent") && windows.contains("RampireGuest"));
        assert!(windows.contains("StartTime='2026-11-13T02:00:00Z'"));
    }
    use rampire_core::MemoryTelemetrySnapshot;
    use std::time::Duration;

    fn unit(text: &str) -> UnitStatus {
        UnitStatus::parse(&text.replace(' ', "\n"))
    }

    const RUNNING: &str = "ActiveState=active SubState=running Result=success MainPID=10 \
        ExecMainPID=10 ExecMainCode=0 ExecMainStatus=0 NRestarts=0 MemoryCurrent=5390336 \
        CPUUsageNSec=1000000000 InvocationID=a";

    fn sample(millis: u64) -> Sample {
        Sample {
            unix_millis: millis,
            controller: Some(unit(RUNNING)),
            qemu_pid: Some(50),
            vm_state: Some("running (booted)".to_owned()),
            ..Sample::default()
        }
    }

    fn line(pid: u32, message: &str) -> JournalEvent {
        JournalEvent {
            unix_micros: 1,
            pid: Some(pid),
            name: None,
            fields: BTreeMap::new(),
            message: message.to_owned(),
        }
    }

    fn names(observed: &Observed) -> Vec<&str> {
        observed
            .events
            .iter()
            .filter_map(|event| event.name.as_deref())
            .collect()
    }

    #[test]
    fn parses_unit_status_and_describes_exits() {
        let running = unit(RUNNING);
        assert!(running.active() && !running.failed());
        assert_eq!(running.main_pid, Some(10));
        assert_eq!(running.memory_bytes, Some(5_390_336));
        let panicked = unit(
            "ActiveState=failed SubState=failed Result=exit-code MainPID=0 ExecMainPID=10 \
             ExecMainCode=1 ExecMainStatus=101 MemoryCurrent=[not set] CPUUsageNSec=[not set]",
        );
        assert!(panicked.failed());
        assert_eq!((panicked.main_pid, panicked.exec_pid), (None, Some(10)));
        assert_eq!(panicked.memory_bytes, None);
        assert_eq!(panicked.exit(), "exit status 101");
        assert_eq!(
            unit("ExecMainCode=3 ExecMainStatus=6").exit(),
            "SIGABRT, core dumped"
        );
        assert_eq!(
            unit("ExecMainCode=2 ExecMainStatus=9").exit(),
            "killed by SIGKILL"
        );
        assert_eq!(
            unit("ExecMainCode=2 ExecMainStatus=40").exit(),
            "killed by signal 40"
        );
        let stopped = unit(
            "ActiveState=inactive SubState=dead Result=success ExecMainCode=2 ExecMainStatus=15",
        );
        assert!(!stopped.active() && !stopped.failed());
    }

    #[test]
    fn measures_controller_cpu_per_invocation() {
        let mut meter = CpuMeter::default();
        let mut status = unit(RUNNING);
        assert_eq!(meter.percent(Some(&status), 1000), None);
        status.cpu_nanos = Some(1_500_000_000);
        assert_eq!(meter.percent(Some(&status), 2000), Some(50.0));
        status.invocation = "b".to_owned();
        assert_eq!(meter.percent(Some(&status), 3000), None);
    }

    #[test]
    fn controller_panic_becomes_an_incident_with_its_last_words() {
        let mut lifecycle = Lifecycle::new(12_000);
        assert!(lifecycle.observe(&sample(0), &[]).events.is_empty());
        let journal = [
            line(10, "rampire: event=control_health ok"),
            line(10, "thread 'main' panicked at host/src/runtime.rs:12:5:"),
            line(10, "index out of bounds"),
            line(
                1,
                "rampire@w.service: Main process exited, code=exited, status=101/n/a",
            ),
        ];
        let failed = Sample {
            controller: Some(unit(
                "ActiveState=failed SubState=failed Result=exit-code ExecMainPID=10 \
                 ExecMainCode=1 ExecMainStatus=101 InvocationID=a",
            )),
            ..sample(1000)
        };
        let observed = lifecycle.observe(&failed, &journal);
        assert_eq!(names(&observed), ["metrics_controller_failed"]);
        let [incident] = observed.incidents.as_slice() else {
            panic!("one incident: {observed:?}");
        };
        assert_eq!(incident.pid, Some(10));
        assert!(
            incident
                .summary
                .ends_with("exit status 101 (result exit-code), panic: index out of bounds"),
            "{}",
            incident.summary
        );
        assert_eq!(
            incident.details["last_words"].as_array().map(Vec::len),
            Some(3)
        );
        let record = record(1, incident, "incidents/001.txt");
        assert_eq!(record["component"], "controller");
        assert_eq!(record["core_dumped"], false);

        // Still failed on the next sample: not a second incident.
        assert!(lifecycle.observe(&failed, &[]).incidents.is_empty());
        let started = lifecycle.observe(
            &Sample {
                controller: Some(unit(&RUNNING.replace("MainPID=10", "MainPID=11"))),
                ..sample(2000)
            },
            &[],
        );
        assert_eq!(names(&started), ["metrics_controller_started"]);
    }

    #[test]
    fn a_clean_stop_is_an_event_only_and_a_failed_start_is_reported() {
        let mut lifecycle = Lifecycle::new(12_000);
        lifecycle.observe(&sample(0), &[]);
        let stopped = lifecycle.observe(
            &Sample {
                controller: Some(unit("ActiveState=inactive SubState=dead Result=success ExecMainCode=2 ExecMainStatus=15")),
                ..sample(1000)
            },
            &[],
        );
        assert_eq!(names(&stopped), ["metrics_controller_stopped"]);
        assert!(stopped.incidents.is_empty());

        let mut late = Lifecycle::new(12_000);
        let observed = late.observe(
            &Sample {
                controller: Some(unit("ActiveState=failed Result=core-dump ExecMainPID=9 ExecMainCode=3 ExecMainStatus=11")),
                ..sample(0)
            },
            &[],
        );
        assert_eq!(observed.incidents.len(), 1);
        assert!(observed.incidents[0]
            .summary
            .starts_with("controller already failed"));
        assert_eq!(observed.incidents[0].details["core_dumped"], true);
    }

    #[test]
    fn qemu_exit_and_crashed_domain_are_vm_incidents() {
        let mut lifecycle = Lifecycle::new(12_000);
        lifecycle.observe(&sample(0), &[]);
        let gone = lifecycle.observe(
            &Sample {
                qemu_pid: None,
                vm_state: Some("shut off (crashed)".to_owned()),
                ..sample(1000)
            },
            &[],
        );
        assert_eq!(names(&gone), ["metrics_qemu_exited", "metrics_vm_state"]);
        assert_eq!(gone.incidents.len(), 1, "one incident for one crash");
        assert_eq!(gone.incidents[0].component, Component::Vm);
        let back = lifecycle.observe(&sample(2000), &[]);
        assert_eq!(names(&back), ["metrics_qemu_started", "metrics_vm_state"]);
        assert!(back.incidents.is_empty());
        let panicked = lifecycle.observe(
            &Sample {
                vm_state: Some("crashed (panicked)".to_owned()),
                ..sample(3000)
            },
            &[],
        );
        assert_eq!(panicked.incidents[0].summary, "VM crashed (panicked)");
    }

    #[test]
    fn file_read_failures_preserve_the_full_error_without_claiming_agent_loss() {
        let mut lifecycle = Lifecycle::new(12_000);
        let error = format!(
            "guest-file-open failed: {}: being used by another process",
            "long/path/".repeat(30)
        );
        let mut failed = sample(1000);
        failed.guest = Some(super::super::GuestRead {
            elapsed: std::time::Duration::ZERO,
            telemetry: Err(error.clone()),
        });
        let observed = lifecycle.observe(&failed, &[]);
        assert_eq!(names(&observed), ["metrics_guest_read_failed"]);
        assert_eq!(observed.events[0].fields["error"], error);
    }

    #[test]
    fn guest_restarts_stalls_and_agent_outages() {
        let envelope = |session: &str, observed: u64, sequence: u64| {
            RawTelemetryEnvelope::new(
                "w",
                "RampireGuest",
                session,
                observed,
                0,
                sequence,
                MemoryTelemetrySnapshot {
                    physical_total_bytes: 8 << 30,
                    physical_available_bytes: 4 << 30,
                    memory_load_percent: 50,
                    commit_total_bytes: 0,
                    commit_limit_bytes: 0,
                    commit_peak_bytes: 0,
                    system_cache_bytes: 0,
                    kernel_paged_bytes: 0,
                    kernel_nonpaged_bytes: 0,
                },
            )
        };
        let read = |millis: u64, telemetry: Result<RawTelemetryEnvelope, String>| Sample {
            guest: Some(GuestRead {
                elapsed: Duration::ZERO,
                telemetry,
            }),
            ..sample(millis)
        };
        let mut lifecycle = Lifecycle::new(12_000);
        let mut all = Vec::new();
        let mut incidents = Vec::new();
        for sample in [
            read(1_000, Ok(envelope("s1", 1_000, 1))),
            read(20_000, Ok(envelope("s1", 1_000, 1))),
            read(21_000, Err("QEMU guest agent is not connected".to_owned())),
            read(22_000, Err("QEMU guest agent is not connected".to_owned())),
            read(30_000, Ok(envelope("s2", 29_000, 1))),
        ] {
            let observed = lifecycle.observe(&sample, &[]);
            all.extend(names(&observed).into_iter().map(str::to_owned));
            incidents.extend(observed.incidents);
        }
        assert_eq!(
            all,
            [
                "metrics_guest_publication_stalled",
                "metrics_guest_agent_lost",
                "metrics_guest_agent_restored",
                "metrics_guest_session_changed",
                "metrics_guest_publication_resumed",
            ]
        );
        let [incident] = incidents.as_slice() else {
            panic!("one guest incident");
        };
        assert_eq!(incident.details["gap_seconds"], "28.0");
        assert_eq!(incident.sections.len(), 3);
    }

    #[test]
    fn bundles_hold_the_evidence_and_the_samples_before() {
        let mut lifecycle = Lifecycle::new(12_000);
        lifecycle.observe(&sample(0), &[]);
        let observed = lifecycle.observe(
            &Sample {
                qemu_pid: None,
                ..sample(1000)
            },
            &[],
        );
        let context = BundleContext {
            vm: "no-such-vm-for-tests",
            unit: "rampire@no-such-vm-for-tests.service",
            repo: Path::new("."),
            recent_rows: vec!["time_utc,elapsed_s".to_owned(), "x,1.0".to_owned()],
        };
        let text = bundle_text(7, &observed.incidents[0], &context);
        for expected in [
            "# RAMpire metrics incident 007",
            "component: vm",
            "## journalctl -u rampire@no-such-vm-for-tests.service",
            "## tail of /var/log/libvirt/qemu/no-such-vm-for-tests.log\n(unavailable:",
            "## samples before detection (samples.csv)\ntime_utc,elapsed_s\nx,1.0",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
    }
}
