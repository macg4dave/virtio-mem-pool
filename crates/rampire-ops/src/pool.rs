//! `cargo xtask pool`: a live, read-only dashboard of the whole pool. One
//! screen shows the pool's charge split by VM, every VM's device and guest,
//! a stacked history of the pool and the controller's pool events for all
//! VMs.
//!
//! It reads `virsh --readonly dumpxml` per VM, the controller's copy of each
//! guest's publication (`/run/rampire/<vm>.publication.json`, so no
//! guest-agent traffic) and `journalctl`. It never resizes or changes
//! anything.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rampire_core::{RawTelemetryEnvelope, VirtioMemState};
use rampire_host::config::HostSettings;
use rampire_host::guest_channel::publication_copy_path;
use rampire_host::virsh::Virsh;

use crate::dashboard::{
    clock, gib, host_name, load_colour, mib, terminal_columns, Frame, BACKGROUND, CYAN, DIM, GIB,
    GREEN, LABEL, RED, TRACK, VALUE, YELLOW,
};
use crate::process;
use crate::watch::{self, HostMemory, COMMAND_TIMEOUT, MAX_TELEMETRY_BYTES};

pub const HELP: &str = "Pool options (read-only live dashboard of every VM):
  --config PATH                 Required host list; the VMs, pool size and
                                host floor come from it.
  --connect URI                 Libvirt URI; default qemu:///system.
  --interval-seconds N          Refresh period; default 1.
  --events N                    Pool events shown; default 8.
  --once                        Print one plain snapshot.
Ctrl+C leaves a plain snapshot on the screen to copy.
";

/// One colour per VM, in host-list order, repeating after eight.
const VM_COLOURS: [u8; 8] = [51, 213, 214, 120, 117, 228, 177, 210];

/// Rows in the stacked history chart.
const CHART_HEIGHT: usize = 8;

/// Samples kept for the chart: more than the widest terminal shows.
const HISTORY_SAMPLES: usize = 300;

/// A publication older than this is shown as stale.
const STALE_PUBLICATION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Vm {
    name: String,
    alias: String,
    priority: u32,
    min_ram: u64,
    max_ram: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    vms: Vec<Vm>,
    /// `None` for a single-VM controller, which has no shared pool.
    pool_bytes: Option<u64>,
    host_floor_bytes: u64,
    unit: String,
    connect: String,
    interval: Duration,
    events: usize,
    once: bool,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut config_path = None;
    let mut command = Command {
        vms: Vec::new(),
        pool_bytes: None,
        host_floor_bytes: 0,
        unit: crate::units::CONTROLLER_UNIT.to_owned(),
        connect: "qemu:///system".to_owned(),
        interval: Duration::from_secs(1),
        events: 8,
        once: false,
    };
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--once" {
            command.once = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--config" => config_path = Some(PathBuf::from(value)),
            "--connect" => command.connect = value.clone(),
            "--interval-seconds" => {
                command.interval = value
                    .parse::<f64>()
                    .ok()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.1)
                    .map(Duration::from_secs_f64)
                    .ok_or("--interval-seconds requires a number of seconds >= 0.1")?;
            }
            "--events" => {
                command.events = value
                    .parse()
                    .ok()
                    .filter(|count| (1..=30).contains(count))
                    .ok_or("--events requires 1..=30")?;
            }
            _ => return Err(format!("unknown pool option: {option}\n\n{HELP}")),
        }
        index += 2;
    }
    let config_path = config_path.ok_or_else(|| format!("pool requires --config\n\n{HELP}"))?;
    let settings = HostSettings::load(&config_path)
        .map_err(|error| format!("{}: {error}", config_path.display()))?;
    command.vms = settings
        .vms
        .iter()
        .map(|vm| Vm {
            name: vm.vm_name.clone(),
            alias: vm.alias.clone(),
            priority: vm.priority,
            min_ram: vm.min_ram,
            max_ram: vm.max_ram,
        })
        .collect();
    if command.vms.is_empty() {
        return Err("RAMpire manages no VMs yet: there is no pool to show".to_owned());
    }
    command.pool_bytes = settings.pool_bytes;
    command.host_floor_bytes = settings
        .vms
        .iter()
        .map(|vm| vm.host_min_headroom_bytes)
        .max()
        .unwrap_or(0);
    Ok(command)
}

#[derive(Debug, Clone, Default)]
struct VmSample {
    device: Option<VirtioMemState>,
    guest: Option<RawTelemetryEnvelope>,
}

impl VmSample {
    /// What the pool charges this VM: `max(requested, plugged)`, as the
    /// controller counts it.
    fn charge(&self) -> u64 {
        self.device
            .map_or(0, |device| device.requested_bytes.max(device.current_bytes))
    }
}

struct Sample {
    host: HostMemory,
    vms: Vec<VmSample>,
    now_unix_millis: u64,
    samples: u64,
}

/// Session extremes, so a brief squeeze stays visible after it passes.
#[derive(Debug, Clone, Copy, Default)]
struct Session {
    peak_charged: u64,
    low_free: Option<u64>,
    low_host_available: Option<u64>,
}

impl Session {
    fn record(&mut self, charged: u64, free: Option<u64>, host_available: u64) {
        self.peak_charged = self.peak_charged.max(charged);
        if let Some(free) = free {
            self.low_free = Some(self.low_free.map_or(free, |low| low.min(free)));
        }
        self.low_host_available = Some(
            self.low_host_available
                .map_or(host_available, |low| low.min(host_available)),
        );
    }
}

pub fn run(command: &Command, cwd: &Path) -> Result<(), String> {
    let interactive = !command.once && std::io::stdout().is_terminal();
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("install signal handler: {error}"))?;
    }
    let virsh = Virsh::persistent_with_connection("virsh", COMMAND_TIMEOUT, &command.connect);
    let mut stdout = std::io::stdout().lock();
    if interactive {
        // Alternate screen, hidden cursor; restored below on every exit.
        let _ = write!(stdout, "\x1b[?1049h\x1b[?25l");
    }
    let mut history: VecDeque<Vec<u64>> = VecDeque::new();
    let mut session = Session::default();
    let mut samples = 0u64;
    let mut copyable = String::new();
    let result = (|| {
        while !stop.load(Ordering::Acquire) {
            let started = Instant::now();
            samples += 1;
            let vms: Vec<VmSample> = command
                .vms
                .iter()
                .map(|vm| VmSample {
                    device: watch::device_state(&virsh, &vm.name, &vm.alias),
                    guest: read_publication(&publication_copy_path(&vm.name)),
                })
                .collect();
            let sample = Sample {
                host: watch::read_meminfo()?,
                vms,
                now_unix_millis: unix_millis(),
                samples,
            };
            let charged = sample.vms.iter().map(VmSample::charge).sum::<u64>();
            session.record(
                charged,
                command.pool_bytes.map(|pool| pool.saturating_sub(charged)),
                sample.host.available,
            );
            history.push_back(sample.vms.iter().map(VmSample::charge).collect());
            if history.len() > HISTORY_SAMPLES {
                history.pop_front();
            }
            let events = read_events(command, cwd);
            let width = terminal_columns()
                .map_or(100, |columns| columns.saturating_sub(1))
                .clamp(80, 140);
            let mut plain = Frame::new(width, false);
            build(&mut plain, command, &sample, &session, &history, &events);
            copyable = plain.render();
            let output = if interactive {
                let mut frame = Frame::new(width, true);
                build(&mut frame, command, &sample, &session, &history, &events);
                frame.render()
            } else {
                copyable.clone()
            };
            stdout
                .write_all(output.as_bytes())
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("write dashboard: {error}"))?;
            if command.once {
                return Ok(());
            }
            // Sleep in short steps so Ctrl+C restores the terminal promptly.
            while !stop.load(Ordering::Acquire) && started.elapsed() < command.interval {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    })();
    if interactive {
        // Leave the last view as plain text on the normal screen to copy.
        let _ = write!(stdout, "\x1b[0m\x1b[?25h\x1b[?1049l{copyable}");
        let _ = stdout.flush();
    }
    result
}

/// The controller's copy of a guest's publication; missing (controller
/// stopped), oversized or mid-write leaves it empty.
fn read_publication(path: &Path) -> Option<RawTelemetryEnvelope> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > MAX_TELEMETRY_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolEvent {
    time: String,
    vm: String,
    text: String,
    colour: u8,
}

/// The pool events, or why the journal gave none: an unprivileged user
/// outside `systemd-journal` gets no lines at all, not an error. Only event
/// lines are asked for, so a burst of other controller output (libvirt's
/// own error lines) cannot push every event out of the window.
fn read_events(command: &Command, cwd: &Path) -> Result<Vec<PoolEvent>, String> {
    let arguments: Vec<OsString> = [
        "-u",
        &command.unit,
        "--grep",
        " event=",
        "-n",
        "600",
        "--no-pager",
        "-o",
        "short-iso",
    ]
    .iter()
    .map(process::os)
    .collect();
    const UNREADABLE: &str = "journal unreadable: run with sudo or join the systemd-journal group";
    match process::bounded_text("journalctl", &arguments, cwd, COMMAND_TIMEOUT) {
        Ok(journal) if journal.lines().any(|line| line.contains(" event=")) => {
            Ok(parse_events(&journal, command.events))
        }
        Ok(_) if process::effective_uid() == 0 => Ok(Vec::new()),
        // journalctl exits 1 when --grep matches nothing.
        Err(error) if process::effective_uid() == 0 && error.contains("status 1") => Ok(Vec::new()),
        Ok(_) => Err(UNREADABLE.to_owned()),
        Err(error) if error.contains("permission") => Err(UNREADABLE.to_owned()),
        Err(error) => Err(error),
    }
}

/// The value of `name=` in a journal line; `reason=` runs to the end.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" {name}=");
    let start = line.find(&marker)? + marker.len();
    let rest = &line[start..];
    if name == "reason" {
        return Some(rest.trim_end());
    }
    Some(rest.split(' ').next().unwrap_or(rest))
}

fn mebibytes(line: &str, name: &str) -> String {
    field(line, name)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or_else(|| "?".to_owned(), |bytes| format!("{}M", mib(bytes)))
}

/// One pool-relevant journal line as a short, coloured event.
fn pool_event(line: &str) -> Option<PoolEvent> {
    let event = field(line, "event")?;
    let operation = || field(line, "operation").unwrap_or("?");
    let (text, colour) = match event {
        "request_completed" => {
            let outcome = field(line, "outcome").unwrap_or("?");
            let colour = match outcome {
                "Completed" => GREEN,
                "Partial" | "PoolEmpty" => YELLOW,
                _ => RED,
            };
            let mut text = format!(
                "{} {} -> {} {outcome}",
                operation(),
                mebibytes(line, "bytes"),
                mebibytes(line, "actual_bytes")
            );
            if let Some(reason) = field(line, "reason") {
                text.push_str(&format!(": {reason}"));
            }
            (text, colour)
        }
        "controller_decision" => match field(line, "action")? {
            "grant" => (
                format!(
                    "grant {} {} of {}",
                    operation(),
                    mebibytes(line, "grant_bytes"),
                    mebibytes(line, "bytes")
                ),
                CYAN,
            ),
            "refuse" => (
                format!(
                    "refused {} {}: {}",
                    operation(),
                    mebibytes(line, "bytes"),
                    field(line, "reason").unwrap_or("?")
                ),
                RED,
            ),
            _ => return None,
        },
        "resize_settled_short" => (
            format!(
                "settled short at {} of {}",
                mebibytes(line, "kept_bytes"),
                mebibytes(line, "target_bytes")
            ),
            YELLOW,
        ),
        "actuation_latched" => (
            format!("LATCHED: {}", field(line, "reason").unwrap_or("?")),
            RED,
        ),
        "vm_stopped" => (
            format!("stopped: {}", field(line, "reason").unwrap_or("?")),
            RED,
        ),
        "control_channel_lost" => ("control channel lost".to_owned(), YELLOW),
        "control_channel_connected" => ("control channel connected".to_owned(), DIM),
        _ => return None,
    };
    Some(PoolEvent {
        time: line.get(11..19).unwrap_or("--:--:--").to_owned(),
        vm: field(line, "vm").unwrap_or("?").to_owned(),
        text,
        colour,
    })
}

/// Newest-first pool events, consecutive repeats per VM collapsed with a count.
fn parse_events(journal: &str, count: usize) -> Vec<PoolEvent> {
    let mut events: Vec<(PoolEvent, usize)> = Vec::new();
    for event in journal.lines().rev().filter_map(pool_event) {
        if let Some((last, repeats)) = events.last_mut() {
            if last.vm == event.vm && last.text == event.text {
                *repeats += 1;
                continue;
            }
        }
        if events.len() == count {
            break;
        }
        events.push((event, 1));
    }
    events
        .into_iter()
        .map(|(mut event, repeats)| {
            if repeats > 1 {
                event.text = format!("(x{repeats}) {}", event.text);
            }
            event
        })
        .collect()
}

fn vm_colour(index: usize) -> u8 {
    VM_COLOURS[index % VM_COLOURS.len()]
}

/// GiB with two decimals and a `G`, for the narrow table columns.
fn short_gib(bytes: u64) -> String {
    format!("{:.2}G", bytes as f64 / GIB as f64)
}

/// Where each VM's segment ends in a bar of `cells` scaled to `scale`
/// bytes: cumulative rounding, so segments never overlap or overflow.
fn segment_ends(charges: &[u64], scale: u64, cells: usize) -> Vec<usize> {
    let scale = scale.max(1) as f64;
    let mut total = 0u64;
    charges
        .iter()
        .map(|charge| {
            total = total.saturating_add(*charge);
            ((total as f64 / scale * cells as f64).round() as usize).min(cells)
        })
        .collect()
}

/// The bar's full scale: the pool, or with no pool the larger of what is
/// charged and every device's size.
fn scale_bytes(command: &Command, sample: &Sample) -> u64 {
    command.pool_bytes.unwrap_or_else(|| {
        sample
            .vms
            .iter()
            .map(|vm| vm.device.map_or(0, |device| device.size_bytes))
            .sum::<u64>()
            .max(sample.vms.iter().map(VmSample::charge).sum())
    })
}

fn build(
    frame: &mut Frame,
    command: &Command,
    sample: &Sample,
    session: &Session,
    history: &VecDeque<Vec<u64>>,
    events: &Result<Vec<PoolEvent>, String>,
) {
    let g = frame.glyphs;
    frame.bar(
        &format!(
            "{} RAMPIRE POOL  {}  {} VMs",
            g.dot,
            host_name(),
            command.vms.len()
        ),
        &clock(),
    );
    frame.line();
    frame.add(
        format!(
            " refresh {:.1}s   samples {}   {}   Ctrl+C stop",
            command.interval.as_secs_f64(),
            sample.samples,
            command.unit
        ),
        LABEL,
    );
    frame.pad_to(frame.width, BACKGROUND);
    pool_panel(frame, command, sample, session);
    vm_panel(frame, command, sample);
    history_chart(frame, command, sample, history);
    events_panel(frame, command, events);
    frame.bar(
        "Read-only: virsh --readonly, publication copies and the journal",
        "",
    );
}

fn pool_panel(frame: &mut Frame, command: &Command, sample: &Sample, session: &Session) {
    let g = frame.glyphs;
    let charges: Vec<u64> = sample.vms.iter().map(VmSample::charge).collect();
    let charged: u64 = charges.iter().sum();
    let scale = scale_bytes(command, sample);
    frame.top("POOL", "charge = max(requested, plugged) per VM");

    // The stacked bar: each VM's charge in its colour, then the free part.
    let cells = frame.width.saturating_sub(4 + 12 + 8);
    frame.row();
    frame.add(format!("{:<12}", "Charged"), LABEL);
    let mut start = 0;
    for (index, end) in segment_ends(&charges, scale, cells).into_iter().enumerate() {
        frame.add(g.full.repeat(end - start), vm_colour(index));
        start = end;
    }
    frame.add(g.empty.repeat(cells - start), TRACK);
    let fraction = charged as f64 / scale.max(1) as f64;
    frame.add(format!(" {:5.1}%", fraction * 100.0), load_colour(fraction));
    frame.end_row();

    frame.row();
    frame.add(" ".repeat(12), LABEL);
    for (index, vm) in command.vms.iter().enumerate() {
        frame.add(g.full, vm_colour(index));
        frame.add(format!(" {}  ", vm.name), LABEL);
    }
    frame.end_row();

    match command.pool_bytes {
        Some(pool) => {
            let free = pool.saturating_sub(charged);
            frame.metric(
                "Pool size",
                &gib(pool),
                "device memory shared by every VM",
                VALUE,
            );
            frame.metric(
                "Charged",
                &gib(charged),
                &format!("peak {} this session", gib(session.peak_charged)),
                load_colour(fraction),
            );
            let free_colour = if free == 0 {
                RED
            } else if free < pool / 10 {
                YELLOW
            } else {
                GREEN
            };
            frame.metric(
                "Free",
                &gib(free),
                &format!("low {} this session", gib(session.low_free.unwrap_or(free))),
                free_colour,
            );
        }
        None => {
            frame.metric(
                "Charged",
                &gib(charged),
                "single-VM controller: no pool",
                VALUE,
            );
        }
    }

    let (growing, shrinking) = sample.vms.iter().filter_map(|vm| vm.device).fold(
        (0, 0),
        |(growing, shrinking), device| {
            (
                growing + device.requested_bytes.saturating_sub(device.current_bytes),
                shrinking + device.current_bytes.saturating_sub(device.requested_bytes),
            )
        },
    );
    let moving = sample
        .vms
        .iter()
        .filter_map(|vm| vm.device)
        .filter(|device| device.requested_bytes != device.current_bytes)
        .count();
    if moving == 0 {
        frame.metric("Resizing", "none", "every device has converged", GREEN);
    } else {
        frame.metric(
            "Resizing",
            &format!("{moving} VMs"),
            &format!(
                "+{} MiB to plug, -{} MiB to unplug",
                mib(growing),
                mib(shrinking)
            ),
            CYAN,
        );
    }

    let host = sample.host;
    let floor = command.host_floor_bytes;
    let floor_colour = if host.available < floor {
        RED
    } else if host.available < floor + floor / 4 {
        YELLOW
    } else {
        GREEN
    };
    frame.metric(
        "Host available",
        &gib(host.available),
        &format!(
            "floor {:.1} GiB, {:+.2} GiB to spare, low {}",
            floor as f64 / GIB as f64,
            (host.available as f64 - floor as f64) / GIB as f64,
            gib(session.low_host_available.unwrap_or(host.available))
        ),
        floor_colour,
    );
    frame.bottom();
}

fn vm_panel(frame: &mut Frame, command: &Command, sample: &Sample) {
    let g = frame.glyphs;
    let wide = frame.width >= 110;
    frame.top("VMS", "host-list order; higher priority is served first");
    frame.row();
    let mut header = format!(
        "  {:<14}{:>4}  {:>8}  {:<17}",
        "VM", "PRI", "PLUGGED", "DEVICE"
    );
    if wide {
        header.push_str(&format!("{:<14}", "RAM LIMITS"));
    }
    header.push_str("GUEST IN USE        REQUEST");
    frame.add(header, DIM);
    frame.end_row();
    for (index, (vm, state)) in command.vms.iter().zip(&sample.vms).enumerate() {
        frame.row();
        frame.add(format!("{} ", g.full), vm_colour(index));
        let name: String = vm.name.chars().take(13).collect();
        frame.add(format!("{name:<14}"), VALUE);
        frame.add(format!("{:>4}  ", vm.priority), LABEL);
        match state.device {
            None => {
                frame.add(format!("{:>8}  ", "-"), DIM);
                frame.add(format!("{:<17}", "off / unreadable"), RED);
            }
            Some(device) => {
                frame.add(format!("{:>8}  ", short_gib(device.current_bytes)), VALUE);
                let (requested, current) = (device.requested_bytes, device.current_bytes);
                let (text, colour) = match requested.cmp(&current) {
                    std::cmp::Ordering::Equal => ("steady".to_owned(), GREEN),
                    std::cmp::Ordering::Greater => {
                        (format!("grow +{}M", mib(requested - current)), CYAN)
                    }
                    std::cmp::Ordering::Less => {
                        (format!("shrink -{}M", mib(current - requested)), YELLOW)
                    }
                };
                frame.add(format!("{text:<17}"), colour);
            }
        }
        if wide {
            frame.add(
                format!(
                    "{:<14}",
                    format!("{}-{}", short_gib(vm.min_ram), short_gib(vm.max_ram))
                ),
                DIM,
            );
        }
        guest_cells(frame, state.guest.as_ref(), sample.now_unix_millis);
        frame.end_row();
    }
    frame.bottom();
}

/// A small in-use meter, the guest's available RAM and its pending request.
fn guest_cells(frame: &mut Frame, guest: Option<&RawTelemetryEnvelope>, now_unix_millis: u64) {
    const CELLS: usize = 8;
    let g = frame.glyphs;
    let Some(guest) = guest else {
        frame.add("no publication (controller stopped?)", DIM);
        return;
    };
    let memory = &guest.memory;
    let total = memory.physical_total_bytes.max(1);
    let fraction = 1.0 - memory.physical_available_bytes as f64 / total as f64;
    let filled = (fraction.clamp(0.0, 1.0) * CELLS as f64).round() as usize;
    frame.add(g.full.repeat(filled), load_colour(fraction));
    frame.add(g.empty.repeat(CELLS - filled), TRACK);
    frame.add(format!(" {:3.0}%", fraction * 100.0), load_colour(fraction));
    frame.add(
        format!(" of {:<6} ", short_gib(memory.physical_total_bytes)),
        LABEL,
    );
    match &guest.request {
        Some(request) => frame.add(
            format!("{:?} {}M", request.operation, mib(request.bytes)),
            CYAN,
        ),
        None => frame.add("none", DIM),
    }
    let age = Duration::from_millis(now_unix_millis.saturating_sub(guest.observed_unix_millis));
    if age >= STALE_PUBLICATION {
        frame.add(format!("  stale {}s", age.as_secs()), YELLOW);
    }
}

fn history_chart(
    frame: &mut Frame,
    command: &Command,
    sample: &Sample,
    history: &VecDeque<Vec<u64>>,
) {
    let g = frame.glyphs;
    let scale = scale_bytes(command, sample).max(1);
    let columns = frame.width.saturating_sub(4 + 7);
    let points: Vec<&Vec<u64>> = history.iter().rev().take(columns).rev().collect();
    frame.top(
        "POOL HISTORY",
        &format!(
            "last {} samples, stacked by VM, 0..{:.1} GiB",
            points.len(),
            scale as f64 / GIB as f64
        ),
    );
    let ends: Vec<Vec<usize>> = points
        .iter()
        .map(|charges| segment_ends(charges, scale, CHART_HEIGHT * 2))
        .collect();
    for y in (1..=CHART_HEIGHT).rev() {
        frame.row();
        frame.add(
            format!(
                "{:5.1}{}",
                (scale as f64 / GIB as f64) * y as f64 / CHART_HEIGHT as f64,
                g.axis
            ),
            DIM,
        );
        // Half-cell resolution: a cell belongs to the VM covering its middle.
        let middle = y * 2 - 1;
        for column in &ends {
            match column.iter().position(|end| *end > middle) {
                Some(index) => frame.add(g.full, vm_colour(index)),
                None => frame.add(" ", LABEL),
            }
        }
        frame.end_row();
    }
    frame.row();
    frame.add(
        format!("     {}{}", g.corner, g.rule.repeat(points.len())),
        DIM,
    );
    frame.end_row();
    frame.bottom();
}

fn events_panel(frame: &mut Frame, command: &Command, events: &Result<Vec<PoolEvent>, String>) {
    frame.top("POOL EVENTS", "every VM, newest first");
    let events = match events {
        Ok(events) => events,
        Err(reason) => {
            frame.message(reason, YELLOW);
            frame.bottom();
            return;
        }
    };
    let colours: Vec<(&str, u8)> = command
        .vms
        .iter()
        .enumerate()
        .map(|(index, vm)| (vm.name.as_str(), vm_colour(index)))
        .collect();
    for index in 0..command.events {
        frame.row();
        match events.get(index) {
            Some(event) => {
                let colour = colours
                    .iter()
                    .find(|(name, _)| *name == event.vm)
                    .map_or(LABEL, |(_, colour)| *colour);
                let vm: String = event.vm.chars().take(13).collect();
                frame.add(format!("{}  ", event.time), DIM);
                frame.add(format!("{vm:<14}"), colour);
                frame.add(event.text.as_str(), event.colour);
            }
            None if index == 0 => frame.add("no pool events in the journal", DIM),
            None => {}
        }
        frame.end_row();
    }
    frame.bottom();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::MIB;

    fn fixture() -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/config")
            .to_string_lossy()
            .into_owned()
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn two_vm_command() -> Command {
        let vm = |name: &str| Vm {
            name: name.to_owned(),
            alias: "memory0".to_owned(),
            priority: 100,
            min_ram: GIB / 4,
            max_ram: 15 * GIB,
        };
        Command {
            vms: vec![vm("alpha"), vm("a-very-long-vm-name")],
            pool_bytes: Some(15 * GIB),
            host_floor_bytes: 4 * GIB,
            unit: "rampire.service".to_owned(),
            connect: "qemu:///system".to_owned(),
            interval: Duration::from_secs(1),
            events: 8,
            once: true,
        }
    }

    fn device(requested: u64, current: u64) -> Option<VirtioMemState> {
        Some(VirtioMemState {
            size_bytes: 15 * GIB,
            block_size_bytes: 2 * MIB,
            requested_bytes: requested,
            current_bytes: current,
        })
    }

    #[test]
    fn reads_every_vm_and_the_pool_from_the_configuration() {
        let config = fixture();
        let settings = HostSettings::load(Path::new(&config)).expect("tracked config");
        let command = parse(&strings(&["--config", &config])).expect("defaults");
        assert_eq!(command.vms.len(), settings.vms.len());
        assert_eq!(command.vms[0].name, settings.vms[0].vm_name);
        assert_eq!(command.pool_bytes, settings.pool_bytes);
        assert_eq!(command.unit, "rampire.service");
        let command = parse(&strings(&[
            "--config",
            &config,
            "--interval-seconds",
            "0.5",
            "--events",
            "3",
            "--once",
        ]))
        .expect("options");
        assert!(command.once);
        assert_eq!(command.interval, Duration::from_millis(500));
        assert_eq!(command.events, 3);
        for bad in [
            vec![],
            vec!["--config"],
            vec!["--config", &config, "--interval-seconds", "0"],
            vec!["--config", &config, "--events", "0"],
            vec!["--config", &config, "--vm", "x"],
        ] {
            assert!(parse(&strings(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn segments_stack_without_overlap_or_overflow() {
        assert_eq!(segment_ends(&[GIB, GIB, 2 * GIB], 4 * GIB, 8), [2, 4, 8]);
        assert_eq!(segment_ends(&[0, GIB], 4 * GIB, 8), [0, 2]);
        // Over the scale (a shrink still charged) clips at the bar's end.
        assert_eq!(segment_ends(&[3 * GIB, 3 * GIB], 4 * GIB, 8), [6, 8]);
        assert_eq!(segment_ends(&[GIB], 0, 8), [8]);
    }

    #[test]
    fn pool_events_cover_every_vm_newest_first() {
        let journal = "\
2026-11-13T10:00:00+00:00 h rampire[1]: rampire: vm=alpha alias=m0 event=control_health direction=Grow
2026-11-13T10:00:01+00:00 h rampire[1]: rampire: vm=alpha alias=m0 event=controller_decision action=grant session_id=s sequence=1 operation=Add bytes=1073741824 grant_bytes=536870912 pool_ceiling_bytes=None requested_bytes=0 current_bytes=0
2026-11-13T10:00:02+00:00 h rampire[1]: rampire: vm=beta alias=m0 event=request_completed operation_id=none session_id=s sequence=4 operation=Add bytes=1073741824 outcome=PoolEmpty actual_bytes=0 reason=the pool is fully charged
2026-11-13T10:00:03+00:00 h rampire[1]: rampire: vm=beta alias=m0 event=request_completed operation_id=none session_id=s sequence=4 operation=Add bytes=1073741824 outcome=PoolEmpty actual_bytes=0 reason=the pool is fully charged
2026-11-13T10:00:04+00:00 h rampire[1]: rampire: vm=alpha alias=m0 event=request_completed operation_id=7 session_id=s sequence=1 operation=Add bytes=1073741824 outcome=Partial actual_bytes=536870912
";
        let events = parse_events(journal, 8);
        assert_eq!(
            events,
            [
                PoolEvent {
                    time: "10:00:04".to_owned(),
                    vm: "alpha".to_owned(),
                    text: "Add 1024M -> 512M Partial".to_owned(),
                    colour: YELLOW,
                },
                PoolEvent {
                    time: "10:00:03".to_owned(),
                    vm: "beta".to_owned(),
                    text: "(x2) Add 1024M -> 0M PoolEmpty: the pool is fully charged".to_owned(),
                    colour: YELLOW,
                },
                PoolEvent {
                    time: "10:00:01".to_owned(),
                    vm: "alpha".to_owned(),
                    text: "grant Add 512M of 1024M".to_owned(),
                    colour: CYAN,
                },
            ]
        );
        assert_eq!(parse_events(journal, 1).len(), 1);
        assert!(parse_events("no events here", 8).is_empty());
    }

    #[test]
    fn plain_frames_keep_every_line_within_the_width() {
        let command = two_vm_command();
        let sample = Sample {
            host: HostMemory {
                total: 32 * GIB,
                available: 5 * GIB,
                swap_total: 0,
                swap_free: 0,
            },
            vms: vec![
                VmSample {
                    device: device(2 * GIB, GIB),
                    guest: None,
                },
                VmSample {
                    device: None,
                    guest: None,
                },
            ],
            now_unix_millis: 0,
            samples: 1,
        };
        let history: VecDeque<_> = std::iter::repeat_n(vec![2 * GIB, 3 * GIB], 400).collect();
        let events = Ok(vec![PoolEvent {
            time: "10:00:00".to_owned(),
            vm: "alpha".to_owned(),
            text: "x".repeat(500),
            colour: RED,
        }]);
        for width in [80, 140] {
            let mut frame = Frame::new(width, false);
            build(
                &mut frame,
                &command,
                &sample,
                &Session::default(),
                &history,
                &events,
            );
            let text = frame.render();
            assert!(text.contains("grow +1024M"), "{text}");
            assert!(text.contains("off / unreadable"));
            assert!(text.contains("no publication"));
            assert!(text.contains("+1024 MiB to plug"));
            for line in text.lines() {
                assert!(line.chars().count() <= width, "{width}: {line}");
            }
        }
        let mut frame = Frame::new(80, false);
        let unreadable = Err("journal unreadable: join systemd-journal".to_owned());
        build(
            &mut frame,
            &command,
            &sample,
            &Session::default(),
            &history,
            &unreadable,
        );
        assert!(frame.render().contains("journal unreadable"));
    }
}
