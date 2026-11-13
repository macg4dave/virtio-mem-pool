//! The guest's side of the request channel: reads the host status QGA
//! writes beside the publication, applies the core guest policy, and keeps the
//! one unresolved request durable before it is ever published.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use virtio_mem_core::{
    host_status_path, request_state_path, GuestMemoryPolicy, GuestRequestState, GuestRequester,
    HostMemoryStatus, MemoryRequest, TargetSample, MAX_HOST_STATUS_BYTES,
};

use crate::demand::replace_file;
use crate::service_host::StopSignal;

/// Bounds a request-state read; the state holds at most one request.
const MAX_REQUEST_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub struct GuestRequests {
    requester: GuestRequester,
    status_path: PathBuf,
    state_path: PathBuf,
    retry_timeout: Duration,
    resolved_last_poll: bool,
    last_consumed_result: Option<virtio_mem_core::MemoryResult>,
    result_consumed_unix_millis: Option<u64>,
    driver_file_version: Result<String, String>,
}

impl GuestRequests {
    /// Restore retained state beside `publication_path`. Missing state is
    /// fresh; unreadable or invalid state fails closed rather than forgetting
    /// a request the host may already have admitted.
    pub fn open(
        policy: GuestMemoryPolicy,
        vm_name: &str,
        service_name: &str,
        session_id: &str,
        publication_path: &str,
        retry_timeout: Duration,
    ) -> Result<Self, String> {
        let state_path = PathBuf::from(request_state_path(publication_path));
        let state = match std::fs::File::open(&state_path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_REQUEST_STATE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("read request state: {error}"))?;
                if bytes.len() as u64 > MAX_REQUEST_STATE_BYTES {
                    return Err("request state exceeds its byte limit".to_owned());
                }
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("request state is invalid: {error}"))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                GuestRequestState::default()
            }
            Err(error) => return Err(format!("open request state: {error}")),
        };
        Ok(Self {
            requester: GuestRequester::new(policy, vm_name, service_name, session_id, state)?,
            status_path: PathBuf::from(host_status_path(publication_path)),
            state_path,
            retry_timeout,
            resolved_last_poll: false,
            last_consumed_result: None,
            result_consumed_unix_millis: None,
            driver_file_version: crate::diagnostics::driver_version(),
        })
    }

    pub fn set_ram_limits(&mut self, minimum: u64, maximum: u64) -> Result<(), String> {
        self.requester.set_ram_limits(minimum, maximum)
    }

    pub fn invalidate_history(&mut self) {
        self.requester.invalidate_history();
    }

    pub fn resolved_last_poll(&self) -> bool {
        self.resolved_last_poll
    }

    pub fn requester(&self) -> &GuestRequester {
        &self.requester
    }

    /// The request to publish with this sample. Changed state is durable
    /// before this returns, so a published request is always retained.
    pub fn poll(
        &mut self,
        sample: TargetSample,
        stop: &StopSignal,
    ) -> Result<Option<MemoryRequest>, String> {
        let observation = (
            sample.native_pressure,
            sample.memory.physical_available_bytes,
            sample
                .memory
                .commit_limit_bytes
                .saturating_sub(sample.memory.commit_total_bytes),
        );
        self.resolved_last_poll = false;
        let status = self.read_status()?;
        let observed = sample.observed_unix_millis;
        let decision = self.requester.poll(sample, status.as_ref())?;
        self.resolved_last_poll = decision.resolved.is_some();
        if decision.persist {
            if let Some(request) = &decision.request {
                eprintln!("memory request: operation={:?} bytes={} native={:?} available_bytes={} commit_headroom_bytes={}",
                    request.operation, request.bytes, observation.0, observation.1, observation.2);
            }
            let bytes = serde_json::to_vec(self.requester.state())
                .map_err(|error| format!("encode request state: {error}"))?;
            replace_file(&self.state_path, &bytes, true, stop, self.retry_timeout)
                .map_err(|error| format!("persist request state: {error}"))?;
        }
        if let Some(result) = decision.resolved {
            self.last_consumed_result = Some(result);
            self.result_consumed_unix_millis = Some(observed);
        }
        Ok(decision.request)
    }

    pub fn diagnostics(
        &self,
        boot_uptime_millis: Option<u64>,
    ) -> virtio_mem_core::demand::GuestDiagnostics {
        virtio_mem_core::demand::GuestDiagnostics {
            build_id: crate::BUILD_ID.to_owned(),
            boot_uptime_millis,
            driver_file_version: self.driver_file_version.clone(),
            policy: self.requester.diagnostics().clone(),
            last_consumed_result: self.last_consumed_result.clone(),
            result_consumed_unix_millis: self.result_consumed_unix_millis,
        }
    }

    /// The latest complete host status, or `None` while there is none to use:
    /// not yet written, mid-write by QGA (truncated or partial), or held open.
    fn read_status(&self) -> Result<Option<HostMemoryStatus>, String> {
        let file = match std::fs::File::open(&self.status_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION while QGA writes.
            Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => return Ok(None),
            Err(error) => return Err(format!("open host status: {error}")),
        };
        let mut bytes = Vec::new();
        file.take(MAX_HOST_STATUS_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read host status: {error}"))?;
        if bytes.len() > MAX_HOST_STATUS_BYTES || !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        // QGA cannot replace the file atomically, so undecodable contents are
        // an expected mid-write view, retried on the next poll.
        Ok(serde_json::from_slice(&bytes).ok())
    }
}
