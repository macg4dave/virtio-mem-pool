//! Guest-owned deltas and host responses. File reads are delivery, never an
//! acknowledgement. A guest durably retains one request (including its session
//! and expiry) until a correlated terminal result is known, even across restart.
//! A host persists admission before executing and persists its result before
//! publishing. A crash with admitted intent is uncertain, never a replay grant.
use serde::{Deserialize, Serialize};

use crate::VirtioMemState;

/// The version of a memory request and of the host status that answers it.
pub const MEMORY_REQUEST_VERSION: u16 = 1;

crate::schema_name! {
    /// The `schema` of the host status the controller publishes to a guest.
    pub struct HostStatusSchema = "rampire.host-status";
}
/// Admission freshness. Execution/result resolution may outlive this interval.
pub const MAX_REQUEST_LIFETIME_MILLIS: u64 = 60_000;
/// Bounds a host status read; a status with its result is a few kilobytes.
pub const MAX_HOST_STATUS_BYTES: usize = 64 * 1024;

/// The host's status file sits beside the guest's telemetry publication, so
/// both sides derive it from the one configured publication path.
pub fn host_status_path(publication_path: &str) -> String {
    format!("{publication_path}.host-status")
}

/// The guest's durable unresolved-request state, beside its publication.
pub fn request_state_path(publication_path: &str) -> String {
    format!("{publication_path}.request-state")
}

/// The one rule for VM, service and session identities on the guest/host
/// wire: non-empty printable ASCII of at most 128 bytes.
pub fn valid_identity(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestIdentity {
    pub vm_name: String,
    pub service_name: String,
    pub session_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOperation {
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRequest {
    pub version: u16,
    pub identity: RequestIdentity,
    pub operation: MemoryOperation,
    pub bytes: u64,
    pub issued_unix_millis: u64,
    pub expires_unix_millis: u64,
}

impl MemoryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != MEMORY_REQUEST_VERSION {
            return Err("unsupported memory request version".to_owned());
        }
        if ![
            &self.identity.vm_name,
            &self.identity.service_name,
            &self.identity.session_id,
        ]
        .into_iter()
        .all(|value| valid_identity(value))
        {
            return Err("invalid memory request identity".to_owned());
        }
        if self.bytes == 0
            || self.issued_unix_millis == 0
            || self.expires_unix_millis <= self.issued_unix_millis
            || self
                .expires_unix_millis
                .saturating_sub(self.issued_unix_millis)
                > MAX_REQUEST_LIFETIME_MILLIS
        {
            return Err(
                "memory request needs a positive delta and a bounded validity interval".to_owned(),
            );
        }
        Ok(())
    }

    /// Check again at the effect boundary, after any blocking preparation.
    /// Expiry blocks a new effect; it does not erase an already admitted one.
    pub fn validate_at(
        &self,
        vm: &str,
        service: &str,
        now: u64,
        future_tolerance_millis: u64,
    ) -> Result<(), String> {
        self.validate()?;
        if self.identity.vm_name != vm || self.identity.service_name != service {
            return Err("foreign memory request".to_owned());
        }
        if self.issued_unix_millis > now.saturating_add(future_tolerance_millis)
            || now >= self.expires_unix_millis
        {
            return Err("memory request is expired or from the future".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOutcome {
    Completed,
    Partial,
    PoolEmpty,
    Rejected,
    /// The command may have taken effect. This is unresolved and must not
    /// permit a fresh delta, even if the service has since changed session.
    Uncertain,
}

/// Longest accepted result or latch reason, in bytes.
pub const MAX_REASON_BYTES: usize = 1024;

/// Make free text, such as an external command's error, a valid reason:
/// control characters (newlines included) become spaces, the text is cut to
/// `MAX_REASON_BYTES` on a character boundary, and it is never empty.
pub fn bounded_reason(text: &str) -> String {
    let mut reason = text
        .split(|character: char| character.is_control() || character.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if reason.len() > MAX_REASON_BYTES {
        let mut end = MAX_REASON_BYTES;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    if reason.is_empty() {
        "unspecified".to_owned()
    } else {
        reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryResult {
    pub request: MemoryRequest,
    pub outcome: MemoryOutcome,
    pub actual_bytes: u64,
    pub observed_unix_millis: u64,
    pub reason: Option<String>,
}

impl MemoryResult {
    /// A refusal before any device command: no effect, and not journalled.
    pub fn rejected(request: MemoryRequest, observed_unix_millis: u64, reason: String) -> Self {
        Self {
            request,
            outcome: MemoryOutcome::Rejected,
            actual_bytes: 0,
            observed_unix_millis,
            reason: Some(reason),
        }
    }

    pub fn validate_for(&self, request: &MemoryRequest) -> Result<(), String> {
        self.request.validate()?;
        if &self.request != request {
            return Err("memory result does not match the retained request".to_owned());
        }
        if self.observed_unix_millis == 0 || self.actual_bytes > request.bytes {
            return Err("memory result has invalid observation/actual bytes".to_owned());
        }
        if self.reason.as_ref().is_some_and(|reason| {
            reason.is_empty()
                || reason.len() > MAX_REASON_BYTES
                || reason.chars().any(char::is_control)
        }) {
            return Err("invalid memory result reason".to_owned());
        }
        let valid = match self.outcome {
            MemoryOutcome::Completed => self.actual_bytes == request.bytes,
            MemoryOutcome::Partial => self.actual_bytes < request.bytes,
            MemoryOutcome::PoolEmpty => {
                request.operation == MemoryOperation::Add && self.actual_bytes == 0
            }
            MemoryOutcome::Rejected => self.actual_bytes == 0 && self.reason.is_some(),
            MemoryOutcome::Uncertain => self.reason.is_some(),
        };
        if !valid {
            return Err("memory result outcome contradicts actual bytes".to_owned());
        }
        Ok(())
    }

    pub fn terminal(&self) -> bool {
        self.outcome != MemoryOutcome::Uncertain
    }
}

/// Status the host writes beside the guest publication every poll. A status
/// without a result bootstraps Windows' device geometry; only a matching
/// response can resolve its durable pending request. Status must be fresh
/// before computing another delta. An old terminal result may resolve a
/// retained identity, but never supplies fresh geometry for policy.
/// What the host tells a guest about itself: the administrator's limits and
/// the policy derived from them. A guest that takes these from the host
/// needs no configuration of its own and cannot drift from the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestSettings {
    pub policy: crate::GuestMemoryPolicy,
    /// Total guest RAM limits, as the administrator set them.
    pub min_ram: u64,
    pub max_ram: u64,
    pub poll_interval_millis: u64,
}

impl GuestSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.poll_interval_millis == 0 || self.poll_interval_millis >= u64::from(u32::MAX) {
            return Err("guest poll interval is zero or too large".to_owned());
        }
        crate::settings::validate_ram_limits(self.min_ram, self.max_ram)?;
        self.policy.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMemoryStatus {
    pub schema: HostStatusSchema,
    pub version: u16,
    pub vm_name: String,
    pub service_name: String,
    pub device_alias: String,
    pub observed_unix_millis: u64,
    pub device_size_bytes: u64,
    pub block_size_bytes: u64,
    pub requested_bytes: u64,
    pub current_bytes: u64,
    pub result: Option<MemoryResult>,
    /// Published to guests that take their settings from the host. Absent
    /// for a guest that carries its own configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_settings: Option<GuestSettings>,
}

impl HostMemoryStatus {
    pub fn device(&self) -> VirtioMemState {
        VirtioMemState {
            size_bytes: self.device_size_bytes,
            block_size_bytes: self.block_size_bytes,
            requested_bytes: self.requested_bytes,
            current_bytes: self.current_bytes,
        }
    }

    /// Identity and internal consistency, independent of age.
    pub fn validate_for(&self, vm: &str, service: &str) -> Result<(), String> {
        if self.version != MEMORY_REQUEST_VERSION
            || self.vm_name != vm
            || self.service_name != service
        {
            return Err("foreign or unsupported host memory status".to_owned());
        }
        if self.observed_unix_millis == 0 {
            return Err("host memory status has no observation time".to_owned());
        }
        if let Some(settings) = &self.guest_settings {
            settings.validate()?;
        }
        self.device()
            .validate()
            .map_err(|error| error.to_string())?;
        if let Some(result) = &self.result {
            result.validate_for(&result.request)?;
            // A retained result describes its original operation, which may
            // predate a device block-size change. Validate its contract and
            // identity, but do not apply current geometry to historical bytes.
            if result.request.identity.vm_name != vm
                || result.request.identity.service_name != service
                || result.observed_unix_millis > self.observed_unix_millis
            {
                return Err("host memory result identity/time contradicts status".to_owned());
            }
        }
        Ok(())
    }

    /// Host and guest clocks may differ, so age is bounded in both directions.
    pub fn fresh_at(&self, now: u64, max_age_millis: u64) -> bool {
        self.observed_unix_millis.abs_diff(now) <= max_age_millis
    }
}

/// The retained host admission record. Persist every successful transition
/// before exposing it to transport or executing a device command. Keep the
/// terminal record as a replay watermark when admitting its successor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRequestJournal {
    pub request: Option<MemoryRequest>,
    pub result: Option<MemoryResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAdmission {
    New,
    Replay,
}

impl MemoryRequestJournal {
    pub fn validate(&self) -> Result<(), String> {
        match (&self.request, &self.result) {
            (None, None) => Ok(()),
            (Some(request), result) => {
                request.validate()?;
                if let Some(result) = result {
                    result.validate_for(request)?;
                }
                Ok(())
            }
            (None, Some(_)) => Err("request journal has a result without an admission".to_owned()),
        }
    }

    pub fn admit(
        &mut self,
        request: MemoryRequest,
        vm: &str,
        service: &str,
        now: u64,
        future_tolerance_millis: u64,
    ) -> Result<RequestAdmission, String> {
        self.validate()?;
        request.validate()?;
        if request.identity.vm_name != vm || request.identity.service_name != service {
            return Err("foreign memory request".to_owned());
        }
        if let Some(previous) = &self.request {
            if previous.identity.vm_name != vm || previous.identity.service_name != service {
                return Err("foreign request journal".to_owned());
            }
            if previous.identity == request.identity {
                if previous != &request {
                    return Err("request identity was reused with different content".to_owned());
                }
                return Ok(RequestAdmission::Replay);
            }
            if self.result.as_ref().is_none_or(|result| !result.terminal()) {
                return Err(
                    "an earlier request is unresolved, including across producer sessions"
                        .to_owned(),
                );
            }
            // Issuance timestamps are a durable watermark, not just freshness.
            // The guest must wait for an advancing wall clock after a rollback.
            if request.issued_unix_millis <= previous.issued_unix_millis
                || (request.identity.session_id == previous.identity.session_id
                    && request.identity.sequence <= previous.identity.sequence)
            {
                return Err("memory request predates the retained replay watermark".to_owned());
            }
        }
        request.validate_at(vm, service, now, future_tolerance_millis)?;
        self.request = Some(request);
        self.result = None;
        Ok(RequestAdmission::New)
    }

    /// Reasons are bounded first, so an external error's text cannot make
    /// an otherwise valid result unrecordable.
    pub fn record_result(&mut self, mut result: MemoryResult) -> Result<(), String> {
        self.validate()?;
        result.reason = result.reason.as_deref().map(bounded_reason);
        let request = self
            .request
            .as_ref()
            .ok_or("memory result has no admitted request")?;
        result.validate_for(request)?;
        if let Some(previous) = &self.result {
            if previous.terminal() && previous != &result {
                return Err("terminal memory result cannot be replaced".to_owned());
            }
            if result.observed_unix_millis < previous.observed_unix_millis {
                return Err("memory result observation moved backwards".to_owned());
            }
        }
        self.result = Some(result);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_reason_makes_external_errors_recordable() {
        let reason = bounded_reason(
            "read live QEMU argv: error: failed to connect\nerror: authentication failed:\r\n",
        );
        assert_eq!(
            reason,
            "read live QEMU argv: error: failed to connect error: authentication failed:"
        );
        assert_eq!(bounded_reason("\n\t"), "unspecified");
        let long = bounded_reason(&"é".repeat(MAX_REASON_BYTES));
        assert!(long.len() <= MAX_REASON_BYTES && long.chars().all(|c| c == 'é'));
    }

    fn request() -> MemoryRequest {
        MemoryRequest {
            version: MEMORY_REQUEST_VERSION,
            identity: RequestIdentity {
                vm_name: "guest".to_owned(),
                service_name: "service".to_owned(),
                session_id: "session".to_owned(),
                sequence: 12,
            },
            operation: MemoryOperation::Add,
            bytes: 1024,
            issued_unix_millis: 100,
            expires_unix_millis: 200,
        }
    }
    #[test]
    fn expiry_and_identity_are_effect_preconditions() {
        let request = request();
        assert!(request.validate_at("guest", "service", 100, 0).is_ok());
        assert!(request.validate_at("guest", "service", 200, 0).is_err());
        assert!(request.validate_at("other", "service", 100, 0).is_err());
        assert!(request.validate_at("guest", "service", 99, 0).is_err());
        assert_eq!(
            serde_json::from_slice::<MemoryRequest>(&serde_json::to_vec(&request).unwrap())
                .unwrap(),
            request
        );
    }
    #[test]
    fn results_correlate_the_complete_request_and_report_actual_bytes() {
        let request = request();
        let mut result = MemoryResult {
            request: request.clone(),
            outcome: MemoryOutcome::Partial,
            actual_bytes: 512,
            observed_unix_millis: 150,
            reason: None,
        };
        result.validate_for(&request).unwrap();
        let mut altered = request.clone();
        altered.bytes += 1;
        assert!(result.validate_for(&altered).is_err());
        result.outcome = MemoryOutcome::PoolEmpty;
        assert!(result.validate_for(&request).is_err());
        result.actual_bytes = 0;
        result.validate_for(&request).unwrap();
        result.outcome = MemoryOutcome::Uncertain;
        result.reason = Some("command timed out".to_owned());
        result.validate_for(&request).unwrap();
        assert!(!result.terminal());
    }
    #[test]
    fn durable_admission_never_regrants_a_retransmission_or_an_uncertain_delta() {
        let first = request();
        let mut journal = MemoryRequestJournal::default();
        assert_eq!(
            journal
                .admit(first.clone(), "guest", "service", 100, 0)
                .unwrap(),
            RequestAdmission::New
        );
        let mut restored: MemoryRequestJournal =
            serde_json::from_slice(&serde_json::to_vec(&journal).unwrap()).unwrap();
        // Expiry does not delete the admission or authorise another effect.
        assert_eq!(
            restored
                .admit(first.clone(), "guest", "service", 500, 0)
                .unwrap(),
            RequestAdmission::Replay
        );
        let mut next = first.clone();
        next.identity.session_id = "new-session".to_owned();
        next.identity.sequence = 37;
        next.issued_unix_millis = 160;
        next.expires_unix_millis = 260;
        assert!(restored
            .admit(next.clone(), "guest", "service", 160, 0)
            .is_err());
        restored
            .record_result(MemoryResult {
                request: first.clone(),
                outcome: MemoryOutcome::Uncertain,
                actual_bytes: 0,
                observed_unix_millis: 150,
                reason: Some("connection lost".to_owned()),
            })
            .unwrap();
        assert!(restored
            .admit(next.clone(), "guest", "service", 160, 0)
            .is_err());
        restored
            .record_result(MemoryResult {
                request: first.clone(),
                outcome: MemoryOutcome::Partial,
                actual_bytes: 512,
                observed_unix_millis: 155,
                reason: None,
            })
            .unwrap();
        assert_eq!(
            restored.admit(next, "guest", "service", 160, 0).unwrap(),
            RequestAdmission::New
        );
        assert!(restored.admit(first, "guest", "service", 160, 0).is_err());
    }
}
