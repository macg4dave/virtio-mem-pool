use std::path::Path;
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

impl HostSettings {
    /// A host list and its sibling global/per-VM files form one configuration.
    pub fn load(path: &Path) -> Result<Self, HostConfigError> {
        let (globals, list, settings) =
            settings::load_configuration(path).map_err(HostConfigError::Parse)?;
        let vms = list
            .vms
            .iter()
            .zip(&settings)
            .map(|(entry, vm)| HostConfig::resolve(&globals, entry, vm))
            .collect();
        Ok(Self {
            pool_bytes: globals.pool_bytes,
            vms,
        })
    }
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

    fn resolve(globals: &GlobalSettings, entry: &HostVm, vm: &VmSettings) -> Self {
        let name = &entry.vm_name;
        Self {
            vm_name: name.clone(),
            alias: entry.alias.clone(),
            priority: entry.priority,
            policy_state_path: format!("/var/lib/virtio-mem-host/state/{name}-requests.json"),
            compatibility_attestation_path: format!("/etc/virtio-mem-host/{name}.attestation.json"),
            poll_interval: Duration::from_secs(globals.poll_interval_seconds),
            command_timeout: Duration::from_secs(globals.command_timeout_seconds),
            convergence_timeout: Duration::from_secs(globals.convergence_timeout_seconds),
            virsh_binary: "/usr/bin/virsh".to_owned(),
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
            settings.vms[0].policy_state_path,
            "/var/lib/virtio-mem-host/state/windows-guest-requests.json"
        );
        assert!(HostConfig::load_vm(&path, Some("absent")).is_err());
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
        let config = HostConfig::resolve(&globals, &entry, &vm);
        assert_eq!(config.guest_os, GuestOs::Linux);
        assert_eq!(config.raw_telemetry_path, GuestOs::Linux.control_path());
        assert_eq!(
            config.raw_telemetry_service_name,
            GuestOs::Linux.service_name()
        );
    }
}
