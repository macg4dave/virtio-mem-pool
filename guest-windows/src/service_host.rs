use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::error::ServiceHostError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ServiceState {
    Created,
    StartPending,
    Running,
    StopPending,
    Stopped,
    Failed,
}

impl ServiceState {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Created,
            1 => Self::StartPending,
            2 => Self::Running,
            3 => Self::StopPending,
            4 => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

/// The cancelled flag lives inside the mutex the condition variable waits on.
/// Setting it under that lock means a waiter either sees it before sleeping or
/// is already asleep when the notification arrives; a stop cannot be missed.
struct StopState {
    cancelled: Mutex<bool>,
    wake: Condvar,
    /// Run once on cancellation, so waiters can block on their own channel.
    on_cancel: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(windows)]
    native_event: Option<usize>,
}

impl std::fmt::Debug for StopState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StopState")
            .field("cancelled", &self.cancelled)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct StopSignal {
    state: Arc<StopState>,
}

impl StopSignal {
    pub fn new() -> Self {
        Self {
            state: Arc::new(StopState {
                cancelled: Mutex::new(false),
                wake: Condvar::new(),
                on_cancel: Mutex::new(Vec::new()),
                #[cfg(windows)]
                native_event: {
                    // Manual reset: cancellation stays signalled for all waiters.
                    let handle = unsafe {
                        winapi::um::synchapi::CreateEventW(
                            std::ptr::null_mut(),
                            1,
                            0,
                            std::ptr::null(),
                        )
                    };
                    (!handle.is_null()).then_some(handle as usize)
                },
            }),
        }
    }

    pub fn cancel(&self) {
        *self.cancelled() = true;
        self.state.wake.notify_all();
        for callback in self.callbacks().drain(..) {
            callback();
        }
        #[cfg(windows)]
        if let Some(handle) = self.state.native_event {
            unsafe {
                winapi::um::synchapi::SetEvent(handle as winapi::shared::ntdef::HANDLE);
            }
        }
    }

    /// Run `callback` once when cancelled, or now if already cancelled.
    pub fn on_cancel(&self, callback: impl Fn() + Send + Sync + 'static) {
        let mut callbacks = self.callbacks();
        if self.is_cancelled() {
            drop(callbacks);
            callback();
        } else {
            callbacks.push(Box::new(callback));
        }
    }

    fn callbacks(&self) -> MutexGuard<'_, Vec<Box<dyn Fn() + Send + Sync>>> {
        self.state
            .on_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(windows)]
    pub(crate) fn native_event(&self) -> Option<usize> {
        self.state.native_event
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancelled()
    }

    /// Sleep for `timeout` or until cancelled, whichever comes first.
    pub fn wait(&self, timeout: Duration) {
        // The timeout flag is not needed: callers re-check `is_cancelled`.
        // Poisoning only means another thread panicked while holding a bool,
        // which cannot leave it inconsistent.
        drop(
            self.state
                .wake
                .wait_timeout_while(self.cancelled(), timeout, |cancelled| !*cancelled)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    fn cancelled(&self) -> MutexGuard<'_, bool> {
        // See `wait`: a poisoned bool is still meaningful.
        self.state
            .cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(windows)]
impl Drop for StopState {
    fn drop(&mut self) {
        if let Some(handle) = self.native_event {
            unsafe {
                winapi::um::handleapi::CloseHandle(handle as winapi::shared::ntdef::HANDLE);
            }
        }
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

pub trait ServiceWorker {
    fn initialize(&mut self, _stop: &StopSignal) -> Result<(), String> {
        Ok(())
    }

    fn run(&mut self, stop: &StopSignal) -> Result<(), String>;
}

impl<F> ServiceWorker for F
where
    F: FnMut(&StopSignal) -> Result<(), String>,
{
    fn run(&mut self, stop: &StopSignal) -> Result<(), String> {
        self(stop)
    }
}

pub struct ServiceHost<W> {
    worker: Option<W>,
    stop: StopSignal,
    state: AtomicU8,
    shutdown_timeout: Duration,
}

impl<W> ServiceHost<W>
where
    W: ServiceWorker + Send + 'static,
{
    pub fn with_shutdown_timeout(worker: W, shutdown_timeout: Duration) -> Self {
        Self::with_stop_and_shutdown_timeout(worker, StopSignal::new(), shutdown_timeout)
    }

    pub fn with_stop_and_shutdown_timeout(
        worker: W,
        stop: StopSignal,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            worker: Some(worker),
            stop,
            state: AtomicU8::new(ServiceState::Created as u8),
            shutdown_timeout,
        }
    }

    pub fn state(&self) -> ServiceState {
        ServiceState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn request_stop(&self) {
        let _ = self.state.compare_exchange(
            ServiceState::Running as u8,
            ServiceState::StopPending as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.stop.cancel();
    }

    pub fn run(&mut self) -> Result<(), ServiceHostError> {
        self.run_with_status(|_| Ok(()))
    }

    /// The hosting thread owns all status transitions. The worker reports
    /// completion through one channel; it never impersonates an operator stop.
    pub fn run_with_status(
        &mut self,
        mut publish: impl FnMut(ServiceState) -> Result<(), String>,
    ) -> Result<(), ServiceHostError> {
        match self.state() {
            ServiceState::Created => {}
            ServiceState::StartPending | ServiceState::Running | ServiceState::StopPending => {
                return Err(ServiceHostError::AlreadyRunning)
            }
            ServiceState::Stopped | ServiceState::Failed => {
                return Err(ServiceHostError::AlreadyStopped)
            }
        }
        if self.shutdown_timeout.is_zero()
            || Instant::now().checked_add(self.shutdown_timeout).is_none()
        {
            self.state
                .store(ServiceState::Failed as u8, Ordering::Release);
            return Err(ServiceHostError::InvalidShutdownTimeout);
        }
        if self.stop.is_cancelled() {
            self.state
                .store(ServiceState::Stopped as u8, Ordering::Release);
            return Ok(());
        }
        self.state
            .store(ServiceState::StartPending as u8, Ordering::Release);
        let mut worker = self
            .worker
            .take()
            .ok_or_else(|| ServiceHostError::Startup("service worker unavailable".to_owned()))?;
        enum Event {
            Initialized,
            Finished(Result<(), ServiceHostError>),
            Stop,
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        let stop_sender = sender.clone();
        self.stop.on_cancel(move || {
            let _ = stop_sender.send(Event::Stop);
        });
        let stop = self.stop.clone();
        let worker_thread = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if stop.is_cancelled() {
                    return Ok(());
                }
                worker
                    .initialize(&stop)
                    .map_err(ServiceHostError::Startup)?;
                if stop.is_cancelled() {
                    return Ok(());
                }
                let _ = sender.send(Event::Initialized);
                worker.run(&stop).map_err(ServiceHostError::Worker)
            }))
            .unwrap_or_else(|_| {
                Err(ServiceHostError::Worker(
                    "service worker panicked".to_owned(),
                ))
            });
            // Drop the worker and its native resources before announcing exit.
            drop(worker);
            let _ = sender.send(Event::Finished(result));
        });
        let mut shutdown_deadline: Option<Instant> = None;
        let mut status_error = None;
        loop {
            // Block for the next event; once stopping, only until the deadline.
            let event = match shutdown_deadline {
                None => receiver
                    .recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
                Some(deadline) => {
                    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
            };
            match event {
                Ok(Event::Stop) => {
                    if shutdown_deadline.is_none() {
                        shutdown_deadline = Instant::now().checked_add(self.shutdown_timeout);
                        self.state
                            .store(ServiceState::StopPending as u8, Ordering::Release);
                        if let Err(error) = publish(ServiceState::StopPending) {
                            status_error = Some(error);
                        }
                    }
                }
                Ok(Event::Initialized) => {
                    if !self.stop.is_cancelled() {
                        self.state
                            .store(ServiceState::Running as u8, Ordering::Release);
                        if let Err(error) = publish(ServiceState::Running) {
                            status_error = Some(error);
                            self.stop.cancel();
                        }
                    }
                }
                Ok(Event::Finished(result)) => {
                    let joined = worker_thread.join().map_err(|_| {
                        ServiceHostError::Worker("worker thread panicked during cleanup".to_owned())
                    });
                    let result = joined.and(result).and(match status_error {
                        Some(error) => Err(ServiceHostError::Worker(error)),
                        None => Ok(()),
                    });
                    self.state.store(
                        if result.is_ok() {
                            ServiceState::Stopped
                        } else {
                            ServiceState::Failed
                        } as u8,
                        Ordering::Release,
                    );
                    return result;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // A Rust thread cannot be safely terminated or detached
                    // while it can publish. Fail the whole own-process service
                    // for SCM recovery, without a final Stopped report from a
                    // live worker.
                    eprintln!("service worker exceeded shutdown deadline; terminating process");
                    std::process::exit(1);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = worker_thread.join();
                    self.state
                        .store(ServiceState::Failed as u8, Ordering::Release);
                    return Err(ServiceHostError::Worker(
                        "service worker exited without reporting a result".to_owned(),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn transitions_to_stopped_after_worker_exits() {
        let mut host = ServiceHost::with_shutdown_timeout(
            |stop: &StopSignal| {
                assert!(!stop.is_cancelled());
                Ok(())
            },
            Duration::from_secs(1),
        );

        assert_eq!(host.state(), ServiceState::Created);
        host.run().expect("worker should exit successfully");
        assert_eq!(host.state(), ServiceState::Stopped);
    }

    #[test]
    fn records_worker_failure() {
        let mut host = ServiceHost::with_shutdown_timeout(
            |_stop: &StopSignal| Err("poll failed".to_owned()),
            Duration::from_secs(1),
        );

        assert_eq!(
            host.run(),
            Err(ServiceHostError::Worker("poll failed".to_owned()))
        );
        assert_eq!(host.state(), ServiceState::Failed);
    }

    struct FailingWorker;

    impl ServiceWorker for FailingWorker {
        fn initialize(&mut self, _stop: &StopSignal) -> Result<(), String> {
            Err("configuration invalid".to_owned())
        }

        fn run(&mut self, _stop: &StopSignal) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn records_startup_failure_before_running() {
        let mut host = ServiceHost::with_shutdown_timeout(FailingWorker, Duration::from_secs(1));

        assert_eq!(
            host.run(),
            Err(ServiceHostError::Startup(
                "configuration invalid".to_owned()
            ))
        );
        assert_eq!(host.state(), ServiceState::Failed);
    }

    #[test]
    fn exposes_stop_request_to_worker() {
        let mut host = ServiceHost::with_shutdown_timeout(
            |stop: &StopSignal| {
                assert!(stop.is_cancelled());
                Ok(())
            },
            Duration::from_secs(1),
        );
        host.request_stop();

        host.run().expect("stopped worker should exit successfully");
    }

    #[test]
    fn rejects_running_host_reentry() {
        let mut host =
            ServiceHost::with_shutdown_timeout(|_stop: &StopSignal| Ok(()), Duration::from_secs(1));
        host.state
            .store(ServiceState::Running as u8, Ordering::Release);

        assert_eq!(host.run(), Err(ServiceHostError::AlreadyRunning));
    }

    #[test]
    fn cancellation_wakes_waiters() {
        let stop = Arc::new(StopSignal::new());
        let waiter = Arc::clone(&stop);
        let thread = thread::spawn(move || waiter.wait(Duration::from_secs(60)));

        stop.cancel();
        thread
            .join()
            .expect("waiter should exit after cancellation");
    }

    #[test]
    fn cancellation_between_a_waiters_check_and_sleep_is_delivered() {
        // Reproduce the exact interleaving: the waiter has checked the flag and
        // is about to sleep when another thread cancels. The canceller must not
        // be able to set the flag and notify inside that gap.
        let stop = StopSignal::new();
        let checked = stop.cancelled();
        assert!(!*checked);
        let canceller = stop.clone();
        let cancelling = thread::spawn(move || canceller.cancel());
        let (cancelled, waited) = stop
            .state
            .wake
            .wait_timeout_while(checked, Duration::from_secs(5), |cancelled| !*cancelled)
            .expect("stop lock is not poisoned");
        assert!(*cancelled);
        assert!(!waited.timed_out());
        drop(cancelled);
        cancelling.join().expect("canceller exits");
    }

    #[test]
    fn timed_wait_returns_promptly_after_cancellation() {
        let stop = StopSignal::new();
        let waiter = stop.clone();
        let started = Instant::now();
        let thread = thread::spawn(move || waiter.wait(Duration::from_secs(60)));
        stop.cancel();
        thread.join().expect("waiter exits");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rejects_zero_shutdown_timeout() {
        let mut host =
            ServiceHost::with_shutdown_timeout(|_stop: &StopSignal| Ok(()), Duration::ZERO);

        assert_eq!(host.run(), Err(ServiceHostError::InvalidShutdownTimeout));
        assert_eq!(host.state(), ServiceState::Failed);
    }

    #[test]
    fn shutdown_timeout_terminates_the_process() {
        const FIXTURE: &str = "VIRTIO_MEM_SHUTDOWN_TIMEOUT_FIXTURE";
        if std::env::var_os(FIXTURE).is_some() {
            let stop = StopSignal::new();
            let cancel = stop.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                cancel.cancel();
            });
            let mut host = ServiceHost::with_stop_and_shutdown_timeout(
                |_stop: &StopSignal| {
                    thread::sleep(Duration::from_secs(30));
                    Ok(())
                },
                stop,
                Duration::from_millis(20),
            );
            let _ = host.run();
            panic!("timeout must terminate the process");
        }
        let started = Instant::now();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service_host::tests::shutdown_timeout_terminates_the_process",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stop_during_initialization_skips_running_and_cleans_up_before_return() {
        struct StopsOnInitialize(Arc<std::sync::atomic::AtomicBool>);
        impl ServiceWorker for StopsOnInitialize {
            fn initialize(&mut self, stop: &StopSignal) -> Result<(), String> {
                stop.cancel();
                Ok(())
            }
            fn run(&mut self, _: &StopSignal) -> Result<(), String> {
                panic!("run after cancelled initialization")
            }
        }
        impl Drop for StopsOnInitialize {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut host = ServiceHost::with_shutdown_timeout(
            StopsOnInitialize(dropped.clone()),
            Duration::from_secs(1),
        );
        let mut statuses = Vec::new();
        host.run_with_status(|state| {
            statuses.push(state);
            Ok(())
        })
        .unwrap();
        assert!(!statuses.contains(&ServiceState::Running));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(host.state(), ServiceState::Stopped);
    }

    #[test]
    fn worker_panic_finishes_without_an_operator_stop() {
        let stop = StopSignal::new();
        let mut host = ServiceHost::with_stop_and_shutdown_timeout(
            |_stop: &StopSignal| -> Result<(), String> { panic!("worker panic") },
            stop.clone(),
            Duration::from_secs(1),
        );
        assert!(matches!(host.run(), Err(ServiceHostError::Worker(_))));
        assert!(!stop.is_cancelled());
        assert_eq!(host.state(), ServiceState::Failed);
    }

    #[test]
    fn waits_for_worker_to_finish_within_shutdown_timeout() {
        let stop = StopSignal::new();
        let cancel = stop.clone();
        let _canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cancel.cancel();
        });
        let mut host = ServiceHost::with_stop_and_shutdown_timeout(
            |stop: &StopSignal| {
                while !stop.is_cancelled() {
                    thread::yield_now();
                }
                Ok(())
            },
            stop,
            Duration::from_secs(1),
        );

        assert!(host.run().is_ok());
        assert_eq!(host.state(), ServiceState::Stopped);
    }
}
