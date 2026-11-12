use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::guest_policy::GuestPolicyDiagnostics;
use crate::memory_request::{valid_identity, MemoryRequest, MemoryResult};

/// Version 5 adds diagnostic explanations; readers still accept version 4.
pub const RAW_TELEMETRY_VERSION: u16 = 5;

/// Native, canonical-byte memory observations collected from the guest. The
/// fields are Windows' counters; the Linux guest service documents how it
/// fills them from `/proc/meminfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTelemetrySnapshot {
    pub physical_total_bytes: u64,
    pub physical_available_bytes: u64,
    pub memory_load_percent: u32,
    pub commit_total_bytes: u64,
    pub commit_limit_bytes: u64,
    pub commit_peak_bytes: u64,
    pub system_cache_bytes: u64,
    pub kernel_paged_bytes: u64,
    pub kernel_nonpaged_bytes: u64,
}

impl MemoryTelemetrySnapshot {
    pub fn validate(&self) -> Result<(), DemandError> {
        if self.physical_total_bytes == 0 {
            return Err(DemandError::ZeroCounter("physical total"));
        }
        if self.commit_limit_bytes == 0 {
            return Err(DemandError::ZeroCounter("commit limit"));
        }
        if self.physical_available_bytes > self.physical_total_bytes {
            return Err(DemandError::InconsistentCounters(
                "physical available exceeds physical total",
            ));
        }
        if self.commit_total_bytes > self.commit_limit_bytes {
            return Err(DemandError::InconsistentCounters(
                "commit total exceeds commit limit",
            ));
        }
        if self.commit_peak_bytes < self.commit_total_bytes {
            return Err(DemandError::InconsistentCounters(
                "commit peak is below commit total",
            ));
        }
        if self.memory_load_percent > 100 {
            return Err(DemandError::InvalidMemoryLoad(self.memory_load_percent));
        }
        Ok(())
    }
}

/// Windows' paired low/high memory resource notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryResourceNotificationState {
    Low,
    Neutral,
    High,
}

/// Observations only, excluded from control/request validation and authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestDiagnostics {
    pub build_id: String,
    /// Windows: the on-disk viomem.sys version, not a claim about a loaded
    /// driver image. Linux: the running kernel release, which carries the
    /// in-tree driver.
    pub driver_file_version: Result<String, String>,
    pub boot_uptime_millis: Option<u64>,
    pub policy: GuestPolicyDiagnostics,
    /// Retained until the next consumed result so a slower recorder sees it.
    pub last_consumed_result: Option<MemoryResult>,
    pub result_consumed_unix_millis: Option<u64>,
}

/// One guest publication: memory counters, Windows' native signal and the
/// guest's one unresolved request. Allocation stays host-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTelemetryEnvelope {
    pub version: u16,
    pub vm_name: String,
    pub service_name: String,
    pub session_id: String,
    pub observed_unix_millis: u64,
    pub monotonic_millis: u64,
    pub sequence: u64,
    pub memory: MemoryTelemetrySnapshot,
    /// `None` when Windows could not report it: growth stays allowed, reclaim
    /// does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_pressure: Option<MemoryResourceNotificationState>,
    /// Retransmitted with every publication until a correlated terminal
    /// result resolves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<MemoryRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<GuestDiagnostics>,
}

impl RawTelemetryEnvelope {
    pub fn new(
        vm_name: impl Into<String>,
        service_name: impl Into<String>,
        session_id: impl Into<String>,
        observed_unix_millis: u64,
        monotonic_millis: u64,
        sequence: u64,
        memory: MemoryTelemetrySnapshot,
    ) -> Self {
        Self {
            version: RAW_TELEMETRY_VERSION,
            vm_name: vm_name.into(),
            service_name: service_name.into(),
            session_id: session_id.into(),
            observed_unix_millis,
            monotonic_millis,
            sequence,
            memory,
            native_pressure: None,
            request: None,
            diagnostics: None,
        }
    }

    pub fn validate_for(
        &self,
        expected_vm_name: &str,
        expected_service_name: &str,
        now_unix_millis: u64,
        max_age_millis: u64,
        future_tolerance_millis: u64,
    ) -> Result<(), DemandError> {
        if !matches!(self.version, 4 | RAW_TELEMETRY_VERSION) {
            return Err(DemandError::UnsupportedTelemetryVersion(self.version));
        }
        for (field, value) in [
            ("vm_name", self.vm_name.as_str()),
            ("service_name", self.service_name.as_str()),
            ("session_id", self.session_id.as_str()),
        ] {
            if !valid_identity(value) {
                return Err(DemandError::InvalidTelemetryIdentity(field));
            }
        }
        if self.vm_name != expected_vm_name {
            return Err(DemandError::WrongTelemetryVm {
                expected: expected_vm_name.to_owned(),
                actual: self.vm_name.clone(),
            });
        }
        if self.service_name != expected_service_name {
            return Err(DemandError::WrongTelemetryService {
                expected: expected_service_name.to_owned(),
                actual: self.service_name.clone(),
            });
        }
        if self.observed_unix_millis > now_unix_millis.saturating_add(future_tolerance_millis) {
            return Err(DemandError::FutureTelemetry {
                observed: self.observed_unix_millis,
                now: now_unix_millis,
            });
        }
        if now_unix_millis.saturating_sub(self.observed_unix_millis) > max_age_millis {
            return Err(DemandError::StaleTelemetry {
                observed: self.observed_unix_millis,
                now: now_unix_millis,
            });
        }
        if let Some(request) = &self.request {
            request
                .validate()
                .map_err(DemandError::InvalidMemoryRequest)?;
            if request.identity.vm_name != self.vm_name
                || request.identity.service_name != self.service_name
            {
                return Err(DemandError::InvalidMemoryRequest(
                    "request identity differs from its publication".to_owned(),
                ));
            }
        }
        self.memory.validate()
    }

    /// Validates ordering against the last accepted record from this producer.
    pub fn validate_successor(&self, previous: &Self) -> Result<(), DemandError> {
        if self.session_id == previous.session_id {
            if self.sequence <= previous.sequence {
                return Err(DemandError::ReplayedTelemetrySequence(self.sequence));
            }
            if self.monotonic_millis <= previous.monotonic_millis {
                return Err(DemandError::NonIncreasingTelemetryMonotonicClock(
                    self.monotonic_millis,
                ));
            }
        }
        if self.observed_unix_millis < previous.observed_unix_millis {
            return Err(DemandError::TelemetryWallClockMovedBackwards);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DemandError {
    #[error("embedded memory request is invalid: {0}")]
    InvalidMemoryRequest(String),
    #[error("memory counter must be greater than zero: {0}")]
    ZeroCounter(&'static str),
    #[error("inconsistent memory counters: {0}")]
    InconsistentCounters(&'static str),
    #[error("memory load percentage is outside 0..=100: {0}")]
    InvalidMemoryLoad(u32),
    #[error("memory counter arithmetic overflow")]
    ArithmeticOverflow,
    #[error("native Windows memory telemetry is unavailable on this platform")]
    UnsupportedPlatform,
    #[error("GetPerformanceInfo failed: {0}")]
    PerformanceInfo(u32),
    #[error("unsupported raw telemetry version: {0}")]
    UnsupportedTelemetryVersion(u16),
    #[error("raw telemetry identity field is invalid: {0}")]
    InvalidTelemetryIdentity(&'static str),
    #[error("raw telemetry belongs to VM {actual}, expected {expected}")]
    WrongTelemetryVm { expected: String, actual: String },
    #[error("raw telemetry belongs to service {actual}, expected {expected}")]
    WrongTelemetryService { expected: String, actual: String },
    #[error("raw telemetry observation {observed} is stale at {now}")]
    StaleTelemetry { observed: u64, now: u64 },
    #[error("raw telemetry observation {observed} is in the future at {now}")]
    FutureTelemetry { observed: u64, now: u64 },
    #[error("raw telemetry sequence is replayed or non-increasing: {0}")]
    ReplayedTelemetrySequence(u64),
    #[error("raw telemetry monotonic clock is non-increasing: {0}")]
    NonIncreasingTelemetryMonotonicClock(u64),
    #[error("raw telemetry wall clock moved backwards across records")]
    TelemetryWallClockMovedBackwards,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn snapshot() -> MemoryTelemetrySnapshot {
        MemoryTelemetrySnapshot {
            physical_total_bytes: 16 * GIB,
            physical_available_bytes: 2 * GIB,
            memory_load_percent: 88,
            commit_total_bytes: 15 * GIB,
            commit_limit_bytes: 16 * GIB,
            commit_peak_bytes: 15 * GIB,
            system_cache_bytes: 0,
            kernel_paged_bytes: 0,
            kernel_nonpaged_bytes: 0,
        }
    }

    fn envelope(
        session: &str,
        observed: u64,
        monotonic: u64,
        sequence: u64,
    ) -> RawTelemetryEnvelope {
        RawTelemetryEnvelope::new(
            "guest",
            "VirtioMemService",
            session,
            observed,
            monotonic,
            sequence,
            snapshot(),
        )
    }

    #[test]
    fn version_four_without_diagnostics_remains_readable() {
        let mut old = envelope("old-service", 995_000, 10, 0);
        old.version = 4;
        let bytes = serde_json::to_vec(&old).unwrap();
        let parsed: RawTelemetryEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.diagnostics.is_none());
        assert_eq!(
            parsed.validate_for("guest", "VirtioMemService", 1_000_000, 60_000, 5000),
            Ok(())
        );
        let mut new = old;
        new.version = RAW_TELEMETRY_VERSION;
        new.diagnostics = Some(GuestDiagnostics {
            build_id: "test".to_owned(),
            driver_file_version: Err("not available".to_owned()),
            boot_uptime_millis: Some(1234),
            policy: GuestPolicyDiagnostics::default(),
            last_consumed_result: None,
            result_consumed_unix_millis: None,
        });
        let parsed: RawTelemetryEnvelope =
            serde_json::from_slice(&serde_json::to_vec(&new).unwrap()).unwrap();
        assert_eq!(parsed, new);
        assert_eq!(
            parsed.validate_for("guest", "VirtioMemService", 1_000_000, 60_000, 5000),
            Ok(())
        );
    }

    #[test]
    fn validates_fresh_vm_scoped_raw_telemetry() {
        let current = envelope("session-a", 995_000, 10, 0);
        let check = |envelope: &RawTelemetryEnvelope, vm, service, now| {
            envelope.validate_for(vm, service, now, 60_000, 5_000)
        };
        assert_eq!(
            check(&current, "guest", "VirtioMemService", 1_000_000),
            Ok(())
        );
        assert!(check(&current, "other", "VirtioMemService", 1_000_000).is_err());
        assert!(check(&current, "guest", "other-service", 1_000_000).is_err());
        assert!(check(&current, "guest", "VirtioMemService", 1_056_000).is_err());
        let future = envelope("session-a", 1_006_000, 20, 1);
        assert!(check(&future, "guest", "VirtioMemService", 1_000_000).is_err());

        let mut old = current.clone();
        old.version = 3;
        assert_eq!(
            check(&old, "guest", "VirtioMemService", 1_000_000),
            Err(DemandError::UnsupportedTelemetryVersion(3))
        );
        let mut control = current.clone();
        control.session_id = "session\n".to_owned();
        assert!(check(&control, "guest", "VirtioMemService", 1_000_000).is_err());
    }

    #[test]
    fn round_trips_native_pressure_and_rejects_unknown_fields() {
        let mut current = envelope("session-a", 1_000, 10, 0);
        let encoded = serde_json::to_value(&current).expect("encode");
        assert!(encoded.get("native_pressure").is_none());
        current.native_pressure = Some(MemoryResourceNotificationState::Low);
        let encoded = serde_json::to_string(&current).expect("encode");
        assert!(encoded.contains(r#""native_pressure":"low""#));
        assert_eq!(
            serde_json::from_str::<RawTelemetryEnvelope>(&encoded).expect("decode"),
            current
        );
        let mut extended = serde_json::to_value(&current).expect("encode");
        extended
            .as_object_mut()
            .expect("object")
            .insert("windows_native".to_owned(), serde_json::json!({}));
        assert!(serde_json::from_value::<RawTelemetryEnvelope>(extended).is_err());
    }

    #[test]
    fn validates_sequence_and_session_ordering() {
        let first = envelope("session-a", 1_000, 10, 0);
        let next = envelope("session-a", 1_001, 20, 1);
        assert_eq!(next.validate_successor(&first), Ok(()));
        assert_eq!(
            first.validate_successor(&first),
            Err(DemandError::ReplayedTelemetrySequence(0))
        );
        let restarted = envelope("session-b", 1_002, 1, 0);
        assert_eq!(restarted.validate_successor(&next), Ok(()));
        let backwards = envelope("session-c", 999, 1, 0);
        assert_eq!(
            backwards.validate_successor(&next),
            Err(DemandError::TelemetryWallClockMovedBackwards)
        );
    }

    #[test]
    fn rejects_inconsistent_counters() {
        let mut memory = snapshot();
        memory.physical_available_bytes = memory.physical_total_bytes + 1;
        assert!(memory.validate().is_err());
        let mut memory = snapshot();
        memory.commit_peak_bytes = memory.commit_total_bytes - 1;
        assert!(memory.validate().is_err());
        let mut memory = snapshot();
        memory.memory_load_percent = 101;
        assert_eq!(memory.validate(), Err(DemandError::InvalidMemoryLoad(101)));
    }
}
