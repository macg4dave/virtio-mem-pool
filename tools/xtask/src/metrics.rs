//! `cargo xtask metrics`: a read-only recorder for stress tests of one
//! virtio-mem guest. Each sample appends a row to `samples.csv` (host RAM,
//! QEMU RSS, the device, the guest's own view read over the guest agent); new
//! controller journal entries go to `events.jsonl`; `summary.json` keeps
//! running minima, maxima, request latencies and event counts, and is
//! rewritten every minute and at exit, so a killed run still leaves one.
//! `health` follows the controller unit, QEMU, the domain and the guest
//! service; crashes and restarts go to `incidents.jsonl` with an evidence
//! bundle each under `incidents/`.
//!
//! On a terminal it runs a live UI (`ui`) where the sample and guest-read
//! periods can be changed, logging paused and markers added while it records;
//! `--plain` or a non-terminal prints one line per sample instead.
//!
//! Like `watch`, it never resizes, clears latches or restarts anything, and
//! never reads `.host-status`.

mod diagnostics;
mod health;
mod pressure;
mod ui;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use virtio_mem_core::{MemoryResourceNotificationState, RawTelemetryEnvelope, VirtioMemState};
use virtio_mem_host::config::HostConfig;
use virtio_mem_host::guest_channel::PublicationCopyFirst;
use virtio_mem_host::qga::{GuestFileReader, VirshGuestFiles};
use virtio_mem_host::virsh::Virsh;

use self::health::{BundleContext, CpuMeter, Incident, Lifecycle, UnitStatus};
use crate::process;
use crate::watch::{self, HostMemory, COMMAND_TIMEOUT, MAX_TELEMETRY_BYTES};

const MIB: u64 = 1 << 20;
const SUMMARY_PERIOD: Duration = Duration::from_secs(60);
/// Samples kept for the UI chart.
const HISTORY: usize = 5000;
/// Journal events kept for the UI list.
const RECENT_EVENTS: usize = 200;
/// `samples.csv` rows copied into an incident bundle.
const BUNDLE_ROWS: usize = 60;

pub const HELP: &str = "Metrics options (read-only stress-test recorder):
  --config PATH                 Required host_list.toml; loads sibling global
                                and per-VM settings. Optional diagnostic
                                telemetry paths are not needed or read.
  --vm NAME                     The [[vm]] to record; required with several.
  --instance NAME               Controller unit virtio-mem-host@NAME.service;
                                default the VM name.
  --connect URI                 Libvirt URI; default qemu:///system.
  --output DIR                  Run directory; default
                                .artifacts/metrics/VM-YYYYMMDDTHHMMSSZ.
  --interval-seconds N          Sample period; default 1.
  --guest-interval-seconds N    Guest-agent telemetry read period; default 1.
  --duration-seconds N          Stop after N seconds; default until quit.
  --plain                       One line per sample instead of the live UI
                                (also used when stdout is not a terminal).
Writes samples.csv, events.jsonl, incidents.jsonl and summary.json into the
run directory, and an evidence bundle per incident (controller failure, guest
service restart, QEMU exit, crashed domain) under incidents/.
Guest memory is read from the built-in request/control publication over QGA.
In the UI: -/+ sample period, [/] guest-read period, space pause logging,
n sample now, m add a marker, s save summary, w chart window, q quit.
";

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    config_path: PathBuf,
    vm: String,
    alias: String,
    unit: String,
    telemetry_path: String,
    connect: String,
    output: Option<PathBuf>,
    interval: Duration,
    guest_interval: Duration,
    duration: Option<Duration>,
    plain: bool,
    host_floor_bytes: u64,
    telemetry_max_age: Duration,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut config_path = None;
    let mut vm = None;
    let mut instance = None;
    let mut command = Command {
        config_path: PathBuf::new(),
        vm: String::new(),
        alias: String::new(),
        unit: String::new(),
        telemetry_path: String::new(),
        connect: "qemu:///system".to_owned(),
        output: None,
        interval: Duration::from_secs(1),
        guest_interval: Duration::from_secs(1),
        duration: None,
        plain: false,
        host_floor_bytes: 0,
        telemetry_max_age: Duration::ZERO,
    };
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--plain" {
            command.plain = true;
            index += 1;
            continue;
        }
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{option} requires a value"))?;
        let seconds = |minimum: f64| {
            value
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= minimum)
                .map(Duration::from_secs_f64)
                .ok_or_else(|| format!("{option} requires a number of seconds >= {minimum}"))
        };
        match option {
            "--config" => config_path = Some(PathBuf::from(value)),
            "--vm" => vm = Some(value.clone()),
            "--instance" => instance = Some(value.clone()),
            "--connect" => command.connect = value.clone(),
            "--output" => command.output = Some(PathBuf::from(value)),
            "--interval-seconds" => command.interval = seconds(0.5)?,
            "--guest-interval-seconds" => command.guest_interval = seconds(0.5)?,
            "--duration-seconds" => command.duration = Some(seconds(1.0)?),
            _ => return Err(format!("unknown metrics option: {option}\n\n{HELP}")),
        }
        index += 1;
    }
    let config_path = config_path.ok_or_else(|| format!("metrics requires --config\n\n{HELP}"))?;
    command.config_path = config_path.clone();
    let config = HostConfig::load_vm(&config_path, vm.as_deref())
        .map_err(|error| format!("{}: {error}", config_path.display()))?;
    command.unit = format!(
        "virtio-mem-host@{}.service",
        instance.as_deref().unwrap_or(&config.vm_name)
    );
    command.vm = config.vm_name;
    command.alias = config.alias;
    command.telemetry_path = config.raw_telemetry_path;
    command.host_floor_bytes = config.host_min_headroom_bytes;
    command.telemetry_max_age = config.raw_telemetry_max_age;
    Ok(command)
}

/// Settings the UI changes while the sampler runs.
#[derive(Debug, Clone)]
struct Controls {
    interval: Duration,
    guest_interval: Duration,
    /// Samples are still taken and shown, but nothing is written.
    paused: bool,
    sample_now: bool,
    /// Markers waiting to be written to `events.jsonl`.
    markers: Vec<JournalEvent>,
}

/// One chart point per sample.
#[derive(Debug, Clone, Copy)]
struct Point {
    elapsed_seconds: f64,
    host_available_mib: u64,
    device: Option<(u64, u64)>,
    guest_available_mib: Option<u64>,
}

/// Everything the UI shows, shared with the sampler.
struct Live {
    controls: Controls,
    directory: PathBuf,
    started_millis: u64,
    host_floor_bytes: u64,
    latest: Option<Sample>,
    /// The newest successful guest read and when it was taken.
    guest: Option<(u64, RawTelemetryEnvelope)>,
    guest_error: Option<String>,
    history: VecDeque<Point>,
    events: VecDeque<JournalEvent>,
    rows_written: u64,
    summary: Summary,
    finished: bool,
}

impl Live {
    fn new(command: &Command, directory: PathBuf, started_millis: u64) -> Self {
        Self {
            controls: Controls {
                interval: command.interval,
                guest_interval: command.guest_interval,
                paused: false,
                sample_now: false,
                markers: Vec::new(),
            },
            directory,
            started_millis,
            host_floor_bytes: command.host_floor_bytes,
            latest: None,
            guest: None,
            guest_error: None,
            history: VecDeque::new(),
            events: VecDeque::new(),
            rows_written: 0,
            summary: Summary::new(command, started_millis),
            finished: false,
        }
    }

    fn show_event(&mut self, event: JournalEvent) {
        self.events.push_front(event);
        self.events.truncate(RECENT_EVENTS);
    }

    /// Queues a marker for `events.jsonl` and shows it at once.
    fn mark(&mut self, text: String) {
        let marker = JournalEvent::marker(unix_millis() * 1000, text);
        self.show_event(marker.clone());
        self.controls.markers.push(marker);
    }

    fn summary_path(&self) -> PathBuf {
        self.directory.join("summary.json")
    }

    fn write_summary(&self) -> Result<(), String> {
        self.summary.write(&self.summary_path(), unix_millis())
    }
}

fn lock(live: &Mutex<Live>) -> MutexGuard<'_, Live> {
    live.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The run directory's open output files.
struct Recorder {
    directory: PathBuf,
    samples: BufWriter<File>,
    events: BufWriter<File>,
    incidents: BufWriter<File>,
    incident_count: u32,
    /// The newest `samples.csv` rows, logged or not, for incident bundles.
    recent_rows: VecDeque<String>,
}

impl Recorder {
    fn create(directory: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("create {}: {error}", directory.display()))?;
        let mut samples = create(&directory.join("samples.csv"))?;
        writeln!(samples, "{CSV_HEADER}")
            .and_then(|()| samples.flush())
            .map_err(write_error)?;
        Ok(Self {
            directory: directory.to_owned(),
            samples,
            events: create(&directory.join("events.jsonl"))?,
            incidents: create(&directory.join("incidents.jsonl"))?,
            incident_count: 0,
            recent_rows: VecDeque::new(),
        })
    }

    fn event(&mut self, event: &JournalEvent) -> Result<(), String> {
        writeln!(self.events, "{}", event.to_json()).map_err(write_error)
    }

    fn remember(&mut self, row: String) {
        self.recent_rows.push_back(row);
        if self.recent_rows.len() > BUNDLE_ROWS {
            self.recent_rows.pop_front();
        }
    }

    /// Writes the incident's bundle and `incidents.jsonl` record, returning
    /// the record.
    fn incident(
        &mut self,
        incident: &Incident,
        command: &Command,
        repo: &Path,
    ) -> Result<Value, String> {
        self.incident_count += 1;
        let number = self.incident_count;
        let name = format!(
            "incidents/{number:03}-{}-{}.txt",
            compact_utc(incident.unix_millis),
            incident.component.name()
        );
        let context = BundleContext {
            vm: &command.vm,
            unit: &command.unit,
            repo,
            recent_rows: std::iter::once(CSV_HEADER.to_owned())
                .chain(self.recent_rows.iter().cloned())
                .collect(),
        };
        let path = self.directory.join(&name);
        std::fs::create_dir_all(self.directory.join("incidents"))
            .and_then(|()| std::fs::write(&path, health::bundle_text(number, incident, &context)))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        let record = health::record(number, incident, &name);
        writeln!(self.incidents, "{record}")
            .and_then(|()| self.incidents.flush())
            .map_err(write_error)?;
        Ok(record)
    }
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("install signal handler: {error}"))?;
    }
    let started_millis = unix_millis();
    let directory = command.output.clone().unwrap_or_else(|| {
        repo.join(".artifacts/metrics").join(format!(
            "{}-{}",
            command.vm,
            compact_utc(started_millis)
        ))
    });
    let recorder = Recorder::create(&directory)?;
    diagnostics::write_manifest(command, repo, &directory, started_millis)?;
    let live = Mutex::new(Live::new(command, directory.clone(), started_millis));
    let result = if !command.plain && std::io::stdout().is_terminal() {
        std::thread::scope(|scope| {
            let sampler =
                scope.spawn(|| record(command, repo, recorder, &live, &stop, |_, _, _| {}));
            let shown = ui::run(command, &live, &stop);
            stop.store(true, Ordering::Release);
            let recorded = sampler
                .join()
                .unwrap_or_else(|_| Err("metrics sampler panicked".to_owned()));
            shown.and(recorded)
        })
    } else {
        println!(
            "recording {} to {} (Ctrl+C to stop)",
            command.vm,
            directory.display()
        );
        record(
            command,
            repo,
            recorder,
            &live,
            &stop,
            |sample, events, incidents| {
                println!("{}", sample.progress(events));
                for incident in incidents {
                    println!(
                        "INCIDENT {}: {} -> {}",
                        incident["number"],
                        incident["summary"].as_str().unwrap_or_default(),
                        directory
                            .join(incident["bundle"].as_str().unwrap_or_default())
                            .display()
                    );
                }
            },
        )
    };
    diagnostics::finish_manifest(&directory, &result)?;
    let live = lock(&live);
    live.write_summary()?;
    println!("{}", live.summary.headline());
    println!("recorded in {}", directory.display());
    result
}

/// Samples until `stop` or `--duration-seconds`, writing each sample and new
/// journal entry unless paused. Lifecycle events and incidents are written
/// even while paused, like markers.
fn record(
    command: &Command,
    repo: &Path,
    mut recorder: Recorder,
    live: &Mutex<Live>,
    stop: &AtomicBool,
    mut progress: impl FnMut(&Sample, usize, &[Value]),
) -> Result<(), String> {
    let virsh = Virsh::persistent_with_connection("virsh", COMMAND_TIMEOUT, &command.connect);
    let guest_files = PublicationCopyFirst::new(
        &command.vm,
        VirshGuestFiles::new(virsh.clone(), &command.vm),
    );
    let started_millis = lock(live).started_millis;
    let mut journal = JournalFollower::start(&command.unit, started_millis, repo);
    if let Some(reason) = &journal.unreadable {
        let mut live = lock(live);
        live.mark(format!(
            "controller journal unreadable: controller events are not recorded ({reason})"
        ));
        live.summary.journal_unreadable = Some(reason.clone());
    }
    let mut qemu = QemuProcess::default();
    let mut cpu = CpuMeter::default();
    let mut pressure = pressure::Meter::default();
    let mut lifecycle = Lifecycle::new(command.telemetry_max_age.as_millis() as u64);
    let mut diagnostics = diagnostics::Diagnostics::new(&recorder.directory)?;
    let mut guest_read: Option<Instant> = None;
    let mut summary_written = Instant::now();
    let clock = Instant::now();
    let result = (|| {
        while !stop.load(Ordering::Acquire)
            && command
                .duration
                .is_none_or(|duration| clock.elapsed() < duration)
        {
            let started = Instant::now();
            write_markers(&mut recorder, live)?;
            let controls = {
                let mut live = lock(live);
                live.controls.sample_now = false;
                live.controls.clone()
            };
            let now = unix_millis();
            let guest = if guest_read.is_none_or(|read| started - read >= controls.guest_interval) {
                guest_read = Some(started);
                Some(read_guest(&guest_files, &command.telemetry_path))
            } else {
                None
            };
            let controller = UnitStatus::read(&command.unit, repo);
            let (qemu_pid, qemu_rss_bytes) = qemu.sample(&command.vm);
            let sample = Sample {
                unix_millis: now,
                elapsed: clock.elapsed(),
                host: watch::read_meminfo()?,
                qemu_pid,
                qemu_rss_bytes,
                controller_cpu_pct: cpu.percent(controller.as_ref(), now),
                controller,
                device: watch::device_state(&virsh, &command.vm, &command.alias),
                vm_state: health::vm_state(&virsh, &command.vm),
                guest,
                pressure: pressure.sample(),
                collection_millis: started.elapsed().as_secs_f64() * 1000.0,
            };
            let new_events = journal.read(repo);
            if let Some(reason) = &journal.unreadable {
                let mut live = lock(live);
                if live.summary.journal_unreadable.is_none() {
                    live.summary.journal_unreadable = Some(reason.clone());
                }
            }
            let mut observed = lifecycle.observe(&sample, &new_events);
            diagnostics.observe(
                &sample,
                &new_events,
                command,
                &mut observed,
                !controls.paused,
            )?;
            let row = sample.csv_row(command.host_floor_bytes);
            if !controls.paused {
                writeln!(recorder.samples, "{row}")
                    .and_then(|()| recorder.samples.flush())
                    .map_err(write_error)?;
                for event in &new_events {
                    recorder.event(event)?;
                }
            }
            recorder.remember(row);
            for event in &observed.events {
                recorder.event(event)?;
            }
            recorder.events.flush().map_err(write_error)?;
            let incidents = observed
                .incidents
                .iter()
                .map(|incident| recorder.incident(incident, command, repo))
                .collect::<Result<Vec<_>, _>>()?;
            {
                let mut live = lock(live);
                if !controls.paused {
                    live.summary.add_sample(&sample);
                    for event in &new_events {
                        live.summary.add_event(event);
                    }
                    live.rows_written += 1;
                }
                for event in &new_events {
                    live.show_event(event.clone());
                }
                for event in &observed.events {
                    live.summary.add_event(event);
                    live.show_event(event.clone());
                }
                for incident in &incidents {
                    live.summary.add_incident(incident);
                }
                if !incidents.is_empty() {
                    live.write_summary()?;
                }
                if let Some(read) = &sample.guest {
                    match &read.telemetry {
                        Ok(telemetry) => {
                            live.guest = Some((now, telemetry.clone()));
                            live.guest_error = None;
                        }
                        Err(error) => live.guest_error = Some(error.clone()),
                    }
                }
                let guest_available_mib = live
                    .guest
                    .as_ref()
                    .map(|(_, telemetry)| mib(telemetry.memory.physical_available_bytes));
                live.history.push_back(Point {
                    elapsed_seconds: sample.elapsed.as_secs_f64(),
                    host_available_mib: mib(sample.host.available),
                    device: sample
                        .device
                        .map(|device| (mib(device.requested_bytes), mib(device.current_bytes))),
                    guest_available_mib,
                });
                if live.history.len() > HISTORY {
                    live.history.pop_front();
                }
                live.latest = Some(sample.clone());
            }
            progress(
                &sample,
                new_events.len() + observed.events.len(),
                &incidents,
            );

            if summary_written.elapsed() >= SUMMARY_PERIOD {
                lock(live).write_summary()?;
                summary_written = Instant::now();
            }
            loop {
                write_markers(&mut recorder, live)?;
                let controls = lock(live).controls.clone();
                if stop.load(Ordering::Acquire)
                    || controls.sample_now
                    || started.elapsed() >= controls.interval
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    })();
    let flushed = write_markers(&mut recorder, live);
    lock(live).finished = true;
    result.and(flushed)
}

/// Writes the UI's queued markers, including while paused.
fn write_markers(recorder: &mut Recorder, live: &Mutex<Live>) -> Result<(), String> {
    let markers = std::mem::take(&mut lock(live).controls.markers);
    if markers.is_empty() {
        return Ok(());
    }
    for marker in &markers {
        recorder.event(marker)?;
    }
    recorder.events.flush().map_err(write_error)?;
    let mut live = lock(live);
    for marker in &markers {
        live.summary.add_event(marker);
    }
    Ok(())
}

fn create(path: &Path) -> Result<BufWriter<File>, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(BufWriter::new)
        .map_err(|error| format!("create {}: {error}", path.display()))
}

fn write_error(error: std::io::Error) -> String {
    format!("write metrics: {error}")
}

fn unix_millis() -> u64 {
    crate::process::unix_millis().unwrap_or_default()
}

// ---- Guest ------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GuestRead {
    /// How long the guest-agent read took, including failures.
    elapsed: Duration,
    telemetry: Result<RawTelemetryEnvelope, String>,
}

fn read_guest(files: &impl GuestFileReader, telemetry_path: &str) -> GuestRead {
    let started = Instant::now();
    let telemetry = files
        .read_file(telemetry_path, MAX_TELEMETRY_BYTES)
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()));
    GuestRead {
        elapsed: started.elapsed(),
        telemetry,
    }
}

// ---- QEMU process ---------------------------------------------------------

/// The VM's QEMU process, found by its `-name guest=VM,...` argument and
/// looked up again when it exits.
#[derive(Default)]
struct QemuProcess {
    pid: Option<u32>,
}

impl QemuProcess {
    /// The process id and resident set size; no pid when no QEMU runs the VM.
    fn sample(&mut self, vm: &str) -> (Option<u32>, Option<u64>) {
        if let Some(rss) = self.pid.and_then(read_rss) {
            return (self.pid, Some(rss));
        }
        self.pid = find_qemu(vm);
        (self.pid, self.pid.and_then(read_rss))
    }
}

fn find_qemu(vm: &str) -> Option<u32> {
    let wanted = format!("guest={vm}");
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            let mut arguments = cmdline.split(|byte| *byte == 0);
            let program = arguments.next()?;
            if !String::from_utf8_lossy(program).contains("qemu") {
                return None;
            }
            arguments
                .any(|argument| {
                    argument == wanted.as_bytes()
                        || argument.starts_with(format!("{wanted},").as_bytes())
                })
                .then_some(pid)
        })
}

fn read_rss(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_rss(&status)
}

fn parse_rss(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kib| kib * 1024)
}

// ---- Samples ----------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Sample {
    unix_millis: u64,
    elapsed: Duration,
    host: HostMemory,
    qemu_pid: Option<u32>,
    qemu_rss_bytes: Option<u64>,
    /// `None` when `systemctl show` failed.
    controller: Option<UnitStatus>,
    controller_cpu_pct: Option<f64>,
    device: Option<VirtioMemState>,
    /// `virsh domstate --reason`, e.g. `running (booted)`.
    vm_state: Option<String>,
    /// Present only on samples that read the guest.
    guest: Option<GuestRead>,
    pressure: pressure::Pressure,
    collection_millis: f64,
}

const CSV_HEADER: &str = "time_utc,elapsed_s,\
host_total_mib,host_available_mib,host_headroom_over_floor_mib,host_swap_used_mib,\
qemu_rss_mib,controller_active,\
device_size_mib,device_requested_mib,device_current_mib,device_divergence_mib,\
guest_read_ok,guest_read_ms,guest_publication_age_ms,guest_sequence,\
guest_total_mib,guest_available_mib,guest_load_pct,\
guest_commit_mib,guest_commit_limit_mib,guest_commit_headroom_mib,guest_cache_mib,\
guest_signal,\
guest_request,guest_request_mib,guest_request_sequence,\
controller_state,controller_pid,controller_memory_mib,controller_cpu_pct,controller_restarts,\
qemu_pid,vm_state,\
guest_session,guest_commit_peak_mib,guest_kernel_paged_mib,guest_kernel_nonpaged_mib,\
host_memory_psi_some_avg10,host_memory_psi_full_avg10,host_swap_in_pages_s,\
host_swap_out_pages_s,host_major_faults_s,collection_ms,\
guest_policy_reason,guest_desired_mib,guest_safe_floor_mib,guest_history_ready,\
guest_retry_remaining_ms,guest_boot_uptime_ms";

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

fn signed_mib(left: u64, right: u64) -> i64 {
    (left as i128 - right as i128).div_euclid(MIB as i128) as i64
}

fn signal_name(telemetry: &RawTelemetryEnvelope) -> &'static str {
    match telemetry.native_pressure {
        Some(MemoryResourceNotificationState::High) => "high",
        Some(MemoryResourceNotificationState::Neutral) => "neutral",
        Some(MemoryResourceNotificationState::Low) => "low",
        None => "unknown",
    }
}

impl Sample {
    fn controller_active(&self) -> bool {
        self.controller.as_ref().is_some_and(UnitStatus::active)
    }

    fn telemetry(&self) -> Option<&RawTelemetryEnvelope> {
        self.guest.as_ref()?.telemetry.as_ref().ok()
    }

    fn csv_row(&self, host_floor_bytes: u64) -> String {
        let optional = |value: Option<String>| value.unwrap_or_default();
        let host = &self.host;
        let mut columns = vec![
            iso_utc(self.unix_millis),
            format!("{:.1}", self.elapsed.as_secs_f64()),
            mib(host.total).to_string(),
            mib(host.available).to_string(),
            signed_mib(host.available, host_floor_bytes).to_string(),
            mib(host.swap_total.saturating_sub(host.swap_free)).to_string(),
            optional(self.qemu_rss_bytes.map(|rss| mib(rss).to_string())),
            u8::from(self.controller_active()).to_string(),
        ];
        match self.device {
            Some(device) => columns.extend([
                mib(device.size_bytes).to_string(),
                mib(device.requested_bytes).to_string(),
                mib(device.current_bytes).to_string(),
                signed_mib(device.current_bytes, device.requested_bytes).to_string(),
            ]),
            None => columns.extend(std::iter::repeat_n(String::new(), 4)),
        }
        match &self.guest {
            Some(read) => columns.extend([
                u8::from(read.telemetry.is_ok()).to_string(),
                read.elapsed.as_millis().to_string(),
            ]),
            None => columns.extend(std::iter::repeat_n(String::new(), 2)),
        }
        match self.telemetry() {
            Some(telemetry) => {
                let memory = &telemetry.memory;
                let request = telemetry.request.as_ref();
                columns.extend([
                    (self.unix_millis as i64 - telemetry.observed_unix_millis as i64).to_string(),
                    telemetry.sequence.to_string(),
                    mib(memory.physical_total_bytes).to_string(),
                    mib(memory.physical_available_bytes).to_string(),
                    memory.memory_load_percent.to_string(),
                    mib(memory.commit_total_bytes).to_string(),
                    mib(memory.commit_limit_bytes).to_string(),
                    signed_mib(memory.commit_limit_bytes, memory.commit_total_bytes).to_string(),
                    mib(memory.system_cache_bytes).to_string(),
                    signal_name(telemetry).to_owned(),
                    optional(request.map(|request| format!("{:?}", request.operation))),
                    optional(request.map(|request| mib(request.bytes).to_string())),
                    optional(request.map(|request| request.identity.sequence.to_string())),
                ]);
            }
            None => columns.extend(std::iter::repeat_n(String::new(), 13)),
        }
        let controller = self.controller.as_ref();
        columns.extend([
            optional(controller.map(UnitStatus::state)),
            optional(
                controller
                    .and_then(|status| status.main_pid)
                    .map(|pid| pid.to_string()),
            ),
            optional(
                controller
                    .and_then(|status| status.memory_bytes)
                    .map(|bytes| mib(bytes).to_string()),
            ),
            optional(
                self.controller_cpu_pct
                    .map(|percent| format!("{percent:.1}")),
            ),
            optional(controller.map(|status| status.restarts.to_string())),
            optional(self.qemu_pid.map(|pid| pid.to_string())),
            optional(self.vm_state.as_ref().map(|state| state.replace(',', ";"))),
        ]);
        match self.telemetry() {
            Some(telemetry) => {
                let memory = &telemetry.memory;
                columns.extend([
                    telemetry.session_id.replace(',', ";"),
                    mib(memory.commit_peak_bytes).to_string(),
                    mib(memory.kernel_paged_bytes).to_string(),
                    mib(memory.kernel_nonpaged_bytes).to_string(),
                ]);
            }
            None => columns.extend(std::iter::repeat_n(String::new(), 4)),
        }
        for value in [
            self.pressure.some_avg10,
            self.pressure.full_avg10,
            self.pressure.swap_in_pages_s,
            self.pressure.swap_out_pages_s,
            self.pressure.major_faults_s,
        ] {
            columns.push(value.map_or_else(String::new, |value| format!("{value:.3}")));
        }
        columns.push(format!("{:.1}", self.collection_millis));
        if let Some(diagnostics) = self
            .telemetry()
            .and_then(|guest| guest.diagnostics.as_ref())
        {
            columns.extend([
                serde_json::to_value(diagnostics.policy.reason).map_or_else(
                    |error| format!("serialization_error:{error}"),
                    |value| value.as_str().unwrap_or("unknown").to_owned(),
                ),
                diagnostics
                    .policy
                    .desired_bytes
                    .map_or_else(String::new, |bytes| mib(bytes).to_string()),
                diagnostics
                    .policy
                    .safe_floor_bytes
                    .map_or_else(String::new, |bytes| mib(bytes).to_string()),
                u8::from(diagnostics.policy.history_ready).to_string(),
                diagnostics.policy.retry_remaining_millis.to_string(),
                diagnostics
                    .boot_uptime_millis
                    .map_or_else(String::new, |uptime| uptime.to_string()),
            ]);
        } else {
            columns.extend(std::iter::repeat_n(String::new(), 6));
        }
        columns.join(",")
    }

    /// One console line per sample.
    fn progress(&self, new_events: usize) -> String {
        let gib = |bytes: u64| format!("{:.1}G", bytes as f64 / (1u64 << 30) as f64);
        let mut line = format!(
            "{} host avail {}",
            &iso_utc(self.unix_millis)[11..19],
            gib(self.host.available)
        );
        if let Some(rss) = self.qemu_rss_bytes {
            line.push_str(&format!(" qemu rss {}", gib(rss)));
        }
        match self.device {
            Some(device) => line.push_str(&format!(
                " | device {}/{}M",
                mib(device.current_bytes),
                mib(device.requested_bytes)
            )),
            None => line.push_str(" | device unreadable"),
        }
        match (&self.guest, self.telemetry()) {
            (_, Some(telemetry)) => {
                line.push_str(&format!(
                    " | guest avail {} load {}% {}",
                    gib(telemetry.memory.physical_available_bytes),
                    telemetry.memory.memory_load_percent,
                    signal_name(telemetry)
                ));
                if let Some(request) = &telemetry.request {
                    line.push_str(&format!(
                        " req {:?} {}M #{}",
                        request.operation,
                        mib(request.bytes),
                        request.identity.sequence
                    ));
                }
            }
            (Some(_), None) => line.push_str(" | guest read failed"),
            (None, None) => {}
        }
        match &self.controller {
            Some(status) if status.failed() => {
                line.push_str(&format!(" | CONTROLLER FAILED ({})", status.exit()));
            }
            Some(status) if status.active() => {}
            _ => line.push_str(" | CONTROLLER INACTIVE"),
        }
        if new_events > 0 {
            line.push_str(&format!(" | +{new_events} events"));
        }
        line
    }
}

// ---- Journal ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct JournalEvent {
    unix_micros: u64,
    pid: Option<u32>,
    /// The `event=` name, or `None` for other unit messages (systemd starts
    /// and stops, panics).
    name: Option<String>,
    fields: BTreeMap<String, String>,
    message: String,
}

impl JournalEvent {
    fn from_json(line: &str) -> Option<(Self, String)> {
        let entry: Value = serde_json::from_str(line).ok()?;
        let text = |key: &str| entry.get(key)?.as_str().map(str::to_owned);
        let message = text("MESSAGE")?;
        let (name, fields) = parse_event(&message);
        Some((
            Self {
                unix_micros: text("__REALTIME_TIMESTAMP")?.parse().ok()?,
                pid: text("_PID").and_then(|pid| pid.parse().ok()),
                name,
                fields,
                message,
            },
            text("__CURSOR")?,
        ))
    }

    /// An operator note written into `events.jsonl` as `event: "marker"`.
    fn marker(unix_micros: u64, text: String) -> Self {
        Self {
            unix_micros,
            pid: None,
            name: Some("marker".to_owned()),
            fields: BTreeMap::new(),
            message: text,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "time_utc": iso_utc(self.unix_micros / 1000),
            "pid": self.pid,
            "event": self.name,
            "fields": self.fields,
            "message": self.message,
        })
    }
}

/// Splits `... event=NAME key=value key=value with spaces ...` into the name
/// and fields; a word without `=` belongs to the previous value.
fn parse_event(message: &str) -> (Option<String>, BTreeMap<String, String>) {
    let Some((_, rest)) = message.split_once("event=") else {
        return (None, BTreeMap::new());
    };
    let mut words = rest.split(' ');
    let name = words.next().unwrap_or_default().to_owned();
    let mut fields = BTreeMap::new();
    let mut last: Option<String> = None;
    while let Some(word) = words.next() {
        match word.split_once('=').filter(|(key, _)| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }) {
            Some((key, value)) => {
                if matches!(key, "reason" | "error" | "command_result") {
                    let tail = words.collect::<Vec<_>>().join(" ");
                    fields.insert(
                        key.to_owned(),
                        if tail.is_empty() {
                            value.to_owned()
                        } else {
                            format!("{value} {tail}")
                        },
                    );
                    break;
                }
                fields.insert(key.to_owned(), value.to_owned());
                last = Some(key.to_owned());
            }
            None => {
                if let Some(value) = last.as_ref().and_then(|key| fields.get_mut(key)) {
                    value.push(' ');
                    value.push_str(word);
                }
            }
        }
    }
    (Some(name), fields)
}

/// Reads the unit's journal entries written since the last read.
struct JournalFollower {
    unit: String,
    cursor: Option<String>,
    since_seconds: u64,
    /// Why controller events cannot be read, while that is so. A user
    /// without system-journal access sees an empty journal, not an error.
    unreadable: Option<String>,
}

/// Why the system journal cannot be read, judged from what a read of the
/// journal's own first process (always present) returned.
fn journal_access(result: Result<String, String>) -> Option<String> {
    match result {
        Err(error) => Some(error),
        Ok(text) if text.trim().is_empty() => Some(
            "no system journal entries are visible to this user; \
             run as root or as a member of systemd-journal or adm"
                .to_owned(),
        ),
        Ok(_) => None,
    }
}

impl JournalFollower {
    /// Starts after the newest existing entry, so a run records only its own.
    fn start(unit: &str, started_millis: u64, repo: &Path) -> Self {
        let probe = ["-n", "1", "-o", "json", "--no-pager", "_PID=1"].map(process::os);
        let mut follower = Self {
            unit: unit.to_owned(),
            cursor: None,
            since_seconds: started_millis / 1000,
            unreadable: journal_access(process::bounded_text(
                "journalctl",
                &probe,
                repo,
                COMMAND_TIMEOUT,
            )),
        };
        let arguments = ["-u", unit, "-n", "1", "-o", "json", "--no-pager"].map(process::os);
        if let Ok(text) = process::bounded_text("journalctl", &arguments, repo, COMMAND_TIMEOUT) {
            follower.cursor = text
                .lines()
                .rev()
                .find_map(JournalEvent::from_json)
                .map(|(_, cursor)| cursor);
        }
        follower
    }

    fn read(&mut self, repo: &Path) -> Vec<JournalEvent> {
        let mut arguments: Vec<OsString> = ["-u", &self.unit, "-o", "json", "--no-pager"]
            .iter()
            .map(process::os)
            .collect();
        arguments.push(process::os(match &self.cursor {
            Some(cursor) => format!("--after-cursor={cursor}"),
            None => format!("--since=@{}", self.since_seconds),
        }));
        let text = match process::bounded_text("journalctl", &arguments, repo, COMMAND_TIMEOUT) {
            Ok(text) => text,
            Err(error) => {
                self.unreadable = Some(error);
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        for (event, cursor) in text.lines().filter_map(JournalEvent::from_json) {
            self.cursor = Some(cursor);
            events.push(event);
        }
        events
    }
}

#[cfg(test)]
mod journal_tests {
    use super::journal_access;

    #[test]
    fn an_empty_or_failed_journal_read_is_reported_not_counted_as_zero() {
        assert_eq!(journal_access(Ok("{\"MESSAGE\":\"x\"}\n".to_owned())), None);
        assert!(journal_access(Ok(String::new()))
            .unwrap()
            .contains("systemd-journal"));
        assert!(journal_access(Ok("\n".to_owned())).is_some());
        assert_eq!(
            journal_access(Err("journalctl failed".to_owned())),
            Some("journalctl failed".to_owned())
        );
    }
}

// ---- Summary ----------------------------------------------------------------

/// Count, minimum, maximum and mean of one metric.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Stat {
    count: u64,
    min: f64,
    max: f64,
    sum: f64,
}

impl Stat {
    fn add(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum += value;
    }

    fn json(&self) -> Value {
        if self.count == 0 {
            return Value::Null;
        }
        let round = |value: f64| (value * 10.0).round() / 10.0;
        json!({
            "count": self.count,
            "min": round(self.min),
            "max": round(self.max),
            "mean": round(self.sum / self.count as f64),
        })
    }
}

#[derive(Default)]
struct RequestTotals {
    count: u64,
    requested_bytes: u64,
    actual_bytes: u64,
}

struct Summary {
    vm: String,
    unit: String,
    /// Controller events could not be read, so their counts are not zero
    /// but unknown.
    journal_unreadable: Option<String>,
    started_millis: u64,
    host_floor_bytes: u64,
    telemetry_max_age_millis: u64,
    samples: u64,
    controller_inactive_samples: u64,
    host_available_mib: Stat,
    host_headroom_over_floor_mib: Stat,
    host_swap_used_mib: Stat,
    qemu_rss_mib: Stat,
    device_unreadable_samples: u64,
    device_current_mib: Stat,
    /// Start of the current requested != current stretch.
    divergence_since_millis: Option<u64>,
    divergence_seconds: Stat,
    guest_read_failures: u64,
    guest_stale_reads: u64,
    guest_read_ms: Stat,
    guest_publication_age_ms: Stat,
    guest_available_mib: Stat,
    guest_commit_headroom_mib: Stat,
    guest_load_pct: Stat,
    guest_signals: BTreeMap<&'static str, u64>,
    guest_policy_reasons: BTreeMap<String, u64>,
    /// Guest `issued_unix_millis` by (session, sequence).
    guest_issued: HashMap<(String, String), u64>,
    /// Controller grant time (microseconds) by (session, sequence).
    granted: HashMap<(String, String), u64>,
    grant_goals: HashMap<(String, String), (String, u64)>,
    latest_device: Option<VirtioMemState>,
    zero_byte_grants: u64,
    grant_to_completion_seconds: Stat,
    issue_to_completion_seconds: Stat,
    grant_latency_by_operation: BTreeMap<String, Stat>,
    issue_latency_by_operation: BTreeMap<String, Stat>,
    pending_request: Option<virtio_mem_core::MemoryRequest>,
    host_below_reserve_samples: u64,
    pressure_stats: [Stat; 5],
    collection_ms: Stat,
    requests: BTreeMap<String, RequestTotals>,
    refusals: BTreeMap<String, u64>,
    events: BTreeMap<String, u64>,
    controller_pids: Vec<u32>,
    controller_memory_mib: Stat,
    controller_cpu_pct: Stat,
    vm_states: BTreeMap<String, u64>,
    guest_sessions: Vec<String>,
    guest_kernel_paged_mib: Stat,
    guest_kernel_nonpaged_mib: Stat,
    publication_stall_seconds: Stat,
    agent_outage_seconds: Stat,
    read_outage_seconds: Stat,
    notable: Vec<Value>,
    /// `incidents.jsonl` records, also kept here.
    incidents: Vec<Value>,
    incident_counts: BTreeMap<String, u64>,
}

/// Events copied in full into the summary.
const NOTABLE_EVENTS: [&str; 5] = [
    "actuation_latched",
    "ownership_conflict",
    "live_state_unavailable",
    // PAUSED (viomem): no longer emitted; kept so older journals still show it.
    "usable_region_widened",
    "vm_stopped",
];

impl Summary {
    fn new(command: &Command, started_millis: u64) -> Self {
        Self {
            vm: command.vm.clone(),
            unit: command.unit.clone(),
            journal_unreadable: None,
            started_millis,
            host_floor_bytes: command.host_floor_bytes,
            telemetry_max_age_millis: command.telemetry_max_age.as_millis() as u64,
            samples: 0,
            controller_inactive_samples: 0,
            host_available_mib: Stat::default(),
            host_headroom_over_floor_mib: Stat::default(),
            host_swap_used_mib: Stat::default(),
            qemu_rss_mib: Stat::default(),
            device_unreadable_samples: 0,
            device_current_mib: Stat::default(),
            divergence_since_millis: None,
            divergence_seconds: Stat::default(),
            guest_read_failures: 0,
            guest_stale_reads: 0,
            guest_read_ms: Stat::default(),
            guest_publication_age_ms: Stat::default(),
            guest_available_mib: Stat::default(),
            guest_commit_headroom_mib: Stat::default(),
            guest_load_pct: Stat::default(),
            guest_signals: BTreeMap::new(),
            guest_policy_reasons: BTreeMap::new(),
            guest_issued: HashMap::new(),
            granted: HashMap::new(),
            grant_goals: HashMap::new(),
            latest_device: None,
            zero_byte_grants: 0,
            grant_to_completion_seconds: Stat::default(),
            issue_to_completion_seconds: Stat::default(),
            grant_latency_by_operation: BTreeMap::new(),
            issue_latency_by_operation: BTreeMap::new(),
            pending_request: None,
            host_below_reserve_samples: 0,
            pressure_stats: [Stat::default(); 5],
            collection_ms: Stat::default(),
            requests: BTreeMap::new(),
            refusals: BTreeMap::new(),
            events: BTreeMap::new(),
            controller_pids: Vec::new(),
            controller_memory_mib: Stat::default(),
            controller_cpu_pct: Stat::default(),
            vm_states: BTreeMap::new(),
            guest_sessions: Vec::new(),
            guest_kernel_paged_mib: Stat::default(),
            guest_kernel_nonpaged_mib: Stat::default(),
            publication_stall_seconds: Stat::default(),
            agent_outage_seconds: Stat::default(),
            read_outage_seconds: Stat::default(),
            notable: Vec::new(),
            incidents: Vec::new(),
            incident_counts: BTreeMap::new(),
        }
    }

    fn add_incident(&mut self, record: &Value) {
        let component = record["component"].as_str().unwrap_or("unknown");
        *self
            .incident_counts
            .entry(component.to_owned())
            .or_default() += 1;
        if self.incidents.len() < 200 {
            self.incidents.push(record.clone());
        }
    }

    fn incident_total(&self) -> u64 {
        self.incident_counts.values().sum()
    }

    fn add_sample(&mut self, sample: &Sample) {
        self.samples += 1;
        self.collection_ms.add(sample.collection_millis);
        for (stat, value) in self.pressure_stats.iter_mut().zip([
            sample.pressure.some_avg10,
            sample.pressure.full_avg10,
            sample.pressure.swap_in_pages_s,
            sample.pressure.swap_out_pages_s,
            sample.pressure.major_faults_s,
        ]) {
            if let Some(value) = value {
                stat.add(value);
            }
        }
        if !sample.controller_active() {
            self.controller_inactive_samples += 1;
        }
        if let Some(bytes) = sample
            .controller
            .as_ref()
            .and_then(|status| status.memory_bytes)
        {
            self.controller_memory_mib.add(mib(bytes) as f64);
        }
        if let Some(percent) = sample.controller_cpu_pct {
            self.controller_cpu_pct.add(percent);
        }
        if let Some(state) = &sample.vm_state {
            *self.vm_states.entry(state.clone()).or_default() += 1;
        }
        let host = &sample.host;
        self.host_available_mib.add(mib(host.available) as f64);
        if host.available < self.host_floor_bytes {
            self.host_below_reserve_samples += 1;
        }
        self.host_headroom_over_floor_mib
            .add(signed_mib(host.available, self.host_floor_bytes) as f64);
        self.host_swap_used_mib
            .add(mib(host.swap_total.saturating_sub(host.swap_free)) as f64);
        if let Some(rss) = sample.qemu_rss_bytes {
            self.qemu_rss_mib.add(mib(rss) as f64);
        }
        match sample.device {
            Some(device) => {
                self.latest_device = Some(device);
                self.device_current_mib
                    .add(mib(device.current_bytes) as f64);
                let diverged = device.requested_bytes != device.current_bytes;
                match (diverged, self.divergence_since_millis) {
                    (true, None) => self.divergence_since_millis = Some(sample.unix_millis),
                    (false, Some(since)) => {
                        self.divergence_seconds
                            .add(sample.unix_millis.saturating_sub(since) as f64 / 1000.0);
                        self.divergence_since_millis = None;
                    }
                    _ => {}
                }
            }
            None => self.device_unreadable_samples += 1,
        }
        let Some(read) = &sample.guest else {
            return;
        };
        self.guest_read_ms.add(read.elapsed.as_millis() as f64);
        let Ok(telemetry) = &read.telemetry else {
            self.guest_read_failures += 1;
            return;
        };
        let age = sample.unix_millis as i64 - telemetry.observed_unix_millis as i64;
        self.guest_publication_age_ms.add(age as f64);
        if age > self.telemetry_max_age_millis as i64 {
            self.guest_stale_reads += 1;
        }
        let memory = &telemetry.memory;
        self.guest_available_mib
            .add(mib(memory.physical_available_bytes) as f64);
        self.guest_commit_headroom_mib
            .add(signed_mib(memory.commit_limit_bytes, memory.commit_total_bytes) as f64);
        self.guest_load_pct
            .add(f64::from(memory.memory_load_percent));
        self.guest_kernel_paged_mib
            .add(mib(memory.kernel_paged_bytes) as f64);
        self.guest_kernel_nonpaged_mib
            .add(mib(memory.kernel_nonpaged_bytes) as f64);
        if !self.guest_sessions.contains(&telemetry.session_id) {
            self.guest_sessions.push(telemetry.session_id.clone());
        }
        *self
            .guest_signals
            .entry(signal_name(telemetry))
            .or_default() += 1;
        if let Some(diagnostics) = &telemetry.diagnostics {
            let reason = serde_json::to_value(diagnostics.policy.reason).map_or_else(
                |error| format!("serialization_error:{error}"),
                |value| value.as_str().unwrap_or("unknown").to_owned(),
            );
            *self.guest_policy_reasons.entry(reason).or_default() += 1;
        }
        self.pending_request = telemetry.request.clone();
        if let Some(request) = &telemetry.request {
            self.guest_issued
                .entry((
                    request.identity.session_id.clone(),
                    request.identity.sequence.to_string(),
                ))
                .or_insert(request.issued_unix_millis);
        }
    }

    fn add_event(&mut self, event: &JournalEvent) {
        if let Some(pid) = event.pid.filter(|pid| *pid != 1) {
            if !self.controller_pids.contains(&pid) {
                self.controller_pids.push(pid);
            }
        }
        let name = event.name.clone().unwrap_or_else(|| "(other)".to_owned());
        *self.events.entry(name.clone()).or_default() += 1;
        let field = |key: &str| event.fields.get(key).cloned().unwrap_or_default();
        let key = || (field("session_id"), field("sequence"));
        match name.as_str() {
            "controller_decision" if field("action") == "grant" => {
                self.granted.insert(key(), event.unix_micros);
                let grant = field("grant_bytes").parse::<u64>().unwrap_or(0);
                if field("grant_bytes") == "0" {
                    self.zero_byte_grants += 1;
                }
                if let Ok(current) = field("current_bytes").parse::<u64>() {
                    let operation = field("operation");
                    let goal = if operation == "Add" {
                        current.saturating_add(grant)
                    } else {
                        current.saturating_sub(grant)
                    };
                    self.grant_goals.insert(key(), (operation, goal));
                }
            }
            "controller_decision" => {
                *self.refusals.entry(field("reason")).or_default() += 1;
            }
            "request_completed" => {
                self.grant_goals.remove(&key());
                if matches!(field("outcome").as_str(), "PoolEmpty" | "Rejected") {
                    *self.refusals.entry(field("reason")).or_default() += 1;
                }
                let totals = self
                    .requests
                    .entry(format!("{} {}", field("operation"), field("outcome")))
                    .or_default();
                totals.count += 1;
                totals.requested_bytes += field("bytes").parse::<u64>().unwrap_or(0);
                totals.actual_bytes += field("actual_bytes").parse::<u64>().unwrap_or(0);
                if let Some(granted) = self.granted.remove(&key()) {
                    let seconds = event.unix_micros.saturating_sub(granted) as f64 / 1e6;
                    self.grant_to_completion_seconds.add(seconds);
                    self.grant_latency_by_operation
                        .entry(field("operation"))
                        .or_default()
                        .add(seconds);
                }
                if let Some(issued) = self.guest_issued.remove(&key()) {
                    let seconds = (event.unix_micros / 1000).saturating_sub(issued) as f64 / 1000.0;
                    self.issue_to_completion_seconds.add(seconds);
                    self.issue_latency_by_operation
                        .entry(field("operation"))
                        .or_default()
                        .add(seconds);
                }
            }
            "metrics_guest_publication_resumed" => {
                if let Ok(seconds) = field("stalled_seconds").parse() {
                    self.publication_stall_seconds.add(seconds);
                }
            }
            "metrics_guest_agent_restored" => {
                if let Ok(seconds) = field("outage_seconds").parse() {
                    self.agent_outage_seconds.add(seconds);
                }
            }
            "metrics_guest_read_restored" => {
                if let Ok(seconds) = field("outage_seconds").parse() {
                    self.read_outage_seconds.add(seconds);
                }
            }
            _ => {}
        }
        // Lifecycle events from `health` are all notable.
        let notable = NOTABLE_EVENTS.contains(&name.as_str())
            || (name.starts_with("metrics_")
                && !matches!(
                    name.as_str(),
                    "metrics_guest_decision" | "metrics_guest_result_consumed"
                ))
            || (event.name.is_none() && event.pid == Some(1));
        if notable && self.notable.len() < 200 {
            self.notable.push(event.to_json());
        }
    }

    fn json(&self, now_millis: u64) -> Value {
        let mut open_divergence = Value::Null;
        if let Some(since) = self.divergence_since_millis {
            open_divergence = json!(now_millis.saturating_sub(since) as f64 / 1000.0);
        }
        let requests: Map<String, Value> = self
            .requests
            .iter()
            .map(|(key, totals)| {
                (
                    key.clone(),
                    json!({
                        "count": totals.count,
                        "requested_mib": mib(totals.requested_bytes),
                        "actual_mib": mib(totals.actual_bytes),
                    }),
                )
            })
            .collect();
        let latency_json = |values: &BTreeMap<String, Stat>| {
            values
                .iter()
                .map(|(operation, stat)| (operation.clone(), stat.json()))
                .collect::<Map<String, Value>>()
        };
        let pending = self.pending_request.as_ref().map(|request| json!({
            "session_id": request.identity.session_id,
            "sequence": request.identity.sequence,
            "operation": request.operation,
            "requested_mib": mib(request.bytes),
            "age_seconds": now_millis.saturating_sub(request.issued_unix_millis) as f64 / 1000.0,
        }));
        let pending_operations = self.grant_goals.iter().map(|(key, (operation, goal))| {
            let remaining = self.latest_device.map(|device| if operation == "Add" { goal.saturating_sub(device.current_bytes) } else { device.current_bytes.saturating_sub(*goal) });
            json!({ "session_id": key.0, "sequence": key.1, "operation": operation,
                "allocation_goal_mib": mib(*goal), "remaining_mib": remaining.map(mib),
                "age_seconds": self.granted.get(key).map(|micros| now_millis.saturating_sub(micros / 1000) as f64 / 1000.0) })
        }).collect::<Vec<_>>();
        json!({
            "vm": self.vm,
            "unit": self.unit,
            "controller_journal": match &self.journal_unreadable {
                Some(reason) => json!({ "readable": false, "reason": reason }),
                None => json!({ "readable": true }),
            },
            "started_utc": iso_utc(self.started_millis),
            "updated_utc": iso_utc(now_millis),
            "duration_seconds": now_millis.saturating_sub(self.started_millis) / 1000,
            "samples": self.samples,
            "host": {
                "available_mib": self.host_available_mib.json(),
                "below_reserve_samples": self.host_below_reserve_samples,
                "memory_psi_some_avg10": self.pressure_stats[0].json(),
                "memory_psi_full_avg10": self.pressure_stats[1].json(),
                "swap_in_pages_s": self.pressure_stats[2].json(),
                "swap_out_pages_s": self.pressure_stats[3].json(),
                "major_faults_s": self.pressure_stats[4].json(),
                "collection_ms": self.collection_ms.json(),
                "headroom_over_floor_mib": self.host_headroom_over_floor_mib.json(),
                "swap_used_mib": self.host_swap_used_mib.json(),
                "qemu_rss_mib": self.qemu_rss_mib.json(),
                "controller_inactive_samples": self.controller_inactive_samples,
                "controller_pids": self.controller_pids,
                "controller_memory_mib": self.controller_memory_mib.json(),
                "controller_cpu_pct": self.controller_cpu_pct.json(),
                "vm_state_samples": self.vm_states,
            },
            "device": {
                "current_mib": self.device_current_mib.json(),
                "unreadable_samples": self.device_unreadable_samples,
                "divergence_seconds": self.divergence_seconds.json(),
                "open_divergence_seconds": open_divergence,
            },
            "guest": {
                "read_ms": self.guest_read_ms.json(),
                "read_failures": self.guest_read_failures,
                "stale_reads": self.guest_stale_reads,
                "publication_age_ms": self.guest_publication_age_ms.json(),
                "available_mib": self.guest_available_mib.json(),
                "commit_headroom_mib": self.guest_commit_headroom_mib.json(),
                "load_pct": self.guest_load_pct.json(),
                "kernel_paged_mib": self.guest_kernel_paged_mib.json(),
                "kernel_nonpaged_mib": self.guest_kernel_nonpaged_mib.json(),
                "signal_samples": self.guest_signals,
                "policy_reason_samples": self.guest_policy_reasons,
                "service_sessions": self.guest_sessions,
                "publication_stall_seconds": self.publication_stall_seconds.json(),
                "agent_outage_seconds": self.agent_outage_seconds.json(),
                "read_outage_seconds": self.read_outage_seconds.json(),
            },
            "requests": {
                "completed": requests,
                "grant_to_completion_seconds_by_operation": latency_json(&self.grant_latency_by_operation),
                "issue_to_completion_seconds_by_operation": latency_json(&self.issue_latency_by_operation),
                "latest_guest_pending_request": pending,
                "pending_operations": pending_operations,
                "zero_byte_grants": self.zero_byte_grants,
                "grant_to_completion_seconds": self.grant_to_completion_seconds.json(),
                "issue_to_completion_seconds": self.issue_to_completion_seconds.json(),
                "refusals": self.refusals,
            },
            "incidents": {
                "counts": self.incident_counts,
                "records": self.incidents,
            },
            "event_counts": self.events,
            "notable_events": self.notable,
        })
    }

    fn write(&self, path: &Path, now_millis: u64) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.json(now_millis))
            .map_err(|error| error.to_string())?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, format!("{text}\n"))
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    fn headline(&self) -> String {
        let minimum = |stat: &Stat| {
            if stat.count == 0 {
                "-".to_owned()
            } else {
                format!("{:.0}", stat.min)
            }
        };
        let journal = if self.journal_unreadable.is_some() {
            "CONTROLLER JOURNAL UNREADABLE, controller counts unknown; "
        } else {
            ""
        };
        format!(
            "{journal}{} samples; min host avail {} MiB, min guest avail {} MiB; \
             {} completed requests; {} latches; {} guest read failures; {} incidents",
            self.samples,
            minimum(&self.host_available_mib),
            minimum(&self.guest_available_mib),
            self.requests
                .values()
                .map(|totals| totals.count)
                .sum::<u64>(),
            self.events.get("actuation_latched").copied().unwrap_or(0),
            self.guest_read_failures,
            self.incident_total(),
        )
    }
}

// ---- Time -------------------------------------------------------------------

/// UTC civil date of a Unix day (Howard Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn utc_parts(unix_millis: u64) -> (i64, u32, u32, u64, u64, u64, u64) {
    let seconds = unix_millis / 1000;
    let (year, month, day) = civil((seconds / 86_400) as i64);
    let time = seconds % 86_400;
    (
        year,
        month,
        day,
        time / 3600,
        time % 3600 / 60,
        time % 60,
        unix_millis % 1000,
    )
}

/// `2026-10-27T15:40:05.123Z`
fn iso_utc(unix_millis: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = utc_parts(unix_millis);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// `20261027T154005Z`, for directory names.
fn compact_utc(unix_millis: u64) -> String {
    let (year, month, day, hour, minute, second, _) = utc_parts(unix_millis);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked_config() -> String {
        crate::repo_root()
            .expect("repository root")
            .join("fixtures/config/host_list.toml")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn reads_the_vm_from_the_host_config_and_options() {
        let config = tracked_config();
        let expected = HostConfig::load_vm(Path::new(&config), None).expect("tracked config");
        let defaults = parse(&["--config".to_owned(), config.clone()]).expect("defaults");
        assert_eq!(defaults.vm, expected.vm_name);
        assert_eq!(defaults.host_floor_bytes, expected.host_min_headroom_bytes);
        assert_eq!(
            defaults.telemetry_path,
            virtio_mem_core::settings::CONTROL_PATH
        );
        assert_eq!(defaults.telemetry_max_age, expected.raw_telemetry_max_age);
        assert_eq!(defaults.interval, Duration::from_secs(1));
        assert_eq!(defaults.duration, None);
        let command = parse(
            &[
                "--config",
                &config,
                "--output",
                "/tmp/run",
                "--duration-seconds",
                "3600",
                "--instance",
                "other",
            ]
            .map(str::to_owned),
        )
        .expect("options");
        assert_eq!(command.output, Some(PathBuf::from("/tmp/run")));
        assert_eq!(command.duration, Some(Duration::from_secs(3600)));
        assert_eq!(command.unit, "virtio-mem-host@other.service");
        for bad in [
            vec![],
            vec!["--config", &config, "--interval-seconds", "0.1"],
            vec!["--config", &config, "--bogus", "1"],
            vec!["--config", &config, "--once"],
        ] {
            assert!(parse(&bad.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn consolidated_settings_record_control_data_with_or_without_diagnostics() {
        use virtio_mem_core::settings::{CONTROL_PATH, SERVICE_NAME};

        struct ControlFile(Result<Vec<u8>, String>);
        impl GuestFileReader for ControlFile {
            fn read_file(&self, path: &str, maximum_bytes: usize) -> Result<Vec<u8>, String> {
                assert_eq!(
                    path, CONTROL_PATH,
                    "never read optional outputs or host status"
                );
                assert_eq!(maximum_bytes, MAX_TELEMETRY_BYTES);
                self.0.clone()
            }
        }

        let root =
            std::env::temp_dir().join(format!("virtio-mem-metrics-config-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("fixture directory");
        let config_path = root.join("host_list.toml");
        std::fs::write(
            root.join("global-settings.toml"),
            "pool_bytes='16GB'\nhost_min_headroom_bytes='6GB'\npoll_interval_seconds=2\n",
        )
        .expect("global settings");
        for diagnostics in [false, true] {
            let mut list = String::new();
            for (name, alias) in [("alpha", "memory0"), ("beta", "memory1")] {
                list.push_str(&format!(
                    "[[vm]]\nvm_name='{name}'\nalias='{alias}'\npriority=100\n"
                ));
                let vm = format!(
                    r#"
vm_name='{name}'
min_ram='4GB'
max_ram='16GB'
free_ram='2GB'
install_path='C:\Program Files\VirtioMemService\virtio-mem-service.exe'
service_account='NT AUTHORITY\LocalService'
"#
                );
                if diagnostics {
                    list.push_str(&format!("telemetry_path='/tmp/{name}-diagnostics.json'\n"));
                }
                std::fs::write(root.join(format!("{name}_settings.toml")), vm)
                    .expect("VM settings");
            }
            std::fs::write(&config_path, list).expect("host list");
            let args = vec![
                "--config".to_owned(),
                config_path.to_string_lossy().into_owned(),
            ];
            assert!(parse(&args).is_err(), "multiple VMs require selection");
            let mut args = args;
            args.extend(["--vm".to_owned(), "beta".to_owned()]);
            let command = parse(&args).expect("new configuration");
            assert_eq!(command.vm, "beta");
            assert_eq!(command.alias, "memory1");
            assert_eq!(command.host_floor_bytes, 6 << 30);
            assert_eq!(command.telemetry_max_age, Duration::from_secs(12));

            let envelope = RawTelemetryEnvelope::new(
                "beta",
                SERVICE_NAME,
                "session",
                1000,
                1,
                0,
                virtio_mem_core::MemoryTelemetrySnapshot {
                    physical_total_bytes: 14 << 30,
                    physical_available_bytes: 3 << 30,
                    memory_load_percent: 79,
                    commit_total_bytes: 8 << 30,
                    commit_limit_bytes: 16 << 30,
                    commit_peak_bytes: 8 << 30,
                    system_cache_bytes: 0,
                    kernel_paged_bytes: 0,
                    kernel_nonpaged_bytes: 0,
                },
            );
            let directory = root.join(if diagnostics {
                "diagnostics-on"
            } else {
                "diagnostics-off"
            });
            let mut recorder = Recorder::create(&directory).expect("recorder");
            let mut summary = Summary::new(&command, 1000);
            for contents in [
                Ok(serde_json::to_vec(&envelope).expect("control publication")),
                Ok(b"{\"version\":".to_vec()),
                Err("agent unavailable".to_owned()),
            ] {
                let sample = Sample {
                    unix_millis: 1000,
                    elapsed: Duration::ZERO,
                    host: HostMemory::default(),
                    controller: Some(UnitStatus::parse("ActiveState=active")),
                    guest: Some(read_guest(&ControlFile(contents), &command.telemetry_path)),
                    ..Sample::default()
                };
                let row = sample.csv_row(command.host_floor_bytes);
                let columns: BTreeMap<_, _> = CSV_HEADER.split(',').zip(row.split(',')).collect();
                assert_eq!(row.split(',').count(), CSV_HEADER.split(',').count());
                if sample.telemetry().is_some() {
                    assert_eq!(columns["guest_session"], "session");
                    assert_eq!(columns["guest_commit_peak_mib"], "8192");
                    assert_eq!(columns["guest_total_mib"], "14336");
                    assert_eq!(columns["guest_available_mib"], "3072");
                } else {
                    assert_eq!(columns["guest_read_ok"], "0");
                    assert_eq!(columns["guest_total_mib"], "");
                }
                writeln!(recorder.samples, "{row}").expect("sample row");
                summary.add_sample(&sample);
            }
            recorder.samples.flush().expect("flush samples");
            summary
                .write(&directory.join("summary.json"), 1000)
                .expect("summary");
            let json: Value = serde_json::from_str(
                &std::fs::read_to_string(directory.join("summary.json")).expect("read summary"),
            )
            .expect("JSON");
            assert_eq!(json["samples"], 3);
            assert_eq!(json["guest"]["read_failures"], 2);
            assert_eq!(
                std::fs::read_to_string(directory.join("samples.csv"))
                    .expect("CSV")
                    .lines()
                    .count(),
                4
            );
        }
        std::fs::remove_dir_all(root).expect("remove fixtures");
    }

    #[test]
    fn formats_utc_times() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(1_793_115_605_123), "2026-10-27T15:40:05.123Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(compact_utc(1_793_115_605_123), "20261027T154005Z");
    }

    #[test]
    fn parses_journal_events_with_spaced_values() {
        let line = serde_json::json!({
            "__CURSOR": "s=1;i=2",
            "__REALTIME_TIMESTAMP": "1793115605123456",
            "_PID": "988126",
            "MESSAGE": "virtio-mem-host: vm=w alias=a event=resize_settled_short operation_id=9 kept_bytes=1130364928 command_result=Err(Rejected(\"live evidence drifted: qemu_argv (attested a, live b)\"))",
        })
        .to_string();
        let (event, cursor) = JournalEvent::from_json(&line).expect("event");
        assert_eq!(cursor, "s=1;i=2");
        assert_eq!(event.pid, Some(988_126));
        assert_eq!(event.name.as_deref(), Some("resize_settled_short"));
        assert_eq!(event.fields["kept_bytes"], "1130364928");
        assert_eq!(
            event.fields["command_result"],
            "Err(Rejected(\"live evidence drifted: qemu_argv (attested a, live b)\"))"
        );
        assert_eq!(event.to_json()["time_utc"], "2026-10-27T15:40:05.123Z");
        let (other, _) = JournalEvent::from_json(
            &serde_json::json!({
                "__CURSOR": "c",
                "__REALTIME_TIMESTAMP": "1",
                "_PID": "1",
                "MESSAGE": "Started virtio-mem-host@w.service.",
            })
            .to_string(),
        )
        .expect("systemd line");
        assert_eq!(other.name, None);
        assert!(JournalEvent::from_json("not json").is_none());
    }

    #[test]
    fn summary_pairs_grants_with_completions_and_times_divergence() {
        let command = parse(&["--config".to_owned(), tracked_config()]).expect("command");
        let mut summary = Summary::new(&command, 0);
        let event = |micros: u64, message: &str| {
            let (name, fields) = parse_event(message);
            JournalEvent {
                unix_micros: micros,
                pid: Some(7),
                name,
                fields,
                message: message.to_owned(),
            }
        };
        summary.add_event(&event(
            1_000_000,
            "event=controller_decision action=grant session_id=s sequence=4 operation=Add bytes=1073741824",
        ));
        summary.add_event(&event(
            6_500_000,
            "event=request_completed session_id=s sequence=4 operation=Add bytes=1073741824 outcome=Completed actual_bytes=1073741824",
        ));
        summary.add_event(&event(
            7_000_000,
            "event=controller_decision action=refuse session_id=s sequence=5 reason=host headroom",
        ));
        assert_eq!(summary.grant_to_completion_seconds.max, 5.5);
        assert_eq!(summary.requests["Add Completed"].actual_bytes, 1 << 30);
        assert_eq!(summary.refusals["host headroom"], 1);
        assert_eq!(summary.controller_pids, vec![7]);

        let device = |requested: u64, current: u64| VirtioMemState {
            size_bytes: 20 << 30,
            block_size_bytes: 2 * MIB,
            requested_bytes: requested,
            current_bytes: current,
        };
        for (millis, state) in [
            (0, device(1 << 30, 1 << 30)),
            (5_000, device(2 << 30, 1 << 30)),
            (10_000, device(2 << 30, 3 << 29)),
            (17_000, device(2 << 30, 2 << 30)),
        ] {
            summary.add_sample(&Sample {
                unix_millis: millis,
                elapsed: Duration::from_millis(millis),
                host: HostMemory::default(),
                controller: Some(UnitStatus::parse("ActiveState=active")),
                device: Some(state),
                ..Sample::default()
            });
        }
        assert_eq!(summary.divergence_seconds.count, 1);
        assert_eq!(summary.divergence_seconds.max, 12.0);
        let json = summary.json(20_000);
        assert_eq!(json["device"]["open_divergence_seconds"], Value::Null);
        assert_eq!(
            json["requests"]["completed"]["Add Completed"]["actual_mib"],
            1024
        );

        summary.add_event(&event(
            30_000_000,
            "event=metrics_guest_agent_restored outage_seconds=4.5",
        ));
        summary.add_incident(&json!({"component": "controller", "summary": "controller failed"}));
        let json = summary.json(30_000);
        assert_eq!(json["guest"]["agent_outage_seconds"]["max"], 4.5);
        assert_eq!(json["incidents"]["counts"]["controller"], 1);
        assert_eq!(
            json["notable_events"][0]["event"],
            "metrics_guest_agent_restored"
        );
        assert!(summary.headline().ends_with("; 1 incidents"));
    }

    #[test]
    fn failed_results_and_nested_errors_remain_visible() {
        let command = parse(&["--config".to_owned(), tracked_config()]).unwrap();
        let mut summary = Summary::new(&command, 0);
        for (sequence, outcome, reason) in [
            (1, "PoolEmpty", "host reserve"),
            (2, "Rejected", "available=123 grow=64"),
        ] {
            let message = format!("event=request_completed session_id=s sequence={sequence} operation=Add bytes=1024 outcome={outcome} actual_bytes=0 reason={reason}");
            let (name, fields) = parse_event(&message);
            assert_eq!(fields["reason"], reason);
            summary.add_event(&JournalEvent {
                unix_micros: 1_000_000,
                pid: Some(7),
                name,
                fields,
                message,
            });
        }
        let json = summary.json(2000);
        assert_eq!(json["requests"]["completed"]["Add PoolEmpty"]["count"], 1);
        assert_eq!(json["requests"]["refusals"]["available=123 grow=64"], 1);
        let (_, fields) = parse_event("event=resize_requested operation_id=1 command_result=Err(Rejected(available=123 grow=64))");
        assert_eq!(
            fields["command_result"],
            "Err(Rejected(available=123 grow=64))"
        );
    }

    #[test]
    fn csv_rows_match_the_header() {
        let columns = CSV_HEADER.split(',').count();
        let sample = Sample {
            unix_millis: 1,
            elapsed: Duration::ZERO,
            host: HostMemory {
                total: 32 << 30,
                available: 3 << 30,
                swap_total: 0,
                swap_free: 0,
            },
            qemu_pid: Some(42),
            qemu_rss_bytes: Some(9 << 30),
            vm_state: Some("running (booted)".to_owned()),
            guest: Some(GuestRead {
                elapsed: Duration::from_millis(250),
                telemetry: Err("agent unavailable".to_owned()),
            }),
            ..Sample::default()
        };
        let row = sample.csv_row(4 << 30);
        assert_eq!(row.split(',').count(), columns);
        assert!(row.contains(",-1024,"), "{row}");
        assert!(sample
            .progress(2)
            .ends_with("guest read failed | CONTROLLER INACTIVE | +2 events"));

        let failed = Sample {
            controller: Some(UnitStatus::parse(
                "ActiveState=failed\nSubState=failed\nResult=signal\nExecMainPID=7\n\
                 ExecMainCode=2\nExecMainStatus=9\nNRestarts=0\nMemoryCurrent=[not set]",
            )),
            controller_cpu_pct: None,
            ..sample
        };
        let row = failed.csv_row(4 << 30);
        assert_eq!(row.split(',').count(), columns);
        let named: BTreeMap<_, _> = CSV_HEADER.split(',').zip(row.split(',')).collect();
        assert_eq!(named["controller_state"], "failed/failed");
        assert_eq!(named["controller_pid"], "");
        assert_eq!(named["qemu_pid"], "42");
        assert_eq!(named["vm_state"], "running (booted)");
        assert!(failed
            .progress(0)
            .ends_with("CONTROLLER FAILED (killed by SIGKILL)"));
    }

    #[test]
    fn reads_rss_from_proc_status() {
        assert_eq!(parse_rss("Name:\tqemu\nVmRSS:\t  2048 kB\n"), Some(2 << 20));
        assert_eq!(parse_rss("Name:\tqemu\n"), None);
    }
}
