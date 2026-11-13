//! The service loop: sample memory and pressure, let the guest policy decide
//! the one add/remove request, and publish both. It never resizes anything
//! itself; the host executes requests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use virtio_mem_core::{
    MemoryResourceNotificationState, MemoryTelemetrySnapshot, RawTelemetryEnvelope, TargetSample,
};

use crate::requests::GuestRequests;
use crate::transport::Transport;

/// Matches the host's bound on one publication record.
const MAX_PUBLICATION_BYTES: usize = 64 * 1024;
/// How often a waiting worker notices a stop signal.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Consecutive failed polls tolerated before the service fails. Host status
/// and the guest's own counters are not read atomically, so a poll can see
/// them disagree while memory is being plugged; a fault that persists is real.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// The guest's memory counters, as the shared telemetry snapshot.
pub trait MemorySource {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String>;
}

/// The guest's own pressure signal.
pub trait PressureSource {
    /// The state for this sample. An event seen since the last call is
    /// consumed by it.
    fn state(&mut self) -> Option<MemoryResourceNotificationState>;

    /// Wait up to `timeout`; true when urgent pressure arrived first.
    fn wait(&mut self, timeout: Duration) -> bool {
        std::thread::sleep(timeout);
        false
    }
}

pub trait Clock {
    fn unix_millis(&self) -> Result<u64, String>;
    fn monotonic_millis(&self) -> Result<u64, String>;
    fn boot_uptime_millis(&self) -> Option<u64> {
        None
    }
}

/// A per-process identity for this service run's publications. Every call
/// differs, so a worker restarted with new settings is a new session.
pub fn process_session_id() -> Result<String, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
    Ok(format!(
        "session-{:x}-{:x}",
        duration.as_nanos(),
        std::process::id()
    ))
}

/// Why `GuestWorker::run` returned without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Stopped,
    /// The host published different settings; start again with them.
    SettingsChanged,
}

#[derive(Debug)]
pub struct GuestWorker<M, P, C> {
    memory: M,
    pressure: P,
    clock: C,
    requests: GuestRequests,
    transport: Box<dyn Transport>,
    interval: Duration,
    next_sequence: u64,
    previous_monotonic_millis: Option<u64>,
    consecutive_failures: u32,
    environment: fn() -> BTreeMap<String, String>,
}

impl<M: MemorySource, P: PressureSource, C: Clock> GuestWorker<M, P, C> {
    /// Identity comes from `requests`, which validates it.
    pub fn new(
        memory: M,
        pressure: P,
        clock: C,
        requests: GuestRequests,
        transport: Box<dyn Transport>,
        interval: Duration,
    ) -> Result<Self, String> {
        if interval.is_zero() {
            return Err("polling interval must be greater than zero".to_owned());
        }
        Ok(Self {
            memory,
            pressure,
            clock,
            requests,
            transport,
            interval,
            next_sequence: 0,
            previous_monotonic_millis: None,
            consecutive_failures: 0,
            environment: BTreeMap::new,
        })
    }

    /// The transport, for a worker started again with new settings.
    pub fn into_transport(self) -> Box<dyn Transport> {
        self.transport
    }

    /// Publish these guest facts with every sample's diagnostics.
    pub fn with_environment(mut self, environment: fn() -> BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// One sample, decision and publication.
    pub fn poll_once(&mut self) -> Result<RawTelemetryEnvelope, String> {
        let memory = match self.memory.sample() {
            Ok(memory) => memory,
            Err(error) => {
                // Without this sample the reclaim history has a hole.
                self.requests.invalidate_history();
                return Err(format!("memory sample failed: {error}"));
            }
        };
        let observed_unix_millis = self.clock.unix_millis()?;
        let monotonic_millis = self.clock.monotonic_millis()?;
        if self
            .previous_monotonic_millis
            .is_some_and(|previous| monotonic_millis <= previous)
        {
            return Err("monotonic clock did not advance".to_owned());
        }
        // The policy may consume this sample's ordering before a later step
        // fails, so a failed poll never reuses its sequence or timestamp.
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or("publication sequence exhausted")?;
        self.previous_monotonic_millis = Some(monotonic_millis);
        let requester = self.requests.requester();
        let mut envelope = RawTelemetryEnvelope::new(
            requester.vm_name(),
            requester.service_name(),
            requester.session_id(),
            observed_unix_millis,
            monotonic_millis,
            sequence,
            memory,
        );
        envelope.native_pressure = self.pressure.state();
        let status = self.transport.host_status()?;
        envelope.request = self
            .requests
            .poll(
                TargetSample {
                    session_id: envelope.session_id.clone(),
                    observed_unix_millis,
                    monotonic_millis,
                    sequence: envelope.sequence,
                    memory,
                    native_pressure: envelope.native_pressure,
                },
                status.as_ref(),
            )
            .map_err(|error| format!("guest memory request failed: {error}"))?;
        envelope.diagnostics = Some(
            self.requests
                .diagnostics(self.clock.boot_uptime_millis(), (self.environment)()),
        );
        let mut encoded = serde_json::to_vec(&envelope)
            .map_err(|error| format!("encode publication: {error}"))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_PUBLICATION_BYTES {
            return Err(format!(
                "publication exceeds {MAX_PUBLICATION_BYTES} byte limit"
            ));
        }
        self.transport
            .publish(&encoded)
            .map_err(|error| format!("publish: {error}"))?;
        Ok(envelope)
    }

    /// One poll whose failure is skipped, with reclaim history reset, until
    /// `MAX_CONSECUTIVE_FAILURES` in a row. A retained request is unaffected:
    /// it stays durable and is published again by the next successful poll.
    pub fn poll_or_skip(&mut self) -> Result<(), String> {
        match self.poll_once() {
            Ok(_) => {
                self.consecutive_failures = 0;
                Ok(())
            }
            Err(error) => {
                self.requests.invalidate_history();
                self.consecutive_failures += 1;
                if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(format!(
                        "{} consecutive polls failed: {error}",
                        self.consecutive_failures
                    ));
                }
                eprintln!("poll skipped; reclaim history reset: {error}");
                Ok(())
            }
        }
    }

    /// Poll until `stop` is set or the host publishes different settings.
    /// A persistent failure ends the service so the supervisor restarts it
    /// and the fault is visible.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<RunEnd, String> {
        while !stop.load(Ordering::Acquire) {
            self.poll_or_skip()?;
            if self.requests.settings_changed() {
                return Ok(RunEnd::SettingsChanged);
            }
            // A consumed result is reassessed at once rather than a poll later.
            let wait = if self.requests.resolved_last_poll() {
                Duration::from_millis(1)
            } else {
                self.interval
            };
            let deadline = Instant::now() + wait;
            while !stop.load(Ordering::Acquire) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // Urgent pressure, or news from the host, does not wait for
                // the interval to end.
                if self.pressure.wait(remaining.min(STOP_CHECK_INTERVAL))
                    || self.transport.status_arrived()
                {
                    break;
                }
            }
        }
        Ok(RunEnd::Stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use virtio_mem_core::settings::GlobalSettings;
    use virtio_mem_core::{
        host_status_path, request_state_path, GuestRequestState, GuestSettings, HostMemoryStatus,
        MemoryOperation, MemoryOutcome, MemoryResourceNotificationState, MemoryResult,
        MemoryTelemetrySnapshot, MEMORY_REQUEST_VERSION,
    };

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    /// 1 GiB of fixed RAM plus what the device has plugged; nothing in swap.
    struct Memory {
        device_bytes: u64,
        available_bytes: u64,
        fail: bool,
    }

    impl MemorySource for Memory {
        fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String> {
            if self.fail {
                return Err("unreadable".to_owned());
            }
            let total = GIB + self.device_bytes;
            Ok(MemoryTelemetrySnapshot {
                physical_total_bytes: total,
                physical_available_bytes: self.available_bytes,
                memory_load_percent: 50,
                commit_total_bytes: total - self.available_bytes,
                commit_limit_bytes: total,
                commit_peak_bytes: total,
                system_cache_bytes: 0,
                kernel_paged_bytes: 0,
                kernel_nonpaged_bytes: 0,
            })
        }
    }

    struct Pressure(MemoryResourceNotificationState);

    impl PressureSource for Pressure {
        fn state(&mut self) -> Option<MemoryResourceNotificationState> {
            Some(self.0)
        }
    }

    /// Advances one second per sample.
    #[derive(Default)]
    struct StepClock(Cell<u64>);

    impl Clock for StepClock {
        fn unix_millis(&self) -> Result<u64, String> {
            Ok(1_000_000 + self.0.get() * 1000)
        }

        fn monotonic_millis(&self) -> Result<u64, String> {
            self.0.set(self.0.get() + 1);
            Ok(self.0.get() * 1000)
        }
    }

    fn settings() -> GuestSettings {
        GuestSettings {
            policy: GlobalSettings::default().guest_policy(2 * GIB, 3 * GIB),
            min_ram: 2 * GIB,
            max_ram: 5 * GIB,
            poll_interval_millis: 1000,
        }
    }

    struct Fixture {
        directory: PathBuf,
        control: String,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "virtio-mem-guest-worker-{name}-{}",
                std::process::id()
            ));
            // A previous run may have left fixtures behind.
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).unwrap();
            let control = directory.join("control.json").display().to_string();
            Self { directory, control }
        }

        fn worker(
            &self,
            session: &str,
            device_bytes: u64,
            available_bytes: u64,
        ) -> GuestWorker<Memory, Pressure, StepClock> {
            let requests = GuestRequests::open(
                settings(),
                "debian",
                "virtio-mem-guest",
                session,
                &self.control,
                crate::requests::GuestIdentity {
                    build_id: "test".to_owned(),
                    driver_version: Ok("test".to_owned()),
                },
            )
            .unwrap();
            GuestWorker::new(
                Memory {
                    device_bytes,
                    available_bytes,
                    fail: false,
                },
                Pressure(MemoryResourceNotificationState::High),
                StepClock::default(),
                requests,
                Box::new(crate::transport::FileTransport::new(&self.control)),
                Duration::from_secs(1),
            )
            .unwrap()
        }

        fn write_status(&self, second: u64, current: u64, result: Option<MemoryResult>) {
            let status = HostMemoryStatus {
                version: MEMORY_REQUEST_VERSION,
                vm_name: "debian".to_owned(),
                service_name: "virtio-mem-guest".to_owned(),
                device_alias: "ua-virtiomem0".to_owned(),
                observed_unix_millis: 1_000_000 + second * 1000,
                device_size_bytes: 4 * GIB,
                block_size_bytes: 2 * MIB,
                requested_bytes: current,
                current_bytes: current,
                result,
                guest_settings: None,
            };
            let mut bytes = serde_json::to_vec(&status).unwrap();
            bytes.push(b'\n');
            std::fs::write(host_status_path(&self.control), bytes).unwrap();
        }

        fn published(&self) -> RawTelemetryEnvelope {
            let text = std::fs::read_to_string(&self.control).unwrap();
            assert!(text.ends_with('\n'));
            serde_json::from_str(&text).unwrap()
        }

        fn retained(&self) -> GuestRequestState {
            serde_json::from_slice(&std::fs::read(request_state_path(&self.control)).unwrap())
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Best effort: leftovers are removed by the next run.
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn without_host_status_the_worker_publishes_telemetry_and_no_request() {
        let fixture = Fixture::new("no-status");
        let worker = fixture.worker("session-a", 0, 100 * MIB);
        let mut worker = worker.with_environment(|| {
            BTreeMap::from([("pressure_signal".to_owned(), "psi".to_owned())])
        });
        let envelope = worker.poll_once().unwrap();
        assert_eq!(envelope.request, None);
        assert_eq!(envelope.sequence, 0);
        assert_eq!(
            envelope.diagnostics.as_ref().unwrap().environment["pressure_signal"],
            "psi"
        );
        assert_eq!(
            envelope.native_pressure,
            Some(MemoryResourceNotificationState::High)
        );
        assert_eq!(fixture.published(), envelope);
        assert_eq!(worker.poll_once().unwrap().sequence, 1);
    }

    #[test]
    fn a_shortage_issues_one_durable_request_that_survives_restart_until_resolved() {
        let fixture = Fixture::new("request");
        fixture.write_status(0, 0, None);
        // 1 GiB total, 512 MiB available: 1.5 GiB short of the 2 GiB reserve,
        // plus the 128 MiB margin that keeps the device off the threshold.
        let mut worker = fixture.worker("session-a", 0, 512 * MIB);
        let request = worker.poll_once().unwrap().request.expect("request");
        assert_eq!(request.operation, MemoryOperation::Add);
        assert_eq!(request.bytes, GIB + 640 * MIB);
        assert_eq!(fixture.retained().pending, Some(request.clone()));
        assert_eq!(fixture.published().request, Some(request.clone()));

        // A restarted service retransmits the same request unchanged.
        let mut restarted = fixture.worker("session-b", 0, 512 * MIB);
        assert_eq!(
            restarted.poll_once().unwrap().request,
            Some(request.clone())
        );

        // The host grew the device and published the correlated result.
        fixture.write_status(
            1,
            request.bytes,
            Some(MemoryResult {
                request: request.clone(),
                outcome: MemoryOutcome::Completed,
                actual_bytes: request.bytes,
                observed_unix_millis: 1_001_000,
                reason: None,
            }),
        );
        // Until Linux has onlined the new memory its total is below what the
        // host reports plugged: that poll is skipped, not fatal.
        restarted.poll_or_skip().unwrap();
        assert_eq!(fixture.retained().pending, Some(request.clone()));
        restarted.memory.device_bytes = request.bytes;
        restarted.memory.available_bytes = 2 * GIB + 128 * MIB;
        let envelope = restarted.poll_once().unwrap();
        assert_eq!(
            envelope.sequence, 2,
            "the skipped poll's sequence is not reused"
        );
        assert_eq!(envelope.request, None);
        assert_eq!(
            envelope
                .diagnostics
                .unwrap()
                .last_consumed_result
                .unwrap()
                .request,
            request
        );
        assert_eq!(fixture.retained().pending, None);
    }

    #[test]
    fn surplus_memory_is_returned_only_after_qualified_history() {
        let fixture = Fixture::new("reclaim");
        // 5 GiB total with 4.5 GiB available: 0.5 GiB in use plus the 2 GiB
        // reserve and the 128 MiB margin, less 1 GiB of fixed RAM, wants
        // 1,664 MiB of the 4 GiB device.
        let mut worker = fixture.worker("session-a", 4 * GIB, 4 * GIB + 512 * MIB);
        fixture.write_status(1, 4 * GIB, None);
        assert_eq!(worker.poll_once().unwrap().request, None, "no history yet");
        fixture.write_status(2, 4 * GIB, None);
        let request = worker.poll_once().unwrap().request.expect("reclaim");
        assert_eq!(request.operation, MemoryOperation::Remove);
        assert_eq!(request.bytes, 2 * GIB + 384 * MIB);
    }

    #[test]
    fn settings_arrive_from_the_host_and_a_change_ends_the_run() {
        use crate::requests::published_settings;
        use crate::transport::{FileTransport, Transport};
        let fixture = Fixture::new("settings");
        let service = "virtio-mem-guest";
        let mut files = FileTransport::new(&fixture.control);
        let mut published = || {
            files
                .host_status()
                .unwrap()
                .and_then(|status| published_settings(&status, service))
        };
        assert_eq!(published(), None);
        // A status without settings, as a Windows guest receives, is not one.
        fixture.write_status(0, 0, None);
        assert_eq!(published(), None);
        let publish = |settings: GuestSettings, vm_name: &str| {
            let path = host_status_path(&fixture.control);
            let mut status: HostMemoryStatus =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            status.guest_settings = Some(settings);
            status.vm_name = vm_name.to_owned();
            let mut bytes = serde_json::to_vec(&status).unwrap();
            bytes.push(b'\n');
            std::fs::write(path, bytes).unwrap();
        };
        publish(settings(), "debian");
        assert_eq!(published(), Some(("debian".to_owned(), settings())));
        let status = FileTransport::new(&fixture.control)
            .host_status()
            .unwrap()
            .unwrap();
        assert_eq!(published_settings(&status, "other-service"), None);

        let stop = AtomicBool::new(false);
        let mut worker = fixture.worker("session-a", 0, 512 * MIB);
        worker.poll_once().unwrap();
        assert!(!worker.requests.settings_changed());
        let mut changed = settings();
        changed.max_ram = 4 * GIB;
        publish(changed, "debian");
        assert_eq!(worker.run(&stop), Ok(RunEnd::SettingsChanged));
        // So does the host knowing this guest by another name.
        let mut worker = fixture.worker("session-b", 0, 512 * MIB);
        publish(settings(), "renamed");
        assert_eq!(worker.run(&stop), Ok(RunEnd::SettingsChanged));
        // Invalid settings are never adopted.
        let mut invalid = settings();
        invalid.min_ram = 6 * GIB;
        publish(invalid, "debian");
        let status = FileTransport::new(&fixture.control)
            .host_status()
            .unwrap()
            .unwrap();
        assert_eq!(published_settings(&status, service), None);
    }

    #[test]
    fn a_persistent_failure_ends_the_service_after_bounded_skips() {
        let fixture = Fixture::new("persistent-failure");
        let mut worker = fixture.worker("session-a", 0, 512 * MIB);
        worker.memory.fail = true;
        for _ in 1..MAX_CONSECUTIVE_FAILURES {
            worker.poll_or_skip().unwrap();
        }
        assert!(worker
            .poll_or_skip()
            .unwrap_err()
            .contains("consecutive polls failed"));
        worker.memory.fail = false;
        worker.poll_or_skip().unwrap();
        worker.memory.fail = true;
        worker.poll_or_skip().expect("a success restarts the count");
    }

    #[test]
    fn a_failed_sample_publishes_nothing_and_resets_reclaim_history() {
        let fixture = Fixture::new("failed-sample");
        let mut worker = fixture.worker("session-a", 4 * GIB, 4 * GIB + 512 * MIB);
        fixture.write_status(1, 4 * GIB, None);
        worker.poll_once().unwrap();
        worker.memory.fail = true;
        assert!(worker
            .poll_once()
            .unwrap_err()
            .starts_with("memory sample failed"));
        worker.memory.fail = false;
        fixture.write_status(2, 4 * GIB, None);
        assert_eq!(
            worker.poll_once().unwrap().request,
            None,
            "history restarts after the gap"
        );
    }
}
