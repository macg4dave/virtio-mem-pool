// ARCHIVED, NOT COMPILED. Tests for the viomem workarounds.
// On pause until the virtio-win viomem removal bug is repaired; see ../README.md.

// ---- runtime test fake: widening: host/src/runtime.rs lines 1513,1525 (commit e665205) ----
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

// ---- runtime test: stalled removal widens: host/src/runtime.rs lines 2042,2079 (commit e665205) ----
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


// ---- runtime test: growth widens once: host/src/runtime.rs lines 2158,2196 (commit e665205) ----
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


// ---- resize_sink test fake and tests: host/src/resize_sink.rs lines 264,350 (commit e665205) ----
    /// Records calls and fails every `update-memory-device` from the given
    /// (zero-based) one onwards.
    struct ScriptedFake {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        fail_updates_from: usize,
    }

    impl VirshCommand for ScriptedFake {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            let updates = self
                .calls
                .borrow()
                .iter()
                .filter(|call| call[0] == "update-memory-device")
                .count();
            self.calls.borrow_mut().push(arguments.to_vec());
            match arguments[0].as_str() {
                "dumpxml" => Ok(CONVERGED_XML.to_owned()),
                "update-memory-device" if updates >= self.fail_updates_from => {
                    Err(VirshError::Timeout(std::time::Duration::from_secs(10)))
                }
                _ => Ok(String::new()),
            }
        }
    }

    fn widen_with(fail_updates_from: usize) -> (Result<(), ResizeSinkError>, Vec<String>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            ScriptedFake {
                calls: Rc::clone(&calls),
                fail_updates_from,
            },
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );
        let result = sink.widen_usable_region_guarded(converged(), &AtomicBool::new(false));
        let actions = calls
            .borrow()
            .iter()
            .filter(|call| call[0] != "dumpxml")
            .map(|call| match call[0].as_str() {
                "update-memory-device" => format!("update {}", call[5]),
                operation => operation.to_owned(),
            })
            .collect();
        (result, actions)
    }

    #[test]
    fn widening_pauses_requests_the_ceiling_restores_and_resumes() {
        let (result, actions) = widen_with(usize::MAX);
        result.expect("widening succeeds");
        // An 8 GiB device: the 8 GiB ceiling, then the converged 4 GiB again.
        assert_eq!(
            actions,
            ["suspend", "update 8388608", "update 4194304", "resume"]
        );
    }

    #[test]
    fn widening_never_resumes_with_the_ceiling_requested() {
        let (result, actions) = widen_with(1);
        assert!(
            matches!(&result, Err(ResizeSinkError::CommandUnknown(message)) if message.contains("left paused")),
            "{result:?}"
        );
        assert_eq!(
            actions,
            [
                "suspend",
                "update 8388608",
                "update 4194304",
                "update 4194304"
            ]
        );

        // A failed widening command may still have applied: restore before
        // resuming, and stay paused when the restore fails too.
        let (result, actions) = widen_with(0);
        assert!(
            matches!(&result, Err(ResizeSinkError::CommandUnknown(message)) if message.contains("left paused"))
        );
        assert!(!actions.contains(&"resume".to_owned()), "{actions:?}");
    }


// ---- runtime test fake: DeviceState field and retarget release (lines 1415-1417, 1503-1507) ----
        /// A retarget of a stalled shrink to a new low frees the blocks it
        /// adds to viomem's window; re-sending a tried target frees nothing.
        release_per_notification: bool,
            let mut device = self.device.0.borrow_mut();
            if device.release_per_notification && target < device.live.requested_bytes {
                let added = device.live.requested_bytes - target;
                device.stall_at = device.stall_at.map(|stall| stall.saturating_sub(added));
            }
