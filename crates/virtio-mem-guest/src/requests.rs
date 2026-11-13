//! The guest's side of the request protocol: applies the core guest policy
//! to each sample and the host status the transport delivered, and keeps the
//! one unresolved request durable before it is ever published.

use std::collections::BTreeMap;
use std::path::PathBuf;

use virtio_mem_core::demand::GuestDiagnostics;
use virtio_mem_core::{
    request_state_path, GuestRequestState, GuestRequester, GuestSettings, HostMemoryStatus,
    MemoryRequest, MemoryResult, TargetSample,
};

use crate::files::{read_bounded, replace_file};

/// Bounds a request-state read; the state holds at most one request.
const MAX_REQUEST_STATE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct GuestRequests {
    requester: GuestRequester,
    state_path: PathBuf,
    settings: GuestSettings,
    settings_changed: bool,
    resolved_last_poll: bool,
    last_consumed_result: Option<MemoryResult>,
    result_consumed_unix_millis: Option<u64>,
    build_id: String,
    driver_version: Result<String, String>,
}

/// What a guest reports about itself in every publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestIdentity {
    /// The stamped build of the guest service.
    pub build_id: String,
    /// The memory driver's version, or why it is unknown.
    pub driver_version: Result<String, String>,
}

impl GuestRequests {
    /// Restore retained state beside `control_path`. Missing state is fresh;
    /// unreadable or invalid state fails closed rather than forgetting a
    /// request the host may already have admitted.
    pub fn open(
        settings: GuestSettings,
        vm_name: &str,
        service_name: &str,
        session_id: &str,
        control_path: &str,
        identity: GuestIdentity,
    ) -> Result<Self, String> {
        let state_path = PathBuf::from(request_state_path(control_path));
        let state = match read_bounded(&state_path, MAX_REQUEST_STATE_BYTES)
            .map_err(|error| format!("read request state: {error}"))?
        {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("request state is invalid: {error}"))?,
            None => GuestRequestState::default(),
        };
        settings.validate()?;
        let mut requester =
            GuestRequester::new(settings.policy, vm_name, service_name, session_id, state)?;
        requester.set_ram_limits(settings.min_ram, settings.max_ram)?;
        Ok(Self {
            requester,
            state_path,
            settings,
            settings_changed: false,
            resolved_last_poll: false,
            last_consumed_result: None,
            result_consumed_unix_millis: None,
            build_id: identity.build_id,
            driver_version: identity.driver_version,
        })
    }

    /// The host has published different settings, or another identity,
    /// from the ones this was opened with: the caller starts again with them.
    pub fn settings_changed(&self) -> bool {
        self.settings_changed
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

    /// The request to publish with this sample and the latest host status.
    /// Changed state is durable before this returns, so a published request
    /// is always retained.
    pub fn poll(
        &mut self,
        sample: TargetSample,
        status: Option<&HostMemoryStatus>,
    ) -> Result<Option<MemoryRequest>, String> {
        let pressure = sample.native_pressure;
        let available = sample.memory.physical_available_bytes;
        let observed = sample.observed_unix_millis;
        self.resolved_last_poll = false;
        if let Some(status) = status {
            if status.vm_name != self.requester.vm_name()
                || status
                    .guest_settings
                    .is_some_and(|published| published != self.settings)
            {
                self.settings_changed = true;
            }
        }
        let decision = self.requester.poll(sample, status)?;
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

    pub fn diagnostics(
        &self,
        boot_uptime_millis: Option<u64>,
        environment: BTreeMap<String, String>,
    ) -> GuestDiagnostics {
        GuestDiagnostics {
            build_id: self.build_id.clone(),
            boot_uptime_millis,
            driver_file_version: self.driver_version.clone(),
            policy: self.requester.diagnostics().clone(),
            last_consumed_result: self.last_consumed_result.clone(),
            result_consumed_unix_millis: self.result_consumed_unix_millis,
            environment,
        }
    }
}

/// The identity and settings the host has published in `status` for this
/// guest's service: the VM name the host knows it by, and its limits and
/// policy. `None` for a status that carries none, or is not valid.
pub fn published_settings(
    status: &HostMemoryStatus,
    service_name: &str,
) -> Option<(String, GuestSettings)> {
    status.validate_for(&status.vm_name, service_name).ok()?;
    status
        .guest_settings
        .map(|settings| (status.vm_name.clone(), settings))
}
