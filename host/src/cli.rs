//! The operator actions behind `rampire status` and `rampire recover`: a
//! read-only status and the reviewed latch recovery. The command line
//! itself is the `rampire` binary's.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use rampire_core::{GuestInputStatus, VirtioMemState};

use crate::attestation::AttestedCompatibilitySource;
use crate::checkpoint::{actuation_latched, clear_actuation_latch, read_controller_status};
use crate::compatibility_source::CompatibilitySource;
use crate::config::HostConfig;
use crate::guest_channel::{GuestChannel, GuestLink, SystemUnixClock, UnixClock};
use crate::qga::VirshGuestFiles;
use crate::resize_sink::VirshResizeSink;
use crate::runtime::{MemoryStateSource, ResizeSink};
use crate::virsh::Virsh;
use crate::xml_source::VirshXmlSource;

/// The configured VM an operator command acts on.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub config: PathBuf,
    pub vm: Option<String>,
    pub connection: String,
}

fn load(target: &Target) -> Result<(HostConfig, Virsh), String> {
    let config =
        HostConfig::load_vm(&target.config, target.vm.as_deref()).map_err(|e| e.to_string())?;
    let virsh = Virsh::with_connection(
        config.virsh_binary.clone(),
        config.command_timeout,
        target.connection.clone(),
    );
    Ok((config, virsh))
}

fn attested(config: &HostConfig, virsh: &Virsh) -> AttestedCompatibilitySource<Virsh> {
    AttestedCompatibilitySource::new(
        virsh.clone(),
        config.vm_name.clone(),
        config.alias.clone(),
        config.guest_os,
        config.compatibility_attestation_path.clone(),
    )
}

fn fingerprint(config: &HostConfig) -> Result<String, String> {
    Ok(
        crate::attestation::read_attestation(Path::new(&config.compatibility_attestation_path))?
            .fingerprint_sha256,
    )
}

/// Clear the VM's actuation latch for `reason`. Without `apply`, only
/// reports what it would do.
pub fn clear_latch<W: Write>(
    reason: &str,
    target: &Target,
    apply: bool,
    output: &mut W,
) -> Result<(), String> {
    let (config, virsh) = load(target)?;
    let _owner = crate::ownership::DeviceOwner::for_domain(&virsh, &config.vm_name)?;
    attested(&config, &virsh)
        .compatibility()?
        .validate_for_resize()
        .map_err(|error| error.to_string())?;
    let state_source =
        VirshXmlSource::new(virsh.clone(), config.vm_name.clone(), config.alias.clone());
    let sink = VirshResizeSink::new(
        virsh.clone(),
        config.vm_name.clone(),
        config.alias.clone(),
        attested(&config, &virsh),
    );
    // Settling moves the device: never do it under a controller that is not
    // latched, whose pending resize it would turn into a conflict.
    if !actuation_latched(&config, fingerprint(&config)?)? {
        return Err("target-controller actuation is not latched".to_owned());
    }
    let live = settle_stuck_shrink(&state_source, &sink, apply, output)?;
    let message = clear_actuation_latch(
        &config,
        live,
        fingerprint(&config)?,
        reason,
        SystemUnixClock.now_unix_millis()?,
        apply,
    )?;
    writeln!(
        output,
        "mode={} {message}",
        if apply { "applied" } else { "dry_run" }
    )
    .map_err(|error| error.to_string())
}

/// A latch over a shrink the guest stopped short of cannot clear on divergent
/// state: request what the guest kept first. Dry runs report the settle and
/// continue with the state it would produce.
fn settle_stuck_shrink<S: MemoryStateSource, R: ResizeSink, W: Write>(
    state_source: &S,
    sink: &R,
    apply: bool,
    output: &mut W,
) -> Result<VirtioMemState, String> {
    let live = state_source.memory_state()?;
    if live.requested_bytes == live.current_bytes {
        return Ok(live);
    }
    if live.requested_bytes > live.current_bytes {
        return Err(
            "clear-latch settles only a stuck shrink; this device is still growing".to_owned(),
        );
    }
    writeln!(
        output,
        "settle_stuck_shrink requested_bytes={} current_bytes={}",
        live.requested_bytes, live.current_bytes
    )
    .map_err(|error| error.to_string())?;
    if !apply {
        return Ok(VirtioMemState {
            requested_bytes: live.current_bytes,
            ..live
        });
    }
    sink.retarget_pending_guarded(live, live.current_bytes, &AtomicBool::new(false))
        .map_err(|error| error.to_string())?;
    let settled = state_source.memory_state()?;
    if settled.requested_bytes != settled.current_bytes {
        return Err("stuck shrink did not settle at the guest's current allocation".to_owned());
    }
    Ok(settled)
}

/// Write the VM's controller status as JSON.
pub fn status<W: Write>(target: &Target, output: &mut W) -> Result<(), String> {
    let (config, virsh) = load(target)?;
    let live = VirshXmlSource::new(virsh.clone(), config.vm_name.clone(), config.alias.clone())
        .memory_state()?;
    let guest_input = match GuestChannel::new(
        VirshGuestFiles::new(virsh, config.vm_name.clone()),
        &config.raw_telemetry_path,
        &config.vm_name,
        &config.raw_telemetry_service_name,
        config.raw_telemetry_max_age,
        config.raw_telemetry_future_tolerance,
    )
    .read()
    {
        Ok(envelope) => GuestInputStatus {
            error: None,
            observed_unix_millis: Some(envelope.observed_unix_millis),
            request: envelope.request,
        },
        Err(error) => GuestInputStatus {
            error: Some(error),
            observed_unix_millis: None,
            request: None,
        },
    };
    let status = read_controller_status(
        &config,
        live,
        guest_input,
        fingerprint(&config)?,
        SystemUnixClock.now_unix_millis()?,
    )?;
    serde_json::to_writer_pretty(&mut *output, &status)
        .map_err(|error| format!("encode controller status: {error}"))?;
    writeln!(output).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::compatibility_source::FixedCompatibilitySource;
    use crate::virsh::{VirshCommand, VirshError};
    use rampire_core::VirtioMemCompatibility;

    const GIB: u64 = 1 << 30;
    const SHRINK_PENDING_XML: &str = "<domain><devices><memory model='virtio-mem' dynamic-memslots='on' unplugged-inaccessible='on'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>4</requested><current unit='GiB'>6</current></target><alias name='memory0'/></memory></devices></domain>";

    struct RecoveryVirsh {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl VirshCommand for RecoveryVirsh {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            self.calls.borrow_mut().push(arguments.to_vec());
            if arguments.first().map(String::as_str) == Some("dumpxml") {
                Ok(SHRINK_PENDING_XML.to_owned())
            } else {
                Ok(String::new())
            }
        }
    }

    struct ScriptedStates(RefCell<VecDeque<VirtioMemState>>);

    impl MemoryStateSource for ScriptedStates {
        fn memory_state(&self) -> Result<VirtioMemState, String> {
            self.0
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "scripted state sequence is exhausted".to_owned())
        }
    }

    fn state(requested_bytes: u64, current_bytes: u64) -> VirtioMemState {
        VirtioMemState {
            size_bytes: 8 * GIB,
            block_size_bytes: 2 * 1024 * 1024,
            requested_bytes,
            current_bytes,
        }
    }

    #[test]
    fn clear_latch_settles_a_stuck_shrink_at_the_current_allocation() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let sink = VirshResizeSink::new(
            RecoveryVirsh {
                calls: Rc::clone(&calls),
            },
            "guest",
            "memory0",
            FixedCompatibilitySource::new(VirtioMemCompatibility::confirmed()),
        );
        let states = ScriptedStates(RefCell::new(VecDeque::from([
            state(4 * GIB, 6 * GIB),
            state(6 * GIB, 6 * GIB),
        ])));
        let mut output = Vec::new();
        let settled = settle_stuck_shrink(&states, &sink, true, &mut output).expect("settled");
        assert_eq!(settled, state(6 * GIB, 6 * GIB));
        assert_eq!(
            calls.borrow().last().expect("update")[0],
            "update-memory-device"
        );

        // A growing device is never settled; a converged one is untouched.
        let growing = ScriptedStates(RefCell::new(VecDeque::from([state(6 * GIB, 4 * GIB)])));
        assert!(settle_stuck_shrink(&growing, &sink, true, &mut Vec::new()).is_err());
        let calls_before = calls.borrow().len();
        let converged = ScriptedStates(RefCell::new(VecDeque::from([state(6 * GIB, 6 * GIB)])));
        settle_stuck_shrink(&converged, &sink, true, &mut Vec::new()).expect("converged");
        assert_eq!(calls.borrow().len(), calls_before);
    }
}
