//! Versioned read-only controller status contract.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::memory_request::{MemoryRequest, MemoryResult};
use crate::VirtioMemState;

pub const CONTROLLER_STATUS_VERSION: u16 = 5;
pub const MAX_CONTROLLER_STATUS_BYTES: usize = 64 * 1024;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_REASON_BYTES: usize = 1024;

/// What a recorded host command does to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandDirection {
    Grow,
    Shrink,
    /// Accept what a stalled shrink left plugged by requesting `current`.
    SettleShrink,
    /// Accept what a stalled growth plugged by requesting `current`.
    SettleGrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlHealth {
    Converged,
    Growing,
    Shrinking,
    RecoveryRequired,
    Latched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOwnership {
    None,
    OwnedPending,
    OwnedConvergedUnresolved,
    UnownedPending,
    RecordedNotApplied,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    NotRequired,
    ReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerCommandStatus {
    pub operation_id: String,
    pub direction: CommandDirection,
    pub prior_requested_bytes: u64,
    pub prior_current_bytes: u64,
    pub target_bytes: u64,
}

/// The guest publication as one read-only status read found it. Pool
/// ceilings and other process-local daemon context are not reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestInputStatus {
    /// Why the publication is unusable: unreachable, invalid or stale.
    pub error: Option<String>,
    pub observed_unix_millis: Option<u64>,
    pub request: Option<MemoryRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerStatusSnapshot {
    pub version: u16,
    pub observed_unix_millis: u64,
    pub vm_name: String,
    pub device_alias: String,
    pub device_size_bytes: u64,
    pub block_size_bytes: u64,
    pub maximum_target_bytes: u64,
    pub requested_bytes: u64,
    pub current_bytes: u64,
    pub observe_only: bool,
    pub guest_input: GuestInputStatus,
    /// The durably admitted request and its result, if any.
    pub admitted_request: Option<MemoryRequest>,
    pub result: Option<MemoryResult>,
    pub command_ownership: CommandOwnership,
    pub command: Option<ControllerCommandStatus>,
    pub control_health: ControlHealth,
    pub actuation_latched: bool,
    pub latch_reason: Option<String>,
    pub recovery_state: RecoveryState,
    pub recovery_reason: Option<String>,
    pub last_latch_clear_reason: Option<String>,
    pub compatibility_fingerprint_sha256: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControllerStatusError {
    #[error("controller status exceeds its byte limit")]
    Oversized,
    #[error("controller status JSON is invalid: {0}")]
    InvalidJson(String),
    #[error("controller status version is unsupported: {0}")]
    UnsupportedVersion(u16),
    #[error("controller status is invalid: {0}")]
    Invalid(&'static str),
}

impl ControllerStatusSnapshot {
    pub fn validate(&self) -> Result<(), ControllerStatusError> {
        if self.version != CONTROLLER_STATUS_VERSION {
            return Err(ControllerStatusError::UnsupportedVersion(self.version));
        }
        validate_identity(&self.vm_name)?;
        validate_identity(&self.device_alias)?;
        validate_sha256(&self.compatibility_fingerprint_sha256)?;
        let device = VirtioMemState {
            size_bytes: self.device_size_bytes,
            block_size_bytes: self.block_size_bytes,
            requested_bytes: self.requested_bytes,
            current_bytes: self.current_bytes,
        };
        device
            .validate()
            .map_err(|_| ControllerStatusError::Invalid("live device state is invalid"))?;
        if self.maximum_target_bytes != device.maximum_target_bytes() {
            return Err(ControllerStatusError::Invalid(
                "maximum target contradicts the device geometry",
            ));
        }
        validate_optional_reason(self.guest_input.error.as_deref())?;
        if self.guest_input.error.is_some() == self.guest_input.observed_unix_millis.is_some()
            || (self.guest_input.request.is_some()
                && self.guest_input.observed_unix_millis.is_none())
        {
            return Err(ControllerStatusError::Invalid(
                "guest input must be either observed or explained",
            ));
        }
        match (&self.admitted_request, &self.result) {
            (None, Some(_)) => {
                return Err(ControllerStatusError::Invalid(
                    "result has no admitted request",
                ))
            }
            (Some(request), Some(result)) if result.validate_for(request).is_err() => {
                return Err(ControllerStatusError::Invalid(
                    "result does not match the admitted request",
                ))
            }
            (Some(request), _) if request.validate().is_err() => {
                return Err(ControllerStatusError::Invalid(
                    "admitted request is invalid",
                ))
            }
            _ => {}
        }
        let command_required = matches!(
            self.command_ownership,
            CommandOwnership::OwnedPending
                | CommandOwnership::OwnedConvergedUnresolved
                | CommandOwnership::RecordedNotApplied
                | CommandOwnership::Conflict
        );
        if command_required != self.command.is_some() {
            return Err(ControllerStatusError::Invalid(
                "command ownership and command detail disagree",
            ));
        }
        if matches!(self.command_ownership, CommandOwnership::None)
            && self.requested_bytes != self.current_bytes
        {
            return Err(ControllerStatusError::Invalid(
                "converged ownership contains divergent live state",
            ));
        }
        if matches!(self.command_ownership, CommandOwnership::UnownedPending)
            && self.requested_bytes == self.current_bytes
        {
            return Err(ControllerStatusError::Invalid(
                "unowned pending status is already converged",
            ));
        }
        if let Some(command) = &self.command {
            validate_identity(&command.operation_id)?;
            if !command.target_bytes.is_multiple_of(self.block_size_bytes) {
                return Err(ControllerStatusError::Invalid("command target is invalid"));
            }
        }
        validate_optional_reason(self.latch_reason.as_deref())?;
        validate_optional_reason(self.recovery_reason.as_deref())?;
        validate_optional_reason(self.last_latch_clear_reason.as_deref())?;
        if self.actuation_latched != self.latch_reason.is_some() {
            return Err(ControllerStatusError::Invalid(
                "latch state and reason disagree",
            ));
        }
        if matches!(self.recovery_state, RecoveryState::ReviewRequired)
            != self.recovery_reason.is_some()
        {
            return Err(ControllerStatusError::Invalid(
                "recovery state and reason disagree",
            ));
        }
        Ok(())
    }
}

pub fn parse_controller_status(
    bytes: &[u8],
) -> Result<ControllerStatusSnapshot, ControllerStatusError> {
    if bytes.len() > MAX_CONTROLLER_STATUS_BYTES {
        return Err(ControllerStatusError::Oversized);
    }
    let status: ControllerStatusSnapshot = serde_json::from_slice(bytes)
        .map_err(|error| ControllerStatusError::InvalidJson(error.to_string()))?;
    status.validate()?;
    Ok(status)
}

fn validate_identity(value: &str) -> Result<(), ControllerStatusError> {
    if value.trim().is_empty()
        || value.len() > MAX_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ControllerStatusError::Invalid(
            "identity is empty, oversized, or contains control characters",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), ControllerStatusError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ControllerStatusError::Invalid(
            "fingerprint is not hexadecimal SHA-256",
        ));
    }
    Ok(())
}

fn validate_optional_reason(value: Option<&str>) -> Result<(), ControllerStatusError> {
    if value.is_some_and(|value| {
        value.trim().is_empty()
            || value.len() > MAX_REASON_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err(ControllerStatusError::Invalid(
            "reason is empty, oversized, or contains control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    fn status() -> ControllerStatusSnapshot {
        ControllerStatusSnapshot {
            version: CONTROLLER_STATUS_VERSION,
            observed_unix_millis: 1,
            vm_name: "guest".to_owned(),
            device_alias: "memory0".to_owned(),
            device_size_bytes: 16 * GIB,
            block_size_bytes: 2 * MIB,
            maximum_target_bytes: 16 * GIB,
            requested_bytes: 0,
            current_bytes: 0,
            observe_only: true,
            guest_input: GuestInputStatus {
                error: Some("guest telemetry is stale".to_owned()),
                observed_unix_millis: None,
                request: None,
            },
            admitted_request: None,
            result: None,
            command_ownership: CommandOwnership::None,
            command: None,
            control_health: ControlHealth::Converged,
            actuation_latched: false,
            latch_reason: None,
            recovery_state: RecoveryState::NotRequired,
            recovery_reason: None,
            last_latch_clear_reason: None,
            compatibility_fingerprint_sha256: "b".repeat(64),
        }
    }

    #[test]
    fn round_trips_a_valid_versioned_snapshot() {
        let expected = status();
        let bytes = serde_json::to_vec(&expected).expect("encode status");
        assert_eq!(parse_controller_status(&bytes), Ok(expected));
    }

    #[test]
    fn rejects_unknown_fields_versions_and_inconsistent_states() {
        let mut value = serde_json::to_value(status()).expect("status value");
        value["unexpected"] = serde_json::json!(true);
        assert!(matches!(
            parse_controller_status(&serde_json::to_vec(&value).expect("encode")),
            Err(ControllerStatusError::InvalidJson(_))
        ));

        let mut invalid = status();
        invalid.version += 1;
        assert_eq!(
            invalid.validate(),
            Err(ControllerStatusError::UnsupportedVersion(
                CONTROLLER_STATUS_VERSION + 1
            ))
        );
        let mut invalid = status();
        invalid.command_ownership = CommandOwnership::OwnedPending;
        assert_eq!(
            invalid.validate(),
            Err(ControllerStatusError::Invalid(
                "command ownership and command detail disagree"
            ))
        );
        let mut invalid = status();
        invalid.actuation_latched = true;
        assert_eq!(
            invalid.validate(),
            Err(ControllerStatusError::Invalid(
                "latch state and reason disagree"
            ))
        );
        let mut invalid = status();
        invalid.guest_input.observed_unix_millis = Some(1);
        assert_eq!(
            invalid.validate(),
            Err(ControllerStatusError::Invalid(
                "guest input must be either observed or explained"
            ))
        );
    }

    #[test]
    fn rejects_oversized_input_before_decoding() {
        assert_eq!(
            parse_controller_status(&vec![b' '; MAX_CONTROLLER_STATUS_BYTES + 1]),
            Err(ControllerStatusError::Oversized)
        );
    }
}
