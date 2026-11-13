use std::time::Duration;

use virtio_mem_core::TargetSample;

use crate::demand::{
    AtomicRawTelemetryPublisher, MemoryResourceNotificationTelemetry, MemoryTelemetry,
    RawTelemetryEnvelope, RawTelemetryPublisher, TelemetryClock,
};
use crate::requests::GuestRequests;
use crate::service_host::{ServiceWorker, StopSignal};
#[cfg(windows)]
use crate::{
    config::ServiceConfig,
    demand::{
        native_memory_resource_notifications, process_session_id,
        MemoryResourceNotificationCollector, NativeMemoryTelemetry, SystemTelemetryClock,
        WindowsMemoryResourceNotificationApi,
    },
    error::RuntimeWiringError,
};

/// Production worker: samples native memory telemetry, lets the guest
/// policy decide the one add/remove request, and publishes both atomically.
/// It never resizes anything itself; the host executes requests.
#[derive(Debug)]
pub struct GuestMemoryWorker<T, P, C, N> {
    telemetry: T,
    memory_resource_notifications: N,
    publisher: P,
    diagnostics: Option<AtomicRawTelemetryPublisher>,
    clock: C,
    requests: GuestRequests,
    next_sequence: u64,
    previous_monotonic_millis: Option<u64>,
    interval: Duration,
    last_native_low: bool,
    collection_failed: bool,
    wake: crate::wake::WakeChannel,
}

impl<T, P, C, N> GuestMemoryWorker<T, P, C, N>
where
    T: MemoryTelemetry,
    P: RawTelemetryPublisher,
    C: TelemetryClock,
    N: MemoryResourceNotificationTelemetry,
{
    /// Identity comes from `requests`, which validates it.
    pub fn new(
        telemetry: T,
        memory_resource_notifications: N,
        publisher: P,
        clock: C,
        requests: GuestRequests,
        interval: Duration,
    ) -> Result<Self, String> {
        if interval.is_zero() {
            return Err("polling interval must be greater than zero".to_owned());
        }
        Ok(Self {
            telemetry,
            memory_resource_notifications,
            publisher,
            diagnostics: None,
            clock,
            requests,
            next_sequence: 0,
            previous_monotonic_millis: None,
            interval,
            last_native_low: false,
            collection_failed: false,
            wake: crate::wake::WakeChannel::default(),
        })
    }

    pub fn with_diagnostics(mut self, path: Option<&str>) -> Self {
        self.diagnostics =
            path.map(|path| AtomicRawTelemetryPublisher::new(path, Duration::from_millis(100)));
        self
    }

    pub fn poll_once(&mut self) -> Result<RawTelemetryEnvelope, String> {
        self.poll_once_with_stop(&StopSignal::new())
    }

    fn poll_once_with_stop(&mut self, stop: &StopSignal) -> Result<RawTelemetryEnvelope, String> {
        self.wake.refresh();
        self.collection_failed = true;
        let memory = self
            .telemetry
            .collect()
            .map_err(|error| format!("native memory telemetry failed: {error}"))?;
        self.collection_failed = false;
        let observed_unix_millis = self.clock.now_unix_millis()?;
        let monotonic_millis = self.clock.monotonic_millis()?;
        if self
            .previous_monotonic_millis
            .is_some_and(|previous| monotonic_millis <= previous)
        {
            return Err("raw telemetry monotonic clock did not advance".to_owned());
        }
        let requester = self.requests.requester();
        let mut envelope = RawTelemetryEnvelope::new(
            requester.vm_name(),
            requester.service_name(),
            requester.session_id(),
            observed_unix_millis,
            monotonic_millis,
            self.next_sequence,
            memory,
        );
        let native_pressure = self.memory_resource_notifications.collect();
        envelope.native_pressure = native_pressure;
        // An unreadable signal also backs off to the timer: a signalled Low
        // handle must not spin.
        self.last_native_low = native_pressure.is_none()
            || native_pressure == Some(virtio_mem_core::MemoryResourceNotificationState::Low);
        envelope.request = self
            .requests
            .poll(
                TargetSample {
                    session_id: envelope.session_id.clone(),
                    observed_unix_millis,
                    monotonic_millis,
                    sequence: envelope.sequence,
                    memory,
                    native_pressure,
                },
                stop,
            )
            .map_err(|error| format!("guest memory request failed: {error}"))?;
        envelope.diagnostics = Some(self.requests.diagnostics(self.clock.boot_uptime_millis()));
        self.publisher
            .publish(&envelope, stop)
            .map_err(|error| format!("raw telemetry publication failed: {error}"))?;
        self.wake.notify(stop);
        if let Some(publisher) = &mut self.diagnostics {
            if let Err(error) = publisher.publish(&envelope, stop) {
                // Diagnostic output is optional; its failure cannot prevent
                // admission or completion of a durable memory request.
                eprintln!("optional telemetry output failed: {error}");
            }
        }
        self.previous_monotonic_millis = Some(monotonic_millis);
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| "raw telemetry sequence exhausted".to_owned())?;
        Ok(envelope)
    }

    fn poll_or_retry(&mut self, stop: &StopSignal) -> Result<(), String> {
        match self.poll_once_with_stop(stop) {
            Ok(_) => Ok(()),
            Err(error) if self.collection_failed => {
                self.requests.invalidate_history();
                self.last_native_low = true;
                eprintln!("memory sample skipped; reclaim history reset: {error}");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn publisher(&self) -> &P {
        &self.publisher
    }
}

impl<T, P, C, N> ServiceWorker for GuestMemoryWorker<T, P, C, N>
where
    T: MemoryTelemetry + Send + 'static,
    P: RawTelemetryPublisher + Send + 'static,
    C: TelemetryClock + Send + 'static,
    N: MemoryResourceNotificationTelemetry + Send + 'static,
{
    fn initialize(&mut self, stop: &StopSignal) -> Result<(), String> {
        if stop.is_cancelled() {
            return Ok(());
        }
        self.poll_or_retry(stop)
    }

    fn run(&mut self, stop: &StopSignal) -> Result<(), String> {
        while !stop.is_cancelled() {
            if !self.requests.resolved_last_poll() {
                self.memory_resource_notifications.wait(
                    stop,
                    self.interval,
                    !self.last_native_low,
                    self.wake.event(),
                );
            }
            // A level transition can wake within the preceding millisecond.
            // Preserve the strict timestamp ordering without busy-looping.
            stop.wait(Duration::from_millis(1));
            if !stop.is_cancelled() {
                self.poll_or_retry(stop)?;
            }
        }
        Ok(())
    }
}

/// The worker the service runs; console and SCM start-up both build it here.
#[cfg(windows)]
pub type NativeWorker = GuestMemoryWorker<
    NativeMemoryTelemetry,
    AtomicRawTelemetryPublisher,
    SystemTelemetryClock,
    MemoryResourceNotificationCollector<WindowsMemoryResourceNotificationApi>,
>;

#[cfg(windows)]
pub fn native_worker(config: &ServiceConfig) -> Result<NativeWorker, RuntimeWiringError> {
    let session_id = process_session_id().map_err(RuntimeWiringError::WorkerConstruction)?;
    let mut requests = GuestRequests::open(
        config.policy,
        &config.vm_name,
        &config.service_name,
        &session_id,
        &config.demand_report_path,
        config.shutdown_timeout(),
    )
    .map_err(RuntimeWiringError::WorkerConstruction)?;
    requests
        .set_ram_limits(config.min_ram, config.max_ram)
        .map_err(RuntimeWiringError::WorkerConstruction)?;
    GuestMemoryWorker::new(
        NativeMemoryTelemetry,
        native_memory_resource_notifications(),
        AtomicRawTelemetryPublisher::new(&config.demand_report_path, config.shutdown_timeout()),
        SystemTelemetryClock::default(),
        requests,
        config.poll_interval(),
    )
    .map(|mut worker| {
        worker.wake = crate::wake::WakeChannel::enabled(&config.service_name);
        worker.with_diagnostics(config.telemetry_path.as_deref())
    })
    .map_err(RuntimeWiringError::WorkerConstruction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demand::{
        DemandError, MemoryResourceNotificationState, MemoryTelemetrySnapshot,
        UnavailableMemoryResourceNotifications,
    };
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use virtio_mem_core::{HostMemoryStatus, MemoryOperation, MEMORY_REQUEST_VERSION};

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[derive(Clone)]
    struct TelemetryFixture;

    impl MemoryTelemetry for TelemetryFixture {
        fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError> {
            Ok(MemoryTelemetrySnapshot {
                physical_total_bytes: 16 * GIB,
                physical_available_bytes: 2 * GIB,
                memory_load_percent: 88,
                commit_total_bytes: 15 * GIB,
                commit_limit_bytes: 16 * GIB,
                commit_peak_bytes: 15 * GIB,
                system_cache_bytes: 0,
                kernel_paged_bytes: 0,
                kernel_nonpaged_bytes: 0,
            })
        }
    }

    struct FixedNotifications {
        state: MemoryResourceNotificationState,
    }

    impl MemoryResourceNotificationTelemetry for FixedNotifications {
        fn collect(&self) -> Option<MemoryResourceNotificationState> {
            Some(self.state)
        }
    }

    struct DropTrackedNotifications(Arc<AtomicBool>);

    impl MemoryResourceNotificationTelemetry for DropTrackedNotifications {
        fn collect(&self) -> Option<MemoryResourceNotificationState> {
            Some(MemoryResourceNotificationState::Neutral)
        }
    }

    impl Drop for DropTrackedNotifications {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// Advances one second per sample.
    #[derive(Debug, Default)]
    struct StepClock(Cell<u64>);

    impl TelemetryClock for StepClock {
        fn now_unix_millis(&self) -> Result<u64, String> {
            Ok(1_000_000 + self.0.get() * 1000)
        }

        fn monotonic_millis(&self) -> Result<u64, String> {
            self.0.set(self.0.get() + 1);
            Ok(self.0.get() * 1000)
        }
    }

    #[derive(Default, Debug)]
    struct PublisherFixture(Vec<RawTelemetryEnvelope>);

    impl RawTelemetryPublisher for PublisherFixture {
        fn publish(
            &mut self,
            envelope: &RawTelemetryEnvelope,
            stop: &StopSignal,
        ) -> Result<(), String> {
            if !stop.is_cancelled() {
                self.0.push(envelope.clone());
            }
            Ok(())
        }
    }

    fn publication(name: &str) -> String {
        let path: PathBuf = std::env::temp_dir().join(format!(
            "virtio-mem-worker-{name}-{}.json",
            std::process::id()
        ));
        let path = path.display().to_string();
        for file in [
            virtio_mem_core::host_status_path(&path),
            virtio_mem_core::request_state_path(&path),
        ] {
            // A previous run may have left fixtures behind.
            let _ = std::fs::remove_file(file);
        }
        path
    }

    fn requests(publication: &str, session: &str) -> GuestRequests {
        GuestRequests::open(
            crate::config::test_config().policy,
            "guest",
            "TelemetryService",
            session,
            publication,
            Duration::from_secs(1),
        )
        .expect("request state")
    }

    fn worker<N: MemoryResourceNotificationTelemetry>(
        notifications: N,
        requests: GuestRequests,
    ) -> GuestMemoryWorker<TelemetryFixture, PublisherFixture, StepClock, N> {
        GuestMemoryWorker::new(
            TelemetryFixture,
            notifications,
            PublisherFixture::default(),
            StepClock::default(),
            requests,
            Duration::from_secs(1),
        )
        .expect("worker should be valid")
    }

    #[test]
    fn optional_diagnostics_do_not_control_request_publication() {
        let path = publication("optional-diagnostics");
        let mut worker = worker(
            UnavailableMemoryResourceNotifications,
            requests(&path, "session"),
        );
        assert!(worker.diagnostics.is_none());
        worker
            .poll_once()
            .expect("control publication without diagnostics");
        assert_eq!(worker.publisher().0.len(), 1);
        let blocked =
            std::env::temp_dir().join(format!("virtio-mem-diagnostic-file-{}", std::process::id()));
        std::fs::write(&blocked, "not a directory").unwrap();
        let output = blocked.join("diagnostic.json");
        let mut worker = worker.with_diagnostics(output.to_str());
        worker.poll_once().expect("diagnostic failure is nonfatal");
        assert_eq!(worker.publisher().0.len(), 2);
        std::fs::remove_file(blocked).unwrap();
    }

    #[test]
    fn cancelled_initialization_does_not_sample_or_publish() {
        let path = publication("cancelled");
        let mut worker = worker(
            UnavailableMemoryResourceNotifications,
            requests(&path, "session"),
        );
        let stop = StopSignal::new();
        stop.cancel();
        worker.initialize(&stop).unwrap();
        assert!(worker.publisher().0.is_empty());
    }

    #[test]
    fn without_host_status_the_worker_publishes_telemetry_and_no_request() {
        let path = publication("no-status");
        let mut worker = worker(
            UnavailableMemoryResourceNotifications,
            requests(&path, "session-a"),
        );
        let envelope = worker.poll_once().expect("publication should pass");
        assert_eq!(envelope.vm_name, "guest");
        assert_eq!(envelope.sequence, 0);
        assert_eq!(envelope.request, None);
        assert_eq!(envelope.native_pressure, None);
        assert_eq!(worker.publisher().0, vec![envelope]);
    }

    #[test]
    fn a_request_is_durable_before_publication_and_survives_restart() {
        let path = publication("request");
        // The fixture leaves 1 GiB less commit headroom than the reserve.
        let status = HostMemoryStatus {
            version: MEMORY_REQUEST_VERSION,
            vm_name: "guest".to_owned(),
            service_name: "TelemetryService".to_owned(),
            device_alias: "memory0".to_owned(),
            observed_unix_millis: 1_000_000,
            device_size_bytes: 32 * GIB,
            block_size_bytes: 2 * MIB,
            requested_bytes: 4 * GIB,
            current_bytes: 4 * GIB,
            result: None,
        };
        let mut bytes = serde_json::to_vec(&status).unwrap();
        bytes.push(b'\n');
        std::fs::write(virtio_mem_core::host_status_path(&path), bytes).unwrap();

        let mut worker = worker(
            UnavailableMemoryResourceNotifications,
            requests(&path, "session-a"),
        );
        let request = worker
            .poll_once()
            .unwrap()
            .request
            .expect("pressure issues a request");
        assert_eq!(request.operation, MemoryOperation::Add);
        assert_eq!(request.bytes, GIB);
        let retained: virtio_mem_core::GuestRequestState = serde_json::from_slice(
            &std::fs::read(virtio_mem_core::request_state_path(&path)).unwrap(),
        )
        .unwrap();
        assert_eq!(retained.pending.as_ref(), Some(&request));

        let mut restarted = worker_after_restart(&path);
        assert_eq!(restarted.poll_once().unwrap().request, Some(request));
    }

    fn worker_after_restart(
        path: &str,
    ) -> GuestMemoryWorker<
        TelemetryFixture,
        PublisherFixture,
        StepClock,
        UnavailableMemoryResourceNotifications,
    > {
        worker(
            UnavailableMemoryResourceNotifications,
            requests(path, "session-b"),
        )
    }

    #[test]
    fn publishes_native_notification_state_uninterpreted() {
        let path = publication("notifications");
        let mut worker = worker(
            FixedNotifications {
                state: MemoryResourceNotificationState::Low,
            },
            requests(&path, "session-a"),
        );
        let envelope = worker.poll_once().expect("publication should pass");
        assert_eq!(
            envelope.native_pressure,
            Some(MemoryResourceNotificationState::Low)
        );
    }

    #[cfg(windows)]
    #[test]
    fn publishes_a_real_native_notification_observation() {
        let path = publication("native-notifications");
        let mut worker = worker(
            crate::demand::native_memory_resource_notifications(),
            requests(&path, "session-a"),
        );
        let envelope = worker
            .poll_once()
            .expect("native observation should publish");
        assert!(envelope.native_pressure.is_some());
        assert_eq!(worker.publisher().0, vec![envelope]);
    }

    #[test]
    fn cancellation_drops_notification_ownership_without_an_extra_poll() {
        let path = publication("drop");
        let dropped = Arc::new(AtomicBool::new(false));
        let mut worker = worker(
            DropTrackedNotifications(dropped.clone()),
            requests(&path, "session-a"),
        );
        let stop = StopSignal::new();
        stop.cancel();

        assert_eq!(worker.run(&stop), Ok(()));
        assert!(worker.publisher().0.is_empty());
        drop(worker);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn rejects_zero_interval_and_invalid_identity() {
        let path = publication("invalid");
        assert!(GuestMemoryWorker::new(
            TelemetryFixture,
            UnavailableMemoryResourceNotifications,
            PublisherFixture::default(),
            StepClock::default(),
            requests(&path, "session-a"),
            Duration::ZERO,
        )
        .is_err());
        assert!(GuestRequests::open(
            crate::config::test_config().policy,
            "",
            "TelemetryService",
            "session-a",
            &path,
            Duration::from_secs(1),
        )
        .is_err());
    }
    struct FailingOnceTelemetry(Cell<bool>);
    impl MemoryTelemetry for FailingOnceTelemetry {
        fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError> {
            if self.0.replace(false) {
                Err(DemandError::PerformanceInfo(5))
            } else {
                TelemetryFixture.collect()
            }
        }
    }

    #[test]
    fn transient_numeric_failure_skips_publication_and_recovers_without_restart() {
        let path = publication("retry-numeric");
        let mut worker = GuestMemoryWorker::new(
            FailingOnceTelemetry(Cell::new(true)),
            UnavailableMemoryResourceNotifications,
            PublisherFixture::default(),
            StepClock::default(),
            requests(&path, "retry"),
            Duration::from_millis(1),
        )
        .unwrap();
        worker.initialize(&StopSignal::new()).unwrap();
        assert!(worker.publisher().0.is_empty());
        worker.poll_or_retry(&StopSignal::new()).unwrap();
        assert_eq!(worker.publisher().0.len(), 1);
        assert_eq!(worker.publisher().0[0].sequence, 0);
    }

    struct WaitTrackingNotifications(Vec<bool>);
    impl MemoryResourceNotificationTelemetry for WaitTrackingNotifications {
        fn collect(&self) -> Option<MemoryResourceNotificationState> {
            Some(MemoryResourceNotificationState::Low)
        }
        fn wait(
            &mut self,
            stop: &StopSignal,
            _timeout: Duration,
            watch_low: bool,
            _wake: Option<usize>,
        ) {
            self.0.push(watch_low);
            if self.0.len() == 2 {
                stop.cancel();
            }
        }
    }

    #[test]
    fn sustained_low_backs_off_instead_of_rewaiting_on_a_signalled_object() {
        let path = publication("low-backoff");
        let mut worker = worker(
            WaitTrackingNotifications(Vec::new()),
            requests(&path, "backoff"),
        );
        let stop = StopSignal::new();
        worker.initialize(&stop).unwrap();
        worker.run(&stop).unwrap();
        assert_eq!(worker.memory_resource_notifications.0, vec![false, false]);
        assert_eq!(worker.publisher().0.len(), 2);
    }
    struct AmpleTelemetry;
    impl MemoryTelemetry for AmpleTelemetry {
        fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError> {
            let mut sample = TelemetryFixture.collect()?;
            sample.physical_available_bytes = 8 * GIB;
            sample.commit_total_bytes = GIB;
            Ok(sample)
        }
    }

    #[test]
    fn native_notification_reaches_the_real_request_and_durable_publication_path() {
        for native in [
            MemoryResourceNotificationState::High,
            MemoryResourceNotificationState::Low,
        ] {
            let path = publication(&format!("native-policy-{native:?}"));
            let status = HostMemoryStatus {
                version: MEMORY_REQUEST_VERSION,
                vm_name: "guest".to_owned(),
                service_name: "TelemetryService".to_owned(),
                device_alias: "memory0".to_owned(),
                observed_unix_millis: 1_000_000,
                device_size_bytes: 32 * GIB,
                block_size_bytes: 2 * MIB,
                requested_bytes: 4 * GIB,
                current_bytes: 4 * GIB,
                result: None,
            };
            let mut bytes = serde_json::to_vec(&status).unwrap();
            bytes.push(b'\n');
            std::fs::write(virtio_mem_core::host_status_path(&path), bytes).unwrap();
            let mut worker = GuestMemoryWorker::new(
                AmpleTelemetry,
                FixedNotifications { state: native },
                PublisherFixture::default(),
                StepClock::default(),
                requests(&path, "native"),
                Duration::from_secs(1),
            )
            .unwrap();
            let envelope = worker.poll_once().unwrap();
            if native == MemoryResourceNotificationState::Low {
                let request = envelope.request.unwrap();
                assert_eq!(request.operation, MemoryOperation::Add);
                assert_eq!(request.bytes, GIB);
                let state: virtio_mem_core::GuestRequestState = serde_json::from_slice(
                    &std::fs::read(virtio_mem_core::request_state_path(&path)).unwrap(),
                )
                .unwrap();
                assert_eq!(state.pending, Some(request));
            } else {
                assert!(envelope.request.is_none());
            }
            let _ = std::fs::remove_file(virtio_mem_core::host_status_path(&path));
            let _ = std::fs::remove_file(virtio_mem_core::request_state_path(&path));
        }
    }
}
