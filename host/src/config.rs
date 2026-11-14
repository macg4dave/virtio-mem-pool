use std::path::{Path, PathBuf};
use std::time::Duration;

use rampire_core::settings::{self, GlobalSettings, GuestOs, VmSettings};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostConfigError {
    #[error("invalid host configuration: {0}")]
    Parse(String),
    #[error("the configuration manages no VMs; add one to vms.d")]
    NoVms,
    #[error("the configuration has {0} VMs; select one with --vm NAME")]
    VmNotSelected(usize),
    #[error("the configuration has no VM {0}")]
    UnknownVm(String),
}

/// One VM's resolved runtime config, from `global.toml` and its own file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub vm_name: String,
    pub alias: String,
    /// Pool priority; larger is served first when VMs contend.
    pub priority: u32,
    pub policy_state_path: String,
    pub poll_interval: Duration,
    pub command_timeout: Duration,
    pub convergence_timeout: Duration,
    /// How long a removal may stop moving before it is settled at what the
    /// guest returned; from the guest OS (`GuestOs::removal_settle_seconds`).
    pub removal_settle: Duration,
    pub virsh_binary: String,
    pub libvirt_uri: String,
    pub raw_telemetry_path: String,
    pub raw_telemetry_service_name: String,
    /// Selects the guest's control path and identity above, and whether the
    /// Windows driver workarounds apply to this VM's resizes.
    pub guest_os: GuestOs,
    pub raw_telemetry_max_age: Duration,
    pub raw_telemetry_future_tolerance: Duration,
    pub host_min_headroom_bytes: u64,
    pub compatibility_attestation_path: String,
    /// Evaluate guest requests and log grants, but refuse every request
    /// before any device command.
    pub observe_only: bool,
    pub min_ram: u64,
    pub max_ram: u64,
    /// Published with the host status: every guest takes its limits and
    /// policy from the host.
    pub guest_settings: Option<rampire_core::GuestSettings>,
    pub telemetry_path: Option<String>,
}

/// Every resolved VM, the server pool budget and the libvirt connection
/// they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSettings {
    /// Device bytes shared by every VM. Required with more than one VM.
    pub pool_bytes: Option<u64>,
    pub virsh_binary: String,
    pub libvirt_uri: String,
    pub command_timeout: Duration,
    /// In name order. Empty when the host manages no VMs yet.
    pub vms: Vec<HostConfig>,
}

/// virsh from PATH, as the attestation's evidence was collected.
const VIRSH_BINARY: &str = "virsh";

/// Where the controller keeps durable state when systemd does not say: the
/// unit's `StateDirectory`, which systemd passes as `STATE_DIRECTORY`.
pub const DEFAULT_STATE_DIRECTORY: &str = "/var/lib/rampire/state";

fn state_directory() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY")
        .and_then(|value| {
            // systemd separates several directories with ':'; the unit names one.
            value
                .to_str()
                .and_then(|text| text.split(':').next())
                .filter(|first| first.starts_with('/'))
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIRECTORY))
}

/// The folder, inside the configuration directory, that holds the VMs'
/// compatibility attestations. They are generated evidence, not settings, so
/// they are kept apart from the settings files people edit.
pub const ATTESTATION_DIRECTORY: &str = "attestations";

/// A VM's compatibility attestation, in the configuration directory's
/// attestation folder.
pub fn attestation_path(config_directory: &Path, vm_name: &str) -> PathBuf {
    config_directory
        .join(ATTESTATION_DIRECTORY)
        .join(format!("{vm_name}.attestation.json"))
}

impl HostSettings {
    /// The configuration in `directory`: `global.toml` and every
    /// `vms.d/NAME.toml` (see `settings::load_configuration`).
    pub fn load(directory: &Path) -> Result<Self, HostConfigError> {
        let (globals, settings) =
            settings::load_configuration(directory).map_err(HostConfigError::Parse)?;
        let paths = Paths {
            config: directory.to_path_buf(),
            state: state_directory(),
        };
        let vms = settings
            .iter()
            .map(|vm| HostConfig::resolve(&globals, vm, &paths))
            .collect();
        Ok(Self {
            pool_bytes: globals.pool,
            virsh_binary: VIRSH_BINARY.to_owned(),
            libvirt_uri: globals.libvirt_uri.clone(),
            command_timeout: Duration::from_secs(globals.command_timeout_seconds),
            vms,
        })
    }
}

impl HostSettings {
    /// Whether a running controller on `self` can take `new` by reloading
    /// (roadmap O4.5): the same VMs on the same connection, each with the
    /// same device, guest and attestation. Priorities, limits, the pool, the
    /// host reserve, observe-only and the convergence timeout may change.
    pub fn reloadable_to(&self, new: &HostSettings) -> Result<(), String> {
        if (&self.virsh_binary, &self.libvirt_uri, self.command_timeout)
            != (&new.virsh_binary, &new.libvirt_uri, new.command_timeout)
        {
            return Err("the libvirt connection or command timeout changed".to_owned());
        }
        let names = |settings: &HostSettings| {
            settings
                .vms
                .iter()
                .map(|vm| vm.vm_name.clone())
                .collect::<Vec<_>>()
        };
        if names(self) != names(new) {
            return Err("VMs were added or removed".to_owned());
        }
        self.vms
            .iter()
            .zip(&new.vms)
            .try_for_each(|(old, new)| reloadable(old, new))
    }
}

/// Whether one VM's configuration can change without a restart.
pub fn reloadable(old: &HostConfig, new: &HostConfig) -> Result<(), String> {
    let fixed = |config: &HostConfig| HostConfig {
        priority: 0,
        min_ram: 0,
        max_ram: 0,
        guest_settings: None,
        host_min_headroom_bytes: 0,
        observe_only: false,
        telemetry_path: None,
        convergence_timeout: Duration::ZERO,
        removal_settle: Duration::ZERO,
        compatibility_attestation_path: String::new(),
        ..config.clone()
    };
    if fixed(old) != fixed(new) {
        return Err(format!(
            "{}: a change beyond its priority, limits and free_ram",
            old.vm_name
        ));
    }
    let read = |path: &str| std::fs::read(path).map_err(|error| format!("read {path}: {error}"));
    if read(&old.compatibility_attestation_path)? != read(&new.compatibility_attestation_path)? {
        return Err(format!("{}: its attestation changed", old.vm_name));
    }
    Ok(())
}

struct Paths {
    config: PathBuf,
    state: PathBuf,
}

impl HostConfig {
    pub fn load_vm(directory: &Path, vm: Option<&str>) -> Result<Self, HostConfigError> {
        let mut vms = HostSettings::load(directory)?.vms;
        match vm {
            Some(name) => vms
                .into_iter()
                .find(|config| config.vm_name == name)
                .ok_or_else(|| HostConfigError::UnknownVm(name.to_owned())),
            None if vms.len() == 1 => Ok(vms.remove(0)),
            None if vms.is_empty() => Err(HostConfigError::NoVms),
            None => Err(HostConfigError::VmNotSelected(vms.len())),
        }
    }

    fn resolve(globals: &GlobalSettings, vm: &VmSettings, paths: &Paths) -> Self {
        let name = &vm.vm_name;
        Self {
            vm_name: name.clone(),
            alias: vm.alias.clone(),
            priority: vm.priority,
            policy_state_path: paths
                .state
                .join(format!("{name}-requests.json"))
                .display()
                .to_string(),
            compatibility_attestation_path: attestation_path(&paths.config, name)
                .display()
                .to_string(),
            poll_interval: Duration::from_secs(globals.poll_interval_seconds),
            command_timeout: Duration::from_secs(globals.command_timeout_seconds),
            convergence_timeout: Duration::from_secs(globals.convergence_timeout_seconds),
            removal_settle: Duration::from_secs(
                vm.guest_os
                    .removal_settle_seconds(globals.convergence_timeout_seconds),
            ),
            virsh_binary: VIRSH_BINARY.to_owned(),
            libvirt_uri: globals.libvirt_uri.clone(),
            raw_telemetry_path: vm.guest_os.control_path().to_owned(),
            raw_telemetry_service_name: vm.guest_os.service_name().to_owned(),
            guest_os: vm.guest_os,
            raw_telemetry_max_age: Duration::from_millis(globals.freshness_millis()),
            raw_telemetry_future_tolerance: Duration::from_secs(globals.poll_interval_seconds),
            host_min_headroom_bytes: globals.host_reserve,
            observe_only: globals.observe_only,
            min_ram: vm.min_ram,
            max_ram: vm.max_ram,
            guest_settings: Some(vm.guest_settings(globals)),
            telemetry_path: vm.telemetry_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_configuration_loads_without_optional_telemetry() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/config");
        let settings = HostSettings::load(&path).expect("fixture config");
        assert_eq!(settings.vms.len(), 1);
        assert!(settings.vms[0].telemetry_path.is_none());
        assert_eq!(settings.vms[0].guest_os, GuestOs::Windows);
        assert_eq!(
            settings.vms[0].raw_telemetry_path,
            GuestOs::Windows.control_path()
        );
        assert_eq!(
            Path::new(&settings.vms[0].compatibility_attestation_path),
            path.join("attestations/windows-guest.attestation.json")
        );
        assert!(settings.vms[0]
            .policy_state_path
            .ends_with("/windows-guest-requests.json"));
        assert_eq!(settings.vms[0].libvirt_uri, "qemu:///system");
        assert!(HostConfig::load_vm(&path, Some("absent")).is_err());
    }

    #[test]
    fn priorities_and_limits_reload_but_new_vms_need_a_restart() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/config");
        let old = HostSettings::load(&path).expect("fixture");
        let root = std::env::temp_dir().join(format!("rampire-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("attestations")).unwrap();
        let attestation = root.join("attestations/windows-guest.attestation.json");
        std::fs::write(&attestation, "{}").unwrap();
        let mut old = old;
        old.vms[0].compatibility_attestation_path = attestation.display().to_string();

        let mut new = old.clone();
        new.vms[0].priority += 50;
        new.vms[0].max_ram += 1 << 30;
        new.pool_bytes = Some(1 << 40);
        let reloads = old.reloadable_to(&new);

        let mut moved = new.clone();
        moved.vms[0].alias = "other".to_owned();
        let alias = old.reloadable_to(&moved);
        let mut grown = new.clone();
        grown.vms.push(grown.vms[0].clone());
        grown.vms[1].vm_name = "second".to_owned();
        let added = old.reloadable_to(&grown);
        let copy = root.join("attestations/copy.json");
        std::fs::write(&copy, "{\"changed\": true}").unwrap();
        let mut attested = new.clone();
        attested.vms[0].compatibility_attestation_path = copy.display().to_string();
        let reattested = old.reloadable_to(&attested);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(reloads, Ok(()));
        assert!(alias.is_err());
        assert!(added.unwrap_err().contains("added or removed"));
        assert!(reattested.unwrap_err().contains("attestation"));
    }

    #[test]
    fn a_configuration_with_no_vms_loads() {
        let root = std::env::temp_dir().join(format!("rampire-no-vms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(settings::VMS_DIRECTORY)).unwrap();
        let settings = HostSettings::load(&root).expect("empty configuration");
        assert!(settings.vms.is_empty());
        assert_eq!(settings.libvirt_uri, "qemu:///system");
        assert_eq!(
            HostConfig::load_vm(&root, None),
            Err(HostConfigError::NoVms)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn documented_examples_load() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/examples");
        let settings = HostSettings::load(&path).expect("example config");
        let guests: Vec<_> = settings.vms.iter().map(|vm| vm.guest_os).collect();
        assert_eq!(guests, [GuestOs::Linux, GuestOs::Windows]);
    }

    #[test]
    fn a_linux_guest_resolves_its_own_control_path_and_service_identity() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/config");
        let globals = settings::load_globals(&directory).expect("fixture globals");
        let vm = settings::load_vm(&directory.join("linux-guest.toml")).expect("fixture VM");
        assert_eq!((vm.vm_name.as_str(), vm.priority), ("linux-guest", 50));
        let paths = Paths {
            config: directory.clone(),
            state: PathBuf::from(DEFAULT_STATE_DIRECTORY),
        };
        let config = HostConfig::resolve(&globals, &vm, &paths);
        assert_eq!(config.guest_os, GuestOs::Linux);
        assert_eq!(config.raw_telemetry_path, GuestOs::Linux.control_path());
        assert_eq!(
            config.raw_telemetry_service_name,
            GuestOs::Linux.service_name()
        );
        // A stalled Linux removal waits for the driver's retries, not the
        // global convergence timeout that Windows uses.
        assert_eq!(
            config.removal_settle,
            Duration::from_secs(GuestOs::Linux.removal_settle_seconds(0))
        );
        assert_ne!(config.removal_settle, config.convergence_timeout);
        let windows = HostSettings::load(&directory).expect("fixture");
        assert_eq!(
            windows.vms[0].removal_settle,
            windows.vms[0].convergence_timeout
        );
    }
}
