//! Linux memory counters from `/proc/meminfo`, expressed in the shared
//! telemetry snapshot the guest policy and the host already understand.
//!
//! The snapshot's commit pair was defined by Windows, where commit is an
//! enforced limit. Linux enforces one only under strict overcommit, so the
//! pair is filled according to what the kernel is doing:
//!
//! - Strict overcommit (`vm.overcommit_memory=2`) with a limit that scales
//!   with RAM: `CommitLimit` and `Committed_AS`, the kernel's own enforced
//!   pair. Allocation fails at that limit however much RAM is free, so the
//!   policy's commit reserve applies as it does on Windows. Each added byte
//!   of RAM raises the limit by `vm.overcommit_ratio` per cent, so a commit
//!   shortfall closes over several requests rather than one.
//! - Otherwise: nothing is enforced and `Committed_AS` may exceed the limit.
//!   The pair is RAM plus swap (limit `MemTotal + SwapTotal`, in use that
//!   less `MemAvailable + SwapFree`). Its headroom is never below available
//!   RAM, so the commit reserve never asks for more than the physical
//!   reserve already does: for these guests it does not exist.
//!
//! A fixed limit (`vm.overcommit_kbytes`) is treated as not enforced: more
//! RAM cannot raise it, so asking the host for memory would not help.

use std::path::Path;

use virtio_mem_core::MemoryTelemetrySnapshot;

pub const MEMINFO_PATH: &str = "/proc/meminfo";
/// `/proc/meminfo` is a few kilobytes; anything larger is not that file.
const MAX_MEMINFO_BYTES: usize = 64 * 1024;

pub trait MemorySource {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String>;
}

/// Whether the kernel enforces a commit limit that more RAM would raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitModel {
    NotEnforced,
    Enforced,
}

impl CommitModel {
    /// From the three `vm.overcommit_*` values as the kernel prints them.
    /// Anything unreadable means the kernel's default: not enforced.
    pub fn from_sysctls(memory: Option<&str>, kbytes: Option<&str>, ratio: Option<&str>) -> Self {
        let number = |text: Option<&str>| text.and_then(|text| text.trim().parse::<u64>().ok());
        match (number(memory), number(kbytes), number(ratio)) {
            (Some(2), Some(0), Some(ratio)) if ratio > 0 => Self::Enforced,
            _ => Self::NotEnforced,
        }
    }

    /// Read on every sample: an administrator can change it while we run.
    pub(crate) fn current() -> Self {
        let read = |name: &str| {
            crate::files::read_bounded(&Path::new("/proc/sys/vm").join(name), 64)
                .ok()
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        };
        let (memory, kbytes, ratio) = (
            read("overcommit_memory"),
            read("overcommit_kbytes"),
            read("overcommit_ratio"),
        );
        Self::from_sysctls(memory.as_deref(), kbytes.as_deref(), ratio.as_deref())
    }
}

/// Reads `/proc/meminfo` and keeps the highest commit total seen by this
/// process, since Linux has no counter for it.
#[derive(Debug, Default)]
pub struct ProcMeminfo {
    commit_peak_bytes: u64,
}

impl MemorySource for ProcMeminfo {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String> {
        let text = crate::files::read_bounded(Path::new(MEMINFO_PATH), MAX_MEMINFO_BYTES)
            .map_err(|error| format!("read {MEMINFO_PATH}: {error}"))?
            .ok_or_else(|| format!("{MEMINFO_PATH} is missing"))?;
        let text = String::from_utf8(text).map_err(|_| format!("{MEMINFO_PATH} is not UTF-8"))?;
        let snapshot =
            snapshot_from_meminfo(&text, CommitModel::current(), self.commit_peak_bytes)?;
        self.commit_peak_bytes = snapshot.commit_peak_bytes;
        Ok(snapshot)
    }
}

/// `previous_peak_bytes` is the highest commit total already observed.
pub fn snapshot_from_meminfo(
    text: &str,
    commit: CommitModel,
    previous_peak_bytes: u64,
) -> Result<MemoryTelemetrySnapshot, String> {
    let field = |name: &str| -> Result<u64, String> {
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .ok_or_else(|| format!("meminfo has no {name}"))?;
        let kibibytes = value
            .trim()
            .strip_suffix("kB")
            .ok_or_else(|| format!("meminfo {name} is not in kB"))?
            .trim()
            .parse::<u64>()
            .map_err(|error| format!("meminfo {name} is invalid: {error}"))?;
        kibibytes
            .checked_mul(1024)
            .ok_or_else(|| format!("meminfo {name} overflows"))
    };
    let add = |left: u64, right: u64| {
        left.checked_add(right)
            .ok_or_else(|| "meminfo counters overflow".to_owned())
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    let swap_total = field("SwapTotal")?;
    let swap_free = field("SwapFree")?;
    if total == 0 || available > total || swap_free > swap_total {
        return Err("meminfo counters are inconsistent".to_owned());
    }
    let (commit_limit, commit_total) = match commit {
        CommitModel::NotEnforced => {
            let limit = add(total, swap_total)?;
            (limit, limit - add(available, swap_free)?)
        }
        CommitModel::Enforced => {
            let limit = field("CommitLimit")?;
            // Removing RAM or swap lowers the limit under what is already
            // committed; that is no headroom, not an inconsistency.
            (limit, field("Committed_AS")?.min(limit))
        }
    };
    let snapshot = MemoryTelemetrySnapshot {
        physical_total_bytes: total,
        physical_available_bytes: available,
        memory_load_percent: ((u128::from(total - available) * 100) / u128::from(total)) as u32,
        commit_total_bytes: commit_total,
        commit_limit_bytes: commit_limit,
        commit_peak_bytes: previous_peak_bytes.max(commit_total),
        system_cache_bytes: add(field("Cached")?, field("Buffers")?)?,
        kernel_paged_bytes: field("SReclaimable")?,
        kernel_nonpaged_bytes: field("SUnreclaim")?,
    };
    snapshot.validate().map_err(|error| error.to_string())?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const NOT_ENFORCED: CommitModel = CommitModel::NotEnforced;

    fn meminfo(available_kb: u64, swap_free_kb: u64) -> String {
        format!(
            "MemTotal:        5111316 kB\nMemFree:         4600000 kB\nMemAvailable:    {available_kb} kB\n\
             Buffers:           10240 kB\nCached:           204800 kB\nSwapCached:            0 kB\n\
             SwapTotal:       1048576 kB\nSwapFree:        {swap_free_kb} kB\nSlab:              60000 kB\n\
             SReclaimable:      20480 kB\nSUnreclaim:        30720 kB\nCommitLimit:     2555656 kB\n\
             Committed_AS:    9999999 kB\n"
        )
    }

    #[test]
    fn commit_is_ram_plus_swap_in_use_not_the_unenforced_kernel_counters() {
        let snapshot =
            snapshot_from_meminfo(&meminfo(4 * 1024 * 1024, 1048576), NOT_ENFORCED, 0).unwrap();
        assert_eq!(snapshot.physical_total_bytes, 5111316 * 1024);
        assert_eq!(snapshot.physical_available_bytes, 4096 * MIB);
        assert_eq!(snapshot.commit_limit_bytes, (5111316 + 1048576) * 1024);
        assert_eq!(
            snapshot.commit_limit_bytes - snapshot.commit_total_bytes,
            (4096 + 1024) * MIB,
            "headroom is available RAM plus free swap"
        );
        assert_eq!(snapshot.system_cache_bytes, 210 * MIB);
        assert_eq!(snapshot.kernel_paged_bytes, 20 * MIB);
        assert_eq!(snapshot.kernel_nonpaged_bytes, 30 * MIB);
        assert_eq!(snapshot.memory_load_percent, 17);
    }

    #[test]
    fn without_an_enforced_limit_commit_headroom_is_never_below_available_ram() {
        for (available_kb, swap_free_kb) in [(0, 0), (1024, 0), (4 * 1024 * 1024, 1048576)] {
            let snapshot =
                snapshot_from_meminfo(&meminfo(available_kb, swap_free_kb), NOT_ENFORCED, 0)
                    .unwrap();
            assert!(
                snapshot.commit_limit_bytes - snapshot.commit_total_bytes
                    >= snapshot.physical_available_bytes
            );
        }
    }

    #[test]
    fn strict_overcommit_reports_the_enforced_kernel_pair() {
        let text = meminfo(4 * 1024 * 1024, 1048576).replace("9999999 kB", "2000000 kB");
        let snapshot = snapshot_from_meminfo(&text, CommitModel::Enforced, 0).unwrap();
        assert_eq!(snapshot.commit_limit_bytes, 2555656 * 1024);
        assert_eq!(snapshot.commit_total_bytes, 2000000 * 1024);
        // Committed above a limit that has since fallen is zero headroom.
        let over =
            snapshot_from_meminfo(&meminfo(4 * 1024 * 1024, 1048576), CommitModel::Enforced, 0)
                .unwrap();
        assert_eq!(over.commit_total_bytes, over.commit_limit_bytes);
        assert!(snapshot_from_meminfo(
            &text.replace("CommitLimit", "CommitLimits"),
            CommitModel::Enforced,
            0
        )
        .is_err());
    }

    #[test]
    fn only_a_ram_scaled_strict_limit_counts_as_enforced() {
        use CommitModel::{Enforced, NotEnforced};
        for (memory, kbytes, ratio, expected) in [
            (Some("2\n"), Some("0\n"), Some("50\n"), Enforced),
            (Some("0\n"), Some("0\n"), Some("50\n"), NotEnforced),
            (Some("1\n"), Some("0\n"), Some("50\n"), NotEnforced),
            (Some("2\n"), Some("1048576\n"), Some("50\n"), NotEnforced),
            (Some("2\n"), Some("0\n"), Some("0\n"), NotEnforced),
            (None, Some("0\n"), Some("50\n"), NotEnforced),
            (Some("2\n"), None, None, NotEnforced),
            (Some("strict"), Some("0"), Some("50"), NotEnforced),
        ] {
            assert_eq!(
                CommitModel::from_sysctls(memory, kbytes, ratio),
                expected,
                "{memory:?} {kbytes:?} {ratio:?}"
            );
        }
    }

    #[test]
    fn the_commit_peak_only_rises() {
        let busy = snapshot_from_meminfo(&meminfo(1024 * 1024, 0), NOT_ENFORCED, 0).unwrap();
        assert_eq!(busy.commit_peak_bytes, busy.commit_total_bytes);
        let idle = snapshot_from_meminfo(
            &meminfo(4 * 1024 * 1024, 1048576),
            NOT_ENFORCED,
            busy.commit_peak_bytes,
        )
        .unwrap();
        assert!(idle.commit_total_bytes < busy.commit_total_bytes);
        assert_eq!(idle.commit_peak_bytes, busy.commit_peak_bytes);
    }

    #[test]
    fn missing_malformed_and_inconsistent_counters_are_rejected() {
        let valid = meminfo(1024, 0);
        for broken in [
            valid.replace("MemAvailable", "MemUnavailable"),
            valid.replace("5111316 kB", "5111316 MB"),
            valid.replace("5111316 kB", "many kB"),
            meminfo(5111317, 0),
            meminfo(1024, 1048577),
            String::new(),
        ] {
            assert!(
                snapshot_from_meminfo(&broken, NOT_ENFORCED, 0).is_err(),
                "{broken}"
            );
        }
    }
}
