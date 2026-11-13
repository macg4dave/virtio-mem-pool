//! The coloured terminal frame shared by the live host dashboards
//! (`watch` and `pool`): boxed panels, meters and bars drawn as ANSI on a
//! terminal or as plain ASCII when piped.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const MIB: u64 = 1 << 20;
pub(crate) const GIB: u64 = 1 << 30;

// 256-colour palette on a navy background.
pub(crate) const BACKGROUND: u8 = 17;
pub(crate) const BAR_BACKGROUND: u8 = 25;
pub(crate) const FRAME: u8 = 45;
pub(crate) const TITLE: u8 = 231;
pub(crate) const LABEL: u8 = 250;
pub(crate) const VALUE: u8 = 231;
pub(crate) const DIM: u8 = 67;
pub(crate) const GREEN: u8 = 46;
pub(crate) const YELLOW: u8 = 226;
pub(crate) const RED: u8 = 196;
pub(crate) const CYAN: u8 = 51;
pub(crate) const MAGENTA: u8 = 213;
pub(crate) const TRACK: u8 = 24;

pub(crate) struct Glyphs {
    pub(crate) top_left: &'static str,
    pub(crate) top_right: &'static str,
    pub(crate) bottom_left: &'static str,
    pub(crate) bottom_right: &'static str,
    pub(crate) horizontal: &'static str,
    pub(crate) vertical: &'static str,
    pub(crate) rule: &'static str,
    pub(crate) full: &'static str,
    pub(crate) empty: &'static str,
    pub(crate) mark: &'static str,
    pub(crate) axis: &'static str,
    pub(crate) corner: &'static str,
    pub(crate) dot: &'static str,
}

pub(crate) const UNICODE: Glyphs = Glyphs {
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

pub(crate) const ASCII: Glyphs = Glyphs {
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
pub(crate) struct Frame {
    pub(crate) width: usize,
    pub(crate) ansi: bool,
    pub(crate) glyphs: &'static Glyphs,
    pub(crate) lines: Vec<Vec<(String, u8, u8)>>,
}

impl Frame {
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self {
            width,
            ansi,
            glyphs: if ansi { &UNICODE } else { &ASCII },
            lines: Vec::new(),
        }
    }

    pub(crate) fn line(&mut self) {
        self.lines.push(Vec::new());
    }

    pub(crate) fn add(&mut self, text: impl Into<String>, colour: u8) {
        self.add_on(text, colour, BACKGROUND);
    }

    pub(crate) fn add_on(&mut self, text: impl Into<String>, colour: u8, background: u8) {
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

    pub(crate) fn length(&self) -> usize {
        self.lines.last().map_or(0, |line| {
            line.iter().map(|(text, _, _)| text.chars().count()).sum()
        })
    }

    pub(crate) fn pad_to(&mut self, column: usize, background: u8) {
        let missing = column.saturating_sub(self.length());
        self.add_on(" ".repeat(missing), LABEL, background);
    }

    pub(crate) fn bar(&mut self, left: &str, right: &str) {
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

    pub(crate) fn top(&mut self, title: &str, detail: &str) {
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

    pub(crate) fn row(&mut self) {
        self.line();
        self.add(format!("{} ", self.glyphs.vertical), FRAME);
    }

    /// Clips overlong content inside the frame, pads and closes the row.
    pub(crate) fn end_row(&mut self) {
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

    pub(crate) fn bottom(&mut self) {
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

    pub(crate) fn meter(&mut self, label: &str, fraction: f64, detail: &str, colour: Option<u8>) {
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

    pub(crate) fn metric(&mut self, label: &str, now: &str, detail: &str, colour: u8) {
        self.row();
        self.add(format!("{label:<22}"), LABEL);
        self.add(format!("{now:>14}"), colour);
        self.add("    ", LABEL);
        self.add(detail, YELLOW);
        self.end_row();
    }

    pub(crate) fn message(&mut self, text: &str, colour: u8) {
        self.row();
        self.add(text, colour);
        self.end_row();
    }

    pub(crate) fn render(&self) -> String {
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

pub(crate) fn load_colour(fraction: f64) -> u8 {
    if fraction >= 0.9 {
        RED
    } else if fraction >= 0.7 {
        YELLOW
    } else {
        GREEN
    }
}

pub(crate) fn gib(bytes: u64) -> String {
    format!("{:.3} GiB", bytes as f64 / GIB as f64)
}

pub(crate) fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

pub(crate) fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|_| "host".to_owned())
}

/// Local wall-clock time as HH:MM:SS, using the system's UTC offset.
pub(crate) fn clock() -> String {
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
pub(crate) fn terminal_columns() -> Option<usize> {
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
pub(crate) fn terminal_columns() -> Option<usize> {
    None
}
