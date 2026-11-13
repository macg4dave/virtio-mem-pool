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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use virtio_mem_core::{
    parse_virtio_mem_xml_for_alias, MemoryResourceNotificationState, RawTelemetryEnvelope,
    VirtioMemState,
};
use virtio_mem_host::config::HostConfig;
use virtio_mem_host::host_memory::meminfo_bytes;
use virtio_mem_host::qga::{GuestFileReader, VirshGuestFiles};
use virtio_mem_host::virsh::{Virsh, VirshCommand};

use crate::process;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
pub(crate) const MAX_TELEMETRY_BYTES: usize = 64 * 1024;
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub const HELP: &str = "Watch options (read-only live dashboard):
  --config PATH                 Required host TOML; the VM, device alias,
                                telemetry path and host floor come from it.
  --vm NAME                     The [[vm]] to show; required with several.
  --instance NAME               Controller unit virtio-mem-host@NAME.service;
                                default the VM name.
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
        "virtio-mem-host@{}.service",
        instance.as_deref().unwrap_or(&config.vm_name)
    );
    command.vm = config.vm_name;
    command.alias = config.alias;
    command.telemetry_path = config.raw_telemetry_path;
    command.host_floor_bytes = config.host_min_headroom_bytes;
    Ok(command)
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    let interactive = !command.once && std::io::stdout().is_terminal();
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("install signal handler: {error}"))?;
    }
    let virsh = Virsh::persistent_with_connection("virsh", COMMAND_TIMEOUT, &command.connect);
    let guest_files = VirshGuestFiles::new(virsh.clone(), &command.vm);
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

// 256-colour palette on a navy background.
const BACKGROUND: u8 = 17;
const BAR_BACKGROUND: u8 = 25;
const FRAME: u8 = 45;
const TITLE: u8 = 231;
const LABEL: u8 = 250;
const VALUE: u8 = 231;
const DIM: u8 = 67;
const GREEN: u8 = 46;
const YELLOW: u8 = 226;
const RED: u8 = 196;
const CYAN: u8 = 51;
const MAGENTA: u8 = 213;
const TRACK: u8 = 24;

struct Glyphs {
    top_left: &'static str,
    top_right: &'static str,
    bottom_left: &'static str,
    bottom_right: &'static str,
    horizontal: &'static str,
    vertical: &'static str,
    rule: &'static str,
    full: &'static str,
    empty: &'static str,
    mark: &'static str,
    axis: &'static str,
    corner: &'static str,
    dot: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    top_left: "╔",
    top_right: "╗",
    bottom_left: "╚",
    bottom_right: "╝",
    horizontal: "═",
    vertical: "║",
    rule: "─",
    full: "█",
    empty: "░",
    mark: "━",
    axis: "┤",
    corner: "└",
    dot: "●",
};

const ASCII: Glyphs = Glyphs {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    horizontal: "=",
    vertical: "|",
    rule: "-",
    full: "#",
    empty: ".",
    mark: "T",
    axis: "|",
    corner: "+",
    dot: "*",
};

/// Lines of coloured segments, rendered as ANSI or plain ASCII text.
struct Frame {
    width: usize,
    ansi: bool,
    glyphs: &'static Glyphs,
    lines: Vec<Vec<(String, u8, u8)>>,
}

impl Frame {
    fn new(width: usize, ansi: bool) -> Self {
        Self {
            width,
            ansi,
            glyphs: if ansi { &UNICODE } else { &ASCII },
            lines: Vec::new(),
        }
    }

    fn line(&mut self) {
        self.lines.push(Vec::new());
    }

    fn add(&mut self, text: impl Into<String>, colour: u8) {
        self.add_on(text, colour, BACKGROUND);
    }

    fn add_on(&mut self, text: impl Into<String>, colour: u8, background: u8) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        let line = self.lines.last_mut().expect("a line was started");
        match line.last_mut() {
            Some((last, fg, bg)) if *fg == colour && *bg == background => last.push_str(&text),
            _ => line.push((text, colour, background)),
        }
    }

    fn length(&self) -> usize {
        self.lines.last().map_or(0, |line| {
            line.iter().map(|(text, _, _)| text.chars().count()).sum()
        })
    }

    fn pad_to(&mut self, column: usize, background: u8) {
        let missing = column.saturating_sub(self.length());
        self.add_on(" ".repeat(missing), LABEL, background);
    }

    fn bar(&mut self, left: &str, right: &str) {
        self.line();
        self.add_on(format!(" {left}"), TITLE, BAR_BACKGROUND);
        let used = left.chars().count() + right.chars().count() + 2;
        self.add_on(
            " ".repeat(self.width.saturating_sub(used)),
            TITLE,
            BAR_BACKGROUND,
        );
        self.add_on(format!("{right} "), TITLE, BAR_BACKGROUND);
    }

    fn top(&mut self, title: &str, detail: &str) {
        let g = self.glyphs;
        self.line();
        self.add(format!("{}{}", g.top_left, g.horizontal), FRAME);
        self.add(format!(" {title} "), TITLE);
        if !detail.is_empty() {
            self.add(format!("{detail} "), DIM);
        }
        let fill = self.width.saturating_sub(1 + self.length());
        self.add(g.horizontal.repeat(fill), FRAME);
        self.add(g.top_right, FRAME);
    }

    fn row(&mut self) {
        self.line();
        self.add(format!("{} ", self.glyphs.vertical), FRAME);
    }

    /// Clips overlong content inside the frame, pads and closes the row.
    fn end_row(&mut self) {
        let limit = self.width - 2;
        let line = self.lines.last_mut().expect("a row was started");
        let mut used = 0;
        line.retain_mut(|(text, _, _)| {
            let room = limit.saturating_sub(used);
            if text.chars().count() > room {
                *text = text.chars().take(room).collect();
            }
            used += text.chars().count();
            !text.is_empty()
        });
        self.pad_to(limit, BACKGROUND);
        self.add(format!(" {}", self.glyphs.vertical), FRAME);
    }

    fn bottom(&mut self) {
        let g = self.glyphs;
        self.line();
        self.add(
            format!(
                "{}{}{}",
                g.bottom_left,
                g.horizontal.repeat(self.width - 2),
                g.bottom_right
            ),
            FRAME,
        );
    }

    fn meter(&mut self, label: &str, fraction: f64, detail: &str, colour: Option<u8>) {
        const WIDTH: usize = 36;
        let fraction = fraction.clamp(0.0, 1.0);
        let filled = (fraction * WIDTH as f64).round() as usize;
        let colour = colour.unwrap_or_else(|| load_colour(fraction));
        self.row();
        self.add(format!("{label:<12}"), LABEL);
        self.add(self.glyphs.full.repeat(filled), colour);
        self.add(self.glyphs.empty.repeat(WIDTH - filled), TRACK);
        self.add(format!(" {:5.1}%", fraction * 100.0), colour);
        self.add(format!("  {detail}"), LABEL);
        self.end_row();
    }

    fn metric(&mut self, label: &str, now: &str, detail: &str, colour: u8) {
        self.row();
        self.add(format!("{label:<22}"), LABEL);
        self.add(format!("{now:>14}"), colour);
        self.add("    ", LABEL);
        self.add(detail, YELLOW);
        self.end_row();
    }

    fn message(&mut self, text: &str, colour: u8) {
        self.row();
        self.add(text, colour);
        self.end_row();
    }

    fn render(&self) -> String {
        if !self.ansi {
            let mut plain = self
                .lines
                .iter()
                .map(|line| {
                    let text: String = line.iter().map(|(text, _, _)| text.as_str()).collect();
                    text.trim_end().to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            plain.push('\n');
            return plain;
        }
        let mut out = String::from("\x1b[H");
        for line in &self.lines {
            for (text, foreground, background) in line {
                out.push_str(&format!("\x1b[38;5;{foreground};48;5;{background}m{text}"));
            }
            out.push_str(&format!("\x1b[48;5;{BACKGROUND}m\x1b[K\n"));
        }
        out.push_str("\x1b[J");
        out
    }
}

fn load_colour(fraction: f64) -> u8 {
    if fraction >= 0.9 {
        RED
    } else if fraction >= 0.7 {
        YELLOW
    } else {
        GREEN
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.3} GiB", bytes as f64 / GIB as f64)
}

fn mib(bytes: u64) -> u64 {
    bytes / MIB
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

fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|_| "host".to_owned())
}

/// Local wall-clock time as HH:MM:SS, using the system's UTC offset.
fn clock() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
        + local_offset_seconds();
    let day = seconds.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
}

#[cfg(unix)]
fn local_offset_seconds() -> i64 {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const u8,
    }
    unsafe extern "C" {
        fn time(timer: *mut i64) -> i64;
        fn localtime_r(timer: *const i64, result: *mut Tm) -> *mut Tm;
    }
    // SAFETY: `time` accepts a null pointer; `localtime_r` writes only the
    // caller-owned, correctly laid out `tm` for this glibc/x86-64 target.
    unsafe {
        let now = time(std::ptr::null_mut());
        let mut tm = std::mem::zeroed::<Tm>();
        if localtime_r(&now, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff
        }
    }
}

#[cfg(not(unix))]
fn local_offset_seconds() -> i64 {
    0
}

#[cfg(unix)]
fn terminal_columns() -> Option<usize> {
    #[repr(C)]
    struct WindowSize {
        rows: u16,
        columns: u16,
        x_pixels: u16,
        y_pixels: u16,
    }
    unsafe extern "C" {
        fn ioctl(descriptor: i32, request: u64, ...) -> i32;
    }
    const TIOCGWINSZ: u64 = 0x5413;
    let mut size = WindowSize {
        rows: 0,
        columns: 0,
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: TIOCGWINSZ writes one `winsize` into the caller-owned struct.
    let result = unsafe { ioctl(1, TIOCGWINSZ, &mut size as *mut WindowSize) };
    (result == 0 && size.columns > 0).then_some(size.columns as usize)
}

#[cfg(not(unix))]
fn terminal_columns() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_vm_from_the_host_config_and_options() {
        let config = crate::repo_root()
            .expect("repository root")
            .join("deploy/config_files/host_list.toml");
        let config = config.to_str().expect("UTF-8 path");
        let expected = HostConfig::load_vm(Path::new(config), None).expect("tracked config");
        let defaults = parse(&["--config", config].map(str::to_owned)).expect("defaults");
        assert_eq!(defaults.vm, expected.vm_name);
        assert_eq!(defaults.alias, expected.alias);
        assert_eq!(defaults.telemetry_path, expected.raw_telemetry_path);
        assert_eq!(defaults.host_floor_bytes, expected.host_min_headroom_bytes);
        assert_eq!(
            defaults.unit,
            format!("virtio-mem-host@{}.service", expected.vm_name)
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
        assert_eq!(command.unit, "virtio-mem-host@other.service");
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
2026-10-27T09:50:31+00:00 pc101 virtio-mem-host[1]: virtio-mem-host: vm=w event=guest_input_invalid reason=raw telemetry observation 1793093751508 is stale at 1793094631000
2026-10-27T09:50:37+00:00 pc101 virtio-mem-host[1]: virtio-mem-host: vm=w event=guest_input_invalid reason=raw telemetry observation 1793093751508 is stale at 1793094637000
2026-10-27T09:57:19+00:00 pc101 virtio-mem-host[1]: virtio-mem-host: vm=w event=controller_decision action=grant session_id=s-1 operation=Remove bytes=3221225472
2026-10-27T09:57:20+00:00 pc101 virtio-mem-host[1]: virtio-mem-host: vm=w event=control_health direction=Shrink
2026-10-27T09:57:40+00:00 pc101 virtio-mem-host[1]: virtio-mem-host: vm=w event=actuation_latched reason=x
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
2026-10-27T09:57:46+00:00 h virtio-mem-host[1]: virtio-mem-host: vm=w event=request_completed operation_id=1 sequence=0 operation=Remove bytes=3221225472 outcome=Partial actual_bytes=1895825408
2026-10-27T10:03:21+00:00 h virtio-mem-host[1]: virtio-mem-host: vm=w event=request_completed operation_id=2 sequence=1 operation=Add bytes=1073741824 outcome=Completed actual_bytes=1073741824
2026-10-27T10:03:33+00:00 h virtio-mem-host[1]: virtio-mem-host: vm=w event=guest_status_unpublished reason=x
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
            .join("deploy/config_files/host_list.toml");
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
