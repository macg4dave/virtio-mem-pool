//! The host control loop. Each guest decides when to add or remove memory
//! and publishes one request at a time. The runtime admits it, grants what is
//! free (pool bytes by priority, device ceiling, host reserve), records the
//! admission and command intent durably, resizes, and publishes the actual
//! result once live state converges. It never estimates guest demand and
//! never queues the unfulfilled remainder of a request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;
use virtio_mem_core::settings::GuestOs;
use virtio_mem_core::{
    allocate_pool, bounded_reason, CommandDirection, CommandOwnership, HostMemoryStatus,
    MemoryOperation, MemoryOutcome, MemoryRequest, MemoryResourceNotificationState, MemoryResult,
    PoolMember, RequestAdmission, VirtioMemState, MEMORY_REQUEST_VERSION, MIN_BLOCK_SIZE_BYTES,
};

use crate::checkpoint::{
    command_ownership, movement_result, Checkpoint, CommandIntent, DurableControlState,
};
use crate::config::HostConfig;
use crate::guest_channel::{GuestLink, SystemUnixClock, UnixClock};
use crate::host_memory::{validate_grow_headroom, HostMemorySource, GROW_ADMISSION_MARGIN_BYTES};

pub trait MemoryStateSource {
    fn memory_state(&self) -> Result<VirtioMemState, String>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResizeSinkError {
    #[error("resize rejected before command execution: {0}")]
    Rejected(String),
    #[error("resize command outcome is unknown: {0}")]
    CommandUnknown(String),
}

pub trait ResizeSink {
    /// Resize a converged device still equal to `expected` to `target`,
    /// after `authorize` passes, unless shutdown was requested.
    fn request_resize_guarded(
        &self,
        expected: VirtioMemState,
        target: u64,
        stop: &AtomicBool,
        authorize: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), ResizeSinkError>;

    /// Move the target of a pending resize still equal to `expected`:
    /// its current allocation settles it, a lower target extends a shrink.
    fn retarget_pending_guarded(
        &self,
        expected: VirtioMemState,
        target: u64,
        stop: &AtomicBool,
    ) -> Result<(), ResizeSinkError>;

    /// With the domain paused, request the whole device and then the
    /// converged `expected` allocation again, so QEMU's usable region (which
    /// only grows until a reset) covers the device while nothing is plugged.
    /// `Rejected` means nothing changed; `CommandUnknown` means the request or
    /// the domain may not be restored and the domain may remain paused.
    fn widen_usable_region_guarded(
        &self,
        expected: VirtioMemState,
        stop: &AtomicBool,
    ) -> Result<(), ResizeSinkError>;
}

#[derive(Debug, Error)]
pub enum HostRuntimeError {
    #[error("memory controller configuration is invalid: {0}")]
    Controller(String),
    #[error("host available-memory check failed: {0}")]
    HostMemory(String),
    #[error("resize request failed: {0}")]
    Resize(String),
    #[error("virtio-mem request did not converge within {0:?}")]
    ConvergenceTimeout(Duration),
}

/// What the loop does after one poll.
enum Step {
    Wait,
    Stop(HostRuntimeError),
}

/// One VM's observation for this round. Only `Ready` may act after the pool
/// has allocated the round.
enum Observed {
    /// Nothing to execute this round. The pool charge and block size, from
    /// live state or, when that is unreadable, the last known charge; `None`
    /// before the VM was ever read.
    Held {
        charge: Option<(u64, u64)>,
    },
    Ready(Ready),
    Stop(HostRuntimeError),
}

/// A converged device and a guest request that may be executed.
struct Ready {
    base_ram: u64,
    state: VirtioMemState,
    request: MemoryRequest,
    admission: RequestAdmission,
}

/// Host memory granted to growth within one round, so two VMs growing in the
/// same round cannot both count the same available host RAM.
struct HostBudget {
    committed: u64,
}

/// One VM: its adapters, durable control state and process-local bookkeeping.
/// Nothing process-local grants authority to actuate; that comes only from
/// the durable control state.
pub struct VmController<G, S, R> {
    guest: G,
    state_source: S,
    resize_sink: R,
    config: HostConfig,
    checkpoint: Checkpoint,
    control: DurableControlState,
    /// Monotonic bound within this process, combined with the durable
    /// original start time so restarts cannot extend an operation.
    pending_since: Option<Instant>,
    /// The owned resize's last observed allocation and when it last moved.
    pending_progress: Option<(u64, Instant)>,
    /// VIOMEM WORKAROUND: the last shrink extension's operation, allocation
    /// and step in blocks.
    extension: Option<(String, u64, u64)>,
    /// A growth settled without plugging anything: widen QEMU's usable
    /// region on the next converged poll.
    widen_due: bool,
    /// Widening was tried since the device last had nothing plugged (a VM
    /// reset shrinks the usable region again); it is not repeated.
    widened: bool,
    operation_counter: u64,
    last_charge: Option<(u64, u64)>,
    outstanding_growth: Option<u64>,
    /// This VM failed and is latched; it is only watched for its pool charge
    /// until the daemon restarts.
    stopped: bool,
    unowned_divergence_reported: bool,
    state_unavailable_reported: bool,
    guest_unavailable_reported: bool,
    /// Separate from `guest_unavailable_reported`: a successful status write
    /// every poll must not re-arm the read-failure report.
    status_unpublished_reported: bool,
    health_report: Option<(Instant, u64, u64)>,
    last_observed_request: Option<virtio_mem_core::RequestIdentity>,
    /// The publication version of the guest's last valid record.
    guest_version: Option<u16>,
}

impl<G, S, R> VmController<G, S, R> {
    /// Opens and locks the VM's checkpoint. The caller holds its device owner.
    pub fn new(
        guest: G,
        state_source: S,
        resize_sink: R,
        config: HostConfig,
        compatibility_fingerprint: String,
    ) -> Result<Self, String> {
        let (checkpoint, control) = Checkpoint::open(&config, compatibility_fingerprint)?;
        Ok(Self {
            guest,
            state_source,
            resize_sink,
            config,
            checkpoint,
            control,
            pending_since: None,
            pending_progress: None,
            extension: None,
            widen_due: false,
            widened: false,
            operation_counter: 0,
            last_charge: None,
            outstanding_growth: None,
            stopped: false,
            unowned_divergence_reported: false,
            state_unavailable_reported: false,
            guest_unavailable_reported: false,
            status_unpublished_reported: false,
            health_report: None,
            last_observed_request: None,
            guest_version: None,
        })
    }
}

pub struct HostRuntime<G, S, R, H> {
    vms: Vec<VmController<G, S, R>>,
    host_memory: H,
    /// Shared device-byte budget; `None` leaves each VM bounded only by its
    /// device and the host reserve.
    pool_bytes: Option<u64>,
    poll_interval: Duration,
    #[cfg(unix)]
    wake: Vec<std::sync::Arc<crate::wake::WakePeer>>,
    #[cfg(unix)]
    controls: Vec<std::sync::Arc<crate::control_channel::ControlChannel>>,
    #[cfg(unix)]
    memory_events: Option<crate::wake::MemoryEvents>,
}

impl<G, S, R, H> HostRuntime<G, S, R, H>
where
    G: GuestLink,
    S: MemoryStateSource,
    R: ResizeSink,
    H: HostMemorySource,
{
    /// The poll interval is a global setting, so every member carries the same one.
    pub fn new(
        vms: Vec<VmController<G, S, R>>,
        host_memory: H,
        pool_bytes: Option<u64>,
    ) -> Result<Self, HostRuntimeError> {
        let poll_interval = vms
            .first()
            .map(|vm| vm.config.poll_interval)
            .ok_or_else(|| HostRuntimeError::Controller("no VMs to control".to_owned()))?;
        Ok(Self {
            vms,
            host_memory,
            pool_bytes,
            poll_interval,
            #[cfg(unix)]
            wake: Vec::new(),
            #[cfg(unix)]
            controls: Vec::new(),
            #[cfg(unix)]
            memory_events: None,
        })
    }

    #[cfg(unix)]
    pub fn with_wake(
        mut self,
        peers: Vec<std::sync::Arc<crate::wake::WakePeer>>,
        controls: Vec<std::sync::Arc<crate::control_channel::ControlChannel>>,
        events: Option<crate::wake::MemoryEvents>,
    ) -> Self {
        self.wake = peers;
        self.controls = controls;
        self.memory_events = events;
        self
    }

    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), HostRuntimeError> {
        while !stop.load(Ordering::Acquire) {
            if let Step::Stop(error) = self.round(stop)? {
                return Err(error);
            }
            #[cfg(unix)]
            if !self.wake.is_empty() || !self.controls.is_empty() {
                crate::wake::wait(
                    &self.wake,
                    &self.controls,
                    self.memory_events.as_mut(),
                    stop,
                    self.poll_interval,
                );
                continue;
            }
            wait_interruptibly(stop, self.poll_interval);
        }
        Ok(())
    }

    /// Observe every VM, allocate the pool, then execute each ready request,
    /// highest priority first. A VM that fails (a resize that never
    /// converges, an unknown growth outcome) is latched and stops; the others
    /// keep running. The daemon stops only once every VM has.
    fn round(&mut self, stop: &AtomicBool) -> Result<Step, HostRuntimeError> {
        let mut last_stop = None;
        let mut observed = Vec::with_capacity(self.vms.len());
        for vm in &mut self.vms {
            if stop.load(Ordering::Acquire) {
                return Ok(Step::Wait);
            }
            let seen = if vm.stopped {
                vm.observe_stopped()
            } else {
                vm.observe(stop)?
            };
            match seen {
                Observed::Stop(error) => {
                    vm.stop(&error);
                    last_stop = Some(error);
                    observed.push(vm.observe_stopped());
                }
                other => observed.push(other),
            }
        }
        if stop.load(Ordering::Acquire) {
            return Ok(Step::Wait);
        }
        let ceilings = self.allocate(&observed)?;
        let mut order = (0..self.vms.len()).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            self.vms[*right]
                .config
                .priority
                .cmp(&self.vms[*left].config.priority)
        });
        let mut host = HostBudget {
            committed: self.vms.iter().fold(0_u64, |total, vm| {
                total.saturating_add(vm.outstanding_growth.unwrap_or(u64::MAX))
            }),
        };
        let mut observed = observed.into_iter().map(Some).collect::<Vec<_>>();
        for index in order {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let Some(Observed::Ready(ready)) = observed[index].take() else {
                continue;
            };
            let ceiling = ceilings.as_ref().map(|ceilings| ceilings[index]);
            let vm = &mut self.vms[index];
            if let Step::Stop(error) = vm.act(ready, ceiling, &self.host_memory, &mut host, stop)? {
                vm.stop(&error);
                last_stop = Some(error);
            }
        }
        match last_stop {
            Some(error) if self.vms.iter().all(|vm| vm.stopped) => Ok(Step::Stop(error)),
            _ => Ok(Step::Wait),
        }
    }

    fn allocate(&self, observed: &[Observed]) -> Result<Option<Vec<u64>>, HostRuntimeError> {
        let Some(pool_bytes) = self.pool_bytes else {
            return Ok(None);
        };
        let members = self
            .vms
            .iter()
            .zip(observed)
            .map(|(vm, observed)| vm.pool_member(observed, pool_bytes))
            .collect::<Vec<_>>();
        let allocations = allocate_pool(pool_bytes, &members)
            .map_err(|error| HostRuntimeError::Controller(error.to_string()))?;
        Ok(Some(
            allocations
                .into_iter()
                .map(|allocation| allocation.ceiling_bytes)
                .collect(),
        ))
    }
}

impl<G, S, R> VmController<G, S, R>
where
    G: GuestLink,
    S: MemoryStateSource,
    R: ResizeSink,
{
    /// Phase one: resolve recorded state and, on a converged device, read the
    /// guest's request. Pending commands are handled here in full; they never
    /// need pool bytes because their charge already includes their target.
    fn observe(&mut self, stop: &AtomicBool) -> Result<Observed, HostRuntimeError> {
        let state = match self.live_state() {
            Ok(state) => state,
            Err(error) if self.control.intent.is_some() => {
                self.latch(&format!(
                    "live state unavailable during an owned operation: {error}"
                ))?;
                return Ok(Observed::Held {
                    charge: self.last_charge,
                });
            }
            Err(error) => {
                // Without an owned operation there is nothing to protect but
                // the pool: hold at the last known charge.
                if !self.state_unavailable_reported {
                    self.log(&format!("event=live_state_unavailable reason={error}"));
                    self.state_unavailable_reported = true;
                }
                return Ok(Observed::Held {
                    charge: self.last_charge,
                });
            }
        };
        self.state_unavailable_reported = false;
        let intent_target = self
            .control
            .intent
            .as_ref()
            .map_or(0, |intent| intent.target_bytes);
        self.outstanding_growth = Some(
            state
                .requested_bytes
                .max(intent_target)
                .saturating_sub(state.current_bytes),
        );
        let charge = (
            state
                .requested_bytes
                .max(state.current_bytes)
                .max(intent_target),
            state.block_size_bytes,
        );
        self.last_charge = Some(charge);
        let held = Observed::Held {
            charge: Some(charge),
        };
        self.resolve_intent(state)?;
        if state.requested_bytes != state.current_bytes {
            let step = self.observe_pending(state, stop)?;
            self.publish(state, None);
            return Ok(match step {
                Step::Wait => held,
                Step::Stop(error) => Observed::Stop(error),
            });
        }
        self.pending_since = None;
        self.pending_progress = None;
        self.unowned_divergence_reported = false;
        if state.current_bytes == 0 {
            self.widened = false;
        }
        if self.widen_due {
            if let Step::Stop(error) = self.widen_usable_region(state, stop)? {
                return Ok(Observed::Stop(error));
            }
        }
        let (request, base_ram) = match self.guest.read() {
            Ok(envelope) => {
                self.guest_unavailable_reported = false;
                self.guest_version = Some(envelope.version);
                if let Some(path) = &self.config.telemetry_path {
                    let result = serde_json::to_vec(&envelope)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| {
                            crate::guest_channel::write_local(std::path::Path::new(path), &bytes)
                        });
                    if let Err(error) = result {
                        self.log(&format!("event=optional_telemetry_failed reason={error}"));
                    }
                }
                let Some(base) = envelope
                    .memory
                    .physical_total_bytes
                    .checked_sub(state.current_bytes)
                else {
                    self.publish(state, None);
                    return Ok(held);
                };
                (envelope.request, base)
            }
            Err(error) => {
                if !self.guest_unavailable_reported {
                    self.log(&format!("event=guest_input_invalid reason={error}"));
                    self.guest_unavailable_reported = true;
                }
                (None, 0)
            }
        };
        let Some(request) = request else {
            self.publish(state, None);
            return Ok(held);
        };
        if self.last_observed_request.as_ref() != Some(&request.identity) {
            self.log(&format!(
                "event=guest_request_observed {} issued_unix_millis={}",
                describe(&request),
                request.issued_unix_millis
            ));
            self.last_observed_request = Some(request.identity.clone());
        }
        let now = self.now()?;
        let mut journal = self.control.journal.clone();
        let refusal = match journal.admit(
            request.clone(),
            &self.config.vm_name,
            &self.config.raw_telemetry_service_name,
            now,
            self.future_tolerance_millis()?,
        ) {
            // A retransmission of a resolved request: republish its result.
            Ok(RequestAdmission::Replay) if self.control.journal.result.is_some() => {
                self.publish(state, None);
                return Ok(held);
            }
            Ok(RequestAdmission::New) if self.control.latched() => format!(
                "automatic actuation is latched: {}",
                self.control.latch_reason.as_deref().unwrap_or_default()
            ),
            Ok(admission) => {
                return Ok(Observed::Ready(Ready {
                    base_ram,
                    state,
                    request,
                    admission,
                }))
            }
            Err(reason) => reason,
        };
        self.log(&format!(
            "event=controller_decision action=refuse {} reason={refusal}",
            describe(&request)
        ));
        self.publish(state, Some(MemoryResult::rejected(request, now, refusal)));
        Ok(held)
    }

    fn stop(&mut self, error: &HostRuntimeError) {
        self.stopped = true;
        self.log(&format!(
            "event=vm_stopped reason={error}; other VMs continue, restart the daemon after recovery"
        ));
    }

    /// A stopped VM still holds memory, so the pool keeps charging it.
    fn observe_stopped(&mut self) -> Observed {
        if let Ok(state) = self.live_state() {
            let requested = state.requested_bytes.max(
                self.control
                    .intent
                    .as_ref()
                    .map_or(0, |intent| intent.target_bytes),
            );
            self.outstanding_growth = Some(requested.saturating_sub(state.current_bytes));
            self.last_charge = Some((requested.max(state.current_bytes), state.block_size_bytes));
        }
        Observed::Held {
            charge: self.last_charge,
        }
    }

    /// This VM's input to the pool. Anything not executing a request asks
    /// for nothing beyond what it holds. An unreadable VM (crashed, off, or a
    /// failing virsh) keeps the memory it last held. A VM never read since
    /// startup could hold anything, so it is charged the whole pool.
    fn pool_member(&self, observed: &Observed, pool_bytes: u64) -> PoolMember {
        let (charged_bytes, block_bytes, wanted) = match observed {
            Observed::Held { charge } => {
                let (charged, block) = charge.unwrap_or((pool_bytes, MIN_BLOCK_SIZE_BYTES));
                (charged, block, 0)
            }
            // A stopping round never reaches allocation.
            Observed::Stop(_) => (pool_bytes, MIN_BLOCK_SIZE_BYTES, 0),
            Observed::Ready(ready) => (
                ready.state.current_bytes,
                ready.state.block_size_bytes,
                match ready.request.operation {
                    MemoryOperation::Add => ready.request.bytes.min(
                        self.config
                            .max_ram
                            .saturating_sub(ready.base_ram)
                            .saturating_sub(ready.state.current_bytes),
                    ),
                    MemoryOperation::Remove => 0,
                },
            ),
        };
        PoolMember {
            priority: self.config.priority,
            charged_bytes,
            desired_bytes: charged_bytes.saturating_add(wanted),
            block_bytes,
        }
    }

    fn live_state(&self) -> Result<VirtioMemState, String> {
        let state = self.state_source.memory_state()?;
        state.validate().map_err(|error| error.to_string())?;
        Ok(state)
    }

    /// Finish an intent the device has converged to and publish its actual
    /// result; latch on an intent the device never applied or contradicts.
    fn resolve_intent(&mut self, live: VirtioMemState) -> Result<(), HostRuntimeError> {
        match command_ownership(self.control.intent.as_ref(), live) {
            CommandOwnership::None
            | CommandOwnership::UnownedPending
            | CommandOwnership::OwnedPending => Ok(()),
            CommandOwnership::OwnedConvergedUnresolved => {
                let Some(intent) = self.control.intent.take() else {
                    return Ok(());
                };
                let now = self.now()?;
                let reason = match intent.direction {
                    CommandDirection::SettleShrink => {
                        Some("the guest returned only part of the removal".to_owned())
                    }
                    CommandDirection::SettleGrow => {
                        Some("the guest plugged only part of the addition".to_owned())
                    }
                    CommandDirection::Grow | CommandDirection::Shrink => None,
                };
                if let Some(result) =
                    movement_result(&self.control.journal, &intent, live, now, reason)
                {
                    // The Windows driver plugs only above its highest plugged
                    // block; QEMU rejects that once it passes the usable
                    // region, which shrinks only on reset.
                    self.widen_due |= intent.direction == CommandDirection::SettleGrow
                        && result.actual_bytes == 0
                        && !self.widened;
                    self.log(&format!(
                        "event=request_completed operation_id={} {} outcome={:?} actual_bytes={}",
                        intent.operation_id,
                        describe(&result.request),
                        result.outcome,
                        result.actual_bytes
                    ));
                    self.control
                        .journal
                        .record_result(result)
                        .map_err(HostRuntimeError::Controller)?;
                }
                self.save()
            }
            CommandOwnership::RecordedNotApplied => self.latch("recorded command was not applied"),
            CommandOwnership::Conflict => self.latch("recorded command result is unknown"),
        }
    }

    fn observe_pending(
        &mut self,
        state: VirtioMemState,
        stop: &AtomicBool,
    ) -> Result<Step, HostRuntimeError> {
        let owned = self
            .control
            .intent
            .as_ref()
            .filter(|intent| intent.target_bytes == state.requested_bytes)
            .map(|intent| intent.direction);
        let Some(direction) = owned else {
            if !self.unowned_divergence_reported {
                self.log(&format!(
                    "event=ownership_conflict requested_bytes={} current_bytes={} reason=unowned_divergence_replay_suppressed",
                    state.requested_bytes, state.current_bytes
                ));
                self.unowned_divergence_reported = true;
            }
            return Ok(Step::Wait);
        };
        // A grow or shrink holds its target until the guest reaches it: no
        // general stall settle or convergence timeout. Exceptions settle it at
        // its current allocation (`partial`), when commands are allowed:
        // - a grow that has plugged nothing before the one-time widening:
        //   QEMU rejects plugs past its usable region and the driver never
        //   retries, so it settles and the widening lets the next plug land;
        // - a shrink while Windows reports Low memory: the held shrink
        //   occupies the guest's only request slot, so release it and let
        //   the guest ask for memory again;
        // - a shrink stuck at its minimum target for the convergence timeout,
        //   where the driver workaround cannot make another attempt.
        // The widening and the shrink extension answer Windows driver
        // behaviour. The Linux driver retries plugs and removals by itself, so
        // a Linux grow is held and a Linux shrink that has not moved for the
        // convergence timeout settles at what the guest returned.
        let can_settle = !self.control.latched() && !self.config.observe_only;
        let windows = self.config.guest_os == GuestOs::Windows;
        let settle = match direction {
            CommandDirection::Grow => {
                can_settle
                    && windows
                    && !self.widened
                    && self
                        .control
                        .intent
                        .as_ref()
                        .is_some_and(|intent| intent.prior_current_bytes == state.current_bytes)
                    && self.command_stalled(state)
            }
            CommandDirection::Shrink if can_settle => {
                let guest = self.guest.read().ok();
                if guest.as_ref().is_some_and(|envelope| {
                    envelope.native_pressure == Some(MemoryResourceNotificationState::Low)
                }) {
                    self.log("event=held_shrink_released reason=guest_low_memory");
                    true
                } else if !windows {
                    let stalled = self.command_stalled(state)
                        && self.pending_progress.is_some_and(|(_, since)| {
                            since.elapsed() >= self.config.convergence_timeout
                        });
                    if stalled {
                        self.log("event=held_shrink_released reason=removal_stalled");
                    }
                    stalled
                } else {
                    // VIOMEM WORKAROUND (see `extend_shrink`).
                    let base_ram = guest.and_then(|envelope| {
                        envelope
                            .memory
                            .physical_total_bytes
                            .checked_sub(state.current_bytes)
                    });
                    if let Some(base_ram) = base_ram.filter(|_| self.command_stalled(state)) {
                        return self.extend_shrink(state, base_ram, stop);
                    }
                    false
                }
            }
            CommandDirection::Shrink => false,
            CommandDirection::SettleGrow | CommandDirection::SettleShrink => self.pending_elapsed(),
        };
        if !settle {
            self.log_pending_health(direction, state, None);
            return Ok(Step::Wait);
        }
        match direction {
            CommandDirection::Grow | CommandDirection::Shrink => {
                self.settle_short(direction, state, stop)
            }
            CommandDirection::SettleGrow => Ok(Step::Stop(self.convergence_timeout("growth")?)),
            CommandDirection::SettleShrink => Ok(Step::Stop(self.convergence_timeout("shrink")?)),
        }
    }

    // =====================================================================
    // VIOMEM WORKAROUND START: widen a stalled removal one block at a time
    //
    // Unpatched viomem driver bug: each removal only tries a window that
    // starts at the lowest plugged block and is `plugged - requested` long,
    // once per device notification. Memory at the bottom of that window that
    // Windows cannot evacuate stops the removal for good. Observed live on
    // the Windows test VM: each request one block (8 MiB then) lower frees
    // exactly the block it adds to the window, and the gap below `current` stays the same;
    // re-sending a target already tried (bouncing up and back) frees nothing.
    // So each time a held shrink stops moving, request less, and settle as
    // soon as `current` reaches the guest's goal: the admitted request's
    // bytes below the allocation it started from, never below `min_ram`.
    // The step starts at one block and doubles, up to
    // `MAX_EXTENSION_BYTES`, while each step is released in full; a short
    // release drops it back to one block. It never steps past the goal.
    //
    // TO REMOVE once a patched viomem is deployed: delete this block and
    // the marked branch in `observe_pending`.
    // =====================================================================

    /// Largest single extension, whatever the device block size.
    const MAX_EXTENSION_BYTES: u64 = 256 * 1024 * 1024;

    fn extend_shrink(
        &mut self,
        state: VirtioMemState,
        base_ram: u64,
        stop: &AtomicBool,
    ) -> Result<Step, HostRuntimeError> {
        let block = state.block_size_bytes;
        let floor = self
            .config
            .min_ram
            .saturating_sub(base_ram)
            .div_ceil(block)
            .saturating_mul(block);
        let Some(prior_current) = self
            .control
            .intent
            .as_ref()
            .map(|intent| intent.prior_current_bytes)
        else {
            return Ok(Step::Wait);
        };
        let asked = self
            .control
            .journal
            .request
            .as_ref()
            .filter(|request| request.operation == MemoryOperation::Remove)
            .map_or(0, |request| request.bytes);
        let goal = prior_current
            .saturating_sub(asked - asked % block)
            .max(floor);
        let operation_id = self
            .control
            .intent
            .as_ref()
            .map(|intent| intent.operation_id.clone())
            .unwrap_or_default();
        if state.current_bytes <= goal {
            self.log(&format!(
                "event=shrink_extension_done goal_bytes={goal} current_bytes={}",
                state.current_bytes
            ));
            return self.settle_short(CommandDirection::Shrink, state, stop);
        }
        let step = match self.extension.take() {
            Some((operation, from, step))
                if operation == operation_id
                    && from.saturating_sub(state.current_bytes) >= step * block =>
            {
                (step * 2).min((Self::MAX_EXTENSION_BYTES / block).max(1))
            }
            _ => 1,
        };
        let step_bytes = (step * block).min((state.current_bytes - goal).div_ceil(block) * block);
        let Some(target) = state
            .requested_bytes
            .checked_sub(step_bytes)
            .map(|target| target.max(floor))
            .filter(|target| *target < state.requested_bytes)
        else {
            // At the floor the driver workaround cannot widen its scan any
            // further. Release a genuinely stalled request so it cannot
            // occupy the guest's only request slot indefinitely.
            if self
                .pending_progress
                .is_some_and(|(_, since)| since.elapsed() >= self.config.convergence_timeout)
            {
                self.log("event=held_shrink_released reason=minimum_target_stalled");
                return self.settle_short(CommandDirection::Shrink, state, stop);
            }
            self.log_pending_health(CommandDirection::Shrink, state, Some(floor));
            return Ok(Step::Wait);
        };
        if stop.load(Ordering::Acquire) {
            return Ok(Step::Wait);
        }
        let Some(intent) = self.control.intent.as_mut() else {
            return Ok(Step::Wait);
        };
        // The new target is durable before the command, like any other.
        intent.target_bytes = target;
        self.save()?;
        self.pending_progress = None;
        self.extension = Some((operation_id, state.current_bytes, step_bytes / block));
        let command = self
            .resize_sink
            .retarget_pending_guarded(state, target, stop);
        self.log(&format!(
            "event=shrink_extended target_bytes={target} step_bytes={step_bytes} current_bytes={} goal_bytes={goal} floor_bytes={floor} command_result={command:?}",
            state.current_bytes
        ));
        match command {
            Ok(()) => {}
            // Nothing ran: keep the previous target.
            Err(ResizeSinkError::Rejected(_)) => {
                if let Some(intent) = self.control.intent.as_mut() {
                    intent.target_bytes = state.requested_bytes;
                }
                self.save()?;
            }
            Err(error @ ResizeSinkError::CommandUnknown(_)) => {
                self.latch(&format!("extending a shrink failed: {error}"))?;
            }
        }
        Ok(Step::Wait)
    }

    // VIOMEM WORKAROUND END
    // =====================================================================

    /// Request the resize's current allocation, which completes it. The
    /// settlement gets its own deadline; failing it latches.
    fn settle_short(
        &mut self,
        direction: CommandDirection,
        state: VirtioMemState,
        stop: &AtomicBool,
    ) -> Result<Step, HostRuntimeError> {
        if stop.load(Ordering::Acquire) {
            return Ok(Step::Wait);
        }
        let settle = CommandIntent {
            operation_id: self.next_operation_id(),
            started_unix_millis: self.now()?,
            direction: if direction == CommandDirection::Grow {
                CommandDirection::SettleGrow
            } else {
                CommandDirection::SettleShrink
            },
            prior_requested_bytes: state.requested_bytes,
            // The request's result measures movement from the original
            // command, so keep its starting allocation.
            prior_current_bytes: self
                .control
                .intent
                .as_ref()
                .map_or(state.current_bytes, |intent| intent.prior_current_bytes),
            target_bytes: state.current_bytes,
        };
        let operation_id = settle.operation_id.clone();
        self.control.intent = Some(settle);
        self.save()?;
        self.pending_since = None;
        let command = self
            .resize_sink
            .retarget_pending_guarded(state, state.current_bytes, stop);
        self.log(&format!(
            "event=resize_settled_short operation_id={operation_id} direction={direction:?} target_bytes={} kept_bytes={} command_result={command:?}",
            state.requested_bytes, state.current_bytes
        ));
        if let Err(error) = command {
            self.latch(&format!("settling a short {direction:?} failed: {error}"))?;
        }
        Ok(Step::Wait)
    }

    /// Phase two on a converged device: recheck the request at the effect
    /// boundary, grant what is free and execute it.
    fn act<H: HostMemorySource>(
        &mut self,
        ready: Ready,
        ceiling: Option<u64>,
        host_memory: &H,
        host: &mut HostBudget,
        stop: &AtomicBool,
    ) -> Result<Step, HostRuntimeError> {
        let Ready {
            base_ram,
            state,
            request,
            admission,
        } = ready;
        let now = self.now()?;
        let admission_started = Instant::now();
        if let Err(reason) = request.validate_at(
            &self.config.vm_name,
            &self.config.raw_telemetry_service_name,
            now,
            self.future_tolerance_millis()?,
        ) {
            return self.refuse(state, request, admission, reason);
        }
        if !request.bytes.is_multiple_of(state.block_size_bytes) {
            return self.refuse(
                state,
                request,
                admission,
                "request is not a multiple of the device block size".to_owned(),
            );
        }
        let grant = match request.operation {
            MemoryOperation::Add => {
                let available = host_memory
                    .available_bytes()
                    .map_err(HostRuntimeError::HostMemory)?;
                let caps = [
                    ("request", request.bytes),
                    (
                        "guest_maximum",
                        self.config
                            .max_ram
                            .saturating_sub(base_ram)
                            .saturating_sub(state.current_bytes),
                    ),
                    (
                        "device_capacity",
                        state
                            .maximum_target_bytes()
                            .saturating_sub(state.current_bytes),
                    ),
                    (
                        "pool",
                        ceiling.map_or(u64::MAX, |ceiling| {
                            ceiling.saturating_sub(state.current_bytes)
                        }),
                    ),
                    (
                        "host_headroom",
                        available
                            .saturating_sub(host.committed)
                            .saturating_sub(self.config.host_min_headroom_bytes)
                            .saturating_sub(GROW_ADMISSION_MARGIN_BYTES),
                    ),
                ];
                let free = caps.iter().map(|(_, bytes)| *bytes).min().unwrap_or(0);
                let limiting = caps
                    .iter()
                    .filter(|(_, bytes)| *bytes == free)
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(",");
                self.log(&format!("event=grant_limits {} available_bytes={available} committed_bytes={} reserve_bytes={} margin_bytes={} guest_maximum_bytes={} device_capacity_bytes={} pool_bytes={} host_room_bytes={} limiting_factors={limiting} alignment_remainder_bytes={}",
                    describe(&request), host.committed, self.config.host_min_headroom_bytes, GROW_ADMISSION_MARGIN_BYTES,
                    caps[1].1, caps[2].1, caps[3].1, caps[4].1, free % state.block_size_bytes));
                free - free % state.block_size_bytes
            }
            MemoryOperation::Remove => {
                let minimum = self.config.min_ram.saturating_sub(base_ram);
                let removable = state.current_bytes.saturating_sub(minimum);
                let grant = request.bytes.min(removable);
                self.log(&format!("event=grant_limits {} minimum_device_bytes={minimum} removable_bytes={removable} limiting_factors={}", describe(&request), if removable < request.bytes { "guest_minimum" } else { "request" }));
                grant - grant % state.block_size_bytes
            }
        };
        self.log(&format!(
            "event=controller_decision action=grant {} grant_bytes={grant} pool_ceiling_bytes={ceiling:?} requested_bytes={} current_bytes={}",
            describe(&request),
            state.requested_bytes,
            state.current_bytes
        ));
        if self.config.observe_only {
            return self.refuse(
                state,
                request,
                admission,
                format!("observe_only: would grant {grant} bytes"),
            );
        }
        if grant == 0 {
            let result = match request.operation {
                MemoryOperation::Add => MemoryResult {
                    outcome: MemoryOutcome::PoolEmpty,
                    actual_bytes: 0,
                    observed_unix_millis: now,
                    reason: Some("no free pool, device or host-reserve bytes remain".to_owned()),
                    request,
                },
                MemoryOperation::Remove => MemoryResult::rejected(
                    request,
                    now,
                    "the device has no plugged memory to remove".to_owned(),
                ),
            };
            return self.record_unexecuted(state, admission, result);
        }
        let (direction, target_bytes) = match request.operation {
            MemoryOperation::Add => (CommandDirection::Grow, state.current_bytes + grant),
            MemoryOperation::Remove => (CommandDirection::Shrink, state.current_bytes - grant),
        };
        if stop.load(Ordering::Acquire) {
            return Ok(Step::Wait);
        }
        let intent = CommandIntent {
            operation_id: self.next_operation_id(),
            started_unix_millis: now,
            direction,
            prior_requested_bytes: state.requested_bytes,
            prior_current_bytes: state.current_bytes,
            target_bytes,
        };
        let operation_id = intent.operation_id.clone();
        // Admission and intent become durable together, before the command.
        if admission == RequestAdmission::New {
            self.control
                .journal
                .admit(
                    request.clone(),
                    &self.config.vm_name,
                    &self.config.raw_telemetry_service_name,
                    now,
                    self.future_tolerance_millis()?,
                )
                .map_err(HostRuntimeError::Controller)?;
        }
        self.control.intent = Some(intent);
        self.save()?;
        let previously_committed = host.committed;
        let growth = target_bytes.saturating_sub(state.current_bytes);
        // Reserve before invoking a command: even a lost/error response may
        // have started growth and must constrain later members in this round.
        host.committed = host.committed.saturating_add(growth);
        self.outstanding_growth = Some(growth);
        let headroom = self.config.host_min_headroom_bytes;
        let authorize = || {
            validate_grow_headroom(
                state.current_bytes,
                target_bytes,
                host_memory
                    .available_bytes()?
                    .saturating_sub(previously_committed),
                headroom,
            )
        };
        let command_started_unix_millis = now.saturating_add(
            u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        let command_started = Instant::now();
        let command =
            self.resize_sink
                .request_resize_guarded(state, target_bytes, stop, &authorize);
        self.log(&format!(
            "event=resize_requested operation_id={operation_id} direction={direction:?} target_bytes={target_bytes} requested_bytes={} current_bytes={} command_started_unix_millis={command_started_unix_millis} command_elapsed_ms={} command_result={command:?}",
            state.requested_bytes, state.current_bytes, command_started.elapsed().as_millis()
        ));
        match command {
            Ok(()) => {
                self.pending_since = Some(Instant::now());
                self.publish(state, None);
                Ok(Step::Wait)
            }
            // Nothing ran: release the intent and refuse the request.
            Err(ResizeSinkError::Rejected(reason)) => {
                self.control.intent = None;
                self.outstanding_growth = Some(0);
                self.record_unexecuted(
                    state,
                    RequestAdmission::Replay,
                    MemoryResult::rejected(request, now, reason),
                )
            }
            Err(error @ ResizeSinkError::CommandUnknown(_)) => {
                self.latch(&format!("resize command failed: {error}"))?;
                self.publish(state, None);
                // An unknown growth stops this VM; an unknown shrink keeps
                // observing, since it can only free memory.
                Ok(if direction == CommandDirection::Grow {
                    Step::Stop(HostRuntimeError::Resize(error.to_string()))
                } else {
                    Step::Wait
                })
            }
        }
    }

    /// A refusal at the effect boundary. Only an earlier admission needs a
    /// durable result; a new request's refusal is simply published.
    fn refuse(
        &mut self,
        state: VirtioMemState,
        request: MemoryRequest,
        admission: RequestAdmission,
        reason: String,
    ) -> Result<Step, HostRuntimeError> {
        self.log(&format!(
            "event=controller_decision action=refuse {} reason={reason}",
            describe(&request)
        ));
        let result = MemoryResult::rejected(request, self.now()?, reason);
        if admission == RequestAdmission::New {
            self.publish(state, Some(result));
            return Ok(Step::Wait);
        }
        self.record_unexecuted(state, admission, result)
    }

    /// Journal a request that ends without a device command.
    fn record_unexecuted(
        &mut self,
        state: VirtioMemState,
        admission: RequestAdmission,
        result: MemoryResult,
    ) -> Result<Step, HostRuntimeError> {
        let event =
            format!(
            "event=request_completed operation_id=none {} outcome={:?} actual_bytes={} reason={}",
            describe(&result.request), result.outcome, result.actual_bytes,
            result.reason.as_deref().unwrap_or("unspecified")
        );
        if admission == RequestAdmission::New {
            self.control
                .journal
                .admit(
                    result.request.clone(),
                    &self.config.vm_name,
                    &self.config.raw_telemetry_service_name,
                    result.observed_unix_millis,
                    self.future_tolerance_millis()?,
                )
                .map_err(HostRuntimeError::Controller)?;
        }
        self.control
            .journal
            .record_result(result)
            .map_err(HostRuntimeError::Controller)?;
        self.save()?;
        self.log(&event);
        self.publish(state, None);
        Ok(Step::Wait)
    }

    /// Latch durably and give any unresolved request an uncertain result.
    /// An existing latch keeps its original reason.
    fn latch(&mut self, reason: &str) -> Result<(), HostRuntimeError> {
        if self.control.latched() {
            return Ok(());
        }
        let reason = &bounded_reason(reason);
        self.control.latch_reason = Some(reason.to_owned());
        if let (Some(request), None) = (&self.control.journal.request, &self.control.journal.result)
        {
            let uncertain = MemoryResult {
                request: request.clone(),
                outcome: MemoryOutcome::Uncertain,
                actual_bytes: 0,
                observed_unix_millis: self.now()?,
                reason: Some(reason.to_owned()),
            };
            self.control
                .journal
                .record_result(uncertain)
                .map_err(HostRuntimeError::Controller)?;
        }
        self.save()?;
        self.log(&format!("event=actuation_latched reason={reason}"));
        Ok(())
    }

    fn save(&self) -> Result<(), HostRuntimeError> {
        self.checkpoint
            .save(&self.control)
            .map_err(HostRuntimeError::Controller)
    }

    /// Every owned operation that misses its deadline ends the same way:
    /// latch durably so a restart cannot resume actuation, then stop.
    fn convergence_timeout(
        &mut self,
        operation: &str,
    ) -> Result<HostRuntimeError, HostRuntimeError> {
        self.latch(&format!(
            "owned {operation} exceeded the convergence deadline"
        ))?;
        Ok(HostRuntimeError::ConvergenceTimeout(
            self.config.convergence_timeout,
        ))
    }

    /// Widen QEMU's usable region once so the guest's next plug can land
    /// above its highest plugged block. The domain is paused for the two
    /// commands; a request left at the whole device latches and stops.
    fn widen_usable_region(
        &mut self,
        state: VirtioMemState,
        stop: &AtomicBool,
    ) -> Result<Step, HostRuntimeError> {
        self.widen_due = false;
        if self.control.latched() || self.config.observe_only || stop.load(Ordering::Acquire) {
            return Ok(Step::Wait);
        }
        self.widened = true;
        let started = Instant::now();
        let command = self.resize_sink.widen_usable_region_guarded(state, stop);
        self.log(&format!(
            "event=usable_region_widened requested_bytes={} paused_millis={} command_result={command:?}",
            state.requested_bytes,
            started.elapsed().as_millis()
        ));
        match command {
            Ok(()) | Err(ResizeSinkError::Rejected(_)) => Ok(Step::Wait),
            Err(error @ ResizeSinkError::CommandUnknown(_)) => {
                self.latch(&format!("widening the usable region failed: {error}"))?;
                Ok(Step::Stop(HostRuntimeError::Resize(error.to_string())))
            }
        }
    }

    /// Whether the owned resize has not moved for one poll, bounded by the
    /// convergence timeout.
    fn command_stalled(&mut self, state: VirtioMemState) -> bool {
        let bound = self
            .config
            .poll_interval
            .min(self.config.convergence_timeout);
        match self.pending_progress {
            Some((current, since)) if current == state.current_bytes => since.elapsed() >= bound,
            _ => {
                self.pending_progress = Some((state.current_bytes, Instant::now()));
                false
            }
        }
    }

    fn pending_elapsed(&mut self) -> bool {
        let timeout = self.config.convergence_timeout;
        let Some(started) = self
            .control
            .intent
            .as_ref()
            .map(|intent| intent.started_unix_millis)
        else {
            return true;
        };
        let durable_elapsed = SystemUnixClock.now_unix_millis().map_or(true, |now| {
            now < started || u128::from(now - started) >= timeout.as_millis()
        });
        durable_elapsed
            || self
                .pending_since
                .get_or_insert_with(Instant::now)
                .elapsed()
                >= timeout
    }

    /// Tell the guest the device state and the result for its request.
    /// Failure is logged: the guest keeps its pending request and retries.
    fn publish(&mut self, state: VirtioMemState, refusal: Option<MemoryResult>) {
        let published = SystemUnixClock.now_unix_millis().and_then(|now| {
            self.guest.publish_status(&HostMemoryStatus {
                version: MEMORY_REQUEST_VERSION,
                vm_name: self.config.vm_name.clone(),
                service_name: self.config.raw_telemetry_service_name.clone(),
                device_alias: self.config.alias.clone(),
                observed_unix_millis: now,
                device_size_bytes: state.size_bytes,
                block_size_bytes: state.block_size_bytes,
                requested_bytes: state.requested_bytes,
                current_bytes: state.current_bytes,
                result: refusal.or_else(|| self.control.journal.result.clone()),
                guest_settings: self
                    .config
                    .guest_settings
                    .filter(|_| virtio_mem_core::sends_guest_settings_to(self.guest_version)),
            })
        });
        match published {
            Ok(()) => self.status_unpublished_reported = false,
            Err(error) if !self.status_unpublished_reported => {
                self.log(&format!("event=guest_status_unpublished reason={error}"));
                self.status_unpublished_reported = true;
            }
            Err(_) => {}
        }
    }

    fn log_pending_health(
        &mut self,
        direction: CommandDirection,
        state: VirtioMemState,
        floor: Option<u64>,
    ) {
        if self
            .health_report
            .is_some_and(|(when, requested, current)| {
                requested == state.requested_bytes
                    && current == state.current_bytes
                    && when.elapsed() < Duration::from_secs(30)
            })
        {
            return;
        }
        self.health_report = Some((Instant::now(), state.requested_bytes, state.current_bytes));
        let now = match SystemUnixClock.now_unix_millis() {
            Ok(now) => now,
            Err(error) => {
                self.log(&format!("event=diagnostic_clock_failed reason={error}"));
                return;
            }
        };
        let age = self
            .control
            .intent
            .as_ref()
            .map_or(0, |intent| now.saturating_sub(intent.started_unix_millis));
        let idle = self
            .pending_progress
            .map_or(0, |(_, since)| since.elapsed().as_millis());
        let timeout = self.config.convergence_timeout.as_millis();
        let identity = self
            .control
            .journal
            .request
            .as_ref()
            .map_or_else(String::new, describe);
        self.log(&format!("event=control_health direction={direction:?} {identity} requested_bytes={} current_bytes={} remaining_to_device_target_bytes={} pending_age_ms={age} idle_ms={idle} idle_timeout_remaining_ms={} floor_bytes={floor:?}",
            state.requested_bytes, state.current_bytes, state.current_bytes.abs_diff(state.requested_bytes), timeout.saturating_sub(idle)));
    }

    fn next_operation_id(&mut self) -> String {
        self.operation_counter = self.operation_counter.saturating_add(1);
        format!("{}-{}", std::process::id(), self.operation_counter)
    }

    fn now(&self) -> Result<u64, HostRuntimeError> {
        SystemUnixClock
            .now_unix_millis()
            .map_err(HostRuntimeError::Controller)
    }

    fn future_tolerance_millis(&self) -> Result<u64, HostRuntimeError> {
        u64::try_from(self.config.raw_telemetry_future_tolerance.as_millis())
            .map_err(|_| HostRuntimeError::Controller("future tolerance is too large".to_owned()))
    }

    fn log(&self, message: &str) {
        eprintln!(
            "virtio-mem-host: vm={} alias={} {message}",
            self.config.vm_name, self.config.alias
        );
    }
}

fn describe(request: &MemoryRequest) -> String {
    format!(
        "session_id={} sequence={} operation={:?} bytes={}",
        request.identity.session_id, request.identity.sequence, request.operation, request.bytes
    )
}

fn wait_interruptibly(stop: &AtomicBool, duration: Duration) {
    let Some(deadline) = Instant::now().checked_add(duration) else {
        return;
    };
    while !stop.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use virtio_mem_core::{MemoryTelemetrySnapshot, RawTelemetryEnvelope, RequestIdentity};

    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    struct DeviceState {
        live: VirtioMemState,
        /// A shrink stops returning memory at this allocation.
        stall_at: Option<u64>,
        /// A growth stops plugging memory at this allocation.
        grow_stall_at: Option<u64>,
        /// A retarget of a stalled shrink to a new low frees the blocks it
        /// adds to viomem's window; re-sending a tried target frees nothing.
        release_per_notification: bool,
    }

    /// A device that converges toward `requested` whenever it is read.
    #[derive(Clone)]
    struct Device(Rc<RefCell<DeviceState>>);

    impl Device {
        fn new(current_bytes: u64) -> Self {
            Self(Rc::new(RefCell::new(DeviceState {
                live: VirtioMemState {
                    size_bytes: 32 * GIB,
                    block_size_bytes: 2 * MIB,
                    requested_bytes: current_bytes,
                    current_bytes,
                },
                stall_at: None,
                grow_stall_at: None,
                release_per_notification: false,
            })))
        }

        fn live(&self) -> VirtioMemState {
            self.0.borrow().live
        }
    }

    impl MemoryStateSource for Device {
        fn memory_state(&self) -> Result<VirtioMemState, String> {
            let mut device = self.0.borrow_mut();
            let requested = device.live.requested_bytes;
            device.live.current_bytes = if requested >= device.live.current_bytes {
                requested
                    .min(device.grow_stall_at.unwrap_or(u64::MAX))
                    .max(device.live.current_bytes)
            } else {
                requested.max(device.stall_at.unwrap_or(0))
            };
            Ok(device.live)
        }
    }

    struct Sink {
        device: Device,
        targets: Rc<RefCell<Vec<u64>>>,
        failure: Option<ResizeSinkError>,
    }

    impl ResizeSink for Sink {
        fn request_resize_guarded(
            &self,
            expected: VirtioMemState,
            target: u64,
            stop: &AtomicBool,
            authorize: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), ResizeSinkError> {
            if self.device.live() != expected {
                return Err(ResizeSinkError::Rejected("state changed".to_owned()));
            }
            authorize().map_err(ResizeSinkError::Rejected)?;
            if stop.load(Ordering::Acquire) {
                return Err(ResizeSinkError::Rejected("shutdown requested".to_owned()));
            }
            match &self.failure {
                Some(ResizeSinkError::Rejected(reason)) => {
                    return Err(ResizeSinkError::Rejected(reason.clone()))
                }
                // An unknown outcome that did in fact take effect.
                Some(ResizeSinkError::CommandUnknown(reason)) => {
                    self.device.0.borrow_mut().live.requested_bytes = target;
                    self.targets.borrow_mut().push(target);
                    return Err(ResizeSinkError::CommandUnknown(reason.clone()));
                }
                None => {}
            }
            self.device.0.borrow_mut().live.requested_bytes = target;
            self.targets.borrow_mut().push(target);
            Ok(())
        }

        fn retarget_pending_guarded(
            &self,
            _expected: VirtioMemState,
            target: u64,
            _stop: &AtomicBool,
        ) -> Result<(), ResizeSinkError> {
            let mut device = self.device.0.borrow_mut();
            if device.release_per_notification && target < device.live.requested_bytes {
                let added = device.live.requested_bytes - target;
                device.stall_at = device.stall_at.map(|stall| stall.saturating_sub(added));
            }
            device.live.requested_bytes = target;
            self.targets.borrow_mut().push(target);
            Ok(())
        }

        /// Records `WIDENED` and lets later growth plug again.
        fn widen_usable_region_guarded(
            &self,
            _expected: VirtioMemState,
            _stop: &AtomicBool,
        ) -> Result<(), ResizeSinkError> {
            self.device.0.borrow_mut().grow_stall_at = None;
            self.targets.borrow_mut().push(WIDENED);
            Ok(())
        }
    }

    const WIDENED: u64 = u64::MAX;

    #[derive(Default)]
    struct GuestState {
        request: Option<MemoryRequest>,
        statuses: Vec<HostMemoryStatus>,
        native: Option<MemoryResourceNotificationState>,
        /// Report Windows' total as fixed RAM plus this device's current
        /// allocation, as a real guest does; otherwise a fixed 8 GiB.
        device: Option<Device>,
        /// Publish this record version instead of the current one.
        version: Option<u16>,
        /// Publish nothing, as a guest still waiting for its settings.
        silent: bool,
    }

    #[derive(Clone, Default)]
    struct Guest(Rc<RefCell<GuestState>>);

    impl Guest {
        fn send(&self, request: Option<MemoryRequest>) {
            self.0.borrow_mut().request = request;
        }

        fn signal(&self, state: MemoryResourceNotificationState) {
            self.0.borrow_mut().native = Some(state);
        }

        fn track(&self, device: &Device) {
            self.0.borrow_mut().device = Some(device.clone());
        }

        fn last_result(&self) -> Option<MemoryResult> {
            self.0
                .borrow()
                .statuses
                .last()
                .and_then(|status| status.result.clone())
        }
    }

    impl GuestLink for Guest {
        fn read(&self) -> Result<RawTelemetryEnvelope, String> {
            if self.0.borrow().silent {
                return Err("no publication yet".to_owned());
            }
            let mut envelope = RawTelemetryEnvelope::new(
                "guest",
                "VirtioMemService",
                "session",
                now(),
                1,
                0,
                MemoryTelemetrySnapshot {
                    physical_total_bytes: 8 * GIB,
                    physical_available_bytes: GIB,
                    memory_load_percent: 80,
                    commit_total_bytes: 4 * GIB,
                    commit_limit_bytes: 8 * GIB,
                    commit_peak_bytes: 4 * GIB,
                    system_cache_bytes: 0,
                    kernel_paged_bytes: 0,
                    kernel_nonpaged_bytes: 0,
                },
            );
            envelope.request = self.0.borrow().request.clone();
            envelope.native_pressure = self.0.borrow().native;
            if let Some(version) = self.0.borrow().version {
                envelope.version = version;
            }
            if let Some(device) = &self.0.borrow().device {
                envelope.memory.physical_total_bytes = device.live().current_bytes;
            }
            Ok(envelope)
        }

        fn publish_status(&self, status: &HostMemoryStatus) -> Result<(), String> {
            status.validate_for("guest", "VirtioMemService")?;
            self.0.borrow_mut().statuses.push(status.clone());
            Ok(())
        }
    }

    struct HostAvailable(u64);

    impl HostMemorySource for HostAvailable {
        fn available_bytes(&self) -> Result<u64, String> {
            Ok(self.0)
        }
    }

    fn now() -> u64 {
        SystemUnixClock.now_unix_millis().unwrap()
    }

    fn request(sequence: u64, operation: MemoryOperation, bytes: u64) -> MemoryRequest {
        let issued = now() - 1000 + sequence;
        MemoryRequest {
            version: MEMORY_REQUEST_VERSION,
            identity: RequestIdentity {
                vm_name: "guest".to_owned(),
                service_name: "VirtioMemService".to_owned(),
                session_id: "session".to_owned(),
                sequence,
            },
            operation,
            bytes,
            issued_unix_millis: issued,
            expires_unix_millis: issued + 60_000,
        }
    }

    fn config(test: &str, priority: u32) -> HostConfig {
        let path = std::env::temp_dir().join(format!(
            "virtio-mem-runtime-{test}-{priority}-{}.json",
            std::process::id()
        ));
        // A previous failed run may have left state behind.
        let _ = std::fs::remove_file(&path);
        HostConfig {
            vm_name: "guest".to_owned(),
            alias: "memory0".to_owned(),
            priority,
            policy_state_path: path.display().to_string(),
            poll_interval: Duration::from_millis(1),
            command_timeout: Duration::from_secs(10),
            convergence_timeout: Duration::from_secs(300),
            virsh_binary: "virsh".to_owned(),
            libvirt_uri: "qemu:///system".to_owned(),
            raw_telemetry_path: "telemetry.json".to_owned(),
            raw_telemetry_service_name: "VirtioMemService".to_owned(),
            guest_os: virtio_mem_core::settings::GuestOs::Windows,
            raw_telemetry_max_age: Duration::from_secs(60),
            raw_telemetry_future_tolerance: Duration::from_secs(5),
            host_min_headroom_bytes: 4 * GIB,
            compatibility_attestation_path: "attestation.json".to_owned(),
            observe_only: false,
            min_ram: 0,
            max_ram: u64::MAX,
            guest_settings: None,
            telemetry_path: None,
        }
    }

    struct Fixture {
        guest: Guest,
        device: Device,
        targets: Rc<RefCell<Vec<u64>>>,
        config: HostConfig,
    }

    impl Fixture {
        fn new(test: &str, priority: u32, current_bytes: u64) -> Self {
            Self {
                guest: Guest::default(),
                device: Device::new(current_bytes),
                targets: Rc::default(),
                config: config(test, priority),
            }
        }

        fn controller(
            &self,
            failure: Option<ResizeSinkError>,
        ) -> VmController<Guest, Device, Sink> {
            VmController::new(
                self.guest.clone(),
                self.device.clone(),
                Sink {
                    device: self.device.clone(),
                    targets: Rc::clone(&self.targets),
                    failure,
                },
                self.config.clone(),
                "a".repeat(64),
            )
            .expect("controller")
        }

        fn journal(&self) -> DurableControlState {
            let (_, control) =
                Checkpoint::open(&self.config, "a".repeat(64)).expect("reopen checkpoint");
            control
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Test cleanup: a missing file is fine.
            let _ = std::fs::remove_file(&self.config.policy_state_path);
        }
    }

    type Runtime = HostRuntime<Guest, Device, Sink, HostAvailable>;

    fn runtime(
        vms: Vec<VmController<Guest, Device, Sink>>,
        available: u64,
        pool: Option<u64>,
    ) -> Runtime {
        HostRuntime::new(vms, HostAvailable(available), pool).expect("runtime")
    }

    fn round(runtime: &mut Runtime) -> Step {
        runtime.round(&AtomicBool::new(false)).expect("round")
    }

    #[test]
    fn host_enforces_vm_ram_bounds_without_optional_telemetry() {
        for (name, operation, minimum, maximum, expected) in [
            ("ram-max", MemoryOperation::Add, 0, 9 * GIB, 5 * GIB),
            (
                "ram-min",
                MemoryOperation::Remove,
                7 * GIB,
                20 * GIB,
                3 * GIB,
            ),
        ] {
            let mut fixture = Fixture::new(name, 100, 4 * GIB);
            fixture.config.min_ram = minimum;
            fixture.config.max_ram = maximum;
            fixture.guest.send(Some(request(1, operation, 4 * GIB)));
            let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
            round(&mut runtime);
            assert_eq!(*fixture.targets.borrow(), [expected]);
        }
    }

    #[test]
    fn guest_settings_reach_only_guests_that_can_read_them() {
        let settings =
            virtio_mem_core::settings::GlobalSettings::default().guest_policy(GIB, 8 * GIB);
        let published = virtio_mem_core::GuestSettings {
            policy: settings,
            min_ram: 4 * GIB,
            max_ram: 16 * GIB,
            poll_interval_millis: 5000,
        };
        let sent = |silent: bool, version: Option<u16>| {
            let mut fixture = Fixture::new("guest-settings", 100, 4 * GIB);
            fixture.config.guest_settings = Some(published);
            {
                let mut guest = fixture.guest.0.borrow_mut();
                guest.silent = silent;
                guest.version = version;
            }
            let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
            round(&mut runtime);
            let statuses = &fixture.guest.0.borrow().statuses;
            statuses.last().expect("a status").guest_settings
        };
        // A guest still waiting for its settings, and a current one, get them.
        assert_eq!(sent(true, None), Some(published));
        assert_eq!(sent(false, None), Some(published));
        // A guest built before settings were published would reject them.
        assert_eq!(sent(false, Some(5)), None);
    }

    #[test]
    fn optional_diagnostic_failure_does_not_block_memory_control() {
        let mut fixture = Fixture::new("optional-diagnostic", 100, 4 * GIB);
        // A checkpoint file is a regular file, so it cannot be a directory.
        fixture.config.telemetry_path = Some(format!(
            "{}/diagnostic.json",
            fixture.config.policy_state_path
        ));
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Add, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
    }

    #[test]
    fn returns_immediately_for_a_cancelled_runtime() {
        let fixture = Fixture::new("cancelled", 100, 4 * GIB);
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        let stop = AtomicBool::new(true);
        assert!(runtime.run(&stop).is_ok());
        assert!(fixture.guest.0.borrow().statuses.is_empty());
    }

    #[test]
    fn an_add_is_executed_once_and_resolved_with_its_actual_bytes() {
        let fixture = Fixture::new("add", 100, 4 * GIB);
        let add = request(1, MemoryOperation::Add, GIB);
        fixture.guest.send(Some(add.clone()));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);

        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
        assert_eq!(fixture.guest.last_result(), None, "executing");

        // Convergence resolves the request; retransmission only republishes.
        for _ in 0..3 {
            round(&mut runtime);
            let result = fixture.guest.last_result().expect("result");
            assert_eq!(result.request, add);
            assert_eq!(result.outcome, MemoryOutcome::Completed);
            assert_eq!(result.actual_bytes, GIB);
        }
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
        drop(runtime);
        let control = fixture.journal();
        assert!(control.intent.is_none() && !control.latched());
        assert_eq!(
            control.journal.result.map(|result| result.actual_bytes),
            Some(GIB)
        );
    }

    #[test]
    fn the_host_reserve_limits_a_grant_and_no_remainder_is_queued() {
        let fixture = Fixture::new("reserve", 100, 4 * GIB);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Add, 4 * GIB)));
        // Admission retains 256 MiB above the hard 4 GiB reserve.
        let mut runtime = runtime(vec![fixture.controller(None)], 6 * GIB, None);
        round(&mut runtime);
        round(&mut runtime);
        let partial = fixture.guest.last_result().expect("result");
        assert_eq!(partial.outcome, MemoryOutcome::Partial);
        assert_eq!(partial.actual_bytes, 2 * GIB - 256 * MIB);
        round(&mut runtime);
        assert_eq!(
            *fixture.targets.borrow(),
            [6 * GIB - 256 * MIB],
            "no remainder queue"
        );
    }

    #[test]
    fn strict_priority_grants_free_pool_bytes_and_reports_an_empty_pool() {
        let high = Fixture::new("priority", 200, 4 * GIB);
        let low = Fixture::new("priority", 100, 4 * GIB);
        for fixture in [&high, &low] {
            fixture
                .guest
                .send(Some(request(1, MemoryOperation::Add, 2 * GIB)));
        }
        // Configuration order puts the low-priority VM first.
        let mut runtime = runtime(
            vec![low.controller(None), high.controller(None)],
            64 * GIB,
            Some(10 * GIB),
        );
        round(&mut runtime);
        assert_eq!(*high.targets.borrow(), [6 * GIB]);
        assert!(low.targets.borrow().is_empty());
        let empty = low.guest.last_result().expect("pool-empty result");
        assert_eq!(empty.outcome, MemoryOutcome::PoolEmpty);
        assert_eq!(empty.actual_bytes, 0);
        drop(runtime);
        assert!(low.journal().journal.result.is_some(), "journalled");
    }

    #[test]
    fn refusals_before_any_command_are_published_without_latching() {
        let fixture = Fixture::new("observe-only", 100, 4 * GIB);
        let mut observe = fixture.config.clone();
        observe.observe_only = true;
        let add = request(1, MemoryOperation::Add, GIB);
        fixture.guest.send(Some(add.clone()));
        {
            let mut runtime = runtime(
                vec![VmController::new(
                    fixture.guest.clone(),
                    fixture.device.clone(),
                    Sink {
                        device: fixture.device.clone(),
                        targets: Rc::clone(&fixture.targets),
                        failure: None,
                    },
                    observe,
                    "a".repeat(64),
                )
                .expect("controller")],
                64 * GIB,
                None,
            );
            round(&mut runtime);
        }
        let refused = fixture.guest.last_result().expect("refusal");
        assert_eq!(refused.outcome, MemoryOutcome::Rejected);
        assert!(refused
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("would grant 1073741824")));
        assert!(fixture.targets.borrow().is_empty());
        assert_eq!(fixture.journal(), DurableControlState::default());

        // A command refused at its final check releases the intent unlatched.
        let mut runtime = runtime(
            vec![fixture.controller(Some(ResizeSinkError::Rejected(
                "attestation drifted".to_owned(),
            )))],
            64 * GIB,
            None,
        );
        round(&mut runtime);
        drop(runtime);
        let control = fixture.journal();
        assert!(!control.latched() && control.intent.is_none());
        assert_eq!(
            control.journal.result.map(|result| result.outcome),
            Some(MemoryOutcome::Rejected)
        );
    }

    #[test]
    fn stale_or_superseded_requests_are_refused_without_admission() {
        let fixture = Fixture::new("stale", 100, 4 * GIB);
        let mut expired = request(1, MemoryOperation::Add, GIB);
        expired.issued_unix_millis = now() - 120_000;
        expired.expires_unix_millis = expired.issued_unix_millis + 60_000;
        fixture.guest.send(Some(expired));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        assert_eq!(
            fixture.guest.last_result().map(|result| result.outcome),
            Some(MemoryOutcome::Rejected)
        );

        fixture
            .guest
            .send(Some(request(5, MemoryOperation::Add, GIB)));
        round(&mut runtime);
        round(&mut runtime);
        // An older identity after a newer one is a replay, not a new delta.
        fixture
            .guest
            .send(Some(request(4, MemoryOperation::Add, GIB)));
        round(&mut runtime);
        assert_eq!(
            fixture.guest.last_result().map(|result| result.outcome),
            Some(MemoryOutcome::Rejected)
        );
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
    }

    #[test]
    fn an_unknown_growth_outcome_is_uncertain_latched_and_never_replayed() {
        let fixture = Fixture::new("unknown", 100, 4 * GIB);
        let add = request(1, MemoryOperation::Add, GIB);
        fixture.guest.send(Some(add.clone()));
        {
            let mut runtime = runtime(
                vec![fixture.controller(Some(ResizeSinkError::CommandUnknown(
                    "virsh timed out".to_owned(),
                )))],
                64 * GIB,
                None,
            );
            assert!(matches!(round(&mut runtime), Step::Stop(_)));
        }
        assert_eq!(
            fixture.guest.last_result().map(|result| result.outcome),
            Some(MemoryOutcome::Uncertain)
        );
        assert!(fixture.journal().latched());

        // After restart the effect is observed, not replayed.
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        let observed = fixture.guest.last_result().expect("result");
        assert_eq!(observed.outcome, MemoryOutcome::Completed);
        fixture
            .guest
            .send(Some(request(2, MemoryOperation::Add, GIB)));
        round(&mut runtime);
        assert_eq!(
            fixture.guest.last_result().map(|result| result.outcome),
            Some(MemoryOutcome::Rejected),
            "latched actuation refuses new requests"
        );
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
    }

    #[test]
    fn a_removal_stalled_at_the_floor_settles_without_losing_its_result() {
        let mut fixture = Fixture::new("floor-shrink", 100, 8 * GIB);
        fixture.config.convergence_timeout = Duration::from_millis(20);
        fixture.guest.track(&fixture.device);
        fixture.config.min_ram = 7 * GIB;
        fixture.device.0.borrow_mut().stall_at = Some(7 * GIB + 512 * MIB);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Remove, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [7 * GIB]);
        assert!(fixture.guest.last_result().is_none());
        thread::sleep(Duration::from_millis(30));
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [7 * GIB, 7 * GIB + 512 * MIB]);
        let result = fixture.guest.last_result().expect("partial result");
        assert_eq!(result.outcome, MemoryOutcome::Partial);
        assert_eq!(result.actual_bytes, 512 * MIB);
        // Pressure can now issue a fresh addition rather than remaining blocked.
        fixture
            .guest
            .send(Some(request(2, MemoryOperation::Add, GIB)));
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(
            fixture.guest.last_result().unwrap().outcome,
            MemoryOutcome::Completed
        );
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    // VIOMEM WORKAROUND test: delete with the workaround.
    #[test]
    fn a_stalled_removal_widens_in_growing_steps_and_stops_at_the_goal() {
        let mut fixture = Fixture::new("extend-shrink", 100, 8 * GIB);
        fixture.config.poll_interval = Duration::from_millis(2);
        {
            let mut device = fixture.device.0.borrow_mut();
            device.stall_at = Some(7 * GIB + 12 * MIB);
            device.release_per_notification = true;
        }
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Remove, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        for _ in 0..12 {
            round(&mut runtime);
            thread::sleep(Duration::from_millis(5));
        }
        // 7 GiB, then extensions of 2, 4 and (doubling to 8, capped at the
        // 6 MiB left) 6 MiB, each released in full; at the goal the shrink
        // settles at what the guest kept, not further down.
        assert_eq!(
            *fixture.targets.borrow(),
            [
                7 * GIB,
                7 * GIB - 2 * MIB,
                7 * GIB - 6 * MIB,
                7 * GIB - 12 * MIB,
                7 * GIB
            ]
        );
        let result = fixture.guest.last_result().expect("result");
        assert_eq!(result.outcome, MemoryOutcome::Completed);
        assert_eq!(result.actual_bytes, GIB);
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    #[test]
    fn a_held_removal_is_released_when_windows_reports_low_memory() {
        let fixture = Fixture::new("low-release", 100, 8 * GIB);
        fixture.device.0.borrow_mut().stall_at = Some(7 * GIB + 512 * MIB);
        fixture.guest.signal(MemoryResourceNotificationState::High);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Remove, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [7 * GIB]);

        // Windows runs short: settle at what it reached, report partial.
        fixture.guest.signal(MemoryResourceNotificationState::Low);
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [7 * GIB, 7 * GIB + 512 * MIB]);
        let result = fixture.guest.last_result().expect("result");
        assert_eq!(result.outcome, MemoryOutcome::Partial);
        assert_eq!(result.actual_bytes, 512 * MIB);
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    #[test]
    fn a_linux_removal_is_never_extended_and_settles_once_it_stops_moving() {
        let mut fixture = Fixture::new("linux-shrink", 100, 8 * GIB);
        fixture.config.guest_os = GuestOs::Linux;
        fixture.config.poll_interval = Duration::from_millis(2);
        fixture.config.convergence_timeout = Duration::from_millis(300);
        fixture.device.0.borrow_mut().stall_at = Some(7 * GIB + 512 * MIB);
        fixture.guest.signal(MemoryResourceNotificationState::High);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Remove, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        for _ in 0..4 {
            round(&mut runtime);
            thread::sleep(Duration::from_millis(5));
        }
        // Stalled for several polls: the Windows driver workaround would have
        // lowered the target by now.
        assert_eq!(*fixture.targets.borrow(), [7 * GIB]);
        assert!(fixture.guest.last_result().is_none());

        thread::sleep(Duration::from_millis(350));
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [7 * GIB, 7 * GIB + 512 * MIB]);
        let result = fixture.guest.last_result().expect("result");
        assert_eq!(result.outcome, MemoryOutcome::Partial);
        assert_eq!(result.actual_bytes, 512 * MIB);
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    #[test]
    fn a_partly_plugged_growth_holds_its_target() {
        let mut fixture = Fixture::new("hold-grow", 100, 4 * GIB);
        fixture.config.poll_interval = Duration::from_millis(2);
        fixture.config.convergence_timeout = Duration::from_millis(1);
        fixture.device.0.borrow_mut().grow_stall_at = Some(4 * GIB + 512 * MIB);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Add, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        for _ in 0..4 {
            round(&mut runtime);
            thread::sleep(Duration::from_millis(5));
        }
        // No settle and no widening: only the original target was set.
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
        assert!(fixture.guest.last_result().is_none());
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    #[test]
    fn a_growth_the_guest_never_plugs_settles_widens_once_and_grows_again() {
        let mut fixture = Fixture::new("grow-stall", 100, 4 * GIB);
        fixture.config.poll_interval = Duration::from_millis(2);
        // The guest driver's plug is rejected: nothing is ever plugged.
        fixture.device.0.borrow_mut().grow_stall_at = Some(4 * GIB);
        fixture
            .guest
            .send(Some(request(1, MemoryOperation::Add, GIB)));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(*fixture.targets.borrow(), [5 * GIB]);
        thread::sleep(Duration::from_millis(10));
        round(&mut runtime);
        round(&mut runtime);
        // The poll that resolves the settle widens the usable region once.
        assert_eq!(*fixture.targets.borrow(), [5 * GIB, 4 * GIB, WIDENED]);
        let result = fixture.guest.last_result().expect("result");
        assert_eq!(result.outcome, MemoryOutcome::Partial);
        assert_eq!(result.actual_bytes, 0);

        // The guest's renewed request then plugs; widening is not repeated.
        fixture
            .guest
            .send(Some(request(2, MemoryOperation::Add, GIB)));
        round(&mut runtime);
        round(&mut runtime);
        round(&mut runtime);
        assert_eq!(
            *fixture.targets.borrow(),
            [5 * GIB, 4 * GIB, WIDENED, 5 * GIB]
        );
        let result = fixture.guest.last_result().expect("result");
        assert_eq!(result.outcome, MemoryOutcome::Completed);
        drop(runtime);
        assert!(!fixture.journal().latched());
    }

    #[test]
    fn restart_latches_a_recorded_command_that_never_applied() {
        let fixture = Fixture::new("not-applied", 100, 4 * GIB);
        let add = request(1, MemoryOperation::Add, GIB);
        {
            let (checkpoint, mut control) =
                Checkpoint::open(&fixture.config, "a".repeat(64)).expect("checkpoint");
            control
                .journal
                .admit(add.clone(), "guest", "VirtioMemService", now(), 0)
                .expect("admit");
            control.intent = Some(CommandIntent {
                operation_id: "earlier".to_owned(),
                started_unix_millis: now(),
                direction: CommandDirection::Grow,
                prior_requested_bytes: 4 * GIB,
                prior_current_bytes: 4 * GIB,
                target_bytes: 5 * GIB,
            });
            checkpoint.save(&control).expect("save");
        }
        fixture.guest.send(Some(add));
        let mut runtime = runtime(vec![fixture.controller(None)], 64 * GIB, None);
        round(&mut runtime);
        assert!(fixture.targets.borrow().is_empty());
        assert_eq!(
            fixture.guest.last_result().map(|result| result.outcome),
            Some(MemoryOutcome::Uncertain)
        );
        drop(runtime);
        assert!(fixture.journal().latched());
    }
}
