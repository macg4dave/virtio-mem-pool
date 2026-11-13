//! Operator commands beside the daemon: a read-only `status` and the
//! reviewed `clear-latch` recovery.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use virtio_mem_core::{GuestInputStatus, VirtioMemState};

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

const DEFAULT_CONNECTION: &str = "qemu:///system";

#[derive(Debug, PartialEq, Eq)]
pub enum CliCommand {
    /// Run the controller daemon.
    Run {
        config: PathBuf,
    },
    Status {
        target: Target,
    },
    ClearLatch {
        reason: String,
        target: Target,
        apply: bool,
    },
}

/// The configured VM an operator command acts on.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub config: PathBuf,
    pub vm: Option<String>,
    pub connection: String,
}

pub fn usage() -> &'static str {
    "Usage: virtio-mem-host --config FILE\n       virtio-mem-host status --config FILE [--vm NAME] [--connect URI]\n       virtio-mem-host clear-latch REASON --config FILE [--vm NAME] [--connect URI] [--apply]"
}

pub fn parse_args(args: &[String]) -> Result<CliCommand, String> {
    let (mode, rest) = args.split_first().ok_or_else(|| usage().to_owned())?;
    if mode == "--config" {
        return match rest {
            [path] if !path.trim().is_empty() => Ok(CliCommand::Run {
                config: PathBuf::from(path),
            }),
            _ => Err(usage().to_owned()),
        };
    }
    let (reason, options) = match mode.as_str() {
        "status" => (None, rest),
        "clear-latch" => {
            let (reason, options) = rest
                .split_first()
                .filter(|(reason, _)| !reason.trim().is_empty() && !reason.starts_with("--"))
                .ok_or_else(|| usage().to_owned())?;
            (Some(reason.clone()), options)
        }
        _ => return Err(format!("unknown CLI command: {mode}\n{}", usage())),
    };
    let mut config = None;
    let mut vm = None;
    let mut connection = None;
    let mut apply = false;
    let mut options = options.iter();
    while let Some(option) = options.next() {
        let mut value = |slot: &mut Option<String>| {
            let value = options
                .next()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("{option} requires a value"))?;
            match slot.replace(value.clone()) {
                Some(_) => Err(format!("{option} may be supplied only once")),
                None => Ok(()),
            }
        };
        match option.as_str() {
            "--config" => value(&mut config)?,
            "--vm" => value(&mut vm)?,
            "--connect" => value(&mut connection)?,
            "--apply" if reason.is_some() && !apply => apply = true,
            _ => return Err(format!("unknown CLI option: {option}\n{}", usage())),
        }
    }
    let target = Target {
        config: PathBuf::from(config.ok_or_else(|| "--config is required".to_owned())?),
        vm,
        connection: connection.unwrap_or_else(|| DEFAULT_CONNECTION.to_owned()),
    };
    Ok(match reason {
        None => CliCommand::Status { target },
        Some(reason) => CliCommand::ClearLatch {
            reason,
            target,
            apply,
        },
    })
}

pub fn run(command: CliCommand) -> Result<(), String> {
    let mut output = std::io::stdout().lock();
    match command {
        CliCommand::Run { .. } => Err("the controller runs from main, not the CLI".to_owned()),
        CliCommand::Status { target } => run_status(&target, &mut output),
        CliCommand::ClearLatch {
            reason,
            target,
            apply,
        } => run_clear_latch(&reason, &target, apply, &mut output),
    }
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

fn run_clear_latch<W: Write>(
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

fn run_status<W: Write>(target: &Target, output: &mut W) -> Result<(), String> {
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
    use virtio_mem_core::VirtioMemCompatibility;

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

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_run_status_and_clear_latch() {
        assert_eq!(
            parse_args(&args(&["--config", "/etc/virtio-mem-host/guest.toml"])),
            Ok(CliCommand::Run {
                config: PathBuf::from("/etc/virtio-mem-host/guest.toml"),
            })
        );
        assert_eq!(
            parse_args(&args(&["status", "--config", "guest.toml"])),
            Ok(CliCommand::Status {
                target: Target {
                    config: PathBuf::from("guest.toml"),
                    vm: None,
                    connection: DEFAULT_CONNECTION.to_owned(),
                },
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "clear-latch",
                "reviewed recovery",
                "--connect",
                "test:///default",
                "--config",
                "guest.toml",
                "--vm",
                "guest",
                "--apply",
            ])),
            Ok(CliCommand::ClearLatch {
                reason: "reviewed recovery".to_owned(),
                target: Target {
                    config: PathBuf::from("guest.toml"),
                    vm: Some("guest".to_owned()),
                    connection: "test:///default".to_owned(),
                },
                apply: true,
            })
        );
    }

    #[test]
    fn rejects_invalid_cli_options() {
        for invalid in [
            &[][..],
            &["--config"],
            &["unknown"],
            &["status"],
            &["status", "--config", "a", "--apply"],
            &["status", "--config", "a", "--vm", "x", "--vm", "y"],
            &["status", "--config"],
            &["clear-latch", "--config", "a"],
            &["clear-latch", "reason", "--apply"],
            &[
                "clear-latch",
                "reason",
                "--config",
                "a",
                "--apply",
                "--apply",
            ],
            &["resize", "guest", "memory0", "0"],
        ] {
            assert!(parse_args(&args(invalid)).is_err(), "{invalid:?}");
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
