//! `cargo xtask watch`: a live, read-only host-side dashboard of one
//! virtio-mem guest. It shows host RAM against the controller floor, the
//! device, Windows' own view read over the guest agent, a device history
//! chart and the controller's latest journal events.
//!
//! It uses `virsh --readonly dumpxml`, guest-agent reads of the Windows
//! publication and `journalctl`. It never resizes, clears latches or restarts
//! anything. It deliberately does not read `.host-status`: a guest-agent read
//! holds that file open across several commands and makes the controller's
//! write of it fail with a sharing violation; the controller journal records
//! the same results.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rampire::config::HostConfig;
use rampire::guest_channel::PublicationCopyFirst;
use rampire::host_memory::meminfo_bytes;
use rampire::qga::{GuestFileReader, VirshGuestFiles};
use rampire::virsh::{Virsh, VirshCommand};
use rampire_core::{
    parse_virtio_mem_xml_for_alias, MemoryResourceNotificationState, RawTelemetryEnvelope,
    VirtioMemState,
};

use crate::dashboard::{
    clock, gib, host_name, mib, terminal_columns, Frame, BACKGROUND, CYAN, DIM, GIB, GREEN, LABEL,
    MAGENTA, MIB, RED, VALUE, YELLOW,
};
use crate::process;

pub(crate) const MAX_TELEMETRY_BYTES: usize = 64 * 1024;
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub const HELP: &str = "Watch options (read-only live dashboard):
  --config PATH                 Required host TOML; the VM, device alias,
                                telemetry path and host floor come from it.
  --vm NAME                     The [[vm]] to show; required with several.
  --instance NAME               Controller unit rampire@NAME.service;
                                default the one installed controller unit,
                                else the VM name.
  --connect URI                 Libvirt URI; default qemu:///system.
  --interval-seconds N          Refresh period; default 1.
  --guest-interval-seconds N    Guest-agent telemetry read period; default 1.
  --events N                    Journal events shown; default 6.
  --no-guest                    Skip guest-agent reads.
  --once                        Print one plain snapshot.
Ctrl+C leaves a plain snapshot and the full recent events on the screen to copy.
";

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    vm: String,
    alias: String,
    unit: String,
    /// `--instance` named the unit; otherwise `run` looks for the installed one.
    unit_named: bool,
    telemetry_path: String,
    connect: String,
    interval: Duration,
    guest_interval: Duration,
    host_floor_bytes: u64,
    events: usize,
    guest: bool,
    once: bool,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut config_path = None;
    let mut vm = None;
    let mut instance = None;
    let mut command = Command {
        vm: String::new(),
        alias: String::new(),
        unit: String::new(),
        unit_named: false,
        telemetry_path: String::new(),
        connect: "qemu:///system".to_owned(),
        interval: Duration::from_secs(1),
        guest_interval: Duration::from_secs(1),
        host_floor_bytes: 0,
        events: 6,
        guest: true,
        once: false,
    };
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        match option {
            "--no-guest" => command.guest = false,
            "--once" => command.once = true,
            _ => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{option} requires a value"))?;
                let seconds = || {
                    value
                        .parse::<f64>()
                        .ok()
                        .filter(|seconds| seconds.is_finite() && *seconds >= 0.1)
                        .map(Duration::from_secs_f64)
                        .ok_or_else(|| format!("{option} requires a number of seconds >= 0.1"))
                };
                match option {
                    "--config" => config_path = Some(std::path::PathBuf::from(value)),
                    "--vm" => vm = Some(value.clone()),
                    "--instance" => instance = Some(value.clone()),
                    "--connect" => command.connect = value.clone(),
                    "--interval-seconds" => command.interval = seconds()?,
                    "--guest-interval-seconds" => command.guest_interval = seconds()?,
                    "--events" => {
                        command.events = value
                            .parse()
                            .ok()
                            .filter(|count| (1..=20).contains(count))
                            .ok_or("--events requires 1..=20")?;
                    }
                    _ => return Err(format!("unknown watch option: {option}\n\n{HELP}")),
                }
            }
        }
        index += 1;
    }
    let config_path = config_path.ok_or_else(|| format!("watch requires --config\n\n{HELP}"))?;
    let config = HostConfig::load_vm(&config_path, vm.as_deref())
        .map_err(|error| format!("{}: {error}", config_path.display()))?;
    command.unit = format!(
        "rampire@{}.service",
        instance.as_deref().unwrap_or(&config.vm_name)
    );
    command.unit_named = instance.is_some();
    command.vm = config.vm_name;
    command.alias = config.alias;
    command.telemetry_path = config.raw_telemetry_path;
    command.host_floor_bytes = config.host_min_headroom_bytes;
    Ok(command)
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    let resolved;
    let command = if command.unit_named {
        command
    } else {
        resolved = Command {
            unit: crate::host_deploy::installed_controller_unit(&command.unit, repo),
            ..command.clone()
        };
        &resolved
    };
    let interactive = !command.once && std::io::stdout().is_terminal();
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("install signal handler: {error}"))?;
    }
    let virsh = Virsh::persistent_with_connection("virsh", COMMAND_TIMEOUT, &command.connect);
    let guest_files = PublicationCopyFirst::new(
        &command.vm,
        VirshGuestFiles::new(virsh.clone(), &command.vm),
    );
    let mut stdout = std::io::stdout().lock();
    if interactive {
        // Alternate screen, hidden cursor; restored below on every exit.
        let _ = write!(stdout, "\x1b[?1049h\x1b[?25l");
    }
    let mut history = VecDeque::new();
    let mut guest = GuestView::default();
    let mut guest_read: Option<Instant> = None;
    let mut samples = 0u64;
    // The last frame as plain text and the full events, left on screen at exit.
    let mut copyable = String::new();
    let result = (|| {
        while !stop.load(Ordering::Acquire) {
            let started = Instant::now();
            samples += 1;
            let device = device_state(&virsh, &command.vm, &command.alias);
            if let Some(device) = device {
                history.push_back(device);
                if history.len() > 300 {
                    history.pop_front();
                }
            }
            if command.guest
                && guest_read.is_none_or(|read| started - read >= command.guest_interval)
            {
                guest = read_guest(&guest_files, &command.telemetry_path);
                guest_read = Some(started);
            }
            let width = terminal_columns()
                .map_or(100, |columns| columns.saturating_sub(1))
                .clamp(80, 120);
            let sample = Sample {
                host: read_meminfo()?,
                device,
                guest: &guest,
                guest_age: guest_read.map(|read| started - read),
                samples,
            };
            let journal = read_journal(command, repo);
            let mut plain = Frame::new(width, false);
            build(&mut plain, command, &sample, &history, &journal);
            copyable = format!("{}{}", plain.render(), full_events(&journal.events));
            let output = if interactive {
                let mut frame = Frame::new(width, true);
                build(&mut frame, command, &sample, &history, &journal);
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

struct Sample<'a> {
    host: HostMemory,
    device: Option<VirtioMemState>,
    guest: &'a GuestView,
    guest_age: Option<Duration>,
    samples: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostMemory {
    pub(crate) total: u64,
    pub(crate) available: u64,
    pub(crate) swap_total: u64,
    pub(crate) swap_free: u64,
}

pub(crate) fn read_meminfo() -> Result<HostMemory, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("read /proc/meminfo: {error}"))?;
    Ok(parse_meminfo(&text))
}

fn parse_meminfo(text: &str) -> HostMemory {
    let field = |name: &str| meminfo_bytes(text, name).unwrap_or(0);
    HostMemory {
        total: field("MemTotal"),
        available: field("MemAvailable"),
        swap_total: field("SwapTotal"),
        swap_free: field("SwapFree"),
    }
}

pub(crate) fn device_state(virsh: &Virsh, vm: &str, alias: &str) -> Option<VirtioMemState> {
    let xml = virsh.run(&["dumpxml".to_owned(), vm.to_owned()]).ok()?;
    parse_virtio_mem_xml_for_alias(&xml, alias)
        .ok()
        .map(|state| state.memory)
}

#[derive(Default)]
struct GuestView {
    telemetry: Option<RawTelemetryEnvelope>,
}

/// A missing agent, file or mid-write view simply leaves it empty.
fn read_guest(files: &impl GuestFileReader, telemetry_path: &str) -> GuestView {
    GuestView {
        telemetry: files
            .read_file(telemetry_path, MAX_TELEMETRY_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    time: String,
    /// Shortened for the panel.
    text: String,
    /// The journal's own text from `event=`, for copying.
    full: String,
    colour: u8,
}

/// The controller's latest `request_completed` result.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastResult {
    outcome: String,
    detail: String,
    colour: u8,
}

#[derive(Debug, Default)]
struct Journal {
    events: Vec<Event>,
    last_result: Option<LastResult>,
}

fn read_journal(command: &Command, repo: &Path) -> Journal {
    let arguments: Vec<OsString> = [
        "-u",
        &command.unit,
        "-n",
        "300",
        "--no-pager",
        "-o",
        "short-iso",
    ]
    .iter()
    .map(process::os)
    .collect();
    process::bounded_text("journalctl", &arguments, repo, COMMAND_TIMEOUT)
        .map(|journal| Journal {
            events: parse_events(&journal, command.events),
            last_result: parse_last_result(&journal),
        })
        .unwrap_or_default()
}

fn parse_last_result(journal: &str) -> Option<LastResult> {
    let line = journal
        .lines()
        .rev()
        .find(|line| line.contains("event=request_completed"))?;
    let field = |name: &str| {
        line.split(' ')
            .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
    };
    let bytes = |name: &str| field(name)?.parse::<u64>().ok().map(mib);
    let outcome = field("outcome")?.to_owned();
    let colour = match outcome.as_str() {
        "Completed" => GREEN,
        "Partial" | "PoolEmpty" => YELLOW,
        _ => RED,
    };
    Some(LastResult {
        detail: format!(
            "{} {}M -> {}M at {}",
            field("operation")?,
            bytes("bytes")?,
            bytes("actual_bytes")?,
            line.get(11..19)?
        ),
        outcome,
        colour,
    })
}

/// The recent events with their full journal text, for copying.
fn full_events(events: &[Event]) -> String {
    let mut text = String::from("\nRecent controller events (full text, newest first):\n");
    for event in events {
        text.push_str(&format!("{}  {}\n", event.time, event.full));
    }
    text
}

/// Controller events worth showing, by the first matching marker.
const EVENT_COLOURS: [(&str, u8); 13] = [
    ("actuation_latched", RED),
    ("Rejected", RED),
    ("refuse", RED),
    ("Uncertain", RED),
    ("error", RED),
    // PAUSED (viomem): no longer emitted; kept for older journals.
    ("usable_region_widened", MAGENTA),
    ("outcome=Partial", YELLOW),
    ("settled_short", YELLOW),
    ("outcome=Completed", GREEN),
    ("action=grant", CYAN),
    ("resize_requested", CYAN),
    ("ownership_conflict", DIM),
    ("guest_", DIM),
];

/// Newest-first events with health samples dropped, byte counts in MiB and
/// consecutive repeats (ignoring embedded timestamps) collapsed with a count.
fn parse_events(journal: &str, count: usize) -> Vec<Event> {
    let mut events: Vec<(Event, String, usize)> = Vec::new();
    for line in journal.lines().rev() {
        let Some((_, text)) = line.split_once("event=") else {
            continue;
        };
        if text.starts_with("control_health") {
            continue;
        }
        let time = line.get(11..19).unwrap_or("--:--:--").to_owned();
        let full = text.to_owned();
        let text = shorten(text);
        let key = mask_long_numbers(&text);
        if let Some((_, last_key, repeats)) = events.last_mut() {
            if *last_key == key {
                *repeats += 1;
                continue;
            }
        }
        if events.len() == count {
            break;
        }
        let colour = EVENT_COLOURS
            .iter()
            .find(|(marker, _)| text.contains(marker))
            .map_or(LABEL, |(_, colour)| *colour);
        events.push((
            Event {
                time,
                text,
                full,
                colour,
            },
            key,
            1,
        ));
    }
    events
        .into_iter()
        .map(|(mut event, _, repeats)| {
            if repeats > 1 {
                event.text = format!("(x{repeats}) {}", event.text);
                event.full = format!("(x{repeats}) {}", event.full);
            }
            event
        })
        .collect()
}

/// Drops session IDs and shows `*bytes=N` values in MiB.
pub(crate) fn shorten(text: &str) -> String {
    text.split(' ')
        .filter(|word| !word.starts_with("session_id="))
        .map(|word| {
            word.split_once('=')
                .filter(|(key, _)| key.ends_with("bytes"))
                .and_then(|(key, value)| Some((key, value.parse::<u64>().ok()?)))
                .map_or_else(
                    || word.to_owned(),
                    |(key, value)| format!("{key}={}M", value / MIB),
                )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn mask_long_numbers(text: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut digits = String::new();
    for character in text.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        masked.push_str(if digits.len() >= 10 { "#" } else { &digits });
        digits.clear();
        masked.push(character);
    }
    masked.pop();
    masked
}

fn build(
    frame: &mut Frame,
    command: &Command,
    sample: &Sample,
    history: &VecDeque<VirtioMemState>,
    journal: &Journal,
) {
    let events = &journal.events;
    let g = frame.glyphs;
    frame.bar(
        &format!(
            "{} VIRTIO-MEM HOST MONITOR  {}  {}/{}",
            g.dot,
            host_name(),
            command.vm,
            command.alias
        ),
        &clock(),
    );
    frame.line();
    frame.add(
        format!(
            " refresh {:.1}s   guest every {:.1}s   samples {}   Ctrl+C stop",
            command.interval.as_secs_f64(),
            command.guest_interval.as_secs_f64(),
            sample.samples
        ),
        LABEL,
    );
    frame.pad_to(frame.width, BACKGROUND);

    let host = sample.host;
    let total = host.total.max(1);
    let floor = command.host_floor_bytes;
    frame.top("HOST", "MemAvailable vs controller floor");
    frame.meter(
        "Used",
        1.0 - host.available as f64 / total as f64,
        &format!(
            "{:.2} / {:.2} GiB",
            (total - host.available) as f64 / GIB as f64,
            total as f64 / GIB as f64
        ),
        None,
    );
    let floor_colour = if host.available < floor {
        RED
    } else if host.available < floor + floor / 4 {
        YELLOW
    } else {
        GREEN
    };
    frame.metric(
        "Available",
        &gib(host.available),
        &format!(
            "floor {:.1} GiB, {:+.2} GiB to spare",
            floor as f64 / GIB as f64,
            (host.available as f64 - floor as f64) / GIB as f64
        ),
        floor_colour,
    );
    frame.metric(
        "Swap used",
        &gib(host.swap_total.saturating_sub(host.swap_free)),
        &format!("of {:.1} GiB", host.swap_total as f64 / GIB as f64),
        VALUE,
    );
    frame.bottom();

    frame.top("VIRTIO-MEM DEVICE", "requested / current");
    match sample.device {
        None => frame.message("device state unavailable (VM stopped or virsh failed)", RED),
        Some(device) => {
            let (requested, current) = (device.requested_bytes, device.current_bytes);
            let (state, colour) = match requested.cmp(&current) {
                std::cmp::Ordering::Equal => ("converged".to_owned(), GREEN),
                std::cmp::Ordering::Greater => (
                    format!("growing  +{} MiB to go", mib(requested - current)),
                    CYAN,
                ),
                std::cmp::Ordering::Less => (
                    format!("shrinking  {} MiB to go", mib(current - requested)),
                    YELLOW,
                ),
            };
            frame.metric(
                "Requested",
                &gib(requested),
                &format!("block {} MiB", mib(device.block_size_bytes)),
                VALUE,
            );
            frame.metric("Current (plugged)", &gib(current), &state, colour);
            frame.meter(
                "Plugged",
                current as f64 / device.size_bytes.max(1) as f64,
                &format!(
                    "{:.2} / {:.0} GiB device",
                    current as f64 / GIB as f64,
                    device.size_bytes as f64 / GIB as f64
                ),
                Some(CYAN),
            );
        }
    }
    match &journal.last_result {
        Some(result) => frame.metric(
            "Last result",
            &result.outcome,
            &result.detail,
            result.colour,
        ),
        None => frame.metric(
            "Last result",
            "none",
            "no request_completed in the journal",
            DIM,
        ),
    }
    frame.bottom();

    let age = sample
        .guest_age
        .map_or("never".to_owned(), |age| format!("{}s ago", age.as_secs()));
    frame.top("GUEST (guest agent)", &format!("read {age}"));
    match &sample.guest.telemetry {
        None if !command.guest => frame.message("guest reads disabled (--no-guest)", DIM),
        None => frame.message("guest publication unavailable", RED),
        Some(telemetry) => guest_rows(frame, telemetry),
    }
    frame.bottom();

    history_chart(frame, history);

    frame.top(
        "CONTROLLER EVENTS",
        &format!("{}, newest first", command.unit),
    );
    for index in 0..command.events {
        frame.row();
        match events.get(index) {
            Some(event) => {
                frame.add(format!("{}  ", event.time), DIM);
                frame.add(event.text.as_str(), event.colour);
            }
            None if index == 0 => frame.add("no controller events in the journal", DIM),
            None => {}
        }
        frame.end_row();
    }
    frame.bottom();
    frame.bar(
        "Read-only: virsh --readonly, guest-agent telemetry reads and the journal",
        "",
    );
}

fn guest_rows(frame: &mut Frame, telemetry: &RawTelemetryEnvelope) {
    let memory = &telemetry.memory;
    let total = memory.physical_total_bytes.max(1);
    let available = memory.physical_available_bytes;
    frame.meter(
        "In use",
        1.0 - available as f64 / total as f64,
        &format!(
            "{:.2} / {:.2} GiB",
            total.saturating_sub(available) as f64 / GIB as f64,
            total as f64 / GIB as f64
        ),
        None,
    );
    frame.metric(
        "Available / commit",
        &gib(available),
        &format!(
            "commit headroom {}",
            gib(memory
                .commit_limit_bytes
                .saturating_sub(memory.commit_total_bytes))
        ),
        VALUE,
    );
    let (signal, colour) = match telemetry.native_pressure {
        Some(MemoryResourceNotificationState::High) => ("HIGH", GREEN),
        Some(MemoryResourceNotificationState::Neutral) => ("NEUTRAL", YELLOW),
        Some(MemoryResourceNotificationState::Low) => ("LOW", RED),
        None => ("UNKNOWN", YELLOW),
    };
    frame.metric(
        "Memory signal",
        signal,
        "High is required before release",
        colour,
    );
    match &telemetry.request {
        Some(request) => frame.metric(
            "Pending request",
            &format!("{:?} {}M", request.operation, mib(request.bytes)),
            &format!("sequence {}", request.identity.sequence),
            CYAN,
        ),
        None => frame.metric("Pending request", "none", "", DIM),
    }
}

fn history_chart(frame: &mut Frame, history: &VecDeque<VirtioMemState>) {
    const HEIGHT: u64 = 5;
    let g = frame.glyphs;
    let points: Vec<_> = history.iter().rev().take(frame.width - 12).rev().collect();
    let peak = points
        .iter()
        .map(|point| point.requested_bytes.max(point.current_bytes))
        .max()
        .unwrap_or(0);
    let scale = peak.div_ceil(GIB).max(1);
    frame.top(
        "DEVICE HISTORY",
        &format!(
            "last {} samples  {}=requested  {}=plugged  0..{scale} GiB",
            points.len(),
            g.mark,
            g.full
        ),
    );
    let level =
        |bytes: u64| ((bytes as f64 / GIB as f64) / scale as f64 * HEIGHT as f64).round() as u64;
    for y in (0..=HEIGHT).rev() {
        frame.row();
        frame.add(
            format!("{:5.1} {}", (scale * y) as f64 / HEIGHT as f64, g.axis),
            DIM,
        );
        for point in &points {
            let requested = level(point.requested_bytes);
            if y == requested && point.requested_bytes != point.current_bytes {
                frame.add(g.mark, YELLOW);
            } else if y <= level(point.current_bytes) {
                frame.add(g.full, CYAN);
            } else if y == requested {
                frame.add(g.mark, DIM);
            } else {
                frame.add(" ", LABEL);
            }
        }
        frame.end_row();
    }
    frame.row();
    frame.add(
        format!("      {}{}", g.corner, g.rule.repeat(points.len())),
        DIM,
    );
    frame.end_row();
    frame.bottom();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_vm_from_the_host_config_and_options() {
        let config = crate::repo_root()
            .expect("repository root")
            .join("fixtures/config/host_list.toml");
        let config = config.to_str().expect("UTF-8 path");
        let expected = HostConfig::load_vm(Path::new(config), None).expect("tracked config");
        let defaults = parse(&["--config", config].map(str::to_owned)).expect("defaults");
        assert_eq!(defaults.vm, expected.vm_name);
        assert_eq!(defaults.alias, expected.alias);
        assert_eq!(defaults.telemetry_path, expected.raw_telemetry_path);
        assert_eq!(defaults.host_floor_bytes, expected.host_min_headroom_bytes);
        assert_eq!(
            defaults.unit,
            format!("rampire@{}.service", expected.vm_name)
        );
        assert_eq!(defaults.interval, Duration::from_secs(1));
        let command = parse(
            &[
                "--config",
                config,
                "--instance",
                "other",
                "--interval-seconds",
                "0.5",
                "--events",
                "3",
                "--no-guest",
            ]
            .map(str::to_owned),
        )
        .expect("options");
        assert_eq!(command.unit, "rampire@other.service");
        assert_eq!(command.interval, Duration::from_millis(500));
        assert_eq!(command.events, 3);
        assert!(!command.guest);
        for bad in [
            vec![],
            vec!["--config", config, "--vm", "missing"],
            vec!["--config", config, "--interval-seconds", "0"],
            vec!["--config", config, "--events", "0"],
            vec!["--config", config, "--bogus", "1"],
            vec!["--config"],
        ] {
            assert!(parse(&bad.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn reads_meminfo_fields_in_bytes() {
        let host = parse_meminfo(
            "MemTotal:       32222876 kB\nMemFree: 1 kB\nMemAvailable:    8068956 kB\nSwapTotal: 4 kB\nSwapFree: 1 kB\n",
        );
        assert_eq!(host.total, 32_222_876 * 1024);
        assert_eq!(host.available, 8_068_956 * 1024);
        assert_eq!((host.swap_total, host.swap_free), (4096, 1024));
    }

    #[test]
    fn events_are_newest_first_shortened_coloured_and_collapsed() {
        let journal = "\
2026-10-27T09:50:31+00:00 kvm-host rampire[1]: rampire: vm=w event=guest_input_invalid reason=raw telemetry observation 1793093751508 is stale at 1793094631000
2026-10-27T09:50:37+00:00 kvm-host rampire[1]: rampire: vm=w event=guest_input_invalid reason=raw telemetry observation 1793093751508 is stale at 1793094637000
2026-10-27T09:57:19+00:00 kvm-host rampire[1]: rampire: vm=w event=controller_decision action=grant session_id=s-1 operation=Remove bytes=3221225472
2026-10-27T09:57:20+00:00 kvm-host rampire[1]: rampire: vm=w event=control_health direction=Shrink
2026-10-27T09:57:40+00:00 kvm-host rampire[1]: rampire: vm=w event=actuation_latched reason=x
";
        let events = parse_events(journal, 6);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[1].full,
            "controller_decision action=grant session_id=s-1 operation=Remove bytes=3221225472"
        );
        assert_eq!(events[0].time, "09:57:40");
        assert_eq!(events[0].colour, RED);
        assert_eq!(
            events[1].text,
            "controller_decision action=grant operation=Remove bytes=3072M"
        );
        assert_eq!(events[1].colour, CYAN);
        assert!(events[2].text.starts_with("(x2) guest_input_invalid"));
        assert!(events[2].full.ends_with("is stale at 1793094637000"));
        assert_eq!(parse_events(journal, 1).len(), 1);
        assert!(full_events(&events).contains("09:57:40  actuation_latched reason=x\n"));
    }

    #[test]
    fn last_result_comes_from_the_newest_completion() {
        let journal = "\
2026-10-27T09:57:46+00:00 h rampire[1]: rampire: vm=w event=request_completed operation_id=1 sequence=0 operation=Remove bytes=3221225472 outcome=Partial actual_bytes=1895825408
2026-10-27T10:03:21+00:00 h rampire[1]: rampire: vm=w event=request_completed operation_id=2 sequence=1 operation=Add bytes=1073741824 outcome=Completed actual_bytes=1073741824
2026-10-27T10:03:33+00:00 h rampire[1]: rampire: vm=w event=guest_status_unpublished reason=x
";
        assert_eq!(
            parse_last_result(journal),
            Some(LastResult {
                outcome: "Completed".to_owned(),
                detail: "Add 1024M -> 1024M at 10:03:21".to_owned(),
                colour: GREEN,
            })
        );
        assert_eq!(parse_last_result("no completions"), None);
    }

    #[test]
    fn plain_frames_keep_every_line_within_the_width() {
        let config = crate::repo_root()
            .expect("repository root")
            .join("fixtures/config/host_list.toml");
        let command =
            parse(&["--config".to_owned(), config.to_string_lossy().into_owned()]).unwrap();
        let device = VirtioMemState {
            size_bytes: 20 * GIB,
            block_size_bytes: 2 * MIB,
            requested_bytes: GIB,
            current_bytes: 3 * GIB,
        };
        let history: VecDeque<_> = std::iter::repeat_n(device, 400).collect();
        let journal = Journal {
            events: vec![Event {
                time: "09:00:00".to_owned(),
                text: "x".repeat(500),
                full: "x".repeat(500),
                colour: RED,
            }],
            last_result: None,
        };
        let sample = Sample {
            host: HostMemory {
                total: 32 * GIB,
                available: 3 * GIB,
                swap_total: GIB,
                swap_free: GIB,
            },
            device: Some(device),
            guest: &GuestView::default(),
            guest_age: None,
            samples: 1,
        };
        for width in [80, 120] {
            let mut frame = Frame::new(width, false);
            build(&mut frame, &command, &sample, &history, &journal);
            let text = frame.render();
            assert!(text.contains("shrinking  2048 MiB to go"));
            assert!(text.contains("guest publication unavailable"));
            for line in text.lines() {
                assert!(line.chars().count() <= width, "{width}: {line}");
            }
        }
    }
}
