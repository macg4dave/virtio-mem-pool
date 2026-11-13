//! Windows' side of the shared guest service: its memory counters, its
//! memory-resource notifications as the pressure signal, its clock, and how
//! the control channel is opened. The request protocol, worker and service
//! loop are the shared guest crate's, as for Linux.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use virtio_mem_core::settings::{CONTROL_PATH, SERVICE_NAME};
use virtio_mem_core::{MemoryResourceNotificationState, MemoryTelemetrySnapshot};
use virtio_mem_guest::requests::GuestIdentity;
use virtio_mem_guest::serve::{serve, Platform, Sources};
use virtio_mem_guest::transport::{FileTransport, Transport};
use virtio_mem_guest::worker::{Clock, MemorySource, PressureSource};

use crate::demand::{
    MemoryResourceNotificationTelemetry, MemoryTelemetry, SystemTelemetryClock, TelemetryClock,
};
use crate::service_host::{ServiceWorker, StopSignal};

/// Windows' performance counters.
#[derive(Debug, Default)]
pub struct Memory<T>(pub T);

impl<T: MemoryTelemetry> MemorySource for Memory<T> {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String> {
        self.0
            .collect()
            .map_err(|error| format!("native memory telemetry failed: {error}"))
    }
}

/// Windows' memory-resource notifications. Low wakes the worker at once.
/// Because the notification is a level, once Low is seen the wait falls back
/// to the timer until it clears, so a lasting shortage does not spin.
#[derive(Debug)]
pub struct Pressure<N> {
    notifications: N,
    stop: StopSignal,
    low: bool,
}

impl<N> Pressure<N> {
    pub fn new(notifications: N, stop: StopSignal) -> Self {
        Self {
            notifications,
            stop,
            low: false,
        }
    }
}

impl<N: MemoryResourceNotificationTelemetry> PressureSource for Pressure<N> {
    fn state(&mut self) -> Option<MemoryResourceNotificationState> {
        let state = self.notifications.collect();
        // An unreadable signal also falls back to the timer.
        self.low = state.is_none() || state == Some(MemoryResourceNotificationState::Low);
        state
    }

    fn wait(&mut self, timeout: Duration) -> bool {
        self.notifications
            .wait(&self.stop, timeout, !self.low, None);
        !self.low && self.notifications.collect() == Some(MemoryResourceNotificationState::Low)
    }
}

/// The shared worker's clock, from Windows' own.
#[derive(Debug, Default)]
pub struct WindowsClock(pub SystemTelemetryClock);

impl Clock for WindowsClock {
    fn unix_millis(&self) -> Result<u64, String> {
        self.0.now_unix_millis()
    }

    fn monotonic_millis(&self) -> Result<u64, String> {
        self.0.monotonic_millis()
    }

    fn boot_uptime_millis(&self) -> Option<u64> {
        self.0.boot_uptime_millis()
    }
}

/// The control channel when the VM has the port, else the control files
/// that the host reaches through the guest agent.
#[cfg(windows)]
fn transport() -> Box<dyn Transport> {
    match crate::port::ControlPort::open() {
        Ok(port) => Box::new(virtio_mem_guest::transport::StreamTransport::new(port)),
        Err(error) => {
            eprintln!("control channel unavailable ({error}); using the control files");
            Box::new(FileTransport::new(CONTROL_PATH))
        }
    }
}

#[cfg(not(windows))]
fn transport() -> Box<dyn Transport> {
    Box::new(FileTransport::new(CONTROL_PATH))
}

/// The service's worker: waits for the host's settings, then samples,
/// decides and publishes until stopped.
#[derive(Debug, Default)]
pub struct ProtocolWorker;

impl ServiceWorker for ProtocolWorker {
    fn run(&mut self, stop: &StopSignal) -> Result<(), String> {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        stop.on_cancel(move || flag.store(true, Ordering::Release));
        let platform = Platform {
            service_name: SERVICE_NAME,
            control_path: CONTROL_PATH,
            identity: GuestIdentity {
                build_id: crate::BUILD_ID.to_owned(),
                driver_version: crate::diagnostics::driver_version(),
            },
            environment: std::collections::BTreeMap::new,
        };
        serve(&platform, transport(), &stopped, |_settings| {
            Ok(Sources {
                memory: Memory(crate::demand::NativeMemoryTelemetry),
                pressure: Pressure::new(notifications(), stop.clone()),
                clock: WindowsClock::default(),
            })
        })
    }
}

#[cfg(windows)]
fn notifications() -> impl MemoryResourceNotificationTelemetry {
    crate::demand::native_memory_resource_notifications()
}

#[cfg(not(windows))]
fn notifications() -> impl MemoryResourceNotificationTelemetry {
    crate::demand::UnavailableMemoryResourceNotifications
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// Reports the state it is told to, and records what waits asked for.
    #[derive(Debug, Default, Clone)]
    struct Signal {
        state: Rc<Cell<Option<MemoryResourceNotificationState>>>,
        watched_low: Rc<Cell<Option<bool>>>,
    }

    impl MemoryResourceNotificationTelemetry for Signal {
        fn wait(&mut self, _: &StopSignal, _: Duration, watch_low: bool, _: Option<usize>) {
            self.watched_low.set(Some(watch_low));
        }

        fn collect(&self) -> Option<MemoryResourceNotificationState> {
            self.state.get()
        }
    }

    #[test]
    fn low_wakes_once_and_a_lasting_low_falls_back_to_the_timer() {
        use MemoryResourceNotificationState::{High, Low};
        let signal = Signal::default();
        let mut pressure = Pressure::new(signal.clone(), StopSignal::new());
        signal.state.set(Some(High));
        assert_eq!(pressure.state(), Some(High));
        assert!(!pressure.wait(Duration::ZERO));
        assert_eq!(signal.watched_low.get(), Some(true));
        // Low arrives during the wait: wake now.
        signal.state.set(Some(Low));
        assert!(pressure.wait(Duration::ZERO));
        assert_eq!(pressure.state(), Some(Low));
        // Still Low: the wait no longer watches it, and does not report it.
        assert!(!pressure.wait(Duration::ZERO));
        assert_eq!(signal.watched_low.get(), Some(false));
        // Unreadable also falls back to the timer.
        signal.state.set(None);
        assert_eq!(pressure.state(), None);
        assert!(!pressure.wait(Duration::ZERO));
        assert_eq!(signal.watched_low.get(), Some(false));
    }
}
