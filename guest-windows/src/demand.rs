use crate::service_host::StopSignal;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use virtio_mem_core::{
    DemandError, MemoryResourceNotificationState, MemoryTelemetrySnapshot, RawTelemetryEnvelope,
};

pub trait TelemetryClock {
    fn now_unix_millis(&self) -> Result<u64, String>;
    fn monotonic_millis(&self) -> Result<u64, String>;
    fn boot_uptime_millis(&self) -> Option<u64> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct SystemTelemetryClock {
    started_at: Instant,
}

impl Default for SystemTelemetryClock {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl TelemetryClock for SystemTelemetryClock {
    fn boot_uptime_millis(&self) -> Option<u64> {
        #[cfg(windows)]
        {
            // SAFETY: GetTickCount64 takes no pointers or handles.
            Some(unsafe { winapi::um::sysinfoapi::GetTickCount64() })
        }
        #[cfg(not(windows))]
        None
    }

    fn now_unix_millis(&self) -> Result<u64, String> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
        u64::try_from(duration.as_millis())
            .map_err(|_| "system Unix timestamp does not fit in u64 milliseconds".to_owned())
    }

    fn monotonic_millis(&self) -> Result<u64, String> {
        u64::try_from(self.started_at.elapsed().as_millis())
            .map_err(|_| "process monotonic timestamp does not fit in u64 milliseconds".to_owned())
    }
}

/// A per-process identity for this service run's publications.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryResourceNotificationKind {
    Low,
    High,
}

/// Injectable boundary around the paired Windows memory-resource handles.
pub trait MemoryResourceNotificationApi {
    type Handle: Copy;

    fn create(&self, kind: MemoryResourceNotificationKind) -> Result<Self::Handle, u32>;
    fn query(&self, handle: Self::Handle) -> Result<bool, u32>;
    fn close(&self, handle: Self::Handle);

    /// True means the low-memory object woke the worker. Default is a timer.
    fn wait_for_pressure(
        &self,
        _handle: Option<Self::Handle>,
        stop: &StopSignal,
        timeout: Duration,
        _wake: Option<usize>,
    ) -> Result<bool, u32> {
        stop.wait(timeout);
        Ok(false)
    }
}

pub trait MemoryResourceNotificationTelemetry {
    /// Remain timer-driven once Low is observed, since notifications are level-triggered.
    fn wait(
        &mut self,
        stop: &StopSignal,
        timeout: Duration,
        _watch_low: bool,
        _wake: Option<usize>,
    ) {
        stop.wait(timeout);
    }

    /// Windows' categorical state, or `None` when it cannot be read.
    fn collect(&self) -> Option<MemoryResourceNotificationState>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableMemoryResourceNotifications;

impl MemoryResourceNotificationTelemetry for UnavailableMemoryResourceNotifications {
    fn collect(&self) -> Option<MemoryResourceNotificationState> {
        None
    }
}

/// Owns both notification handles for one service worker lifetime.
#[derive(Debug)]
pub struct MemoryResourceNotificationCollector<A: MemoryResourceNotificationApi> {
    api: A,
    /// The low and high handles; `None` until both could be created.
    handles: Option<(A::Handle, A::Handle)>,
}

impl<A: MemoryResourceNotificationApi> MemoryResourceNotificationCollector<A> {
    pub fn new(api: A) -> Self {
        let handles = create_handles(&api);
        Self { api, handles }
    }
}

fn create_handles<A: MemoryResourceNotificationApi>(api: &A) -> Option<(A::Handle, A::Handle)> {
    let low = api.create(MemoryResourceNotificationKind::Low).ok()?;
    match api.create(MemoryResourceNotificationKind::High) {
        Ok(high) => Some((low, high)),
        Err(_) => {
            api.close(low);
            None
        }
    }
}

impl<A: MemoryResourceNotificationApi> MemoryResourceNotificationTelemetry
    for MemoryResourceNotificationCollector<A>
{
    fn wait(&mut self, stop: &StopSignal, timeout: Duration, watch_low: bool, wake: Option<usize>) {
        if stop.is_cancelled() {
            return;
        }
        // A missing optional facility must not permanently disable observation.
        if self.handles.is_none() {
            self.handles = create_handles(&self.api);
        }
        let low = self.handles.filter(|_| watch_low).map(|(low, _)| low);
        if let Err(error) = self.api.wait_for_pressure(low, stop, timeout, wake) {
            eprintln!("native memory notification wait failed: {error}");
            stop.wait(timeout);
        }
    }

    fn collect(&self) -> Option<MemoryResourceNotificationState> {
        let (low, high) = self.handles?;
        match (self.api.query(low).ok()?, self.api.query(high).ok()?) {
            (true, false) => Some(MemoryResourceNotificationState::Low),
            (false, true) => Some(MemoryResourceNotificationState::High),
            (false, false) => Some(MemoryResourceNotificationState::Neutral),
            // Both signalled contradicts the API; report nothing.
            (true, true) => None,
        }
    }
}

impl<A: MemoryResourceNotificationApi> Drop for MemoryResourceNotificationCollector<A> {
    fn drop(&mut self) {
        if let Some((low, high)) = self.handles {
            self.api.close(low);
            self.api.close(high);
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsMemoryResourceNotificationApi;

#[cfg(windows)]
impl MemoryResourceNotificationApi for WindowsMemoryResourceNotificationApi {
    type Handle = usize;

    fn create(&self, kind: MemoryResourceNotificationKind) -> Result<Self::Handle, u32> {
        use winapi::um::memoryapi::{
            CreateMemoryResourceNotification, HighMemoryResourceNotification,
            LowMemoryResourceNotification,
        };

        let notification_type = match kind {
            MemoryResourceNotificationKind::Low => LowMemoryResourceNotification,
            MemoryResourceNotificationKind::High => HighMemoryResourceNotification,
        };
        // SAFETY: The API receives a valid documented enum and returns an owned
        // handle or null. The collector closes every successfully created handle.
        let handle = unsafe { CreateMemoryResourceNotification(notification_type) };
        if handle.is_null() {
            Err(last_windows_error_code())
        } else {
            Ok(handle as usize)
        }
    }

    fn query(&self, handle: Self::Handle) -> Result<bool, u32> {
        use winapi::ctypes::c_void;
        use winapi::shared::minwindef::FALSE;
        use winapi::um::memoryapi::QueryMemoryResourceNotification;

        let mut signaled = FALSE;
        // SAFETY: `handle` was returned by the create call and remains owned by
        // the collector; the output pointer is valid for the duration of the call.
        let result =
            unsafe { QueryMemoryResourceNotification(handle as *mut c_void, &mut signaled) };
        if result == FALSE {
            Err(last_windows_error_code())
        } else {
            Ok(signaled != FALSE)
        }
    }

    fn wait_for_pressure(
        &self,
        low: Option<Self::Handle>,
        stop: &StopSignal,
        timeout: Duration,
        wake: Option<usize>,
    ) -> Result<bool, u32> {
        use winapi::shared::ntdef::HANDLE;
        use winapi::shared::winerror::WAIT_TIMEOUT;
        use winapi::um::synchapi::WaitForMultipleObjects;
        use winapi::um::winbase::WAIT_OBJECT_0;
        let mut handles = Vec::new();
        if let Some(event) = stop.native_event() {
            handles.push(event as HANDLE);
        }
        if let Some(event) = low {
            handles.push(event as HANDLE);
        }
        if let Some(event) = wake {
            handles.push(event as HANDLE);
        }
        if handles.is_empty() {
            stop.wait(timeout);
            return Ok(false);
        }
        let started = Instant::now();
        while !stop.is_cancelled() && started.elapsed() < timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            let bound = if stop.native_event().is_some() {
                remaining
            } else {
                remaining.min(Duration::from_millis(50))
            };
            let millis = bound.as_millis().max(1).min(u128::from(u32::MAX - 1)) as u32;
            // SAFETY: each handle is owned by the collector, stop signal or
            // wake channel, all borrowed for this wait and alive until it ends.
            let result = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, millis)
            };
            if result < WAIT_OBJECT_0 + handles.len() as u32 {
                return Ok(!stop.is_cancelled());
            }
            if result != WAIT_TIMEOUT {
                return Err(last_windows_error_code());
            }
        }
        Ok(false)
    }

    fn close(&self, handle: Self::Handle) {
        use winapi::ctypes::c_void;
        use winapi::um::handleapi::CloseHandle;

        // SAFETY: The collector calls this exactly once for each owned handle.
        let _ = unsafe { CloseHandle(handle as *mut c_void) };
    }
}

#[cfg(windows)]
fn last_windows_error_code() -> u32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default() as u32
}

#[cfg(windows)]
pub fn native_memory_resource_notifications(
) -> MemoryResourceNotificationCollector<WindowsMemoryResourceNotificationApi> {
    MemoryResourceNotificationCollector::new(WindowsMemoryResourceNotificationApi)
}

/// Collects native Windows memory counters using the documented system APIs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeMemoryTelemetry;

fn pages_to_bytes_checked(pages: u64, page_size: u64) -> Result<u64, DemandError> {
    pages
        .checked_mul(page_size)
        .ok_or(DemandError::ArithmeticOverflow)
}

pub trait MemoryTelemetry {
    fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError>;
}

impl MemoryTelemetry for NativeMemoryTelemetry {
    #[cfg(windows)]
    fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError> {
        use std::mem::{size_of, zeroed};
        use winapi::shared::minwindef::FALSE;
        use winapi::um::psapi::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

        // SAFETY: The Windows API initializes the supplied structure and only
        // writes within its declared size.
        let performance = unsafe {
            let mut value: PERFORMANCE_INFORMATION = zeroed();
            if GetPerformanceInfo(&mut value, size_of::<PERFORMANCE_INFORMATION>() as u32) == FALSE
            {
                return Err(DemandError::PerformanceInfo(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or_default() as u32,
                ));
            }
            value
        };
        let page_size = performance.PageSize as u64;
        if page_size == 0 {
            return Err(DemandError::ZeroCounter("page size"));
        }
        let pages_to_bytes = |pages: usize| pages_to_bytes_checked(pages as u64, page_size);
        let total = pages_to_bytes(performance.PhysicalTotal)?;
        let available = pages_to_bytes(performance.PhysicalAvailable)?;
        if total == 0 || available > total {
            return Err(DemandError::InconsistentCounters(
                "invalid physical memory snapshot",
            ));
        }

        let snapshot = MemoryTelemetrySnapshot {
            physical_total_bytes: total,
            physical_available_bytes: available,
            memory_load_percent: ((u128::from(total - available) * 100) / u128::from(total)) as u32,
            commit_total_bytes: pages_to_bytes(performance.CommitTotal)?,
            commit_limit_bytes: pages_to_bytes(performance.CommitLimit)?,
            commit_peak_bytes: pages_to_bytes(performance.CommitPeak)?,
            system_cache_bytes: pages_to_bytes(performance.SystemCache)?,
            kernel_paged_bytes: pages_to_bytes(performance.KernelPaged)?,
            kernel_nonpaged_bytes: pages_to_bytes(performance.KernelNonpaged)?,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    #[cfg(not(windows))]
    fn collect(&self) -> Result<MemoryTelemetrySnapshot, DemandError> {
        Err(DemandError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn snapshot(available: u64, commit: u64) -> MemoryTelemetrySnapshot {
        MemoryTelemetrySnapshot {
            physical_total_bytes: 16 * GIB,
            physical_available_bytes: available,
            memory_load_percent: 50,
            commit_total_bytes: commit,
            commit_limit_bytes: 16 * GIB,
            commit_peak_bytes: commit,
            system_cache_bytes: 0,
            kernel_paged_bytes: 0,
            kernel_nonpaged_bytes: 0,
        }
    }

    #[derive(Debug, Default)]
    struct NotificationFixtureState {
        creates: VecDeque<Result<u8, u32>>,
        low_queries: VecDeque<Result<bool, u32>>,
        high_queries: VecDeque<Result<bool, u32>>,
        closed: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    struct NotificationFixtureApi(Arc<Mutex<NotificationFixtureState>>);

    impl MemoryResourceNotificationApi for NotificationFixtureApi {
        type Handle = u8;

        fn create(&self, _kind: MemoryResourceNotificationKind) -> Result<Self::Handle, u32> {
            self.0
                .lock()
                .expect("notification fixture lock")
                .creates
                .pop_front()
                .expect("scripted create result")
        }

        fn query(&self, handle: Self::Handle) -> Result<bool, u32> {
            let mut state = self.0.lock().expect("notification fixture lock");
            let results = if handle == 1 {
                &mut state.low_queries
            } else {
                &mut state.high_queries
            };
            results.pop_front().expect("scripted query result")
        }

        fn close(&self, handle: Self::Handle) {
            self.0
                .lock()
                .expect("notification fixture lock")
                .closed
                .push(handle);
        }
    }

    fn notification_fixture(
        state: NotificationFixtureState,
    ) -> (NotificationFixtureApi, Arc<Mutex<NotificationFixtureState>>) {
        let state = Arc::new(Mutex::new(state));
        (NotificationFixtureApi(state.clone()), state)
    }

    #[test]
    fn notification_collector_reports_neutral_low_and_high() {
        let (api, state) = notification_fixture(NotificationFixtureState {
            creates: VecDeque::from([Ok(1), Ok(2)]),
            low_queries: VecDeque::from([Ok(false), Ok(true), Ok(false)]),
            high_queries: VecDeque::from([Ok(false), Ok(false), Ok(true)]),
            ..NotificationFixtureState::default()
        });
        let collector = MemoryResourceNotificationCollector::new(api);

        for expected in [
            MemoryResourceNotificationState::Neutral,
            MemoryResourceNotificationState::Low,
            MemoryResourceNotificationState::High,
        ] {
            assert_eq!(collector.collect(), Some(expected));
        }
        drop(collector);
        assert_eq!(
            state.lock().expect("notification fixture lock").closed,
            vec![1, 2]
        );
    }

    #[test]
    fn notification_creation_failure_is_reported_and_cleans_partial_state() {
        let (api, state) = notification_fixture(NotificationFixtureState {
            creates: VecDeque::from([Ok(1), Err(5)]),
            ..NotificationFixtureState::default()
        });
        let collector = MemoryResourceNotificationCollector::new(api);

        assert_eq!(collector.collect(), None);
        drop(collector);
        assert_eq!(
            state.lock().expect("notification fixture lock").closed,
            vec![1]
        );
    }

    #[test]
    fn notification_query_failure_and_contradiction_fail_closed_then_recover() {
        let (api, _state) = notification_fixture(NotificationFixtureState {
            creates: VecDeque::from([Ok(1), Ok(2)]),
            low_queries: VecDeque::from([Err(0), Ok(true), Ok(false)]),
            high_queries: VecDeque::from([Ok(true), Ok(true)]),
            ..NotificationFixtureState::default()
        });
        let collector = MemoryResourceNotificationCollector::new(api);

        assert_eq!(collector.collect(), None);
        assert_eq!(collector.collect(), None);
        assert_eq!(
            collector.collect(),
            Some(MemoryResourceNotificationState::High)
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_notification_api_reports_a_supported_categorical_state() {
        let collector = native_memory_resource_notifications();
        assert!(matches!(
            collector.collect(),
            Some(
                MemoryResourceNotificationState::Low
                    | MemoryResourceNotificationState::Neutral
                    | MemoryResourceNotificationState::High
            )
        ));
    }

    #[test]
    fn rejects_invalid_counters() {
        let mut value = snapshot(8 * GIB, 4 * GIB);
        value.physical_available_bytes = 17 * GIB;
        assert_eq!(
            value.validate(),
            Err(DemandError::InconsistentCounters(
                "physical available exceeds physical total"
            ))
        );

        value = snapshot(8 * GIB, 4 * GIB);
        value.commit_limit_bytes = 0;
        assert_eq!(
            value.validate(),
            Err(DemandError::ZeroCounter("commit limit"))
        );
    }

    #[test]
    fn native_collector_has_platform_behavior() {
        #[cfg(not(windows))]
        assert_eq!(
            NativeMemoryTelemetry.collect(),
            Err(DemandError::UnsupportedPlatform)
        );
    }

    #[test]
    fn checked_page_conversion_rejects_overflow() {
        assert_eq!(pages_to_bytes_checked(3, 4096).unwrap(), 12288);
        assert!(pages_to_bytes_checked(u64::MAX, 4096).is_err());
    }

    #[test]
    fn failed_notification_creation_is_retried_on_a_bounded_wait() {
        let (api, state) = notification_fixture(NotificationFixtureState {
            creates: VecDeque::from([Err(5), Ok(1), Ok(2)]),
            low_queries: VecDeque::from([Ok(false)]),
            high_queries: VecDeque::from([Ok(true)]),
            ..NotificationFixtureState::default()
        });
        let mut collector = MemoryResourceNotificationCollector::new(api);
        assert_eq!(collector.collect(), None);
        collector.wait(&StopSignal::new(), Duration::ZERO, false, None);
        assert_eq!(
            collector.collect(),
            Some(MemoryResourceNotificationState::High)
        );
        drop(collector);
        assert_eq!(state.lock().unwrap().closed, vec![1, 2]);
    }

    #[cfg(windows)]
    #[test]
    fn result_notification_wakes_even_when_low_is_not_watched() {
        let api = WindowsMemoryResourceNotificationApi;
        // SAFETY: private signalled event, owned until the bounded wait ends.
        let handle = unsafe {
            winapi::um::synchapi::CreateEventW(std::ptr::null_mut(), 1, 1, std::ptr::null())
        };
        assert!(!handle.is_null());
        let start = Instant::now();
        assert!(api
            .wait_for_pressure(
                None,
                &StopSignal::new(),
                Duration::from_secs(10),
                Some(handle as usize)
            )
            .unwrap());
        api.close(handle as usize);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(windows)]
    #[test]
    fn native_memory_wait_is_cancelled_promptly() {
        let stop = StopSignal::new();
        let other = stop.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            other.cancel();
        });
        let api = WindowsMemoryResourceNotificationApi;
        // A private nonsignalled event makes this test independent of the
        // machine's actual memory pressure. The wait uses the same kernel path.
        let handle = unsafe {
            winapi::um::synchapi::CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null())
        };
        assert!(!handle.is_null());
        let handle = handle as usize;
        let start = Instant::now();
        api.wait_for_pressure(Some(handle), &stop, Duration::from_secs(10), None)
            .unwrap();
        api.close(handle);
        thread.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
