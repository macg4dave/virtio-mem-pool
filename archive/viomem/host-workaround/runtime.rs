// ARCHIVED, NOT COMPILED. The viomem workarounds removed from host/src/runtime.rs.
// On pause until the virtio-win viomem removal bug is repaired; see ../README.md.

// ---- ResizeSink trait method: host/src/runtime.rs lines 59,69 (commit e665205) ----
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

// ---- HostRuntime fields: host/src/runtime.rs lines 131,139 (commit e665205) ----
    /// VIOMEM WORKAROUND: the last shrink extension's operation, allocation
    /// and step in blocks.
    extension: Option<(String, u64, u64)>,
    /// A growth settled without plugging anything: widen QEMU's usable
    /// region on the next converged poll.
    widen_due: bool,
    /// Widening was tried since the device last had nothing plugged (a VM
    /// reset shrinks the usable region again); it is not repeated.
    widened: bool,

// ---- converged poll: one-time widening: host/src/runtime.rs lines 425,432 (commit e665205) ----
        if state.current_bytes == 0 {
            self.widened = false;
        }
        if self.widen_due {
            if let Step::Stop(error) = self.widen_usable_region(state, stop)? {
                return Ok(Observed::Stop(error));
            }
        }

// ---- movement result: widening due: host/src/runtime.rs lines 601,606 (commit e665205) ----
                    // The Windows driver plugs only above its highest plugged
                    // block; QEMU rejects that once it passes the usable
                    // region, which shrinks only on reset.
                    self.widen_due |= intent.direction == CommandDirection::SettleGrow
                        && result.actual_bytes == 0
                        && !self.widened;

// ---- observe_pending: Windows branches: host/src/runtime.rs lines 646,705 (commit e665205) ----
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

// ---- extend_shrink: host/src/runtime.rs lines 722,854 (commit e665205) ----
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

// ---- widen_usable_region: host/src/runtime.rs lines 1213,1240 (commit e665205) ----
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
