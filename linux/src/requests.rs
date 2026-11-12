//! The guest's side of the request channel: reads the host status the guest
//! agent writes beside the publication, applies the core guest policy, and
//! keeps the one unresolved request durable before it is ever published.

use std::path::PathBuf;

use virtio_mem_core::demand::GuestDiagnostics;
use virtio_mem_core::{
    host_status_path, request_state_path, GuestMemoryPolicy, GuestRequestState, GuestRequester,
    HostMemoryStatus, MemoryRequest, MemoryResult, TargetSample, MAX_HOST_STATUS_BYTES,
};

use crate::files::{read_bounded, replace_file};

/// Bounds a request-state read; the state holds at most one request.
const MAX_REQUEST_STATE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct GuestRequests {
    requester: GuestRequester,
    status_path: PathBuf,
    state_path: PathBuf,
    resolved_last_poll: bool,
    last_consumed_result: Option<MemoryResult>,
    result_consumed_unix_millis: Option<u64>,
    driver_version: Result<String, String>,
}

impl GuestRequests {
    /// Restore retained state beside `control_path`. Missing state is fresh;
    /// unreadable or invalid state fails closed rather than forgetting a
    /// request the host may already have admitted.
    pub fn open(
        policy: GuestMemoryPolicy,
        vm_name: &str,
        service_name: &str,
        session_id: &str,
        control_path: &str,
    ) -> Result<Self, String> {
        let state_path = PathBuf::from(request_state_path(control_path));
        let state = match read_bounded(&state_path, MAX_REQUEST_STATE_BYTES)
            .map_err(|error| format!("read request state: {error}"))?
        {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("request state is invalid: {error}"))?,
            None => GuestRequestState::default(),
        };
        Ok(Self {
            requester: GuestRequester::new(policy, vm_name, service_name, session_id, state)?,
            status_path: PathBuf::from(host_status_path(control_path)),
            state_path,
            resolved_last_poll: false,
            last_consumed_result: None,
            result_consumed_unix_millis: None,
            driver_version: kernel_release(),
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
    pub fn poll(&mut self, sample: TargetSample) -> Result<Option<MemoryRequest>, String> {
        let pressure = sample.native_pressure;
        let available = sample.memory.physical_available_bytes;
        let observed = sample.observed_unix_millis;
        self.resolved_last_poll = false;
        let status = self.read_status()?;
        let decision = self.requester.poll(sample, status.as_ref())?;
        self.resolved_last_poll = decision.resolved.is_some();
        if decision.persist {
            if let Some(request) = &decision.request {
                eprintln!(
                    "memory request: operation={:?} bytes={} pressure={pressure:?} available_bytes={available}",
                    request.operation, request.bytes
                );
            }
            let bytes = serde_json::to_vec(self.requester.state())
                .map_err(|error| format!("encode request state: {error}"))?;
            replace_file(&self.state_path, &bytes, true)
                .map_err(|error| format!("persist request state: {error}"))?;
        }
        if let Some(result) = decision.resolved {
            eprintln!(
                "memory result: operation={:?} outcome={:?} actual_bytes={}",
                result.request.operation, result.outcome, result.actual_bytes
            );
            self.last_consumed_result = Some(result);
            self.result_consumed_unix_millis = Some(observed);
        }
        Ok(decision.request)
    }

    pub fn diagnostics(&self, boot_uptime_millis: Option<u64>) -> GuestDiagnostics {
        GuestDiagnostics {
            build_id: crate::BUILD_ID.to_owned(),
            boot_uptime_millis,
            driver_file_version: self.driver_version.clone(),
            policy: self.requester.diagnostics().clone(),
            last_consumed_result: self.last_consumed_result.clone(),
            result_consumed_unix_millis: self.result_consumed_unix_millis,
        }
    }

    /// The latest complete host status, or `None` while there is none to use:
    /// not yet written, or mid-write by the guest agent (truncated or partial).
    fn read_status(&self) -> Result<Option<HostMemoryStatus>, String> {
        let Some(bytes) = read_bounded(&self.status_path, MAX_HOST_STATUS_BYTES)
            .map_err(|error| format!("read host status: {error}"))?
        else {
            return Ok(None);
        };
        if !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        // The guest agent cannot replace the file atomically, so undecodable
        // contents are an expected mid-write view, retried on the next poll.
        Ok(serde_json::from_slice(&bytes).ok())
    }
}

/// The in-tree virtio-mem driver is versioned with its kernel.
fn kernel_release() -> Result<String, String> {
    let path = std::path::Path::new("/proc/sys/kernel/osrelease");
    let bytes = read_bounded(path, 256)
        .map_err(|error| format!("read kernel release: {error}"))?
        .ok_or("kernel release is unavailable on this platform")?;
    let release = String::from_utf8(bytes).map_err(|_| "kernel release is not UTF-8")?;
    Ok(format!("linux {}", release.trim()))
}
