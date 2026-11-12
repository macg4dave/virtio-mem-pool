//! Linux's own memory-pressure signal, from pressure stall information
//! (`/proc/pressure/memory`), mapped onto the categorical state the shared
//! policy takes from Windows' memory resource notifications:
//!
//! - Low: tasks were stalled on memory for a meaningful share of the last ten
//!   seconds. An urgent growth trigger.
//! - High: no memory stall at all in that window. Only this permits reclaim.
//! - Neutral: some stall, short of Low. Neither grows urgently nor reclaims.
//!
//! An unreadable signal (PSI compiled out or disabled with `psi=0`) reports
//! nothing: growth stays allowed and reclaim does not.

use virtio_mem_core::MemoryResourceNotificationState;

pub const PRESSURE_PATH: &str = "/proc/pressure/memory";
const MAX_PRESSURE_BYTES: usize = 4096;
/// `some avg10` at or above 10.00 %, in hundredths of a per cent. Ordinary
/// page-cache reclaim produces small values that are not a shortage.
const LOW_SOME_AVG10_HUNDREDTHS: u32 = 1000;

pub trait PressureSource {
    fn state(&self) -> Option<MemoryResourceNotificationState>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcPressure;

impl PressureSource for ProcPressure {
    fn state(&self) -> Option<MemoryResourceNotificationState> {
        let bytes =
            crate::files::read_bounded(std::path::Path::new(PRESSURE_PATH), MAX_PRESSURE_BYTES)
                .ok()??;
        classify(std::str::from_utf8(&bytes).ok()?)
    }
}

pub fn classify(text: &str) -> Option<MemoryResourceNotificationState> {
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
    Some(if hundredths >= LOW_SOME_AVG10_HUNDREDTHS {
        MemoryResourceNotificationState::Low
    } else if hundredths == 0 {
        MemoryResourceNotificationState::High
    } else {
        MemoryResourceNotificationState::Neutral
    })
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
        for (value, expected) in [
            ("0.00", High),
            ("0.01", Neutral),
            ("9.99", Neutral),
            ("10.00", Low),
            ("100.00", Low),
        ] {
            assert_eq!(classify(&psi(value)), Some(expected), "{value}");
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
            assert_eq!(classify(text), None, "{text}");
        }
    }
}
