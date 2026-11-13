//! The live terminal UI of `cargo xtask metrics`. It only reads the shared
//! [`Live`] state and changes its [`Controls`](super::Controls); the sampler
//! thread does all I/O except an on-demand summary save.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Chart, Dataset, GraphType, LineGauge, List, ListItem, Paragraph,
};
use ratatui::{DefaultTerminal, Frame};
use virtio_mem_core::RawTelemetryEnvelope;

use super::{lock, mib, signal_name, unix_millis, Command, JournalEvent, Live, Point, Stat, MIB};
use crate::watch;

/// Periods the sample and guest-read keys step through.
const PERIODS: [f64; 10] = [0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0];
/// Chart windows the `w` key steps through; `None` shows the whole run.
const WINDOWS: [Option<f64>; 4] = [Some(300.0), Some(900.0), Some(3600.0), None];

const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const INFO: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const HOST: Color = Color::Green;
const GUEST: Color = Color::Cyan;
const CURRENT: Color = Color::Magenta;
const REQUESTED: Color = Color::Yellow;

#[derive(Default)]
struct Ui {
    /// Marker text being typed, while `m` input is open.
    input: Option<String>,
    window: usize,
    flash: Option<(Instant, String)>,
}

pub(super) fn run(command: &Command, live: &Mutex<Live>, stop: &AtomicBool) -> Result<(), String> {
    let mut terminal =
        ratatui::try_init().map_err(|error| format!("start terminal UI: {error}"))?;
    let result = draw_loop(&mut terminal, command, live, stop);
    ratatui::restore();
    result
}

fn draw_loop(
    terminal: &mut DefaultTerminal,
    command: &Command,
    live: &Mutex<Live>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut ui = Ui::default();
    loop {
        if stop.load(Ordering::Acquire) || lock(live).finished {
            return Ok(());
        }
        terminal
            .draw(|frame| draw(frame, command, &lock(live), &ui))
            .map_err(|error| format!("draw terminal UI: {error}"))?;
        let ready = event::poll(Duration::from_millis(250))
            .map_err(|error| format!("read terminal input: {error}"))?;
        if !ready {
            continue;
        }
        let input = event::read().map_err(|error| format!("read terminal input: {error}"))?;
        if let Event::Key(key) = input {
            if key.kind == KeyEventKind::Press && !ui.handle(key, live) {
                return Ok(());
            }
        }
    }
}

fn step(current: Duration, up: bool, minimum: f64) -> Duration {
    let seconds = current.as_secs_f64();
    let next = if up {
        PERIODS
            .iter()
            .copied()
            .find(|period| *period > seconds + 1e-9)
    } else {
        PERIODS
            .iter()
            .rev()
            .copied()
            .find(|period| *period < seconds - 1e-9 && *period >= minimum)
    };
    next.map_or(current, Duration::from_secs_f64)
}

fn seconds_label(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{seconds:.0}s")
    }
}

impl Ui {
    /// Applies one key press; false quits.
    fn handle(&mut self, key: KeyEvent, live: &Mutex<Live>) -> bool {
        if let Some(text) = &mut self.input {
            match key.code {
                KeyCode::Enter => {
                    let text = text.trim().to_owned();
                    if !text.is_empty() {
                        lock(live).mark(text);
                        self.say("marker added");
                    }
                    self.input = None;
                }
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(character) if text.len() < 200 => text.push(character),
                _ => {}
            }
            return true;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut live = lock(live);
        match key.code {
            KeyCode::Char('c') if control => return false,
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char(direction @ ('+' | '=' | '-' | '_')) => {
                let interval = step(live.controls.interval, matches!(direction, '+' | '='), 0.5);
                if interval != live.controls.interval {
                    live.controls.interval = interval;
                    live.mark(format!("sample period {}", seconds_label(interval)));
                }
                self.say(&format!("sample every {}", seconds_label(interval)));
            }
            KeyCode::Char(direction @ ('[' | ']')) => {
                let interval = step(live.controls.guest_interval, direction == ']', 1.0);
                if interval != live.controls.guest_interval {
                    live.controls.guest_interval = interval;
                    live.mark(format!("guest read period {}", seconds_label(interval)));
                }
                self.say(&format!("read guest every {}", seconds_label(interval)));
            }
            KeyCode::Char(' ' | 'p') => {
                let paused = !live.controls.paused;
                live.mark(
                    if paused {
                        "logging paused"
                    } else {
                        "logging resumed"
                    }
                    .to_owned(),
                );
                live.controls.paused = paused;
                self.say(if paused {
                    "logging paused"
                } else {
                    "logging resumed"
                });
            }
            KeyCode::Char('n') => {
                live.controls.sample_now = true;
                self.say("sampling now");
            }
            KeyCode::Char('s') => match live.write_summary() {
                Ok(()) => self.say("summary saved"),
                Err(error) => self.say(&error),
            },
            KeyCode::Char('w') => self.window = (self.window + 1) % WINDOWS.len(),
            KeyCode::Char('m') => self.input = Some(String::new()),
            _ => {}
        }
        true
    }

    fn say(&mut self, text: &str) {
        self.flash = Some((Instant::now(), text.to_owned()));
    }
}

// ---- Drawing ------------------------------------------------------------------

fn gib(bytes: u64) -> String {
    format!("{:.2}G", bytes as f64 / (1u64 << 30) as f64)
}

fn signed_gib(left: u64, right: u64) -> String {
    let difference = (left as i128 - right as i128) as f64 / (1u64 << 30) as f64;
    format!("{difference:+.2}G")
}

fn row<'a>(label: &'a str, value: impl Into<Span<'a>>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), DIM),
        value.into(),
    ])
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
        .border_style(Style::new().fg(DIM))
}

fn gauge<'a>(ratio: f64, label: String, colour: Color) -> LineGauge<'a> {
    LineGauge::default()
        .ratio(ratio.clamp(0.0, 1.0))
        .label(label)
        .filled_style(Style::new().fg(colour))
        .unfilled_style(Style::new().fg(DIM))
}

fn load_colour(ratio: f64) -> Color {
    if ratio >= 0.9 {
        BAD
    } else if ratio >= 0.75 {
        WARN
    } else {
        GOOD
    }
}

fn draw(frame: &mut Frame, command: &Command, live: &Live, ui: &Ui) {
    let area = frame.area();
    if area.width < 90 || area.height < 28 {
        frame.render_widget(
            Paragraph::new("metrics UI needs at least 90x28; still recording. q quits."),
            area,
        );
        return;
    }
    let [header, panels, chart, lower, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(8),
        Constraint::Min(8),
        Constraint::Length(14),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(frame, header, command, live);
    let [host, device, guest] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(panels);
    draw_host(frame, host, live);
    draw_device(frame, device, live);
    draw_guest(frame, guest, live);
    draw_chart(frame, chart, live, WINDOWS[ui.window]);
    let [requests, events] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(lower);
    draw_requests(frame, requests, live);
    draw_events(frame, events, live);
    draw_footer(frame, footer, live, ui);
}

fn draw_header(frame: &mut Frame, area: Rect, command: &Command, live: &Live) {
    let elapsed = unix_millis().saturating_sub(live.started_millis) / 1000;
    let state = if live.controls.paused {
        Span::styled(" PAUSED ", Style::new().fg(Color::Black).bg(WARN).bold())
    } else {
        Span::styled(" ● REC ", Style::new().fg(Color::White).bg(BAD).bold())
    };
    let line = Line::from(vec![
        Span::styled(" virtio-mem metrics ", Style::new().bold()),
        Span::styled(command.vm.clone(), Style::new().fg(INFO).bold()),
        Span::raw("  "),
        state,
        Span::raw(format!(
            "  {:02}:{:02}:{:02}  {} rows  ",
            elapsed / 3600,
            elapsed % 3600 / 60,
            elapsed % 60,
            live.rows_written
        )),
        Span::styled(live.directory.display().to_string(), DIM),
    ]);
    let incidents = live.summary.incident_total();
    let line = if incidents == 0 {
        line
    } else {
        let badge = format!(
            " {incidents} INCIDENT{} ",
            if incidents == 1 { "" } else { "S" }
        );
        let mut spans = vec![Span::styled(
            badge,
            Style::new().fg(Color::White).bg(BAD).bold(),
        )];
        spans.extend(line.spans);
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn split_gauge(area: Rect) -> (Rect, Rect) {
    let [gauge, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    (gauge, rest)
}

fn draw_host(frame: &mut Frame, area: Rect, live: &Live) {
    let block = panel("Host");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(sample) = &live.latest else {
        frame.render_widget(Paragraph::new("waiting for the first sample"), inner);
        return;
    };
    let host = &sample.host;
    let used = host.total.saturating_sub(host.available) as f64 / host.total.max(1) as f64;
    let (gauge_area, rest) = split_gauge(inner);
    frame.render_widget(
        gauge(
            used,
            format!("RAM {:>3.0}%", used * 100.0),
            load_colour(used),
        ),
        gauge_area,
    );
    let over_floor = host.available as i128 - live.host_floor_bytes as i128;
    let floor_colour = if over_floor < 0 {
        BAD
    } else if over_floor < (1 << 30) {
        WARN
    } else {
        GOOD
    };
    let lines = vec![
        row(
            "available",
            format!("{} of {}", gib(host.available), gib(host.total)),
        ),
        row(
            "over floor",
            Span::styled(
                signed_gib(host.available, live.host_floor_bytes),
                floor_colour,
            ),
        ),
        row(
            "swap used",
            gib(host.swap_total.saturating_sub(host.swap_free)),
        ),
        row(
            "QEMU RSS",
            sample
                .qemu_rss_bytes
                .map_or_else(|| "unknown".to_owned(), gib),
        ),
        row("controller", controller_span(sample)),
    ];
    frame.render_widget(Paragraph::new(lines), rest);
}

fn controller_span(sample: &super::Sample) -> Span<'static> {
    let alarm = Style::new().fg(BAD).bold();
    match &sample.controller {
        Some(status) if status.active() => {
            let mut text = status
                .main_pid
                .map_or_else(|| "active".to_owned(), |pid| format!("pid {pid}"));
            if let Some(bytes) = status.memory_bytes {
                text.push_str(&format!(", {} MiB", mib(bytes)));
            }
            if let Some(percent) = sample.controller_cpu_pct {
                text.push_str(&format!(", {percent:.1}% cpu"));
            }
            Span::styled(text, GOOD)
        }
        Some(status) if status.failed() => {
            Span::styled(format!("FAILED: {}", status.exit()), alarm)
        }
        Some(status) => Span::styled(format!("INACTIVE ({})", status.active_state), alarm),
        None => Span::styled("INACTIVE (systemctl unreadable)", alarm),
    }
}

fn draw_device(frame: &mut Frame, area: Rect, live: &Live) {
    let block = panel("virtio-mem device");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(device) = live.latest.as_ref().and_then(|sample| sample.device) else {
        frame.render_widget(
            Paragraph::new(Span::styled("device unreadable", BAD)),
            inner,
        );
        return;
    };
    let plugged = device.current_bytes as f64 / device.size_bytes.max(1) as f64;
    let (gauge_area, rest) = split_gauge(inner);
    frame.render_widget(
        gauge(
            plugged,
            format!("plugged {:>3.0}%", plugged * 100.0),
            CURRENT,
        ),
        gauge_area,
    );
    let state = match device.current_bytes.cmp(&device.requested_bytes) {
        std::cmp::Ordering::Equal => Span::styled("converged", GOOD),
        std::cmp::Ordering::Greater => Span::styled(
            format!(
                "shrinking, {}M to go",
                mib(device.current_bytes - device.requested_bytes)
            ),
            WARN,
        ),
        std::cmp::Ordering::Less => Span::styled(
            format!(
                "growing, {}M to go",
                mib(device.requested_bytes - device.current_bytes)
            ),
            WARN,
        ),
    };
    let diverged_for = live
        .summary
        .divergence_since_millis
        .map(|since| format!("{}s", unix_millis().saturating_sub(since) / 1000))
        .unwrap_or_else(|| "-".to_owned());
    let lines = vec![
        row(
            "current",
            Span::styled(format!("{} MiB", mib(device.current_bytes)), CURRENT),
        ),
        row(
            "requested",
            Span::styled(format!("{} MiB", mib(device.requested_bytes)), REQUESTED),
        ),
        row("state", state),
        row("diverged for", diverged_for),
        row(
            "size / block",
            format!(
                "{} / {} MiB",
                gib(device.size_bytes),
                device.block_size_bytes / MIB
            ),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), rest);
}

fn draw_guest(frame: &mut Frame, area: Rect, live: &Live) {
    let block = panel("Guest");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some((read_millis, telemetry)) = &live.guest else {
        let text = live
            .guest_error
            .as_deref()
            .map_or("waiting for a guest read", |error| error);
        frame.render_widget(Paragraph::new(Span::styled(text.to_owned(), WARN)), inner);
        return;
    };
    let memory = &telemetry.memory;
    let load = f64::from(memory.memory_load_percent) / 100.0;
    let (gauge_area, rest) = split_gauge(inner);
    frame.render_widget(
        gauge(
            load,
            format!("load {:>3}%", memory.memory_load_percent),
            load_colour(load),
        ),
        gauge_area,
    );
    let read = match (
        &live.guest_error,
        live.latest.as_ref().and_then(|s| s.guest.as_ref()),
    ) {
        (Some(_), _) => Span::styled(
            format!(
                "FAILING, last ok {}s ago",
                unix_millis().saturating_sub(*read_millis) / 1000
            ),
            BAD,
        ),
        (None, Some(read)) => Span::raw(format!(
            "{}ms, age {:.1}s",
            read.elapsed.as_millis(),
            read_millis.saturating_sub(telemetry.observed_unix_millis) as f64 / 1000.0
        )),
        (None, None) => Span::raw(format!(
            "published {:.1}s before read",
            read_millis.saturating_sub(telemetry.observed_unix_millis) as f64 / 1000.0
        )),
    };
    let lines = vec![
        row(
            "available",
            Span::styled(
                format!(
                    "{} of {}",
                    gib(memory.physical_available_bytes),
                    gib(memory.physical_total_bytes)
                ),
                GUEST,
            ),
        ),
        row(
            "commit room",
            signed_gib(memory.commit_limit_bytes, memory.commit_total_bytes),
        ),
        row("signal", signal_span(telemetry)),
        row("request", request_span(telemetry)),
        row("agent read", read),
    ];
    frame.render_widget(Paragraph::new(lines), rest);
}

fn signal_span(telemetry: &RawTelemetryEnvelope) -> Span<'static> {
    let name = signal_name(telemetry);
    let colour = match name {
        "high" => GOOD,
        "low" => BAD,
        _ => WARN,
    };
    Span::styled(name.to_uppercase(), Style::new().fg(colour).bold())
}

fn request_span(telemetry: &RawTelemetryEnvelope) -> Span<'static> {
    match &telemetry.request {
        Some(request) => Span::styled(
            format!(
                "{:?} {} MiB #{}",
                request.operation,
                mib(request.bytes),
                request.identity.sequence
            ),
            INFO,
        ),
        None => Span::styled("none", DIM),
    }
}

fn draw_chart(frame: &mut Frame, area: Rect, live: &Live, window: Option<f64>) {
    let newest = live
        .history
        .back()
        .map_or(0.0, |point| point.elapsed_seconds);
    let oldest = window.map_or_else(
        || {
            live.history
                .front()
                .map_or(0.0, |point| point.elapsed_seconds)
        },
        |window| (newest - window).max(0.0),
    );
    let visible: Vec<&Point> = live
        .history
        .iter()
        .filter(|point| point.elapsed_seconds >= oldest)
        .collect();
    let series = |value: &dyn Fn(&Point) -> Option<u64>| -> Vec<(f64, f64)> {
        visible
            .iter()
            .filter_map(|point| Some((point.elapsed_seconds, value(point)? as f64 / 1024.0)))
            .collect()
    };
    let host = series(&|point| Some(point.host_available_mib));
    let guest = series(&|point| point.guest_available_mib);
    let current = series(&|point| point.device.map(|(_, current)| current));
    let requested = series(&|point| point.device.map(|(requested, _)| requested));
    let top = [&host, &guest, &current, &requested]
        .iter()
        .flat_map(|series| series.iter().map(|(_, value)| *value))
        .fold(1.0_f64, f64::max)
        * 1.1;
    let datasets = [
        ("host available", HOST, &host),
        ("guest available", GUEST, &guest),
        ("device current", CURRENT, &current),
        ("device requested", REQUESTED, &requested),
    ]
    .into_iter()
    .map(|(name, colour, data)| {
        Dataset::default()
            .name(name)
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(colour))
            .data(data)
    })
    .collect::<Vec<_>>();
    let span = (newest - oldest).max(1.0);
    let time = |seconds: f64| {
        let seconds = seconds.max(0.0) as u64;
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    };
    let title = match window {
        Some(window) => format!(" Memory GiB, last {} min  ", window as u64 / 60),
        None => " Memory GiB, whole run  ".to_owned(),
    };
    // The legend is in the title: the built-in one hides on small charts.
    let mut legend = vec![Span::styled(title, Style::new().bold())];
    for (name, colour) in [
        ("host available", HOST),
        ("guest available", GUEST),
        ("device current", CURRENT),
        ("device requested", REQUESTED),
    ] {
        legend.push(Span::styled(format!("━ {name}  "), colour));
    }
    let chart = Chart::new(datasets)
        .block(
            Block::bordered()
                .title(Line::from(legend))
                .border_style(Style::new().fg(DIM)),
        )
        .x_axis(
            Axis::default()
                .style(Style::new().fg(DIM))
                .bounds([oldest, oldest + span])
                .labels([time(oldest), time(oldest + span / 2.0), time(oldest + span)]),
        )
        .y_axis(
            Axis::default()
                .style(Style::new().fg(DIM))
                .bounds([0.0, top])
                .labels([
                    "0".to_owned(),
                    format!("{:.1}", top / 2.0),
                    format!("{top:.1}"),
                ]),
        )
        .legend_position(None);
    frame.render_widget(chart, area);
}

fn stat_text(stat: &Stat, unit: &str) -> String {
    if stat.count == 0 {
        return "-".to_owned();
    }
    format!(
        "n {}  mean {:.1}{unit}  max {:.1}{unit}",
        stat.count,
        stat.sum / stat.count as f64,
        stat.max
    )
}

fn minimum(stat: &Stat) -> String {
    if stat.count == 0 {
        "-".to_owned()
    } else {
        format!("{:.2}G", stat.min / 1024.0)
    }
}

fn draw_requests(frame: &mut Frame, area: Rect, live: &Live) {
    let summary = &live.summary;
    let count = |name: &str| summary.events.get(name).copied().unwrap_or(0);
    let incidents = |component: &str| summary.incident_counts.get(component).copied().unwrap_or(0);
    let mut lines: Vec<Line> = summary
        .requests
        .iter()
        .map(|(key, totals)| {
            let colour = if key.ends_with("Completed") {
                GOOD
            } else {
                WARN
            };
            Line::from(vec![
                Span::styled(format!("{key:<18}"), colour),
                Span::raw(format!(
                    "{:>3}x  asked {:>6}M  moved {:>6}M",
                    totals.count,
                    mib(totals.requested_bytes),
                    mib(totals.actual_bytes)
                )),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::styled("no terminal request results yet", DIM));
    }
    lines.extend([
        row(
            "grow→done",
            summary
                .grant_latency_by_operation
                .get("Add")
                .map_or_else(|| "-".to_owned(), |stat| stat_text(stat, "s")),
        ),
        row(
            "shrink→done",
            summary
                .grant_latency_by_operation
                .get("Remove")
                .map_or_else(|| "-".to_owned(), |stat| stat_text(stat, "s")),
        ),
        row(
            "refusals",
            summary.refusals.values().sum::<u64>().to_string(),
        ),
        Line::from(vec![
            Span::styled(format!("{:<14}", "controller"), DIM),
            problem("latched", count("actuation_latched")),
            problem("conflicts", count("ownership_conflict")),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<14}", "guest agent"), DIM),
            problem("read failures", summary.guest_read_failures),
            problem("stale", summary.guest_stale_reads),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<14}", "incidents"), DIM),
            problem("controller", incidents("controller")),
            problem("guest service", incidents("guest_service")),
            problem("vm", incidents("vm")),
        ]),
        row(
            "minimum avail",
            format!(
                "host {}  guest {}  over floor {}",
                minimum(&summary.host_available_mib),
                minimum(&summary.guest_available_mib),
                minimum(&summary.host_headroom_over_floor_mib)
            ),
        ),
        row(
            "processes",
            format!(
                "{} controller pid(s), inactive in {} samples",
                summary.controller_pids.len(),
                summary.controller_inactive_samples
            ),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(lines).block(panel("Requests & health (logged samples)")),
        area,
    );
}

fn problem(label: &str, count: u64) -> Span<'static> {
    let colour = if count == 0 { DIM } else { BAD };
    Span::styled(format!("{label} {count}  "), colour)
}

fn event_colour(event: &JournalEvent) -> Color {
    match event.name.as_deref() {
        Some(
            "actuation_latched"
            | "ownership_conflict"
            | "live_state_unavailable"
            | "metrics_controller_failed"
            | "metrics_qemu_exited"
            | "metrics_qemu_restarted"
            | "metrics_guest_session_changed"
            | "metrics_guest_agent_lost"
            | "metrics_guest_publication_stalled",
        ) => BAD,
        Some(name) if name.starts_with("metrics_") => WARN,
        Some("guest_input_invalid" | "guest_status_unpublished" | "resize_settled_short") => WARN,
        Some("request_completed") => GOOD,
        Some("controller_decision" | "resize_requested") => INFO,
        Some("marker") => Color::LightMagenta,
        _ => Color::Gray,
    }
}

fn draw_events(frame: &mut Frame, area: Rect, live: &Live) {
    let width = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = live
        .events
        .iter()
        .filter(|event| event.name.as_deref() != Some("control_health"))
        .take(area.height.saturating_sub(2) as usize)
        .map(|event| {
            let time = super::iso_utc(event.unix_micros / 1000);
            let text = match (&event.name, event.message.split_once("event=")) {
                (Some(name), _) if name == "marker" => format!("▶ {}", event.message),
                (_, Some((_, rest))) => watch::shorten(rest),
                _ => event.message.clone(),
            };
            let line = format!("{} {text}", &time[11..19]);
            let line: String = line.chars().take(width).collect();
            ListItem::new(Line::styled(line, event_colour(event)))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(panel("Controller and lifecycle events")),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, live: &Live, ui: &Ui) {
    if let Some(text) = &ui.input {
        let line = Line::from(vec![
            Span::styled(" marker: ", Style::new().fg(Color::LightMagenta).bold()),
            Span::raw(format!("{text}_")),
            Span::styled("   Enter saves, Esc cancels", DIM),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let key = |key: &'static str| Span::styled(key, Style::new().fg(Color::Black).bg(Color::Gray));
    let mut spans = vec![
        key(" -/+ "),
        Span::raw(format!(
            " sample {}  ",
            seconds_label(live.controls.interval)
        )),
        key(" [/] "),
        Span::raw(format!(
            " guest {}  ",
            seconds_label(live.controls.guest_interval)
        )),
        key(" space "),
        Span::raw(if live.controls.paused {
            " resume  "
        } else {
            " pause  "
        }),
        key(" n "),
        Span::raw(" now  "),
        key(" m "),
        Span::raw(" mark  "),
        key(" s "),
        Span::raw(" save  "),
        key(" w "),
        Span::raw(" window  "),
        key(" q "),
        Span::raw(" quit"),
    ];
    if let Some((at, text)) = &ui.flash {
        if at.elapsed() < Duration::from_secs(4) {
            spans.push(Span::styled(
                format!("   {text}"),
                Style::new().fg(INFO).add_modifier(Modifier::BOLD),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_step_within_their_range() {
        let five = Duration::from_secs(5);
        assert_eq!(step(five, true, 0.5), Duration::from_secs(10));
        assert_eq!(step(five, false, 0.5), Duration::from_secs(3));
        assert_eq!(
            step(Duration::from_secs(1), false, 1.0),
            Duration::from_secs(1)
        );
        assert_eq!(
            step(Duration::from_secs(120), true, 0.5),
            Duration::from_secs(120)
        );
        // A period from the command line that is not a step moves to the next.
        assert_eq!(step(Duration::from_secs(4), true, 0.5), five);
        assert_eq!(seconds_label(Duration::from_millis(500)), "0.5s");
    }

    fn screen(live: &Live, ui: &Ui, width: u16, height: u16) -> String {
        let command = test_command();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, &command, live, ui))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_command() -> Command {
        let config = crate::repo_root()
            .expect("repository root")
            .join("deploy/config_files/host_list.toml");
        super::super::parse(&["--config".to_owned(), config.to_string_lossy().into_owned()])
            .expect("command")
    }

    #[test]
    fn draws_every_panel_and_handles_keys() {
        use super::super::{GuestRead, HostMemory, Sample};
        use virtio_mem_core::{MemoryTelemetrySnapshot, VirtioMemState};

        let command = test_command();
        let mut live = Live::new(&command, "/tmp/run".into(), unix_millis());
        assert!(screen(&live, &Ui::default(), 120, 40).contains("waiting for the first sample"));

        let telemetry = RawTelemetryEnvelope::new(
            "win11_gpu",
            "VirtioMemService",
            "session",
            unix_millis(),
            1,
            7,
            MemoryTelemetrySnapshot {
                physical_total_bytes: 9 << 30,
                physical_available_bytes: 4 << 30,
                memory_load_percent: 55,
                commit_total_bytes: 7 << 30,
                commit_limit_bytes: 16 << 30,
                commit_peak_bytes: 8 << 30,
                system_cache_bytes: 1 << 30,
                kernel_paged_bytes: 0,
                kernel_nonpaged_bytes: 0,
            },
        );
        let device = VirtioMemState {
            size_bytes: 20 << 30,
            block_size_bytes: 2 * MIB,
            requested_bytes: 76 * MIB,
            current_bytes: 1068 * MIB,
        };
        for second in 0..50u64 {
            live.history.push_back(Point {
                elapsed_seconds: second as f64 * 5.0,
                host_available_mib: 9000 + second,
                device: Some((76, 1068)),
                guest_available_mib: Some(4096),
            });
        }
        live.latest = Some(Sample {
            unix_millis: unix_millis(),
            elapsed: Duration::from_secs(250),
            host: HostMemory {
                total: 31 << 30,
                available: 3 << 30,
                swap_total: 8 << 30,
                swap_free: 3 << 30,
            },
            qemu_rss_bytes: Some(9 << 30),
            controller: Some(super::super::UnitStatus::parse(
                "ActiveState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=101",
            )),
            device: Some(device),
            guest: Some(GuestRead {
                elapsed: Duration::from_millis(280),
                telemetry: Ok(telemetry.clone()),
            }),
            ..Sample::default()
        });
        live.summary
            .add_incident(&serde_json::json!({"component": "controller"}));
        live.guest = Some((unix_millis(), telemetry));
        live.mark("started 12 GB allocation".to_owned());
        let text = screen(&live, &Ui::default(), 120, 40);
        for expected in [
            "1 INCIDENT ",
            "FAILED: exit status 101",
            "controller 1",
            "shrinking, 992M to go",
            "-1.00G",
            "load  55%",
            "UNKNOWN",
            "▶ started 12 GB allocation",
            "no terminal request results yet",
            "Memory GiB, last 5 min",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        assert!(screen(&live, &Ui::default(), 60, 20).contains("needs at least"));

        let shared = Mutex::new(live);
        let mut ui = Ui::default();
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert!(ui.handle(press(KeyCode::Char('+')), &shared));
        assert!(ui.handle(press(KeyCode::Char(']')), &shared));
        assert!(ui.handle(press(KeyCode::Char(' ')), &shared));
        assert!(ui.handle(press(KeyCode::Char('m')), &shared));
        for character in "hog".chars() {
            assert!(ui.handle(press(KeyCode::Char(character)), &shared));
        }
        assert!(ui.handle(press(KeyCode::Enter), &shared));
        {
            let live = lock(&shared);
            assert_eq!(live.controls.interval, Duration::from_secs(2));
            assert_eq!(live.controls.guest_interval, Duration::from_secs(2));
            assert!(live.controls.paused);
            let markers: Vec<_> = live
                .controls
                .markers
                .iter()
                .map(|marker| marker.message.as_str())
                .collect();
            assert_eq!(
                markers,
                [
                    "started 12 GB allocation",
                    "sample period 2s",
                    "guest read period 2s",
                    "logging paused",
                    "hog"
                ]
            );
        }
        assert!(!ui.handle(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &shared
        ));
    }
}
