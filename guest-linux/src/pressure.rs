//! Linux's own memory-pressure signal, from pressure stall information
//! (`/proc/pressure/memory`), mapped onto the categorical state the shared
//! policy takes:
//!
//! - Low: tasks are stalling on memory now. An urgent growth trigger.
//! - High: no memory stall at all in the last ten seconds. Only this permits
//!   reclaim.
//! - Neutral: neither. Does not grow urgently and does not reclaim.
//!
//! Low comes from a PSI trigger: the kernel wakes the service as soon as stall
//! time in a short window crosses a threshold, so growth does not wait for the
//! next poll or for a ten-second average to rise, and stops being urgent when
//! the stalls stop rather than when the average decays. The kernel serves
//! windows shorter than two seconds only to a process with
//! `CAP_SYS_RESOURCE`; without it the service registers a two-second window,
//! which the kernel evaluates every two seconds.
//!
//! Where no trigger can be registered, Low falls back to the ten-second
//! average. An unreadable signal (PSI compiled out or disabled with `psi=0`)
//! reports nothing: growth stays allowed and reclaim does not.

use std::sync::OnceLock;
use std::time::Duration;

use virtio_mem_core::MemoryResourceNotificationState;

pub const PRESSURE_PATH: &str = "/proc/pressure/memory";
const MAX_PRESSURE_BYTES: usize = 4096;
/// `some avg10` at or above 10.00 %, in hundredths of a per cent. Ordinary
/// page-cache reclaim produces small values that are not a shortage. Used
/// only where no trigger is available.
const LOW_SOME_AVG10_HUNDREDTHS: u32 = 1000;
/// Triggers in order of preference, as `some <stall µs> <window µs>`: the
/// same 10 % stall share as the average, over one second where the kernel
/// allows it and over two where it does not.
const TRIGGERS: [(&str, &str); 2] = [
    ("some 100000 1000000", "100ms/1s"),
    ("some 200000 2000000", "200ms/2s"),
];

/// The trigger this process registered, or `none`, for the readiness report.
/// The first registration attempt decides it.
static TRIGGER: OnceLock<&'static str> = OnceLock::new();

/// `100ms/1s`, `200ms/2s`, `none` when growth relies on polling, or
/// `pending` before the first attempt: the trigger is registered only once
/// the host has published this VM's settings.
pub fn trigger_description() -> &'static str {
    TRIGGER.get().copied().unwrap_or("pending")
}

pub use virtio_mem_guest::worker::PressureSource;

#[derive(Debug, Default)]
pub struct ProcPressure {
    trigger: Option<std::fs::File>,
    /// An event seen by `wait` and not yet reported by `state`.
    pending: bool,
}

impl ProcPressure {
    /// Registers the most responsive trigger the kernel grants this process.
    pub fn open() -> Self {
        let trigger = TRIGGERS.iter().find_map(|(spec, description)| {
            let file = trigger::register(spec)?;
            // First registration wins; a process has one pressure source.
            let _ = TRIGGER.set(description);
            Some(file)
        });
        if trigger.is_none() && TRIGGER.set("none").is_ok() {
            eprintln!(
                "warning: no pressure trigger could be registered: urgent growth waits for \
                 the next poll"
            );
        }
        Self {
            trigger,
            pending: false,
        }
    }

    /// True when the trigger fired within `timeout`. A trigger the kernel has
    /// withdrawn is dropped, so the caller falls back to the average.
    fn event(&mut self, timeout: Duration) -> Option<bool> {
        let file = self.trigger.as_ref()?;
        match trigger::wait(file, timeout) {
            Ok(fired) => Some(fired),
            Err(()) => {
                eprintln!("pressure trigger was withdrawn; using the ten-second average");
                self.trigger = None;
                None
            }
        }
    }
}

impl PressureSource for ProcPressure {
    fn state(&mut self) -> Option<MemoryResourceNotificationState> {
        let fired = self
            .event(Duration::ZERO)
            .map(|fired| fired || std::mem::take(&mut self.pending));
        if fired == Some(true) {
            return Some(MemoryResourceNotificationState::Low);
        }
        let bytes = virtio_mem_guest::files::read_bounded(
            std::path::Path::new(PRESSURE_PATH),
            MAX_PRESSURE_BYTES,
        )
        .ok()??;
        classify(std::str::from_utf8(&bytes).ok()?, fired.is_some())
    }

    fn wait(&mut self, timeout: Duration) -> bool {
        match self.event(timeout) {
            Some(fired) => {
                self.pending |= fired;
                fired
            }
            None => {
                std::thread::sleep(timeout);
                false
            }
        }
    }
}

/// The state from the ten-second average. With a trigger registered and not
/// fired, the average never means Low: it stays high long after stalls end.
pub fn classify(text: &str, has_trigger: bool) -> Option<MemoryResourceNotificationState> {
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("some "))?
        .split_ascii_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?;
    let (whole, fraction) = value.split_once('.')?;
    if fraction.len() != 2 {
        return None;
    }
    let hundredths = whole
        .parse::<u32>()
        .ok()?
        .checked_mul(100)?
        .checked_add(fraction.parse::<u32>().ok()?)?;
    Some(if hundredths == 0 {
        MemoryResourceNotificationState::High
    } else if hundredths >= LOW_SOME_AVG10_HUNDREDTHS && !has_trigger {
        MemoryResourceNotificationState::Low
    } else {
        MemoryResourceNotificationState::Neutral
    })
}

#[cfg(unix)]
mod trigger {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// The open file is the registration: closing it removes the trigger.
    pub fn register(spec: &str) -> Option<File> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(super::PRESSURE_PATH)
            .ok()?;
        // The kernel takes one write and replaces its last byte with a
        // terminator, so the specification carries its own.
        let mut bytes = spec.as_bytes().to_vec();
        bytes.push(0);
        (file.write(&bytes).ok()? == bytes.len()).then_some(file)
    }

    /// `Ok(true)` when the trigger fired within `timeout`; `Err` when the
    /// kernel reports the trigger gone.
    pub fn wait(file: &File, timeout: Duration) -> Result<bool, ()> {
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `descriptor` is one valid pollfd for the duration of the
        // call, and `file` keeps its descriptor open.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready <= 0 {
            // Timed out, or interrupted by a signal the caller checks for.
            return Ok(false);
        }
        if descriptor.revents & libc::POLLPRI != 0 {
            Ok(true)
        } else {
            Err(())
        }
    }
}

#[cfg(not(unix))]
mod trigger {
    use std::fs::File;
    use std::time::Duration;

    pub fn register(_spec: &str) -> Option<File> {
        None
    }

    pub fn wait(_file: &File, _timeout: Duration) -> Result<bool, ()> {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(some_avg10: &str) -> String {
        format!(
            "some avg10={some_avg10} avg60=0.50 avg300=0.10 total=12345\n\
             full avg10=99.00 avg60=0.00 avg300=0.00 total=6789\n"
        )
    }

    #[test]
    fn stall_share_selects_the_categorical_state() {
        use MemoryResourceNotificationState::{High, Low, Neutral};
        for (value, polled, triggered) in [
            ("0.00", High, High),
            ("0.01", Neutral, Neutral),
            ("9.99", Neutral, Neutral),
            ("10.00", Low, Neutral),
            ("100.00", Low, Neutral),
        ] {
            assert_eq!(classify(&psi(value), false), Some(polled), "{value}");
            assert_eq!(classify(&psi(value), true), Some(triggered), "{value}");
        }
    }

    #[test]
    fn an_unreadable_signal_reports_nothing() {
        for text in [
            "",
            "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            "some avg60=0.00\n",
            &psi("1"),
            &psi("1.5"),
            &psi("-1.00"),
            &psi("x.yz"),
        ] {
            assert_eq!(classify(text, false), None, "{text}");
        }
    }

    #[test]
    fn a_trigger_event_seen_while_waiting_is_reported_once_as_low() {
        // No trigger: nothing fires, nothing is pending, the wait just waits.
        let mut pressure = ProcPressure::default();
        assert!(!pressure.wait(Duration::from_millis(1)));
        assert_eq!(pressure.event(Duration::ZERO), None);
        // Before the first registration attempt the trigger is pending, not
        // missing: nothing has been asked of the kernel yet.
        assert_eq!(trigger_description(), "pending");
    }
}
