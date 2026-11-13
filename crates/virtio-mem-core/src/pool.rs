//! Shared-pool bookkeeping across VMs, as a pure function of one round's inputs.
//!
//! The pool only tracks how much of `pool_bytes` is free and who gets it first.
//! It never takes memory away: releasing and requesting memory are decided by
//! each VM's own controller. A member's charge is `max(requested, current)`,
//! so memory a guest is still giving back stays charged until its `current`
//! drops, and a crashed VM keeps its charge until it releases the memory.
//! No pool state is persisted; every round is recomputed from the charges.

use thiserror::Error;

/// One member's inputs for a round, in device bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolMember {
    /// Larger is served first when free bytes run out.
    pub priority: u32,
    /// When the member's request was issued (guest clock, Unix ms): among
    /// equal priorities the earlier request is served first. `u64::MAX` for a
    /// member with no request.
    pub arrival_unix_millis: u64,
    /// Bytes the member holds now: `max(requested, current)`.
    pub charged_bytes: u64,
    /// The member's own desired size; growth beyond its charge needs pool bytes.
    pub desired_bytes: u64,
    pub block_bytes: u64,
}

/// One member's result for a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolAllocation {
    /// The member's desired size must be clamped to at most this. It is never
    /// below the member's charge.
    pub ceiling_bytes: u64,
    /// Growth the member wanted but the pool could not grant this round.
    pub unmet_bytes: u64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PoolError {
    #[error("pool member block size must be a non-zero power of two")]
    InvalidBlock,
    #[error("pool accounting overflowed")]
    Overflow,
}

/// Hands this round's free bytes out, highest priority first, then first
/// come, first served; configuration order breaks any remaining tie. Results
/// are in the same order as `members`.
pub fn allocate_pool(
    pool_bytes: u64,
    members: &[PoolMember],
) -> Result<Vec<PoolAllocation>, PoolError> {
    if members
        .iter()
        .any(|member| member.block_bytes == 0 || !member.block_bytes.is_power_of_two())
    {
        return Err(PoolError::InvalidBlock);
    }
    let charged_total = members
        .iter()
        .try_fold(0_u64, |total, member| {
            total.checked_add(member.charged_bytes)
        })
        .ok_or(PoolError::Overflow)?;
    let mut free = pool_bytes.saturating_sub(charged_total);

    let mut order = (0..members.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| service_order(&members[*index]));

    let mut allocations = vec![
        PoolAllocation {
            ceiling_bytes: 0,
            unmet_bytes: 0,
        };
        members.len()
    ];
    for index in order {
        let member = &members[index];
        let wanted = member.desired_bytes.saturating_sub(member.charged_bytes);
        let granted = align_down(wanted.min(free), member.block_bytes);
        free -= granted;
        allocations[index] = PoolAllocation {
            ceiling_bytes: member.charged_bytes + granted,
            unmet_bytes: wanted - granted,
        };
    }
    Ok(allocations)
}

/// The key members are served in, smallest first: highest priority, then
/// earliest request. The sort is stable, so configuration order breaks ties.
pub fn service_order(member: &PoolMember) -> (std::cmp::Reverse<u32>, u64) {
    (
        std::cmp::Reverse(member.priority),
        member.arrival_unix_millis,
    )
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value - value % alignment
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn member(priority: u32, charged: u64, desired: u64) -> PoolMember {
        PoolMember {
            priority,
            arrival_unix_millis: u64::MAX,
            charged_bytes: charged,
            desired_bytes: desired,
            block_bytes: 2 * MIB,
        }
    }

    fn ceilings(allocations: &[PoolAllocation]) -> Vec<u64> {
        allocations.iter().map(|a| a.ceiling_bytes).collect()
    }

    #[test]
    fn growth_that_fits_is_granted_to_everyone() {
        let allocations = allocate_pool(
            16 * GIB,
            &[member(100, 4 * GIB, 6 * GIB), member(80, 4 * GIB, 6 * GIB)],
        )
        .expect("allocation");
        assert_eq!(ceilings(&allocations), [6 * GIB, 6 * GIB]);
        assert!(allocations.iter().all(|a| a.unmet_bytes == 0));
    }

    #[test]
    fn contended_growth_serves_the_higher_priority_first() {
        // 2 GiB free; both want 2 GiB more. Order in the slice does not matter.
        let allocations = allocate_pool(
            10 * GIB,
            &[member(80, 4 * GIB, 6 * GIB), member(100, 4 * GIB, 6 * GIB)],
        )
        .expect("allocation");
        assert_eq!(allocations[1].ceiling_bytes, 6 * GIB);
        assert_eq!(allocations[0].ceiling_bytes, 4 * GIB);
        assert_eq!(allocations[0].unmet_bytes, 2 * GIB);
    }

    #[test]
    fn equal_priorities_are_served_first_come_first_served() {
        // 2 GiB free; both want 2 GiB more at the same priority. The later
        // request is listed first but the earlier one is served.
        let early = PoolMember {
            arrival_unix_millis: 1_000,
            ..member(100, 4 * GIB, 6 * GIB)
        };
        let late = PoolMember {
            arrival_unix_millis: 1_001,
            ..member(100, 4 * GIB, 6 * GIB)
        };
        let allocations = allocate_pool(10 * GIB, &[late, early]).expect("allocation");
        assert_eq!(ceilings(&allocations), [4 * GIB, 6 * GIB]);
        // Priority still comes before arrival.
        let urgent = PoolMember {
            priority: 200,
            ..late
        };
        let allocations = allocate_pool(10 * GIB, &[early, urgent]).expect("allocation");
        assert_eq!(ceilings(&allocations), [4 * GIB, 6 * GIB]);
        // With equal arrival, configuration order decides.
        let allocations = allocate_pool(10 * GIB, &[early, early]).expect("allocation");
        assert_eq!(ceilings(&allocations), [6 * GIB, 4 * GIB]);
    }

    #[test]
    fn the_pool_never_lowers_a_ceiling_below_what_a_vm_holds() {
        // Full pool and unmet high-priority demand: the low VM keeps its 4 GiB
        // until it releases memory itself.
        let allocations = allocate_pool(
            8 * GIB,
            &[member(100, 4 * GIB, 7 * GIB), member(80, 4 * GIB, 4 * GIB)],
        )
        .expect("allocation");
        assert_eq!(ceilings(&allocations), [4 * GIB, 4 * GIB]);
        assert_eq!(allocations[0].unmet_bytes, 3 * GIB);
    }

    #[test]
    fn memory_still_being_released_stays_charged() {
        // The low VM is releasing toward 2 GiB but still holds 4 GiB, so the
        // high VM cannot grow into the pending release.
        let allocations = allocate_pool(
            8 * GIB,
            &[member(100, 4 * GIB, 6 * GIB), member(80, 4 * GIB, 2 * GIB)],
        )
        .expect("allocation");
        assert_eq!(allocations[0].ceiling_bytes, 4 * GIB);
    }

    #[test]
    fn grants_stay_block_aligned_and_an_overcommitted_pool_grants_nothing() {
        let allocations =
            allocate_pool(4 * GIB + 3 * MIB, &[member(100, 4 * GIB, 6 * GIB)]).expect("allocation");
        assert_eq!(allocations[0].ceiling_bytes, 4 * GIB + 2 * MIB);
        let allocations =
            allocate_pool(2 * GIB, &[member(100, 4 * GIB, 6 * GIB)]).expect("allocation");
        assert_eq!(allocations[0].ceiling_bytes, 4 * GIB);
        assert_eq!(
            allocate_pool(
                GIB,
                &[PoolMember {
                    block_bytes: 3,
                    ..member(1, 0, 0)
                }]
            ),
            Err(PoolError::InvalidBlock)
        );
    }
}
