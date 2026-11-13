//! Absolute target estimation from raw Windows memory telemetry.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MemoryResourceNotificationState, MemoryTelemetrySnapshot};

const MAX_TARGET_HISTORY_ENTRIES: usize = 4096;
#[cfg(test)]
const MIB: u64 = 1024 * 1024;
#[cfg(test)]
const GIB: u64 = 1024 * MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPolicyConfig {
    /// Device bytes never planned below; may be zero.
    #[serde(with = "crate::units::byte_size")]
    pub configured_minimum_bytes: u64,
    #[serde(with = "crate::units::byte_size")]
    pub physical_reserve_bytes: u64,
    #[serde(with = "crate::units::byte_size")]
    pub commit_reserve_bytes: u64,
    #[serde(with = "crate::units::byte_size")]
    pub safe_floor_physical_reserve_bytes: u64,
    #[serde(with = "crate::units::byte_size")]
    pub safe_floor_commit_reserve_bytes: u64,
    pub history_window_millis: u64,
    pub maximum_gap_millis: u64,
    #[serde(with = "crate::units::byte_size")]
    pub downward_hysteresis_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetGeometry {
    pub device_size_bytes: u64,
    pub block_size_bytes: u64,
    pub current_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSample {
    pub session_id: String,
    pub observed_unix_millis: u64,
    pub monotonic_millis: u64,
    pub sequence: u64,
    pub memory: MemoryTelemetrySnapshot,
    /// None means failed/unavailable native observation: growth is allowed, reclaim is not.
    pub native_pressure: Option<MemoryResourceNotificationState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstantaneousTarget {
    pub desired_now_bytes: u64,
    pub floor_now_bytes: u64,
    pub effective_maximum_bytes: u64,
    pub capacity_limited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetEstimate {
    pub desired_bytes: u64,
    pub safe_floor_bytes: u64,
    pub desired_now_bytes: u64,
    pub floor_now_bytes: u64,
    pub effective_maximum_bytes: u64,
    pub history_ready: bool,
    pub capacity_limited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateHistoryEntry {
    monotonic_millis: u64,
    desired_now_bytes: u64,
    /// `desired_now_bytes` plus the margin, where the RAM limits allow it.
    target_now_bytes: u64,
    floor_now_bytes: u64,
}

/// Process-local estimator state. A restarted guest starts cold, so reclaim
/// waits for a full history window again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TargetEstimatorState {
    desired_bytes: u64,
    safe_floor_bytes: u64,
    session_id: String,
    last_monotonic_millis: u64,
    last_sequence: u64,
    last_visible_base_bytes: Option<u64>,
    history: VecDeque<CandidateHistoryEntry>,
}

impl TargetEstimatorState {
    fn invalidate_history(&mut self) {
        self.history.clear();
        self.safe_floor_bytes = self.desired_bytes;
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TargetEstimatorError {
    #[error("target policy is invalid: {0}")]
    InvalidPolicy(&'static str),
    #[error("target geometry is invalid: {0}")]
    InvalidGeometry(&'static str),
    #[error("target arithmetic overflow")]
    ArithmeticOverflow,
    #[error("target telemetry ordering is invalid: {0}")]
    InvalidOrdering(&'static str),
    #[error("memory telemetry is invalid: {0}")]
    InvalidTelemetry(String),
}

impl TargetPolicyConfig {
    pub fn validate(&self) -> Result<(), TargetEstimatorError> {
        if self.physical_reserve_bytes == 0
            || self.commit_reserve_bytes == 0
            || self.safe_floor_physical_reserve_bytes == 0
            || self.safe_floor_commit_reserve_bytes == 0
            || self.history_window_millis == 0
            || self.maximum_gap_millis == 0
            || self.downward_hysteresis_bytes == 0
        {
            return Err(TargetEstimatorError::InvalidPolicy(
                "byte values and durations must be positive",
            ));
        }
        if self.safe_floor_physical_reserve_bytes > self.physical_reserve_bytes
            || self.safe_floor_commit_reserve_bytes > self.commit_reserve_bytes
        {
            return Err(TargetEstimatorError::InvalidPolicy(
                "safe-floor reserves exceed normal reserves",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TargetEstimator {
    config: TargetPolicyConfig,
    min_ram: u64,
    max_ram: u64,
    state: TargetEstimatorState,
}

impl TargetEstimator {
    pub fn new(config: TargetPolicyConfig) -> Result<Self, TargetEstimatorError> {
        config.validate()?;
        Ok(Self {
            config,
            min_ram: 0,
            max_ram: u64::MAX,
            state: TargetEstimatorState::default(),
        })
    }

    pub fn set_ram_limits(&mut self, minimum: u64, maximum: u64) -> Result<(), String> {
        if minimum > maximum {
            return Err("min_ram exceeds max_ram".to_owned());
        }
        self.state.invalidate_history();
        self.min_ram = minimum;
        self.max_ram = maximum;
        Ok(())
    }

    /// Discard reclaim evidence after an observation failure or missing geometry.
    pub fn invalidate_history(&mut self) {
        self.state.invalidate_history();
    }

    pub fn estimate(
        &mut self,
        sample: TargetSample,
        geometry: TargetGeometry,
    ) -> Result<TargetEstimate, TargetEstimatorError> {
        let mut instantaneous =
            match calculate_instantaneous(&self.config, &sample.memory, geometry) {
                Ok(value) => value,
                Err(error) => {
                    self.state.invalidate_history();
                    return Err(error);
                }
            };
        if let Err(error) = self.prepare_sample(&sample) {
            self.state.invalidate_history();
            return Err(error);
        }

        let visible_base = sample.memory.physical_total_bytes - geometry.current_bytes;
        // The administrator chooses total RAM, with no configured boot/base RAM.
        // Derive the removable device portion from the same validated sample.
        let block = geometry.block_size_bytes;
        let minimum = align_up(self.min_ram.saturating_sub(visible_base), block)?;
        let maximum = self.max_ram.saturating_sub(visible_base) / block * block;
        let maximum = maximum.min(instantaneous.effective_maximum_bytes);
        if minimum > maximum || self.max_ram < visible_base {
            self.state.invalidate_history();
            return Err(TargetEstimatorError::InvalidGeometry(
                "configured RAM limits cannot be met by this VM's live memory geometry",
            ));
        }
        instantaneous.effective_maximum_bytes = maximum;
        // Growth and reclaim both aim half a hysteresis band above demand.
        // Aiming at demand itself leaves the device on the growth threshold,
        // where every fluctuation in guest memory costs a host resize.
        let margin = align_up(self.config.downward_hysteresis_bytes / 2, block)?;
        let target_now = instantaneous
            .desired_now_bytes
            .saturating_add(margin)
            .clamp(minimum, maximum);
        instantaneous.desired_now_bytes = instantaneous.desired_now_bytes.clamp(minimum, maximum);
        instantaneous.floor_now_bytes = instantaneous.floor_now_bytes.clamp(minimum, maximum);

        if self
            .state
            .last_visible_base_bytes
            .is_some_and(|base| base != visible_base)
        {
            // Hotplug observations and host status need not arrive atomically.
            // Never use mixed-geometry history as evidence for releasing RAM.
            self.state.invalidate_history();
        }
        self.state.last_visible_base_bytes = Some(visible_base);
        if self.state.session_id.is_empty() {
            self.state.desired_bytes = geometry.current_bytes;
            self.state.safe_floor_bytes = geometry.current_bytes;
        }
        if instantaneous.desired_now_bytes > self.state.desired_bytes {
            self.state.desired_bytes = target_now;
        }
        self.state.desired_bytes = self
            .state
            .desired_bytes
            .min(instantaneous.effective_maximum_bytes);

        // Reclaim requires a continuous window of native High observations.
        // Neutral, Low, failed and unavailable observations all restart that window.
        if sample.native_pressure != Some(MemoryResourceNotificationState::High) {
            self.state.invalidate_history();
        } else {
            // Conservative time buckets bound memory without losing a full window
            // when notification wakeups or short poll intervals produce many samples.
            let bucket = self
                .config
                .history_window_millis
                .div_ceil((MAX_TARGET_HISTORY_ENTRIES - 2) as u64)
                .max(1);
            if let Some(last) = self.state.history.back_mut().filter(|last| {
                sample
                    .monotonic_millis
                    .saturating_sub(last.monotonic_millis)
                    < bucket
            }) {
                last.desired_now_bytes =
                    last.desired_now_bytes.max(instantaneous.desired_now_bytes);
                last.target_now_bytes = last.target_now_bytes.max(target_now);
                last.floor_now_bytes = last.floor_now_bytes.max(instantaneous.floor_now_bytes);
            } else {
                self.state.history.push_back(CandidateHistoryEntry {
                    monotonic_millis: sample.monotonic_millis,
                    desired_now_bytes: instantaneous.desired_now_bytes,
                    target_now_bytes: target_now,
                    floor_now_bytes: instantaneous.floor_now_bytes,
                });
            }
        }
        if self.state.history.len() > MAX_TARGET_HISTORY_ENTRIES {
            self.state.history.pop_front();
        }
        prune_history(
            &mut self.state.history,
            sample.monotonic_millis,
            self.config.history_window_millis,
        );
        let history_ready = history_is_ready(
            &self.state.history,
            sample.monotonic_millis,
            self.config.history_window_millis,
        );
        if history_ready {
            let window_desired = self
                .state
                .history
                .iter()
                .map(|entry| entry.desired_now_bytes)
                .max()
                .ok_or(TargetEstimatorError::InvalidOrdering(
                    "empty qualified history",
                ))?;
            if window_desired
                .checked_add(self.config.downward_hysteresis_bytes)
                .ok_or(TargetEstimatorError::ArithmeticOverflow)?
                <= self.state.desired_bytes
            {
                // At most `window_desired` plus the margin, so never a rise.
                self.state.desired_bytes = self
                    .state
                    .history
                    .iter()
                    .map(|entry| entry.target_now_bytes)
                    .max()
                    .unwrap_or(window_desired);
            }
            self.state.safe_floor_bytes = self
                .state
                .history
                .iter()
                .map(|entry| entry.floor_now_bytes)
                .max()
                .ok_or(TargetEstimatorError::InvalidOrdering(
                    "empty qualified history",
                ))?
                .min(self.state.desired_bytes);
        } else {
            self.state.safe_floor_bytes = geometry.current_bytes.min(self.state.desired_bytes);
        }

        self.state.session_id = sample.session_id;
        self.state.last_monotonic_millis = sample.monotonic_millis;
        self.state.last_sequence = sample.sequence;

        Ok(TargetEstimate {
            desired_bytes: self.state.desired_bytes,
            safe_floor_bytes: self.state.safe_floor_bytes,
            desired_now_bytes: instantaneous.desired_now_bytes,
            floor_now_bytes: instantaneous.floor_now_bytes,
            effective_maximum_bytes: instantaneous.effective_maximum_bytes,
            history_ready,
            capacity_limited: instantaneous.capacity_limited,
        })
    }

    fn prepare_sample(&mut self, sample: &TargetSample) -> Result<(), TargetEstimatorError> {
        if sample.session_id.trim().is_empty() {
            return Err(TargetEstimatorError::InvalidOrdering(
                "empty producer session",
            ));
        }
        if self.state.session_id.is_empty() {
            return Ok(());
        }
        if sample.session_id != self.state.session_id {
            self.state.history.clear();
            return Ok(());
        }
        if sample.sequence <= self.state.last_sequence
            || sample.monotonic_millis <= self.state.last_monotonic_millis
        {
            return Err(TargetEstimatorError::InvalidOrdering(
                "sample did not advance within the producer session",
            ));
        }
        if sample
            .monotonic_millis
            .saturating_sub(self.state.last_monotonic_millis)
            > self.config.maximum_gap_millis
        {
            self.state.history.clear();
        }
        Ok(())
    }
}

fn calculate_instantaneous(
    config: &TargetPolicyConfig,
    memory: &MemoryTelemetrySnapshot,
    geometry: TargetGeometry,
) -> Result<InstantaneousTarget, TargetEstimatorError> {
    let effective_maximum_bytes = calculate_effective_maximum(config, geometry)?;
    memory
        .validate()
        .map_err(|error| TargetEstimatorError::InvalidTelemetry(error.to_string()))?;

    // The fixed (non-virtio-mem) RAM the guest sees, from this sample.
    let visible_base = memory
        .physical_total_bytes
        .checked_sub(geometry.current_bytes)
        .ok_or(TargetEstimatorError::InvalidGeometry(
            "physical total is below current allocation",
        ))?;

    let desired_raw = candidate(
        memory,
        geometry.current_bytes,
        visible_base,
        config.physical_reserve_bytes,
        config.commit_reserve_bytes,
    )?
    .max(config.configured_minimum_bytes);
    let floor_raw = candidate(
        memory,
        geometry.current_bytes,
        visible_base,
        config.safe_floor_physical_reserve_bytes,
        config.safe_floor_commit_reserve_bytes,
    )?
    .max(config.configured_minimum_bytes);
    let capacity_limited = desired_raw > effective_maximum_bytes;
    let desired_now_bytes = align_up(
        desired_raw.min(effective_maximum_bytes),
        geometry.block_size_bytes,
    )?
    .min(effective_maximum_bytes);
    let floor_now_bytes = align_up(
        floor_raw.min(effective_maximum_bytes),
        geometry.block_size_bytes,
    )?
    .min(desired_now_bytes);

    Ok(InstantaneousTarget {
        desired_now_bytes,
        floor_now_bytes,
        effective_maximum_bytes,
        capacity_limited,
    })
}

/// Returns the aligned usable target ceiling after validating policy and live
/// device geometry, without consuming telemetry or changing estimator state.
fn calculate_effective_maximum(
    config: &TargetPolicyConfig,
    geometry: TargetGeometry,
) -> Result<u64, TargetEstimatorError> {
    config.validate()?;
    if geometry.device_size_bytes == 0 {
        return Err(TargetEstimatorError::InvalidGeometry(
            "device size must be greater than zero",
        ));
    }
    if geometry.block_size_bytes == 0 || !geometry.block_size_bytes.is_power_of_two() {
        return Err(TargetEstimatorError::InvalidGeometry(
            "block size must be a non-zero power of two",
        ));
    }
    if !geometry
        .current_bytes
        .is_multiple_of(geometry.block_size_bytes)
    {
        return Err(TargetEstimatorError::InvalidGeometry(
            "current allocation is not block aligned",
        ));
    }
    let effective_maximum_bytes = crate::virtio_mem::maximum_target_bytes(
        geometry.device_size_bytes,
        geometry.block_size_bytes,
    );
    if config.configured_minimum_bytes > effective_maximum_bytes
        || !config
            .configured_minimum_bytes
            .is_multiple_of(geometry.block_size_bytes)
        || geometry.current_bytes > effective_maximum_bytes
    {
        return Err(TargetEstimatorError::InvalidGeometry(
            "minimum or current allocation exceeds the aligned effective maximum",
        ));
    }
    for value in [
        config.physical_reserve_bytes,
        config.commit_reserve_bytes,
        config.safe_floor_physical_reserve_bytes,
        config.safe_floor_commit_reserve_bytes,
        config.downward_hysteresis_bytes,
    ] {
        if !value.is_multiple_of(geometry.block_size_bytes) {
            return Err(TargetEstimatorError::InvalidGeometry(
                "reserves and hysteresis must be block aligned",
            ));
        }
    }
    Ok(effective_maximum_bytes)
}

fn candidate(
    memory: &MemoryTelemetrySnapshot,
    current_bytes: u64,
    fixed_visible_base_bytes: u64,
    physical_reserve_bytes: u64,
    commit_reserve_bytes: u64,
) -> Result<u64, TargetEstimatorError> {
    let physical_used = memory
        .physical_total_bytes
        .checked_sub(memory.physical_available_bytes)
        .ok_or(TargetEstimatorError::ArithmeticOverflow)?;
    let physical_candidate = physical_used
        .checked_add(physical_reserve_bytes)
        .ok_or(TargetEstimatorError::ArithmeticOverflow)?
        .saturating_sub(fixed_visible_base_bytes);
    let commit_headroom = memory
        .commit_limit_bytes
        .checked_sub(memory.commit_total_bytes)
        .ok_or(TargetEstimatorError::ArithmeticOverflow)?;
    let commit_candidate = current_bytes
        .checked_add(commit_reserve_bytes)
        .ok_or(TargetEstimatorError::ArithmeticOverflow)?
        .saturating_sub(commit_headroom);
    Ok(physical_candidate.max(commit_candidate))
}

fn align_up(value: u64, alignment: u64) -> Result<u64, TargetEstimatorError> {
    let remainder = value % alignment;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(alignment - remainder)
            .ok_or(TargetEstimatorError::ArithmeticOverflow)
    }
}

fn prune_history(history: &mut VecDeque<CandidateHistoryEntry>, now: u64, window: u64) {
    let boundary = now.saturating_sub(window);
    while history.len() > 1
        && history
            .get(1)
            .is_some_and(|entry| entry.monotonic_millis <= boundary)
    {
        history.pop_front();
    }
}

fn history_is_ready(history: &VecDeque<CandidateHistoryEntry>, now: u64, window: u64) -> bool {
    let Some(first) = history.front() else {
        return false;
    };
    if now.saturating_sub(first.monotonic_millis) < window {
        return false;
    }
    // prepare_sample checks every real observation gap before bucketing.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TargetPolicyConfig {
        TargetPolicyConfig {
            configured_minimum_bytes: 2 * GIB,
            physical_reserve_bytes: 2 * GIB,
            commit_reserve_bytes: 2 * GIB,
            safe_floor_physical_reserve_bytes: GIB,
            safe_floor_commit_reserve_bytes: GIB,
            history_window_millis: 600_000,
            maximum_gap_millis: 60_000,
            downward_hysteresis_bytes: 256 * MIB,
        }
    }

    fn geometry(current: u64) -> TargetGeometry {
        TargetGeometry {
            device_size_bytes: 24 * GIB,
            block_size_bytes: 2 * MIB,
            current_bytes: current,
        }
    }

    fn memory(current: u64, used_dynamic: u64, commit_headroom: u64) -> MemoryTelemetrySnapshot {
        let total = 8 * GIB + current;
        let used = 8 * GIB + used_dynamic;
        MemoryTelemetrySnapshot {
            physical_total_bytes: total,
            physical_available_bytes: total - used,
            memory_load_percent: 50,
            commit_total_bytes: 32 * GIB - commit_headroom,
            commit_limit_bytes: 32 * GIB,
            commit_peak_bytes: 32 * GIB - commit_headroom,
            system_cache_bytes: 0,
            kernel_paged_bytes: 0,
            kernel_nonpaged_bytes: 0,
        }
    }

    fn sample(at: u64, sequence: u64, memory: MemoryTelemetrySnapshot) -> TargetSample {
        TargetSample {
            session_id: "session-a".to_owned(),
            observed_unix_millis: at,
            monotonic_millis: at,
            sequence,
            memory,
            native_pressure: Some(MemoryResourceNotificationState::High),
        }
    }

    #[test]
    fn total_ram_limits_derive_device_bounds_from_each_live_sample() {
        let mut estimator = TargetEstimator::new(config()).unwrap();
        estimator.set_ram_limits(14 * GIB, 16 * GIB).unwrap();
        let result = estimator
            .estimate(
                sample(1, 0, memory(4 * GIB, 0, 32 * GIB)),
                geometry(4 * GIB),
            )
            .unwrap();
        assert_eq!(
            result.desired_bytes,
            6 * GIB,
            "minimum total is 14 with observed base 8"
        );
        let mut estimator = TargetEstimator::new(config()).unwrap();
        estimator.set_ram_limits(4 * GIB, 13 * GIB).unwrap();
        let result = estimator
            .estimate(sample(1, 0, memory(4 * GIB, 4 * GIB, 0)), geometry(4 * GIB))
            .unwrap();
        assert_eq!(
            result.desired_bytes,
            5 * GIB,
            "growth respects maximum total 13"
        );
        assert_eq!(result.effective_maximum_bytes, 5 * GIB);
        estimator.set_ram_limits(GIB, 4 * GIB).unwrap();
        assert!(
            estimator
                .estimate(
                    sample(2, 1, memory(4 * GIB, 0, 32 * GIB)),
                    geometry(4 * GIB)
                )
                .is_err(),
            "maximum below observable non-device memory is impossible"
        );
    }

    #[test]
    fn initialized_zero_does_not_reinitialize_to_current() {
        let policy = TargetPolicyConfig {
            configured_minimum_bytes: 0,
            ..config()
        };
        let mut idle = memory(0, 0, 32 * GIB);
        idle.physical_available_bytes = idle.physical_total_bytes;
        let mut estimator = TargetEstimator::new(policy).unwrap();
        assert_eq!(
            estimator
                .estimate(sample(1, 0, idle), geometry(0))
                .unwrap()
                .desired_bytes,
            0
        );
        let mut idle = memory(GIB, 0, 32 * GIB);
        idle.physical_available_bytes = idle.physical_total_bytes;
        let next = estimator
            .estimate(sample(60_001, 1, idle), geometry(GIB))
            .unwrap();
        assert_eq!(next.desired_bytes, 0);
        assert_eq!(next.safe_floor_bytes, 0);
    }

    #[test]
    fn calculates_separate_physical_and_commit_candidates() {
        let physical = calculate_instantaneous(
            &config(),
            &memory(8 * GIB, 6 * GIB, 8 * GIB),
            geometry(8 * GIB),
        )
        .expect("physical candidate");
        assert_eq!(physical.desired_now_bytes, 8 * GIB);
        assert_eq!(physical.floor_now_bytes, 7 * GIB);

        let commit = calculate_instantaneous(
            &config(),
            &memory(8 * GIB, GIB, 512 * MIB),
            geometry(8 * GIB),
        )
        .expect("commit candidate");
        assert_eq!(commit.desired_now_bytes, 10 * GIB - 512 * MIB);
        assert_eq!(commit.floor_now_bytes, 9 * GIB - 512 * MIB);
    }

    #[test]
    fn rejects_effective_maximum_failures() {
        let too_small = TargetGeometry {
            device_size_bytes: 2 * GIB,
            block_size_bytes: 2 * MIB,
            current_bytes: 2 * GIB + 2 * MIB,
        };
        assert!(calculate_instantaneous(&config(), &memory(2 * GIB, GIB, GIB), too_small).is_err());
    }

    #[test]
    fn accepts_small_devices_and_full_current_allocation() {
        let policy = TargetPolicyConfig {
            configured_minimum_bytes: 0,
            ..config()
        };
        for size in [512 * MIB, GIB, 24 * GIB] {
            let geometry = TargetGeometry {
                device_size_bytes: size,
                block_size_bytes: 2 * MIB,
                current_bytes: size,
            };
            assert_eq!(calculate_effective_maximum(&policy, geometry), Ok(size));
        }
        assert!(calculate_effective_maximum(
            &policy,
            TargetGeometry {
                device_size_bytes: 0,
                block_size_bytes: 2 * MIB,
                current_bytes: 0,
            }
        )
        .is_err());
    }

    #[test]
    fn reports_capacity_limited_and_preserves_alignment() {
        let result = calculate_instantaneous(
            &config(),
            &memory(24 * GIB, 24 * GIB, 0),
            geometry(24 * GIB),
        )
        .expect("bounded target");
        // The full device size is the only limit.
        assert_eq!(result.effective_maximum_bytes, 24 * GIB);
        assert_eq!(result.desired_now_bytes, 24 * GIB);
        assert!(result.capacity_limited);
    }

    #[test]
    fn grows_immediately_then_lowers_only_after_complete_window() {
        let mut estimator = TargetEstimator::new(config()).expect("valid estimator");
        let initial = estimator
            .estimate(
                sample(1_000_000, 0, memory(8 * GIB, 8 * GIB, GIB)),
                geometry(8 * GIB),
            )
            .expect("growth estimate");
        assert_eq!(initial.desired_bytes, 10 * GIB + 128 * MIB);
        assert!(!initial.history_ready);
        assert_eq!(initial.safe_floor_bytes, 8 * GIB);

        for sequence in 1..=11 {
            let result = estimator
                .estimate(
                    sample(
                        1_000_000 + sequence * 60_000,
                        sequence,
                        memory(8 * GIB, 2 * GIB, 8 * GIB),
                    ),
                    geometry(8 * GIB),
                )
                .expect("window sample");
            if sequence < 11 {
                assert_eq!(result.desired_bytes, 10 * GIB + 128 * MIB);
            } else {
                assert!(result.history_ready);
                assert_eq!(result.desired_bytes, 4 * GIB + 128 * MIB);
                assert_eq!(result.safe_floor_bytes, 3 * GIB);
            }
        }
    }

    #[test]
    fn growth_and_reclaim_aim_half_a_band_above_demand_so_noise_causes_no_resize() {
        let policy = TargetPolicyConfig {
            configured_minimum_bytes: 0,
            history_window_millis: 1_000,
            ..config()
        };
        let mut estimator = TargetEstimator::new(policy).unwrap();
        let mut sequence = 0;
        let mut estimate = |used: u64, current: u64| {
            sequence += 1;
            estimator
                .estimate(
                    sample(sequence * 1_000, sequence, memory(current, used, 32 * GIB)),
                    geometry(current),
                )
                .unwrap()
        };
        // 2 MiB short of the 2 GiB reserve with 4 GiB plugged.
        let short = estimate(2 * GIB + 2 * MIB, 4 * GIB);
        assert_eq!(short.desired_now_bytes, 4 * GIB + 2 * MIB);
        assert_eq!(short.desired_bytes, 4 * GIB + 130 * MIB);

        // Use that moves less than the margin either way changes nothing.
        let grown = 4 * GIB + 130 * MIB;
        for used in [2 * GIB + 2 * MIB, 2 * GIB + 130 * MIB, 2 * GIB - 124 * MIB] {
            assert_eq!(estimate(used, grown).desired_bytes, grown, "{used}");
        }
        // A full band of surplus is returned, again to half a band above use.
        estimate(GIB, grown);
        let idle = estimate(GIB, grown);
        assert!(idle.history_ready);
        assert_eq!(idle.desired_bytes, 3 * GIB + 128 * MIB);
    }

    #[test]
    fn the_margin_never_lifts_the_target_past_the_ram_limits() {
        let policy = TargetPolicyConfig {
            configured_minimum_bytes: 0,
            history_window_millis: 1_000,
            ..config()
        };
        // Fixed RAM is 8 GiB; an idle guest returns to min_ram exactly.
        let mut estimator = TargetEstimator::new(policy).unwrap();
        estimator.set_ram_limits(10 * GIB, 12 * GIB).unwrap();
        let mut idle = memory(4 * GIB, 0, 32 * GIB);
        idle.physical_available_bytes = idle.physical_total_bytes - GIB;
        estimator
            .estimate(sample(1_000, 0, idle), geometry(4 * GIB))
            .unwrap();
        let settled = estimator
            .estimate(sample(2_000, 1, idle), geometry(4 * GIB))
            .unwrap();
        assert_eq!(settled.desired_bytes, 2 * GIB);
        // Demand just under max_ram is capped at it.
        let busy = estimator
            .estimate(
                sample(3_000, 2, memory(4 * GIB, 2 * GIB - 2 * MIB, 32 * GIB)),
                geometry(4 * GIB),
            )
            .unwrap();
        assert_eq!(busy.desired_bytes, 4 * GIB);
    }

    #[test]
    fn four_gibibyte_growth_trace_settles_at_two_gibibytes_extra() {
        let mut estimator = TargetEstimator::new(config()).expect("valid estimator");
        let first = estimator
            .estimate(
                sample(1_000_000, 0, memory(8 * GIB, 8 * GIB, 0)),
                geometry(8 * GIB),
            )
            .expect("first growth sample");
        assert_eq!(first.desired_bytes, 10 * GIB + 128 * MIB);

        let second = estimator
            .estimate(
                sample(1_060_000, 1, memory(10 * GIB, 10 * GIB, 0)),
                geometry(10 * GIB),
            )
            .expect("second growth sample");
        assert_eq!(second.desired_bytes, 12 * GIB + 128 * MIB);

        for sequence in 2..=12 {
            let result = estimator
                .estimate(
                    sample(
                        1_000_000 + sequence * 60_000,
                        sequence,
                        memory(12 * GIB, 8 * GIB, 8 * GIB),
                    ),
                    geometry(12 * GIB),
                )
                .expect("settling sample");
            if sequence == 12 {
                assert!(result.history_ready);
                assert_eq!(result.desired_bytes, 10 * GIB + 128 * MIB);
            }
        }
    }

    #[test]
    fn gap_and_new_session_restart_reclaim_warmup() {
        let mut estimator = TargetEstimator::new(config()).expect("valid estimator");
        estimator
            .estimate(
                sample(1_000, 0, memory(8 * GIB, 2 * GIB, 8 * GIB)),
                geometry(8 * GIB),
            )
            .expect("first sample");
        let after_gap = estimator
            .estimate(
                sample(62_000, 1, memory(8 * GIB, 2 * GIB, 8 * GIB)),
                geometry(8 * GIB),
            )
            .expect("gap sample");
        assert!(!after_gap.history_ready);
        let mut restarted = sample(63_000, 0, memory(8 * GIB, 2 * GIB, 8 * GIB));
        restarted.session_id = "session-b".to_owned();
        restarted.monotonic_millis = 1;
        let result = estimator
            .estimate(restarted, geometry(8 * GIB))
            .expect("restart");
        assert!(!result.history_ready);
        assert_eq!(estimator.state.history.len(), 1);
    }

    #[test]
    fn invalid_sample_clears_reclaim_readiness() {
        let mut estimator = TargetEstimator::new(config()).expect("estimator");
        estimator.state = TargetEstimatorState {
            desired_bytes: 8 * GIB,
            safe_floor_bytes: 4 * GIB,
            session_id: "session-a".to_owned(),
            last_monotonic_millis: 2,
            last_sequence: 2,
            last_visible_base_bytes: None,
            history: VecDeque::from([CandidateHistoryEntry {
                monotonic_millis: 1,
                desired_now_bytes: 4 * GIB,
                target_now_bytes: 4 * GIB,
                floor_now_bytes: 3 * GIB,
            }]),
        };
        assert!(estimator
            .estimate(
                sample(2, 2, memory(8 * GIB, 2 * GIB, 8 * GIB)),
                geometry(8 * GIB)
            )
            .is_err());
        assert!(estimator.state.history.is_empty());
        assert_eq!(
            estimator.state.safe_floor_bytes,
            estimator.state.desired_bytes
        );
    }

    #[test]
    fn detects_candidate_overflow() {
        let mut overflow_config = config();
        overflow_config.commit_reserve_bytes = u64::MAX - (2 * MIB - 1);
        assert_eq!(
            calculate_instantaneous(
                &overflow_config,
                &memory(8 * GIB, 2 * GIB, 8 * GIB),
                geometry(8 * GIB)
            ),
            Err(TargetEstimatorError::ArithmeticOverflow)
        );
    }
    #[test]
    fn pressure_or_unknown_restarts_the_entire_reclaim_window() {
        for native in [
            None,
            Some(MemoryResourceNotificationState::Low),
            Some(MemoryResourceNotificationState::Neutral),
        ] {
            let mut estimator = TargetEstimator::new(config()).unwrap();
            for sequence in 0..=10 {
                estimator
                    .estimate(
                        sample(
                            1_000_000 + sequence * 60_000,
                            sequence,
                            memory(8 * GIB, 2 * GIB, 8 * GIB),
                        ),
                        geometry(8 * GIB),
                    )
                    .unwrap();
            }
            let mut observation = sample(1_660_000, 11, memory(8 * GIB, 2 * GIB, 8 * GIB));
            observation.native_pressure = native;
            assert!(
                !estimator
                    .estimate(observation, geometry(8 * GIB))
                    .unwrap()
                    .history_ready
            );
            let result = estimator
                .estimate(
                    sample(1_720_000, 12, memory(8 * GIB, 2 * GIB, 8 * GIB)),
                    geometry(8 * GIB),
                )
                .unwrap();
            assert!(!result.history_ready);
        }
    }

    #[test]
    fn wall_clock_jumps_do_not_qualify_or_reset_monotonic_reclaim_history() {
        let mut estimator = TargetEstimator::new(config()).unwrap();
        for sequence in 0..=10 {
            let mut observation = sample(
                1_000_000 + sequence * 60_000,
                sequence,
                memory(8 * GIB, 2 * GIB, 8 * GIB),
            );
            observation.observed_unix_millis = if sequence % 2 == 0 { 1 } else { u64::MAX };
            let result = estimator.estimate(observation, geometry(8 * GIB)).unwrap();
            assert_eq!(result.history_ready, sequence == 10);
        }
    }

    #[test]
    fn frequent_samples_preserve_full_window_and_peak_with_bounded_history() {
        let mut cfg = config();
        cfg.history_window_millis = 10_000;
        cfg.maximum_gap_millis = 2;
        let mut estimator = TargetEstimator::new(cfg).unwrap();
        for sequence in 0..=10_000 {
            // A single intra-bucket high target must survive compression.
            let used = if sequence == 4_999 { 7 * GIB } else { 2 * GIB };
            let result = estimator
                .estimate(
                    sample(
                        1_000_000 + sequence,
                        sequence,
                        memory(8 * GIB, used, 8 * GIB),
                    ),
                    geometry(8 * GIB),
                )
                .unwrap();
            assert!(estimator.state.history.len() <= MAX_TARGET_HISTORY_ENTRIES);
            if sequence == 10_000 {
                assert!(result.history_ready);
                assert_eq!(result.desired_bytes, 9 * GIB + 128 * MIB);
            }
        }
    }

    #[test]
    fn standby_surplus_is_not_subtracted_twice_and_commit_still_bounds_reclaim() {
        let mut observation = memory(8 * GIB, 2 * GIB, GIB);
        observation.system_cache_bytes = 8 * GIB;
        let estimate = calculate_instantaneous(&config(), &observation, geometry(8 * GIB)).unwrap();
        assert_eq!(estimate.desired_now_bytes, 9 * GIB);
        assert_eq!(estimate.floor_now_bytes, 8 * GIB);
    }
    #[test]
    fn mixed_hotplug_geometry_restarts_reclaim_evidence() {
        let mut estimator = TargetEstimator::new(config()).unwrap();
        for sequence in 0..=10 {
            estimator
                .estimate(
                    sample(
                        1_000_000 + sequence * 60_000,
                        sequence,
                        memory(8 * GIB, 2 * GIB, 8 * GIB),
                    ),
                    geometry(8 * GIB),
                )
                .unwrap();
        }
        // Host current has advanced while the Windows physical-total observation
        // still describes the old allocation. It must not justify reclaim.
        let result = estimator
            .estimate(
                sample(1_660_000, 11, memory(8 * GIB, 2 * GIB, 8 * GIB)),
                geometry(9 * GIB),
            )
            .unwrap();
        assert!(!result.history_ready);
    }
}
