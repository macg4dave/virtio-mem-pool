// ARCHIVED, NOT COMPILED. The viomem workarounds removed from host/src/resize_sink.rs.
// On pause until the virtio-win viomem removal bug is repaired; see ../README.md.

// ---- VirshResizeSink::widen_usable_region_guarded: host/src/resize_sink.rs lines 75,127 (commit e665205) ----
    fn widen_usable_region_guarded(
        &self,
        expected: VirtioMemState,
        stop: &AtomicBool,
    ) -> Result<(), ResizeSinkError> {
        let converged = |state: &VirtioMemState| {
            if *state != expected || state.requested_bytes != state.current_bytes {
                return Err("live device changed since the widening decision".to_owned());
            }
            Ok(())
        };
        // The largest valid target; QEMU adds its extent above it.
        let ceiling = expected.maximum_target_bytes();
        let widen = self
            .prepare(ceiling, converged)
            .map_err(ResizeSinkError::Rejected)?;
        let restore = self
            .prepare(expected.requested_bytes, converged)
            .map_err(ResizeSinkError::Rejected)?;
        if stop.load(Ordering::Acquire) {
            return Err(ResizeSinkError::Rejected("shutdown requested".to_owned()));
        }
        let domain = |operation: &str| {
            self.command
                .run(&[operation.to_owned(), self.vm_name.clone()])
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        domain("suspend").map_err(ResizeSinkError::Rejected)?;
        // The guest cannot act on the device while paused. Never resume with
        // the whole device possibly requested: the driver would plug all of
        // it. A failed widening command may still have applied, so the
        // restore always runs.
        let widened = self.apply_prepared(widen);
        let restored = self
            .apply_prepared(restore)
            .or_else(|_| self.apply_prepared(self.prepare_restore(expected)?));
        if let Err(error) = restored {
            return Err(ResizeSinkError::CommandUnknown(format!(
                "{WIDEN_UNRESTORED}: {error}"
            )));
        }
        match (widened, domain("resume")) {
            (Ok(()), Ok(())) => Ok(()),
            (_, Err(error)) => Err(ResizeSinkError::CommandUnknown(format!(
                "domain could not be resumed after widening: {error}"
            ))),
            (Err(error), Ok(())) => Err(ResizeSinkError::CommandUnknown(format!(
                "widening the usable region failed after pausing: {error}"
            ))),
        }
    }
}

// ---- WIDEN_UNRESTORED and prepare_restore: host/src/resize_sink.rs lines 129,143 (commit e665205) ----
/// Prefix of a widening failure that left the whole device requested; the
/// domain is deliberately left paused for operator recovery.
const WIDEN_UNRESTORED: &str = "requested size was not restored after widening; domain left paused";

impl<C: VirshCommand, E: CompatibilitySource> VirshResizeSink<C, E> {
    /// The restoring command for a widened device, built without the live
    /// precondition (the device then requests its whole size).
    fn prepare_restore(&self, expected: VirtioMemState) -> Result<PreparedResize, String> {
        self.prepare(expected.requested_bytes, |state| {
            if state.current_bytes != expected.current_bytes {
                return Err("device allocation changed while paused".to_owned());
            }
            Ok(())
        })
    }
