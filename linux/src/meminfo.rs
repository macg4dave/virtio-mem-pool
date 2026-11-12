//! Linux memory counters from `/proc/meminfo`, expressed in the shared
//! telemetry snapshot the guest policy and the host already understand.
//!
//! The snapshot's commit pair was defined by Windows, where commit is an
//! enforced limit. Linux's `CommitLimit`/`Committed_AS` are not enforced under
//! the default overcommit heuristic and `Committed_AS` may exceed the limit,
//! so they cannot fill it. The Linux equivalent of "how much more can be
//! used before allocation fails" is RAM plus swap:
//!
//! - commit limit = `MemTotal + SwapTotal`
//! - commit total = that limit less `MemAvailable + SwapFree`
//!
//! Without swap the commit headroom equals available RAM, so the commit
//! reserve adds nothing to the physical reserve. Strict overcommit
//! (`vm.overcommit_memory=2`) is not modelled.

use virtio_mem_core::MemoryTelemetrySnapshot;

pub const MEMINFO_PATH: &str = "/proc/meminfo";
/// `/proc/meminfo` is a few kilobytes; anything larger is not that file.
const MAX_MEMINFO_BYTES: usize = 64 * 1024;

pub trait MemorySource {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String>;
}

/// Reads `/proc/meminfo` and keeps the highest commit total seen by this
/// process, since Linux has no counter for it.
#[derive(Debug, Default)]
pub struct ProcMeminfo {
    commit_peak_bytes: u64,
}

impl MemorySource for ProcMeminfo {
    fn sample(&mut self) -> Result<MemoryTelemetrySnapshot, String> {
        let text =
            crate::files::read_bounded(std::path::Path::new(MEMINFO_PATH), MAX_MEMINFO_BYTES)
                .map_err(|error| format!("read {MEMINFO_PATH}: {error}"))?
                .ok_or_else(|| format!("{MEMINFO_PATH} is missing"))?;
        let text = String::from_utf8(text).map_err(|_| format!("{MEMINFO_PATH} is not UTF-8"))?;
        let snapshot = snapshot_from_meminfo(&text, self.commit_peak_bytes)?;
        self.commit_peak_bytes = snapshot.commit_peak_bytes;
        Ok(snapshot)
    }
}

/// `previous_peak_bytes` is the highest commit total already observed.
pub fn snapshot_from_meminfo(
    text: &str,
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
    let commit_limit = add(total, swap_total)?;
    let commit_total = commit_limit - add(available, swap_free)?;
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
        let snapshot = snapshot_from_meminfo(&meminfo(4 * 1024 * 1024, 1048576), 0).unwrap();
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
    fn the_commit_peak_only_rises() {
        let busy = snapshot_from_meminfo(&meminfo(1024 * 1024, 0), 0).unwrap();
        assert_eq!(busy.commit_peak_bytes, busy.commit_total_bytes);
        let idle =
            snapshot_from_meminfo(&meminfo(4 * 1024 * 1024, 1048576), busy.commit_peak_bytes)
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
            assert!(snapshot_from_meminfo(&broken, 0).is_err(), "{broken}");
        }
    }
}
