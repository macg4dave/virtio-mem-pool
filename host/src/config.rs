use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use virtio_mem_core::settings::{self, GlobalSettings, GuestOs, HostVm, VmSettings};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostConfigError {
    #[error("invalid host configuration: {0}")]
    Parse(String),
    #[error("host config lists {0} VMs; select one with --vm NAME")]
    VmNotSelected(usize),
    #[error("vm {0} is not in the host config")]
    UnknownVm(String),
}

/// One resolved runtime config from the global, host-list and per-VM files.
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
    /// Published with the host status: every current guest takes its limits
    /// and policy from the host. Guests built before that are never sent it
    /// (see `virtio_mem_core::sends_guest_settings_to`).
    pub guest_settings: Option<virtio_mem_core::GuestSettings>,
    pub telemetry_path: Option<String>,
}

/// Every resolved VM plus the server pool budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSettings {
    /// Device bytes shared by every VM. Required with more than one VM.
    pub pool_bytes: Option<u64>,
    pub vms: Vec<HostConfig>,
}

/// Where the controller keeps durable state when systemd does not say: the
/// unit's `StateDirectory`, which systemd passes as `STATE_DIRECTORY`.
pub const DEFAULT_STATE_DIRECTORY: &str = "/var/lib/virtio-mem-host/state";

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

/// A VM's compatibility attestation sits beside the host list that names it.
pub fn attestation_path(config_directory: &Path, vm_name: &str) -> PathBuf {
    config_directory.join(format!("{vm_name}.attestation.json"))
}

impl HostSettings {
    /// A host list and its sibling global/per-VM files form one configuration.
    pub fn load(path: &Path) -> Result<Self, HostConfigError> {
        let (globals, list, settings) =
            settings::load_configuration(path).map_err(HostConfigError::Parse)?;
        let paths = Paths {
            config: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            state: state_directory(),
        };
        let vms = list
            .vms
            .iter()
            .zip(&settings)
            .map(|(entry, vm)| HostConfig::resolve(&globals, entry, vm, &paths))
            .collect();
        Ok(Self {
            pool_bytes: globals.pool_bytes,
            vms,
        })
    }
}

struct Paths {
    config: PathBuf,
    state: PathBuf,
}

impl HostConfig {
    pub fn load_vm(path: &Path, vm: Option<&str>) -> Result<Self, HostConfigError> {
        let mut vms = HostSettings::load(path)?.vms;
        match vm {
            Some(name) => vms
                .into_iter()
                .find(|config| config.vm_name == name)
                .ok_or_else(|| HostConfigError::UnknownVm(name.to_owned())),
            None if vms.len() == 1 => Ok(vms.remove(0)),
            None => Err(HostConfigError::VmNotSelected(vms.len())),
        }
    }

    fn resolve(globals: &GlobalSettings, entry: &HostVm, vm: &VmSettings, paths: &Paths) -> Self {
        let name = &entry.vm_name;
        Self {
            vm_name: name.clone(),
            alias: entry.alias.clone(),
            priority: entry.priority,
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
            // From PATH, as the attestation's evidence was collected.
            virsh_binary: "virsh".to_owned(),
            libvirt_uri: globals.libvirt_uri.clone(),
            raw_telemetry_path: vm.guest_os.control_path().to_owned(),
            raw_telemetry_service_name: vm.guest_os.service_name().to_owned(),
            guest_os: vm.guest_os,
            raw_telemetry_max_age: Duration::from_millis(globals.freshness_millis()),
            raw_telemetry_future_tolerance: Duration::from_secs(globals.poll_interval_seconds),
            host_min_headroom_bytes: globals.host_min_headroom_bytes,
            observe_only: globals.observe_only,
            min_ram: vm.min_ram,
            max_ram: vm.max_ram,
            guest_settings: Some(vm.guest_settings(globals)),
            telemetry_path: entry.telemetry_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_configuration_loads_without_optional_telemetry() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/config/host_list.toml");
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
            path.with_file_name("windows-guest.attestation.json")
        );
        assert!(settings.vms[0]
            .policy_state_path
            .ends_with("/windows-guest-requests.json"));
        assert_eq!(settings.vms[0].libvirt_uri, "qemu:///system");
        assert!(HostConfig::load_vm(&path, Some("absent")).is_err());
    }

    #[test]
    fn documented_examples_load() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/examples/host_list.toml");
        let settings = HostSettings::load(&path).expect("example config");
        let guests: Vec<_> = settings.vms.iter().map(|vm| vm.guest_os).collect();
        assert_eq!(guests, [GuestOs::Linux, GuestOs::Windows]);
    }

    #[test]
    fn a_linux_guest_resolves_its_own_control_path_and_service_identity() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/config");
        let globals = settings::load_globals(&directory).expect("fixture globals");
        let vm =
            settings::load_vm(&directory.join("linux-guest_settings.toml")).expect("fixture VM");
        let entry = HostVm {
            vm_name: vm.vm_name.clone(),
            alias: "ua-virtiomem0".to_owned(),
            priority: 50,
            telemetry_path: None,
        };
        let paths = Paths {
            config: directory.clone(),
            state: PathBuf::from(DEFAULT_STATE_DIRECTORY),
        };
        let config = HostConfig::resolve(&globals, &entry, &vm, &paths);
        assert_eq!(config.guest_os, GuestOs::Linux);
        assert_eq!(config.raw_telemetry_path, GuestOs::Linux.control_path());
        assert_eq!(
            config.raw_telemetry_service_name,
            GuestOs::Linux.service_name()
        );
    }
}
