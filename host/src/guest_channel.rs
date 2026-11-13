//! The host side of the guest channel: bounded reads of the Windows
//! publication (telemetry plus the guest's unresolved request) and writes of
//! the host status beside it, over the QEMU guest agent.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use virtio_mem_core::{
    host_status_path, HostMemoryStatus, RawTelemetryEnvelope, MAX_HOST_STATUS_BYTES,
};

use crate::qga::{GuestFileReader, GuestFileWriter};

pub const MAX_RAW_TELEMETRY_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_RAW_TELEMETRY_FILE_BYTES: usize = 1024 * 1024;

pub trait UnixClock {
    fn now_unix_millis(&self) -> Result<u64, String>;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn now_unix_millis(&self) -> Result<u64, String> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
        u64::try_from(duration.as_millis())
            .map_err(|_| "system Unix timestamp does not fit in u64 milliseconds".to_owned())
    }
}

/// The runtime's view of one guest, so control-loop tests can inject input.
pub trait GuestLink {
    /// The latest validated publication.
    fn read(&self) -> Result<RawTelemetryEnvelope, String>;
    fn publish_status(&self, status: &HostMemoryStatus) -> Result<(), String>;
}

#[derive(Debug)]
pub struct GuestChannel<Q, T = SystemUnixClock> {
    files: Q,
    publication_path: String,
    vm_name: String,
    service_name: String,
    max_age: Duration,
    future_tolerance: Duration,
    clock: T,
    #[cfg(unix)]
    wake: Option<std::sync::Arc<crate::wake::WakePeer>>,
    last_status: std::cell::RefCell<Option<HostMemoryStatus>>,
    last_write: std::cell::Cell<Option<Instant>>,
}

impl<Q> GuestChannel<Q, SystemUnixClock> {
    pub fn new(
        files: Q,
        publication_path: impl Into<String>,
        vm_name: impl Into<String>,
        service_name: impl Into<String>,
        max_age: Duration,
        future_tolerance: Duration,
    ) -> Self {
        Self::with_clock(
            files,
            publication_path,
            vm_name,
            service_name,
            max_age,
            future_tolerance,
            SystemUnixClock,
        )
    }
}

impl<Q, T> GuestChannel<Q, T> {
    #[cfg(unix)]
    pub fn with_wake(mut self, wake: std::sync::Arc<crate::wake::WakePeer>) -> Self {
        self.wake = Some(wake);
        self
    }

    pub fn with_clock(
        files: Q,
        publication_path: impl Into<String>,
        vm_name: impl Into<String>,
        service_name: impl Into<String>,
        max_age: Duration,
        future_tolerance: Duration,
        clock: T,
    ) -> Self {
        Self {
            files,
            publication_path: publication_path.into(),
            vm_name: vm_name.into(),
            service_name: service_name.into(),
            max_age,
            future_tolerance,
            clock,
            #[cfg(unix)]
            wake: None,
            last_status: std::cell::RefCell::new(None),
            last_write: std::cell::Cell::new(None),
        }
    }
}

impl<Q: GuestFileReader + GuestFileWriter, T: UnixClock> GuestLink for GuestChannel<Q, T> {
    fn read(&self) -> Result<RawTelemetryEnvelope, String> {
        let bytes = self
            .files
            .read_file(&self.publication_path, MAX_RAW_TELEMETRY_FILE_BYTES)
            .map_err(|error| format!("guest publication unavailable: {error}"))?;
        let contents = String::from_utf8(bytes)
            .map_err(|error| format!("guest publication is not valid UTF-8: {error}"))?;
        validate_contents(
            &contents,
            &self.vm_name,
            &self.service_name,
            duration_millis(self.max_age)?,
            duration_millis(self.future_tolerance)?,
            self.clock.now_unix_millis()?,
        )
    }

    /// Writes only a semantic change, or a refresh before the guest would
    /// consider the last status stale: every write is a guest-agent round
    /// trip that can collide with the guest's own file access.
    fn publish_status(&self, status: &HostMemoryStatus) -> Result<(), String> {
        let changed = self.last_status.borrow().as_ref().is_none_or(|last| {
            last.requested_bytes != status.requested_bytes
                || last.current_bytes != status.current_bytes
                || last.result != status.result
                || last.device_size_bytes != status.device_size_bytes
                || last.block_size_bytes != status.block_size_bytes
                || last.guest_settings != status.guest_settings
        });
        let refresh_due = self
            .last_write
            .get()
            .is_none_or(|written| written.elapsed() >= self.max_age / 3);
        if !changed && !refresh_due {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec(status)
            .map_err(|error| format!("encode host memory status: {error}"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_HOST_STATUS_BYTES {
            return Err("host memory status exceeds its byte limit".to_owned());
        }
        self.files
            .write_file(&host_status_path(&self.publication_path), &bytes)?;
        self.last_write.set(Some(Instant::now()));
        if changed {
            #[cfg(unix)]
            if let Some(wake) = &self.wake {
                wake.notify();
            }
            *self.last_status.borrow_mut() = Some(status.clone());
        }
        Ok(())
    }
}

/// Replace a host-local file by rename, so readers never see it partial.
pub(crate) fn write_local(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| {
            // Best effort: the publish error below is the failure reported.
            let _ = std::fs::remove_file(&temporary);
            format!("publish host memory status {}: {error}", path.display())
        })
}

fn validate_contents(
    contents: &str,
    expected_vm_name: &str,
    expected_service_name: &str,
    max_age_millis: u64,
    future_tolerance_millis: u64,
    now: u64,
) -> Result<RawTelemetryEnvelope, String> {
    if !contents.is_empty() && !contents.ends_with('\n') {
        return Err("guest publication ends with a partial record".to_owned());
    }
    let record = contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| "guest publication has no records".to_owned())?;
    if record.len() > MAX_RAW_TELEMETRY_RECORD_BYTES {
        return Err(format!(
            "latest guest record exceeds {MAX_RAW_TELEMETRY_RECORD_BYTES} byte limit"
        ));
    }
    let envelope: RawTelemetryEnvelope = serde_json::from_str(record)
        .map_err(|error| format!("parse latest guest record: {error}"))?;
    envelope
        .validate_for(
            expected_vm_name,
            expected_service_name,
            now,
            max_age_millis,
            future_tolerance_millis,
        )
        .map_err(|error| error.to_string())?;
    Ok(envelope)
}

fn duration_millis(duration: Duration) -> Result<u64, String> {
    u64::try_from(duration.as_millis())
        .map_err(|_| "guest channel duration is too large".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use virtio_mem_core::{
        MemoryOperation, MemoryRequest, MemoryResourceNotificationState, MemoryTelemetrySnapshot,
        RequestIdentity, MEMORY_REQUEST_VERSION,
    };

    #[derive(Default)]
    struct FakeGuestFiles {
        publication: Vec<u8>,
        written: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl GuestFileReader for FakeGuestFiles {
        fn read_file(&self, _path: &str, maximum_bytes: usize) -> Result<Vec<u8>, String> {
            if self.publication.len() > maximum_bytes {
                Err("fixture exceeds bound".to_owned())
            } else {
                Ok(self.publication.clone())
            }
        }
    }

    impl GuestFileWriter for FakeGuestFiles {
        fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String> {
            self.written
                .lock()
                .map_err(|_| "fixture lock poisoned".to_owned())?
                .push((path.to_owned(), contents.to_vec()));
            Ok(())
        }
    }

    struct FixedClock(u64);

    impl UnixClock for FixedClock {
        fn now_unix_millis(&self) -> Result<u64, String> {
            Ok(self.0)
        }
    }

    fn snapshot() -> MemoryTelemetrySnapshot {
        MemoryTelemetrySnapshot {
            physical_total_bytes: 100,
            physical_available_bytes: 40,
            memory_load_percent: 60,
            commit_total_bytes: 50,
            commit_limit_bytes: 100,
            commit_peak_bytes: 50,
            system_cache_bytes: 0,
            kernel_paged_bytes: 0,
            kernel_nonpaged_bytes: 0,
        }
    }

    fn envelope(session: &str, observed: u64, sequence: u64) -> RawTelemetryEnvelope {
        RawTelemetryEnvelope::new(
            "guest",
            "VirtioMemService",
            session,
            observed,
            10 + sequence,
            sequence,
            snapshot(),
        )
    }

    fn line(envelope: &RawTelemetryEnvelope) -> String {
        format!("{}\n", serde_json::to_string(envelope).expect("encode"))
    }

    fn qga(publication: Vec<u8>) -> GuestChannel<FakeGuestFiles, FixedClock> {
        GuestChannel::with_clock(
            FakeGuestFiles {
                publication,
                written: Mutex::default(),
            },
            r"C:\ProgramData\VirtioMemService\telemetry.json",
            "guest",
            "VirtioMemService",
            Duration::from_secs(60),
            Duration::from_secs(5),
            FixedClock(1_000_000),
        )
    }

    #[test]
    fn reads_the_latest_fresh_matching_record() {
        let latest = envelope("session-a", 995_000, 1);
        let publication = format!(
            "{}{}",
            line(&envelope("session-a", 900_000, 0)),
            line(&latest)
        );
        assert_eq!(qga(publication.into_bytes()).read(), Ok(latest));
    }

    #[test]
    fn carries_the_guest_request_and_native_signals_uninterpreted() {
        let mut current = envelope("session-a", 995_000, 0);
        current.native_pressure = Some(MemoryResourceNotificationState::Low);
        current.request = Some(MemoryRequest {
            version: MEMORY_REQUEST_VERSION,
            identity: RequestIdentity {
                vm_name: "guest".to_owned(),
                service_name: "VirtioMemService".to_owned(),
                session_id: "earlier-session".to_owned(),
                sequence: 3,
            },
            operation: MemoryOperation::Add,
            bytes: 1 << 30,
            issued_unix_millis: 990_000,
            expires_unix_millis: 1_010_000,
        });
        assert_eq!(qga(line(&current).into_bytes()).read(), Ok(current.clone()));

        let mut foreign = current;
        if let Some(request) = foreign.request.as_mut() {
            request.identity.vm_name = "other".to_owned();
        }
        assert!(qga(line(&foreign).into_bytes()).read().is_err());
    }

    #[test]
    fn rejects_missing_stale_cross_vm_partial_and_oversized_records() {
        let cross_vm = RawTelemetryEnvelope::new(
            "other",
            "VirtioMemService",
            "session-a",
            995_000,
            1,
            0,
            snapshot(),
        );
        for (name, contents) in [
            ("missing", String::new()),
            ("stale", line(&envelope("session-a", 900_000, 0))),
            ("cross-vm", line(&cross_vm)),
            ("malformed", "{not-json\n".to_owned()),
            ("partial", "{}".to_owned()),
            (
                "oversized-record",
                format!("{}\n", "x".repeat(MAX_RAW_TELEMETRY_RECORD_BYTES + 1)),
            ),
            (
                "oversized-file",
                "\n".repeat(MAX_RAW_TELEMETRY_FILE_BYTES + 1),
            ),
        ] {
            assert!(qga(contents.into_bytes()).read().is_err(), "{name}");
        }
    }

    #[test]
    fn publishes_complete_status_beside_the_publication() {
        let status = HostMemoryStatus {
            version: MEMORY_REQUEST_VERSION,
            vm_name: "guest".to_owned(),
            service_name: "VirtioMemService".to_owned(),
            device_alias: "memory0".to_owned(),
            observed_unix_millis: 1_000_000,
            device_size_bytes: 8 << 30,
            block_size_bytes: 2 << 20,
            requested_bytes: 0,
            current_bytes: 0,
            result: None,
            guest_settings: None,
        };
        let channel = qga(Vec::new());
        channel.publish_status(&status).expect("QGA status");
        // Unchanged within the refresh interval: no second write.
        channel.publish_status(&status).expect("QGA status");
        let changed = HostMemoryStatus {
            current_bytes: 2 << 20,
            requested_bytes: 2 << 20,
            ..status.clone()
        };
        channel.publish_status(&changed).expect("QGA status");
        let written = channel.files.written.lock().expect("written");
        assert_eq!(written.len(), 2);
        assert_eq!(
            written[0].0,
            r"C:\ProgramData\VirtioMemService\telemetry.json.host-status"
        );
        assert_eq!(
            serde_json::from_slice::<HostMemoryStatus>(&written[0].1).expect("status"),
            status
        );
        assert!(written[1].1.ends_with(b"\n"));
    }
}
