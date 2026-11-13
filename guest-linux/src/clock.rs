//! Linux time sources for the shared worker.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use virtio_mem_guest::worker::Clock;

#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn unix_millis(&self) -> Result<u64, String> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
        u64::try_from(duration.as_millis())
            .map_err(|_| "system Unix timestamp does not fit in u64 milliseconds".to_owned())
    }

    fn monotonic_millis(&self) -> Result<u64, String> {
        u64::try_from(self.started.elapsed().as_millis())
            .map_err(|_| "process monotonic timestamp does not fit in u64 milliseconds".to_owned())
    }

    fn boot_uptime_millis(&self) -> Option<u64> {
        let bytes =
            virtio_mem_guest::files::read_bounded(std::path::Path::new("/proc/uptime"), 256)
                .ok()??;
        let text = String::from_utf8(bytes).ok()?;
        let (seconds, fraction) = text.split_ascii_whitespace().next()?.split_once('.')?;
        let centiseconds = fraction.get(..2)?.parse::<u64>().ok()?;
        seconds
            .parse::<u64>()
            .ok()?
            .checked_mul(1000)?
            .checked_add(centiseconds * 10)
    }
}
