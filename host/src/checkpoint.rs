//! Durable per-VM control state: the admitted-request journal, the command
//! intent executing it, and the actuation latch. Each change is written and
//! flushed before any effect it authorises, so one file is the authority for
//! the daemon, `status` and `clear-latch`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use virtio_mem_core::{
    CommandDirection, CommandOwnership, ControlHealth, ControllerCommandStatus,
    ControllerStatusSnapshot, GuestInputStatus, MemoryOperation, MemoryOutcome,
    MemoryRequestJournal, MemoryResult, RecoveryState, VirtioMemState, CONTROLLER_STATUS_VERSION,
};

use crate::config::HostConfig;
use crate::ownership::CheckpointOwner;

const CHECKPOINT_VERSION: u16 = 2;
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandIntent {
    pub operation_id: String,
    /// Original execution start; restarts never extend an operation's bound.
    pub started_unix_millis: u64,
    pub direction: CommandDirection,
    pub prior_requested_bytes: u64,
    pub prior_current_bytes: u64,
    pub target_bytes: u64,
}

/// Control state that must survive a restart. Actuation is latched exactly
/// when `latch_reason` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableControlState {
    pub latch_reason: Option<String>,
    pub intent: Option<CommandIntent>,
    pub journal: MemoryRequestJournal,
}

impl DurableControlState {
    pub fn latched(&self) -> bool {
        self.latch_reason.is_some()
    }

    /// Anything a restart must not discard or resume under new evidence.
    fn pending(&self) -> bool {
        self.latched()
            || self.intent.is_some()
            || (self.journal.request.is_some()
                && self
                    .journal
                    .result
                    .as_ref()
                    .is_none_or(|result| !result.terminal()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointFile {
    version: u16,
    vm_name: String,
    device_alias: String,
    compatibility_fingerprint_sha256: String,
    control: DurableControlState,
    last_latch_clear_reason: Option<String>,
}

/// The locked checkpoint of one VM. The caller also holds the VM's device
/// owner for as long as it may act on the checkpoint.
pub struct Checkpoint {
    path: PathBuf,
    vm_name: String,
    device_alias: String,
    compatibility_fingerprint: String,
    last_latch_clear_reason: Option<String>,
    _owner: CheckpointOwner,
}

impl Checkpoint {
    /// Lock and load. Control state recorded under different compatibility
    /// evidence is refused rather than resumed.
    pub fn open(
        config: &HostConfig,
        compatibility_fingerprint: String,
    ) -> Result<(Self, DurableControlState), String> {
        Self::open_with(config, compatibility_fingerprint, false)
    }

    /// `migrate_evidence` is for `clear-latch` alone: it restamps pending
    /// control state with the current attestation after operator review.
    fn open_with(
        config: &HostConfig,
        compatibility_fingerprint: String,
        migrate_evidence: bool,
    ) -> Result<(Self, DurableControlState), String> {
        let path = PathBuf::from(&config.policy_state_path);
        let owner = CheckpointOwner::acquire(&path)?;
        let file = load(&path, &config.vm_name, &config.alias)?;
        if let Some(file) = &file {
            if file.compatibility_fingerprint_sha256 != compatibility_fingerprint
                && file.control.pending()
                && !migrate_evidence
            {
                return Err(
                    "compatibility attestation changed while control state is pending; review it and use clear-latch"
                        .to_owned(),
                );
            }
        }
        let (control, last_latch_clear_reason) = file
            .map(|file| (file.control, file.last_latch_clear_reason))
            .unwrap_or_default();
        Ok((
            Self {
                path,
                vm_name: config.vm_name.clone(),
                device_alias: config.alias.clone(),
                compatibility_fingerprint,
                last_latch_clear_reason,
                _owner: owner,
            },
            control,
        ))
    }

    pub fn save(&self, control: &DurableControlState) -> Result<(), String> {
        persist(
            &self.path,
            &CheckpointFile {
                version: CHECKPOINT_VERSION,
                vm_name: self.vm_name.clone(),
                device_alias: self.device_alias.clone(),
                compatibility_fingerprint_sha256: self.compatibility_fingerprint.clone(),
                control: control.clone(),
                last_latch_clear_reason: self.last_latch_clear_reason.clone(),
            },
        )
    }
}

/// How live state relates to the recorded command, if any. Shared by the
/// control loop and the read-only status report.
pub fn command_ownership(intent: Option<&CommandIntent>, live: VirtioMemState) -> CommandOwnership {
    let Some(intent) = intent else {
        return if live.requested_bytes == live.current_bytes {
            CommandOwnership::None
        } else {
            CommandOwnership::UnownedPending
        };
    };
    if live.requested_bytes == intent.target_bytes {
        if live.current_bytes == intent.target_bytes {
            CommandOwnership::OwnedConvergedUnresolved
        } else {
            CommandOwnership::OwnedPending
        }
    } else if live.requested_bytes == intent.prior_requested_bytes
        && live.current_bytes == intent.prior_current_bytes
    {
        CommandOwnership::RecordedNotApplied
    } else {
        CommandOwnership::Conflict
    }
}

/// The admitted request's outcome from the device movement its command
/// produced, measured against the state before the original command.
pub fn movement_result(
    journal: &MemoryRequestJournal,
    intent: &CommandIntent,
    live: VirtioMemState,
    observed_unix_millis: u64,
    reason: Option<String>,
) -> Option<MemoryResult> {
    let request = journal.request.clone()?;
    let moved = match request.operation {
        MemoryOperation::Add => live
            .current_bytes
            .saturating_sub(intent.prior_current_bytes),
        MemoryOperation::Remove => intent
            .prior_current_bytes
            .saturating_sub(live.current_bytes),
    }
    .min(request.bytes);
    Some(MemoryResult {
        outcome: if moved == request.bytes {
            MemoryOutcome::Completed
        } else {
            MemoryOutcome::Partial
        },
        actual_bytes: moved,
        observed_unix_millis,
        reason,
        request,
    })
}

/// Reads the durable checkpoint and joins it with one fresh alias-scoped live
/// state and a read-only view of the guest input. It never writes, resolves an
/// intent or invokes a resize sink.
pub fn read_controller_status(
    config: &HostConfig,
    live: VirtioMemState,
    guest_input: GuestInputStatus,
    compatibility_fingerprint: String,
    observed_unix_millis: u64,
) -> Result<ControllerStatusSnapshot, String> {
    live.validate().map_err(|error| error.to_string())?;
    let file = load(
        Path::new(&config.policy_state_path),
        &config.vm_name,
        &config.alias,
    )?;
    let attestation_changed = file
        .as_ref()
        .is_some_and(|file| file.compatibility_fingerprint_sha256 != compatibility_fingerprint);
    let (control, last_latch_clear_reason) = file
        .map(|file| (file.control, file.last_latch_clear_reason))
        .unwrap_or_default();
    let command_ownership = command_ownership(control.intent.as_ref(), live);
    let recovery_reason = if let Some(reason) = &control.latch_reason {
        Some(format!("actuation_latched: {reason}"))
    } else if attestation_changed && control.pending() {
        Some("compatibility attestation changed while control state is pending".to_owned())
    } else {
        match command_ownership {
            CommandOwnership::None | CommandOwnership::OwnedPending => None,
            CommandOwnership::OwnedConvergedUnresolved => Some(
                "recorded command has converged but its durable intent remains unresolved"
                    .to_owned(),
            ),
            CommandOwnership::UnownedPending => {
                Some("live requested/current divergence has no durable command owner".to_owned())
            }
            CommandOwnership::RecordedNotApplied => {
                Some("recorded command is not reflected in live state".to_owned())
            }
            CommandOwnership::Conflict => {
                Some("live state conflicts with the durable command intent".to_owned())
            }
        }
    };
    let control_health = if control.latched() {
        ControlHealth::Latched
    } else if recovery_reason.is_some() {
        ControlHealth::RecoveryRequired
    } else if command_ownership == CommandOwnership::OwnedPending {
        match control.intent.as_ref().map(|intent| intent.direction) {
            Some(CommandDirection::Grow | CommandDirection::SettleGrow) => ControlHealth::Growing,
            _ => ControlHealth::Shrinking,
        }
    } else {
        ControlHealth::Converged
    };
    let snapshot = ControllerStatusSnapshot {
        version: CONTROLLER_STATUS_VERSION,
        observed_unix_millis,
        vm_name: config.vm_name.clone(),
        device_alias: config.alias.clone(),
        device_size_bytes: live.size_bytes,
        block_size_bytes: live.block_size_bytes,
        maximum_target_bytes: live.maximum_target_bytes(),
        requested_bytes: live.requested_bytes,
        current_bytes: live.current_bytes,
        observe_only: config.observe_only,
        guest_input,
        admitted_request: control.journal.request,
        result: control.journal.result,
        command_ownership,
        command: control
            .intent
            .as_ref()
            .map(|intent| ControllerCommandStatus {
                operation_id: intent.operation_id.clone(),
                direction: intent.direction,
                prior_requested_bytes: intent.prior_requested_bytes,
                prior_current_bytes: intent.prior_current_bytes,
                target_bytes: intent.target_bytes,
            }),
        control_health,
        actuation_latched: control.latch_reason.is_some(),
        latch_reason: control.latch_reason,
        recovery_state: if recovery_reason.is_some() {
            RecoveryState::ReviewRequired
        } else {
            RecoveryState::NotRequired
        },
        recovery_reason,
        last_latch_clear_reason,
        compatibility_fingerprint_sha256: compatibility_fingerprint,
    };
    snapshot.validate().map_err(|error| error.to_string())?;
    Ok(snapshot)
}

/// Whether the checkpoint is latched. The checkpoint lock is released on
/// return, so a later `clear_actuation_latch` can take it.
pub fn actuation_latched(
    config: &HostConfig,
    compatibility_fingerprint: String,
) -> Result<bool, String> {
    let (_checkpoint, control) = Checkpoint::open_with(config, compatibility_fingerprint, true)?;
    Ok(control.latched())
}

/// Clears a latch on converged live state, stamps the current attestation and
/// gives any unresolved request a terminal result from the observed movement,
/// so the guest can reassess. Dry-run unless `apply`.
pub fn clear_actuation_latch(
    config: &HostConfig,
    live: VirtioMemState,
    compatibility_fingerprint: String,
    reason: &str,
    observed_unix_millis: u64,
    apply: bool,
) -> Result<String, String> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 256 || reason.chars().any(char::is_control) {
        return Err(
            "latch-clear reason must be 1..=256 bytes without control characters".to_owned(),
        );
    }
    live.validate().map_err(|error| error.to_string())?;
    if live.requested_bytes != live.current_bytes {
        return Err("clear-latch refuses divergent requested/current state".to_owned());
    }
    let (mut checkpoint, mut control) =
        Checkpoint::open_with(config, compatibility_fingerprint, true)?;
    if !control.latched() {
        return Err("target-controller actuation is not latched".to_owned());
    }
    let message = format!(
        "clear_latch vm={} alias={} requested_bytes={} current_bytes={} reason={}",
        config.vm_name, config.alias, live.requested_bytes, live.current_bytes, reason
    );
    if apply {
        let unresolved = control
            .journal
            .result
            .as_ref()
            .is_none_or(|result| !result.terminal());
        if control.journal.request.is_some() && unresolved {
            let operator = Some(format!("operator cleared the latch: {reason}"));
            let result = match control.intent.as_ref() {
                Some(intent) => movement_result(
                    &control.journal,
                    intent,
                    live,
                    observed_unix_millis,
                    operator,
                ),
                // Without an intent no command was recorded, so none ran.
                None => control.journal.request.clone().map(|request| {
                    MemoryResult::rejected(
                        request,
                        observed_unix_millis,
                        format!("operator cleared the latch: {reason}"),
                    )
                }),
            };
            if let Some(result) = result {
                control.journal.record_result(result)?;
            }
        }
        control.latch_reason = None;
        control.intent = None;
        checkpoint.last_latch_clear_reason = Some(reason.to_owned());
        checkpoint.save(&control)?;
    }
    Ok(message)
}

/// The one checkpoint reader. A missing file is fresh state. Anything
/// unreadable, oversized, foreign or invalid fails closed rather than
/// discarding possible control state.
fn load(path: &Path, vm_name: &str, device_alias: &str) -> Result<Option<CheckpointFile>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect control checkpoint: {error}")),
    };
    if metadata.len() > MAX_CHECKPOINT_BYTES {
        return Err("control checkpoint is oversized; refusing to discard it".to_owned());
    }
    let bytes = fs::read(path).map_err(|error| format!("read control checkpoint: {error}"))?;
    let file: CheckpointFile = serde_json::from_slice(&bytes).map_err(|error| {
        format!("control checkpoint is invalid; refusing to discard it: {error}")
    })?;
    if file.version != CHECKPOINT_VERSION
        || file.vm_name != vm_name
        || file.device_alias != device_alias
    {
        return Err("control checkpoint belongs to another VM, alias or version".to_owned());
    }
    file.control.journal.validate()?;
    if file
        .control
        .intent
        .as_ref()
        .is_some_and(|intent| intent.operation_id.trim().is_empty())
    {
        return Err("control checkpoint command intent is invalid".to_owned());
    }
    Ok(Some(file))
}

fn persist(path: &Path, file: &CheckpointFile) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(file).map_err(|error| format!("encode control checkpoint: {error}"))?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err("control checkpoint exceeds its size limit".to_owned());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("control checkpoint path has no parent directory")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create control checkpoint directory: {error}"))?;
    let temporary = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let written = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if let Err(error) = written {
        // Best effort: the write error is the failure reported.
        let _ = fs::remove_file(&temporary);
        return Err(format!("persist control checkpoint: {error}"));
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("flush control checkpoint directory: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use virtio_mem_core::{MemoryRequest, RequestIdentity, MEMORY_REQUEST_VERSION};

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    fn config(name: &str) -> HostConfig {
        let base = std::env::temp_dir().join(format!(
            "virtio-mem-checkpoint-{name}-{}",
            std::process::id()
        ));
        HostConfig {
            vm_name: "guest".to_owned(),
            alias: "memory0".to_owned(),
            priority: 100,
            policy_state_path: base.with_extension("state.json").display().to_string(),
            poll_interval: Duration::from_secs(30),
            command_timeout: Duration::from_secs(10),
            convergence_timeout: Duration::from_secs(300),
            virsh_binary: "virsh".to_owned(),
            libvirt_uri: "qemu:///system".to_owned(),
            raw_telemetry_path: "telemetry.jsonl".to_owned(),
            raw_telemetry_service_name: "VirtioMemService".to_owned(),
            guest_os: virtio_mem_core::settings::GuestOs::Windows,
            raw_telemetry_max_age: Duration::from_secs(60),
            raw_telemetry_future_tolerance: Duration::from_secs(5),
            host_min_headroom_bytes: 4 * GIB,
            compatibility_attestation_path: base
                .with_extension("attestation.json")
                .display()
                .to_string(),
            observe_only: false,
            min_ram: 0,
            max_ram: u64::MAX,
            guest_settings: None,
            telemetry_path: None,
        }
    }

    fn live(requested_bytes: u64, current_bytes: u64) -> VirtioMemState {
        VirtioMemState {
            size_bytes: 24 * GIB,
            block_size_bytes: 2 * MIB,
            requested_bytes,
            current_bytes,
        }
    }

    fn admitted() -> MemoryRequestJournal {
        let mut journal = MemoryRequestJournal::default();
        journal
            .admit(
                MemoryRequest {
                    version: MEMORY_REQUEST_VERSION,
                    identity: RequestIdentity {
                        vm_name: "guest".to_owned(),
                        service_name: "VirtioMemService".to_owned(),
                        session_id: "session".to_owned(),
                        sequence: 1,
                    },
                    operation: MemoryOperation::Remove,
                    bytes: 2 * GIB,
                    issued_unix_millis: 100,
                    expires_unix_millis: 200,
                },
                "guest",
                "VirtioMemService",
                150,
                0,
            )
            .expect("admit");
        journal
    }

    fn shrink_intent() -> CommandIntent {
        CommandIntent {
            operation_id: "operation".to_owned(),
            started_unix_millis: 150,
            direction: CommandDirection::Shrink,
            prior_requested_bytes: 8 * GIB,
            prior_current_bytes: 8 * GIB,
            target_bytes: 6 * GIB,
        }
    }

    fn remove(config: &HostConfig) {
        // Test cleanup: a missing file is fine.
        let _ = fs::remove_file(&config.policy_state_path);
    }

    #[test]
    fn control_state_round_trips_and_is_exclusive() {
        let config = config("round-trip");
        remove(&config);
        let (checkpoint, fresh) = Checkpoint::open(&config, "a".repeat(64)).expect("open");
        assert_eq!(fresh, DurableControlState::default());
        assert!(Checkpoint::open(&config, "a".repeat(64)).is_err(), "locked");
        let control = DurableControlState {
            latch_reason: None,
            intent: Some(shrink_intent()),
            journal: admitted(),
        };
        checkpoint.save(&control).expect("save");
        drop(checkpoint);
        let (_, restored) = Checkpoint::open(&config, "a".repeat(64)).expect("reopen");
        assert_eq!(restored, control);
        remove(&config);
    }

    #[test]
    fn invalid_foreign_and_changed_evidence_fail_closed() {
        let config = config("fail-closed");
        remove(&config);
        let (checkpoint, _) = Checkpoint::open(&config, "a".repeat(64)).expect("open");
        checkpoint
            .save(&DurableControlState {
                latch_reason: None,
                intent: Some(shrink_intent()),
                journal: admitted(),
            })
            .expect("save");
        drop(checkpoint);
        assert!(Checkpoint::open(&config, "b".repeat(64)).is_err());
        let mut foreign = config.clone();
        foreign.alias = "memory1".to_owned();
        assert!(Checkpoint::open(&foreign, "a".repeat(64)).is_err());
        for contents in [
            b"{invalid".to_vec(),
            vec![b' '; MAX_CHECKPOINT_BYTES as usize + 1],
        ] {
            fs::write(&config.policy_state_path, contents).expect("write");
            assert!(Checkpoint::open(&config, "a".repeat(64)).is_err());
        }
        remove(&config);
    }

    #[test]
    fn status_reports_requests_ownership_and_guest_input_without_writing() {
        let config = config("status");
        remove(&config);
        let input = GuestInputStatus {
            error: Some("guest publication unavailable".to_owned()),
            observed_unix_millis: None,
            request: None,
        };
        let cold = read_controller_status(&config, live(0, 0), input.clone(), "a".repeat(64), 1)
            .expect("cold status");
        assert_eq!(cold.control_health, ControlHealth::Converged);
        assert_eq!(cold.maximum_target_bytes, 24 * GIB);
        assert!(!Path::new(&config.policy_state_path).exists());

        let (checkpoint, _) = Checkpoint::open(&config, "a".repeat(64)).expect("open");
        checkpoint
            .save(&DurableControlState {
                latch_reason: None,
                intent: Some(shrink_intent()),
                journal: admitted(),
            })
            .expect("save");
        let pending = read_controller_status(
            &config,
            live(6 * GIB, 7 * GIB),
            input.clone(),
            "a".repeat(64),
            2,
        )
        .expect("pending status");
        assert_eq!(pending.command_ownership, CommandOwnership::OwnedPending);
        assert_eq!(pending.control_health, ControlHealth::Shrinking);
        assert!(pending.admitted_request.is_some() && pending.result.is_none());
        let changed =
            read_controller_status(&config, live(6 * GIB, 7 * GIB), input, "b".repeat(64), 3)
                .expect("changed attestation is reported");
        assert_eq!(changed.recovery_state, RecoveryState::ReviewRequired);
        remove(&config);
    }

    #[test]
    fn clear_latch_resolves_the_request_from_observed_movement() {
        let config = config("clear-latch");
        remove(&config);
        let (checkpoint, _) = Checkpoint::open(&config, "a".repeat(64)).expect("open");
        checkpoint
            .save(&DurableControlState {
                latch_reason: Some("owned shrink exceeded the convergence deadline".to_owned()),
                intent: Some(shrink_intent()),
                journal: admitted(),
            })
            .expect("save");
        drop(checkpoint);
        assert!(clear_actuation_latch(
            &config,
            live(6 * GIB, 7 * GIB),
            "a".repeat(64),
            "reviewed",
            300,
            true
        )
        .is_err());
        clear_actuation_latch(
            &config,
            live(7 * GIB, 7 * GIB),
            "a".repeat(64),
            "reviewed",
            300,
            false,
        )
        .expect("dry run");
        let (_, unchanged) = Checkpoint::open(&config, "a".repeat(64)).expect("reopen");
        assert!(unchanged.latched());

        clear_actuation_latch(
            &config,
            live(7 * GIB, 7 * GIB),
            "a".repeat(64),
            "reviewed",
            300,
            true,
        )
        .expect("apply");
        let (_, cleared) = Checkpoint::open(&config, "a".repeat(64)).expect("reopen");
        assert!(!cleared.latched() && cleared.intent.is_none());
        let result = cleared.journal.result.expect("terminal result");
        assert_eq!(result.outcome, MemoryOutcome::Partial);
        assert_eq!(result.actual_bytes, GIB);
        remove(&config);
    }
}
