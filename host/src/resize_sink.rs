use std::sync::atomic::{AtomicBool, Ordering};
use virtio_mem_core::{bytes_to_kibibytes, parse_virtio_mem_xml_for_alias, VirtioMemState};

use crate::compatibility_source::CompatibilitySource;
use crate::runtime::{ResizeSink, ResizeSinkError};
use crate::virsh::VirshCommand;

pub struct VirshResizeSink<C, E> {
    command: C,
    vm_name: String,
    alias: String,
    compatibility_source: E,
}

/// The validated `virsh update-memory-device` arguments for one resize.
type PreparedResize = Vec<String>;

impl<C, E: CompatibilitySource> VirshResizeSink<C, E> {
    /// Every resize adapter requires its evidence source explicitly. Production
    /// passes live attestation checks; tests inject their own source.
    pub fn new(
        command: C,
        vm_name: impl Into<String>,
        alias: impl Into<String>,
        compatibility_source: E,
    ) -> Self {
        Self {
            command,
            vm_name: vm_name.into(),
            alias: alias.into(),
            compatibility_source,
        }
    }
}

impl<C: VirshCommand, E: CompatibilitySource> ResizeSink for VirshResizeSink<C, E> {
    fn request_resize_guarded(
        &self,
        expected: VirtioMemState,
        target: u64,
        stop: &AtomicBool,
        authorize: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), ResizeSinkError> {
        let prepared = self
            .prepare(target, |state| {
                if *state != expected || state.requested_bytes != state.current_bytes {
                    return Err("live device state changed since the resize decision".to_owned());
                }
                Ok(())
            })
            .map_err(ResizeSinkError::Rejected)?;
        authorize().map_err(ResizeSinkError::Rejected)?;
        self.apply_unless_stopped(prepared, stop)
    }

    fn retarget_pending_guarded(
        &self,
        expected: VirtioMemState,
        target: u64,
        stop: &AtomicBool,
    ) -> Result<(), ResizeSinkError> {
        let prepared = self
            .prepare(target, |state| {
                if *state != expected || state.requested_bytes == state.current_bytes {
                    return Err(
                        "live pending resize changed since the retarget decision".to_owned()
                    );
                }
                Ok(())
            })
            .map_err(ResizeSinkError::Rejected)?;
        self.apply_unless_stopped(prepared, stop)
    }
}

impl<C: VirshCommand, E: CompatibilitySource> VirshResizeSink<C, E> {
    fn apply_prepared(&self, prepared: PreparedResize) -> Result<(), String> {
        self.command
            .run(&prepared)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn apply_unless_stopped(
        &self,
        prepared: PreparedResize,
        stop: &AtomicBool,
    ) -> Result<(), ResizeSinkError> {
        if stop.load(Ordering::Acquire) {
            return Err(ResizeSinkError::Rejected("shutdown requested".to_owned()));
        }
        self.apply_prepared(prepared)
            .map_err(ResizeSinkError::CommandUnknown)
    }

    /// Reads live XML immediately before a command, validates compatibility and
    /// the target, then applies the operation's own live-state precondition.
    /// The fresh read is the race check; callers must not reuse an older one.
    fn prepare(
        &self,
        target_bytes: u64,
        precondition: impl FnOnce(&VirtioMemState) -> Result<(), String>,
    ) -> Result<PreparedResize, String> {
        let compatibility = self.compatibility_source.compatibility()?;
        let snapshot = self
            .command
            .run(&["dumpxml".to_owned(), self.vm_name.clone()])
            .map_err(|error| error.to_string())?;
        let snapshot = parse_virtio_mem_xml_for_alias(&snapshot, &self.alias)
            .map_err(|error| error.to_string())?;
        snapshot
            .compatibility
            .merge(compatibility)
            .map_err(|error| error.to_string())?
            .validate_for_resize()
            .map_err(|error| error.to_string())?;
        let state = snapshot.memory;
        state
            .validate_target(target_bytes)
            .map_err(|error| error.to_string())?;
        precondition(&state)?;
        let requested_kib = bytes_to_kibibytes(target_bytes)
            .ok_or_else(|| "resize target must be an integer number of KiB".to_owned())?;
        Ok(vec![
            "update-memory-device".to_owned(),
            self.vm_name.clone(),
            "--alias".to_owned(),
            self.alias.clone(),
            "--requested-size".to_owned(),
            requested_kib.to_string(),
            "--live".to_owned(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::compatibility_source::FixedCompatibilitySource;
    use crate::virsh::{VirshCommand, VirshError};
    use virtio_mem_core::VirtioMemCompatibility;

    struct DriftedAttestation;

    impl CompatibilitySource for DriftedAttestation {
        fn compatibility(&self) -> Result<VirtioMemCompatibility, String> {
            Err("live compatibility evidence drifted: domain_xml".to_owned())
        }
    }

    const CONVERGED_XML: &str = "<domain><memory model='virtio-mem' dynamic-memslots='on' unplugged-inaccessible='on'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>4</requested><current unit='GiB'>4</current></target><alias name='memory0'/></memory></domain>";
    const PENDING_XML: &str = "<domain><memory model='virtio-mem' dynamic-memslots='on' unplugged-inaccessible='on'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>6</requested><current unit='GiB'>4</current></target><alias name='memory0'/></memory></domain>";
    const SHRINK_PENDING_XML: &str = "<domain><memory model='virtio-mem' dynamic-memslots='on' unplugged-inaccessible='on'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>4</requested><current unit='GiB'>6</current></target><alias name='memory0'/></memory></domain>";
    const UNKNOWN_COMPATIBILITY_XML: &str = "<domain><memory model='virtio-mem'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>4</requested><current unit='GiB'>4</current></target><alias name='memory0'/></memory></domain>";

    struct Fake {
        xml: &'static str,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl VirshCommand for Fake {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            self.calls.borrow_mut().push(arguments.to_vec());
            if arguments[0] == "dumpxml" {
                Ok(self.xml.to_owned())
            } else {
                Ok(String::new())
            }
        }
    }

    fn converged() -> VirtioMemState {
        parse_virtio_mem_xml_for_alias(CONVERGED_XML, "memory0")
            .unwrap()
            .memory
    }

    struct FailedUpdateFake {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl VirshCommand for FailedUpdateFake {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            self.calls.borrow_mut().push(arguments.to_vec());
            if arguments[0] == "dumpxml" {
                Ok(CONVERGED_XML.to_owned())
            } else {
                Err(VirshError::Timeout(std::time::Duration::from_secs(10)))
            }
        }
    }

    // PAUSED (viomem): the usable-region widening tests are archived in
    // archive/viomem/host-workaround/tests.rs until the driver bug is repaired.

    #[test]
    fn refreshes_state_before_sending_one_validated_resize() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            xml: CONVERGED_XML,
            calls: Rc::clone(&calls),
        };
        let sink = VirshResizeSink::new(
            fake,
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );

        sink.request_resize_guarded(
            converged(),
            6 * 1024 * 1024,
            &AtomicBool::new(false),
            &|| Ok(()),
        )
        .expect("aligned converged request succeeds");

        assert_eq!(
            calls.take(),
            vec![
                vec!["dumpxml", "guest"],
                vec![
                    "update-memory-device",
                    "guest",
                    "--alias",
                    "memory0",
                    "--requested-size",
                    "6144",
                    "--live",
                ],
            ]
            .into_iter()
            .map(|arguments| arguments.into_iter().map(str::to_owned).collect())
            .collect::<Vec<Vec<String>>>(),
        );
    }

    #[test]
    fn final_guard_rejects_drift_and_cancellation_during_compatibility() {
        struct Cancelling<'a>(&'a AtomicBool);
        impl CompatibilitySource for Cancelling<'_> {
            fn compatibility(&self) -> Result<VirtioMemCompatibility, String> {
                self.0.store(true, Ordering::Release);
                Ok(VirtioMemCompatibility::confirmed())
            }
        }
        let expected = parse_virtio_mem_xml_for_alias(CONVERGED_XML, "memory0")
            .unwrap()
            .memory;
        let stop = AtomicBool::new(false);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            Fake {
                xml: CONVERGED_XML,
                calls: calls.clone(),
            },
            "guest",
            "memory0",
            Cancelling(&stop),
        );
        assert!(matches!(
            sink.request_resize_guarded(expected, 5 << 30, &stop, &|| Ok(())),
            Err(ResizeSinkError::Rejected(_))
        ));
        assert_eq!(calls.borrow().len(), 1);
        stop.store(false, Ordering::Release);
        let sink = VirshResizeSink::new(
            Fake {
                xml: CONVERGED_XML,
                calls: calls.clone(),
            },
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );
        let drifted = VirtioMemState {
            current_bytes: 3 << 30,
            requested_bytes: 3 << 30,
            ..expected
        };
        assert!(matches!(
            sink.request_resize_guarded(drifted, 5 << 30, &stop, &|| Ok(())),
            Err(ResizeSinkError::Rejected(_))
        ));
        assert!(calls.borrow().iter().all(|call| call[0] == "dumpxml"));
        assert!(matches!(
            sink.request_resize_guarded(expected, 5 << 30, &stop, &|| Err(
                "headroom changed".to_owned()
            )),
            Err(ResizeSinkError::Rejected(_))
        ));
    }

    /// A resize of the converged fixture device to 6 MiB.
    fn resize_to_6_mib<C: VirshCommand, E: CompatibilitySource>(
        sink: &VirshResizeSink<C, E>,
    ) -> Result<(), ResizeSinkError> {
        let expected = parse_virtio_mem_xml_for_alias(CONVERGED_XML, "memory0")
            .expect("fixture")
            .memory;
        sink.request_resize_guarded(expected, 6 * 1024 * 1024, &AtomicBool::new(false), &|| {
            Ok(())
        })
    }

    #[test]
    fn rejects_pending_state_without_sending_a_resize() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            xml: PENDING_XML,
            calls: Rc::clone(&calls),
        };
        let sink = VirshResizeSink::new(
            fake,
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );

        assert!(matches!(
            resize_to_6_mib(&sink),
            Err(ResizeSinkError::Rejected(_))
        ));
        assert_eq!(calls.take().len(), 1);
    }

    #[test]
    fn distinguishes_preflight_rejection_from_an_unknown_command_outcome() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            FailedUpdateFake {
                calls: Rc::clone(&calls),
            },
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );

        let error = sink
            .request_resize_guarded(
                converged(),
                6 * 1024 * 1024,
                &AtomicBool::new(false),
                &|| Ok(()),
            )
            .expect_err("timed-out update has an unknown outcome");

        assert!(matches!(error, ResizeSinkError::CommandUnknown(_)));
        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn a_stalled_shrink_settles_only_at_its_unchanged_current_allocation() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            Fake {
                xml: SHRINK_PENDING_XML,
                calls: Rc::clone(&calls),
            },
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );
        let pending = parse_virtio_mem_xml_for_alias(SHRINK_PENDING_XML, "memory0")
            .unwrap()
            .memory;
        let stop = AtomicBool::new(false);

        assert!(sink
            .retarget_pending_guarded(
                VirtioMemState {
                    current_bytes: 5 << 30,
                    ..pending
                },
                5 << 30,
                &stop
            )
            .is_err());
        sink.retarget_pending_guarded(pending, pending.current_bytes, &stop)
            .expect("an unchanged pending shrink settles at current");
        let calls = calls.borrow();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2][5], "6291456");
    }

    #[test]
    fn rejects_unknown_compatibility_before_sending_a_resize() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            xml: UNKNOWN_COMPATIBILITY_XML,
            calls: Rc::clone(&calls),
        };
        let sink = VirshResizeSink::new(
            fake,
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::unknown()),
        );

        let error = resize_to_6_mib(&sink).expect_err("unknown compatibility must fail closed");
        assert!(
            matches!(error, ResizeSinkError::Rejected(message) if message.contains("dynamic-memslots"))
        );
        assert_eq!(calls.take().len(), 1);
    }

    #[test]
    fn rejects_attestation_drift_before_sending_a_resize() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            Fake {
                xml: CONVERGED_XML,
                calls: Rc::clone(&calls),
            },
            "guest",
            "memory0",
            DriftedAttestation,
        );

        let error = resize_to_6_mib(&sink).expect_err("drift must fail before actuation");
        assert!(
            matches!(error, ResizeSinkError::Rejected(message) if message.contains("drifted: domain_xml"))
        );
        assert_eq!(calls.take(), Vec::<Vec<String>>::new());
    }
}
