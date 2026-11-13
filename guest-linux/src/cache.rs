//! A floor under the page cache, learned from the kernel's refault counter.
//!
//! Linux reports page cache as available memory, so the shared policy sees a
//! guest that lives on file cache as idle: it never asks for more and reclaim
//! squeezes its cache until every read goes back to disk. The kernel counts
//! exactly that event. `workingset_refault_file` in `/proc/vmstat` rises when
//! a file page that was evicted moments ago is read again: the cache is
//! smaller than the files in use.
//!
//! The floor is an amount of file cache treated as in use rather than
//! available:
//!
//! - While file pages are being refaulted, the floor is the present cache
//!   plus one growth step, so the policy asks for more memory until the
//!   refaults stop or the VM reaches `max_ram`. One sample of refaults is
//!   not enough: writing a file can cause a moment of them.
//! - It never exceeds the cache that exists, so cache that goes away (files
//!   deleted, caches dropped) is not held.
//! - After a quiet period the floor is lowered one step at a time, which
//!   lets reclaim take that memory back. If lowering it brings refaults back
//!   the guest still needed it: the floor returns to where it was and the
//!   quiet period doubles, so a steady workload is probed less and less
//!   often. Each step that passes without refaults halves it again, so a
//!   workload that has stopped gives its cache back progressively faster.
//!
//! Reading a large file once never refaults and so never raises the floor.

use std::sync::atomic::{AtomicU64, Ordering};

pub const VMSTAT_PATH: &str = "/proc/vmstat";
/// `/proc/vmstat` is a few kilobytes of counters.
const MAX_VMSTAT_BYTES: usize = 256 * 1024;
/// Refaulted file data at or above this rate means the cache does not fit.
/// Idle guests measure zero; a loop over files larger than memory measured
/// hundreds of mebibytes a second.
const THRASH_BYTES_PER_SECOND: u64 = 1024 * 1024;
/// How long refaults must stay away before the floor is first lowered.
const QUIET_MILLIS: u64 = 60_000;
/// The longest the quiet period grows to for a workload that keeps needing
/// its cache.
const MAX_QUIET_MILLIS: u64 = 3_600_000;
/// After the floor is restored, the removal already under way goes on
/// evicting cache and the restored memory takes a few seconds to arrive and
/// refill. Refaults in that time are the trial's own doing, not new demand.
const SETTLE_MILLIS: u64 = 10_000;

/// The floor in force, for the readiness report.
static PROTECTED_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn protected_bytes() -> u64 {
    PROTECTED_BYTES.load(Ordering::Relaxed)
}

/// `workingset_refault_file` in pages, or `None` when the kernel does not
/// report it.
pub fn refault_file_pages(vmstat: &str) -> Option<u64> {
    vmstat
        .lines()
        .find_map(|line| line.strip_prefix("workingset_refault_file "))?
        .trim()
        .parse()
        .ok()
}

pub fn read_refault_file_pages() -> Option<u64> {
    let bytes =
        virtio_mem_guest::files::read_bounded(std::path::Path::new(VMSTAT_PATH), MAX_VMSTAT_BYTES)
            .ok()??;
    refault_file_pages(std::str::from_utf8(&bytes).ok()?)
}

pub fn page_size_bytes() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes no pointers and has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if let Ok(size) = u64::try_from(size) {
            if size > 0 {
                return size;
            }
        }
    }
    4096
}

#[derive(Debug, Clone)]
pub struct CacheFloor {
    step_bytes: u64,
    page_bytes: u64,
    protected_bytes: u64,
    previous: Option<(u64, u64)>,
    /// When refaults were last seen, or the floor last lowered.
    quiet_since_millis: u64,
    quiet_millis: u64,
    /// The floor before it was last lowered, while that step is on trial.
    probing: Option<u64>,
    /// Until then, refaults follow from a trial that was just undone.
    settling_until_millis: u64,
    /// Consecutive samples with refaults at or above the threshold.
    refaulting_samples: u32,
}

impl CacheFloor {
    /// `step_bytes` is how much cache is added or given back at a time; zero
    /// disables the floor.
    pub fn new(step_bytes: u64, page_bytes: u64) -> Self {
        Self {
            step_bytes,
            page_bytes,
            protected_bytes: 0,
            previous: None,
            quiet_since_millis: 0,
            quiet_millis: QUIET_MILLIS,
            probing: None,
            settling_until_millis: 0,
            refaulting_samples: 0,
        }
    }

    /// The floor for this sample. `refault_pages` is the kernel's running
    /// count, `None` when unavailable; `file_cache_bytes` is the file cache
    /// that exists now.
    pub fn update(
        &mut self,
        monotonic_millis: u64,
        refault_pages: Option<u64>,
        file_cache_bytes: u64,
    ) -> u64 {
        let before = self.protected_bytes;
        let (Some(pages), true) = (refault_pages, self.step_bytes > 0) else {
            // Without the counter nothing can be learned or checked.
            self.previous = None;
            self.protected_bytes = 0;
            return self.publish(before, 0);
        };
        let rate = self.previous.and_then(|(at, count)| {
            let elapsed = monotonic_millis.checked_sub(at).filter(|ms| *ms > 0)?;
            let bytes = pages.saturating_sub(count).saturating_mul(self.page_bytes);
            Some(bytes.saturating_mul(1000) / elapsed)
        });
        if self.previous.is_none() {
            self.quiet_since_millis = monotonic_millis;
        }
        self.previous = Some((monotonic_millis, pages));
        let rate = rate.unwrap_or(0);
        if rate >= THRASH_BYTES_PER_SECOND {
            self.quiet_since_millis = monotonic_millis;
            self.refaulting_samples = self.refaulting_samples.saturating_add(1);
            if let Some(before) = self.probing.take() {
                // Lowering the floor is what brought this back: the level
                // it had was enough, so return to that and no further.
                self.quiet_millis = self.quiet_millis.saturating_mul(2).min(MAX_QUIET_MILLIS);
                self.protected_bytes = self.protected_bytes.max(before);
                self.settling_until_millis = monotonic_millis.saturating_add(SETTLE_MILLIS);
            } else if monotonic_millis >= self.settling_until_millis && self.refaulting_samples >= 2
            {
                self.protected_bytes = self
                    .protected_bytes
                    .max(file_cache_bytes.saturating_add(self.step_bytes));
            }
        } else {
            self.refaulting_samples = 0;
            // Cache that no longer exists is not held.
            self.protected_bytes = self.protected_bytes.min(file_cache_bytes);
            if self.protected_bytes > 0
                && monotonic_millis.saturating_sub(self.quiet_since_millis) >= self.quiet_millis
            {
                if self.probing.is_some() {
                    // The last step went unnoticed for a whole quiet period.
                    self.quiet_millis = (self.quiet_millis / 2).max(QUIET_MILLIS);
                }
                self.probing = Some(self.protected_bytes);
                self.protected_bytes = self.protected_bytes.saturating_sub(self.step_bytes);
                self.quiet_since_millis = monotonic_millis;
            }
            if self.protected_bytes == 0 {
                self.quiet_millis = QUIET_MILLIS;
                self.probing = None;
            }
        }
        self.publish(before, rate)
    }

    fn publish(&self, before: u64, rate: u64) -> u64 {
        PROTECTED_BYTES.store(self.protected_bytes, Ordering::Relaxed);
        if self.protected_bytes != before {
            eprintln!(
                "cache floor: protected_bytes={} was={before} refault_bytes_per_second={rate} quiet_seconds={}",
                self.protected_bytes,
                self.quiet_millis / 1000
            );
        }
        self.protected_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const PAGE: u64 = 4096;
    const STEP: u64 = 256 * MIB;
    /// Pages a second that refault 100 MiB a second.
    const BUSY: u64 = 100 * MIB / PAGE;

    #[test]
    fn the_counter_is_read_from_vmstat() {
        let vmstat = "workingset_refault_anon 7\nworkingset_refault_file 12345\nnr_free_pages 9\n";
        assert_eq!(refault_file_pages(vmstat), Some(12345));
        assert_eq!(refault_file_pages("workingset_refault_anon 7\n"), None);
        assert_eq!(refault_file_pages("workingset_refault_file many\n"), None);
    }

    /// A floor raised by two seconds of refaults over 1 GiB of cache and
    /// settled on that cache at 3 s. The counter stands at `2 * BUSY`.
    fn raised() -> CacheFloor {
        let mut floor = CacheFloor::new(STEP, PAGE);
        floor.update(0, Some(0), 1024 * MIB);
        floor.update(1_000, Some(BUSY), 1024 * MIB);
        assert_eq!(
            floor.update(2_000, Some(2 * BUSY), 1024 * MIB),
            1024 * MIB + STEP
        );
        assert_eq!(floor.update(3_000, Some(2 * BUSY), 1024 * MIB), 1024 * MIB);
        floor
    }

    #[test]
    fn sustained_refaults_raise_the_floor_a_step_above_the_cache_until_they_stop() {
        let mut floor = CacheFloor::new(STEP, PAGE);
        // The first sample has no rate, and one sample of refaults is a blip.
        assert_eq!(floor.update(1_000, Some(0), 1024 * MIB), 0);
        assert_eq!(floor.update(2_000, Some(BUSY), 1024 * MIB), 0);
        assert_eq!(floor.update(3_000, Some(BUSY), 1024 * MIB), 0);
        assert_eq!(floor.update(4_000, Some(2 * BUSY), 1024 * MIB), 0);
        assert_eq!(
            floor.update(5_000, Some(3 * BUSY), 1024 * MIB),
            1024 * MIB + STEP
        );
        // More memory arrived, the cache grew and is still too small.
        assert_eq!(
            floor.update(6_000, Some(4 * BUSY), 1500 * MIB),
            1500 * MIB + STEP
        );
        // It fits: no refaults. The floor settles on the cache that exists.
        assert_eq!(floor.update(7_000, Some(4 * BUSY), 1700 * MIB), 1700 * MIB);
        assert_eq!(floor.update(8_000, Some(4 * BUSY), 1700 * MIB), 1700 * MIB);
        // A trickle below the threshold is not thrashing.
        assert_eq!(
            floor.update(9_000, Some(4 * BUSY + 10), 1700 * MIB),
            1700 * MIB
        );
    }

    #[test]
    fn cache_that_goes_away_is_not_held() {
        let mut floor = raised();
        assert_eq!(floor.update(4_000, Some(2 * BUSY), 100 * MIB), 100 * MIB);
        assert_eq!(floor.update(5_000, Some(2 * BUSY), 0), 0);
    }

    #[test]
    fn a_quiet_floor_is_given_back_a_step_at_a_time() {
        let mut floor = raised();
        let mut at = |millis| floor.update(millis, Some(2 * BUSY), 1024 * MIB);
        assert_eq!(at(61_000), 1024 * MIB);
        assert_eq!(at(62_000), 768 * MIB);
        assert_eq!(at(63_000), 768 * MIB);
        assert_eq!(at(122_000), 512 * MIB);
        assert_eq!(at(182_000), 256 * MIB);
        assert_eq!(at(242_000), 0);
        assert_eq!(at(302_000), 0);
    }

    #[test]
    fn a_floor_that_was_still_needed_is_restored_and_probed_less_often() {
        let mut floor = raised();
        // Lowered after a quiet minute; reclaim starts to shrink the cache.
        assert_eq!(floor.update(62_000, Some(2 * BUSY), 1024 * MIB), 768 * MIB);
        // The workload reads what was evicted: at once back to the level
        // that was enough, and not above it.
        assert_eq!(floor.update(63_000, Some(3 * BUSY), 1024 * MIB), 1024 * MIB);
        // The removal under way keeps evicting for a moment: not new demand.
        assert_eq!(floor.update(64_000, Some(4 * BUSY), 900 * MIB), 1024 * MIB);
        let mut at = |millis| floor.update(millis, Some(4 * BUSY), 1024 * MIB);
        assert_eq!(at(65_000), 1024 * MIB);
        // The next probe waits two minutes from the last refault, not one.
        assert_eq!(at(125_000), 1024 * MIB);
        assert_eq!(at(183_000), 1024 * MIB);
        assert_eq!(at(184_000), 768 * MIB);
        // That step went unnoticed, so the wait halves back to one minute.
        assert_eq!(at(303_000), 768 * MIB);
        assert_eq!(at(304_000), 512 * MIB);
        assert_eq!(at(363_000), 512 * MIB);
        assert_eq!(at(364_000), 256 * MIB);
    }

    #[test]
    fn refaults_that_outlast_the_settling_period_are_new_demand() {
        let mut floor = raised();
        floor.update(62_000, Some(2 * BUSY), 1024 * MIB);
        assert_eq!(floor.update(63_000, Some(3 * BUSY), 1024 * MIB), 1024 * MIB);
        assert_eq!(
            floor.update(72_000, Some(12 * BUSY), 1024 * MIB),
            1024 * MIB
        );
        assert_eq!(
            floor.update(73_000, Some(13 * BUSY), 1024 * MIB),
            1024 * MIB + STEP
        );
    }

    #[test]
    fn without_the_counter_or_a_step_there_is_no_floor() {
        let mut floor = raised();
        assert_eq!(floor.update(4_000, None, 1024 * MIB), 0);
        let mut disabled = CacheFloor::new(0, PAGE);
        disabled.update(1_000, Some(0), 1024 * MIB);
        disabled.update(2_000, Some(BUSY), 1024 * MIB);
        assert_eq!(disabled.update(3_000, Some(2 * BUSY), 1024 * MIB), 0);
    }
}
