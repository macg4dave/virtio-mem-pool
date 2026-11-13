//! Administrator-owned TOML settings. Runtime wire files are generated from
//! these three formats, never a second source of administrator settings.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    GuestMemoryPolicy, LinuxServiceConfigFile, TargetPolicyConfig, WindowsServiceConfigFile,
    LINUX_SERVICE_CONFIG_SCHEMA_VERSION, WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION,
};

pub const SERVICE_NAME: &str = "VirtioMemService";
// The control channel is required for memory requests, independently of optional
// diagnostic output. Keep its historical path to retain durable pending requests.
pub const CONTROL_PATH: &str = r"C:\ProgramData\VirtioMemService\telemetry.json";
/// The Linux guest service's wire identity and systemd unit name.
pub const LINUX_SERVICE_NAME: &str = "virtio-mem-guest";
/// The Linux guest's control publication; host status and durable request
/// state are derived beside it, as on Windows.
pub const LINUX_CONTROL_PATH: &str = "/var/lib/virtio-mem-guest/control.json";

/// The guest operating system decides the guest service's identity, its
/// control path and which driver-specific host behaviour applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOs {
    #[default]
    Windows,
    Linux,
}

impl GuestOs {
    fn is_windows(&self) -> bool {
        *self == Self::Windows
    }

    pub fn service_name(self) -> &'static str {
        match self {
            Self::Windows => SERVICE_NAME,
            Self::Linux => LINUX_SERVICE_NAME,
        }
    }

    pub fn control_path(self) -> &'static str {
        match self {
            Self::Windows => CONTROL_PATH,
            Self::Linux => LINUX_CONTROL_PATH,
        }
    }
}
pub const GLOBAL_FILE: &str = "global-settings.toml";
pub const HOST_LIST_FILE: &str = "host_list.toml";
const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettings {
    #[serde(
        default,
        deserialize_with = "crate::units::byte_size::option::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub pool_bytes: Option<u64>,
    #[serde(with = "crate::units::byte_size")]
    pub host_min_headroom_bytes: u64,
    pub observe_only: bool,
    pub poll_interval_seconds: u64,
    pub command_timeout_seconds: u64,
    pub convergence_timeout_seconds: u64,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            pool_bytes: None,
            host_min_headroom_bytes: 4 * GIB,
            observe_only: false,
            poll_interval_seconds: 5,
            command_timeout_seconds: 10,
            convergence_timeout_seconds: 300,
        }
    }
}

impl GlobalSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.pool_bytes == Some(0) || self.host_min_headroom_bytes == 0 {
            return Err("pool_bytes and host_min_headroom_bytes must be positive".to_owned());
        }
        if !(1..=10).contains(&self.poll_interval_seconds) {
            return Err(
                "poll_interval_seconds must be 1..=10 (request freshness bound)".to_owned(),
            );
        }
        for (name, seconds) in [
            ("command_timeout_seconds", self.command_timeout_seconds),
            (
                "convergence_timeout_seconds",
                self.convergence_timeout_seconds,
            ),
        ] {
            if seconds == 0 || seconds > 86_400 {
                return Err(format!("{name} must be 1..=86400"));
            }
        }
        Ok(())
    }

    /// How old a guest publication, host status or request may be, on both
    /// sides: six polls.
    pub fn freshness_millis(&self) -> u64 {
        self.poll_interval_seconds * 6000
    }

    /// The guest policy for one VM. Every size comes from the VM's
    /// `free_ram`, the memory the guest keeps free above what it uses:
    ///
    /// - the guest asks for more when free memory falls below `free_ram`;
    /// - reclaim never plans below half of it, and that half is also the
    ///   level treated as critical;
    /// - memory is returned only once the surplus exceeds an eighth of it;
    /// - growth and reclaim both leave a sixteenth of it above `free_ram`, so
    ///   ordinary fluctuation at the threshold causes no resize;
    /// - native Low pressure asks for at least half of it.
    ///
    /// `removal_step_bytes` bounds one removal request. Polling controls both
    /// workers and the corresponding freshness/gap bounds.
    pub fn guest_policy(&self, free_ram_bytes: u64, removal_step_bytes: u64) -> GuestMemoryPolicy {
        let freshness = self.freshness_millis();
        GuestMemoryPolicy {
            target: TargetPolicyConfig {
                configured_minimum_bytes: 0,
                physical_reserve_bytes: free_ram_bytes,
                commit_reserve_bytes: free_ram_bytes,
                safe_floor_physical_reserve_bytes: free_ram_bytes / 2,
                safe_floor_commit_reserve_bytes: free_ram_bytes / 2,
                history_window_millis: 1_000,
                maximum_gap_millis: self.poll_interval_seconds * 3000,
                downward_hysteresis_bytes: free_ram_bytes / 8,
            },
            grow_step_bytes: free_ram_bytes / 2,
            shrink_step_bytes: removal_step_bytes,
            host_status_max_age_millis: freshness,
            request_lifetime_millis: freshness,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSettings {
    #[serde(with = "crate::units::byte_size")]
    pub min_ram: u64,
    #[serde(with = "crate::units::byte_size")]
    pub max_ram: u64,
    /// Memory the guest keeps free above what it is using. It asks for more
    /// when free memory falls below this and returns memory when it has more
    /// than this spare. Every policy size is derived from it.
    #[serde(with = "crate::units::byte_size")]
    pub free_ram: u64,
    /// Omitted means Windows.
    #[serde(default, skip_serializing_if = "GuestOs::is_windows")]
    pub guest_os: GuestOs,
    /// Windows only: the installed service executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_path: Option<String>,
    /// Windows only: the account the service runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
    pub vm_name: String,
    /// Optional Windows diagnostic output, independent of memory requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_path: Option<String>,
}

impl VmSettings {
    pub fn validate(&self) -> Result<(), String> {
        vm_filename(&self.vm_name)?;
        validate_ram_limits(self.min_ram, self.max_ram)?;
        // An eighth of it must still be a whole number of device blocks.
        let unit = 8 * crate::MIN_BLOCK_SIZE_BYTES;
        if self.free_ram == 0
            || !self.free_ram.is_multiple_of(unit)
            || self.free_ram >= self.max_ram
        {
            return Err(format!(
                "free_ram must be a positive multiple of {unit} bytes (and of eight live device blocks) and less than max_ram"
            ));
        }
        match self.guest_os {
            GuestOs::Windows => self.validate_windows(),
            GuestOs::Linux => {
                if self.install_path.is_some()
                    || self.service_account.is_some()
                    || self.telemetry_path.is_some()
                {
                    return Err("install_path, service_account and telemetry_path are Windows settings; omit them for a Linux guest".to_owned());
                }
                Ok(())
            }
        }
    }

    /// The Windows install path and service account, or why there are none.
    pub fn windows_install(&self) -> Result<(&str, &str), String> {
        match (self.guest_os, &self.install_path, &self.service_account) {
            (GuestOs::Windows, Some(path), Some(account)) => Ok((path, account)),
            (GuestOs::Windows, _, _) => {
                Err("a Windows guest needs install_path and service_account".to_owned())
            }
            (GuestOs::Linux, _, _) => Err(format!("VM {} is not a Windows guest", self.vm_name)),
        }
    }

    fn validate_windows(&self) -> Result<(), String> {
        let (install_path, service_account) = self.windows_install()?;
        crate::windows_service_config::normalize_windows_path(install_path)?;
        if !install_path.to_ascii_lowercase().ends_with(".exe") {
            return Err("install_path must name an .exe file".to_owned());
        }
        if service_account.trim().is_empty() || service_account.chars().any(char::is_control) {
            return Err("service_account must be nonempty without control characters".to_owned());
        }
        if let Some(path) = &self.telemetry_path {
            let path = crate::windows_service_config::normalize_windows_path(path)?;
            let control = crate::windows_service_config::normalize_windows_path(CONTROL_PATH)?;
            let config = crate::windows_service_config::normalize_windows_path(
                r"C:\ProgramData\VirtioMemService\config.json",
            )?;
            let executable = crate::windows_service_config::normalize_windows_path(install_path)?;
            if path == control
                || path.starts_with(&format!("{control}."))
                || path == config
                || path == executable
            {
                return Err("telemetry_path collides with control, configuration or executable files; omit it to disable diagnostics".to_owned());
            }
        }
        Ok(())
    }

    /// This VM's guest policy. The Windows `viomem` driver removes memory
    /// in bounded steps; a Linux guest may return its whole surplus at once.
    pub fn guest_policy(&self, globals: &GlobalSettings) -> GuestMemoryPolicy {
        let removal_step = match self.guest_os {
            GuestOs::Windows => self.free_ram + self.free_ram / 2,
            GuestOs::Linux => self.max_ram,
        };
        globals.guest_policy(self.free_ram, removal_step)
    }

    /// The Windows service's generated runtime configuration.
    pub fn service_config(
        &self,
        globals: &GlobalSettings,
    ) -> Result<WindowsServiceConfigFile, String> {
        let (_, service_account) = self.windows_install()?;
        Ok(WindowsServiceConfigFile {
            schema_version: WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION,
            vm_name: self.vm_name.clone(),
            service_name: SERVICE_NAME.to_owned(),
            display_name: "Virtio Memory Service".to_owned(),
            description: "Manages guest virtio-mem requests.".to_owned(),
            demand_report_path: CONTROL_PATH.to_owned(),
            service_account: service_account.to_owned(),
            poll_interval_millis: globals.poll_interval_seconds * 1000,
            shutdown_timeout_millis: 30_000,
            policy: self.guest_policy(globals),
            min_ram: self.min_ram,
            max_ram: self.max_ram,
            telemetry_path: self.telemetry_path.clone(),
        })
    }

    /// The Linux guest service's generated runtime configuration.
    pub fn linux_service_config(
        &self,
        globals: &GlobalSettings,
    ) -> Result<LinuxServiceConfigFile, String> {
        if self.guest_os != GuestOs::Linux {
            return Err(format!("VM {} is not a Linux guest", self.vm_name));
        }
        Ok(LinuxServiceConfigFile {
            schema_version: LINUX_SERVICE_CONFIG_SCHEMA_VERSION,
            vm_name: self.vm_name.clone(),
            service_name: LINUX_SERVICE_NAME.to_owned(),
            control_path: LINUX_CONTROL_PATH.to_owned(),
            poll_interval_millis: globals.poll_interval_seconds * 1000,
            policy: self.guest_policy(globals),
            min_ram: self.min_ram,
            max_ram: self.max_ram,
        })
    }
}

pub fn validate_ram_limits(minimum: u64, maximum: u64) -> Result<(), String> {
    if minimum == 0 || minimum > maximum {
        return Err(
            "min_ram must be positive and no greater than max_ram (total guest RAM)".to_owned(),
        );
    }
    if !minimum.is_multiple_of(crate::MIN_BLOCK_SIZE_BYTES)
        || !maximum.is_multiple_of(crate::MIN_BLOCK_SIZE_BYTES)
    {
        return Err(format!("min_ram and max_ram must be multiples of {} bytes; live device limits are aligned to its block size", crate::MIN_BLOCK_SIZE_BYTES));
    }
    Ok(())
}

/// A libvirt device alias as this project accepts it: non-empty ASCII
/// letters, digits, `_`, `.` and `-`.
pub fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

/// Lower-case ASCII identities avoid case-folding, reserved-name and path
/// traversal collisions on both Linux and Windows; no lossy sanitisation.
pub fn vm_filename(name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err("vm_name must be 1..=64 lower-case ASCII letters, digits, '-' or '_', starting with a letter or digit".to_owned());
    }
    Ok(format!("{name}_settings.toml"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostVm {
    pub vm_name: String,
    pub alias: String,
    pub priority: u32,
    /// Optional host-local diagnostic output. No guest path or transport here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostList {
    #[serde(rename = "vm")]
    pub vms: Vec<HostVm>,
}

impl HostList {
    pub fn validate(&self) -> Result<(), String> {
        if self.vms.is_empty() {
            return Err("host_list.toml must contain at least one [[vm]] entry".to_owned());
        }
        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for vm in &self.vms {
            vm_filename(&vm.vm_name)?;
            if !names.insert(&vm.vm_name) {
                return Err(format!("duplicate VM {}", vm.vm_name));
            }
            if vm.priority == 0 || !valid_alias(&vm.alias) {
                return Err(format!(
                    "VM {} needs a positive priority and a valid libvirt device alias",
                    vm.vm_name
                ));
            }
            if let Some(path) = &vm.telemetry_path {
                let file = Path::new(path);
                if file
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                    || file.file_name().is_none()
                    || file.starts_with("/var/lib/virtio-mem-host")
                    || file.starts_with("/etc/virtio-mem-host")
                    || !file.is_absolute()
                    || path.chars().any(char::is_control)
                    || !paths.insert(file)
                {
                    return Err(format!(
                        "VM {} telemetry_path must be a unique absolute host file path outside controller state/config directories",
                        vm.vm_name
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if text.len() > 1024 * 1024 {
        return Err(format!("{} exceeds 1 MiB", path.display()));
    }
    toml::from_str(&text).map_err(|error| format!("parse {}: {error}; migrate old manifests/host configs to global-settings.toml, host_list.toml and <vm_name>_settings.toml", path.display()))
}

pub fn load_globals(directory: &Path) -> Result<GlobalSettings, String> {
    let path = directory.join(GLOBAL_FILE);
    let globals = match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GlobalSettings::default(),
        _ => read_toml(&path)?,
    };
    globals.validate()?;
    Ok(globals)
}

pub fn load_vm(path: &Path) -> Result<VmSettings, String> {
    let vm: VmSettings = read_toml(path)?;
    vm.validate()?;
    if path.file_name().and_then(|name| name.to_str()) != Some(vm_filename(&vm.vm_name)?.as_str()) {
        return Err(format!(
            "VM {} settings must be named {}",
            vm.vm_name,
            vm_filename(&vm.vm_name)?
        ));
    }
    Ok(vm)
}

/// Load and validate the complete source set before any consumer can act.
pub fn load_configuration(
    list_path: &Path,
) -> Result<(GlobalSettings, HostList, Vec<VmSettings>), String> {
    let directory = list_path.parent().unwrap_or(Path::new("."));
    let globals = load_globals(directory)?;
    let list: HostList = read_toml(list_path)?;
    list.validate()?;
    if list.vms.len() > 1 && globals.pool_bytes.is_none() {
        return Err("set pool_bytes in global-settings.toml when managing multiple VMs".to_owned());
    }
    let vms = list
        .vms
        .iter()
        .map(|entry| {
            let vm = load_vm(&directory.join(vm_filename(&entry.vm_name)?))?;
            if vm.vm_name != entry.vm_name {
                return Err(format!(
                    "VM settings identity does not match {}",
                    entry.vm_name
                ));
            }
            Ok(vm)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((globals, list, vms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str) -> VmSettings {
        VmSettings {
            min_ram: 4 * GIB,
            max_ram: 16 * GIB,
            free_ram: 2 * GIB,
            vm_name: name.to_owned(),
            guest_os: GuestOs::Windows,
            install_path: Some(
                r"C:\Program Files\VirtioMemService\virtio-mem-service.exe".to_owned(),
            ),
            service_account: Some(r"NT AUTHORITY\LocalService".to_owned()),
            telemetry_path: None,
        }
    }

    #[test]
    fn filenames_are_safe_and_injective_without_case_folding() {
        let names = ["guest", "guest-1", "guest_1", "con", "nul"];
        let filenames: BTreeSet<_> = names
            .iter()
            .map(|name| vm_filename(name).unwrap())
            .collect();
        assert_eq!(names.len(), filenames.len());
        for name in [
            "", "Guest", "../guest", ".", "guest/x", "guest\\x", "-guest", "guest ", "a.b", "é",
        ] {
            assert!(vm_filename(name).is_err(), "{name}");
        }
        assert!(vm_filename(&"x".repeat(65)).is_err());
    }

    #[test]
    fn minimal_settings_round_trip_and_generate_a_complete_control_config() {
        let settings = vm("guest");
        let text = toml::to_string(&settings).unwrap();
        assert!(!text.contains("telemetry"));
        assert_eq!(toml::from_str::<VmSettings>(&text).unwrap(), settings);
        let globals: GlobalSettings = toml::from_str("").unwrap();
        globals.validate().unwrap();
        assert!(!text.contains("guest_os"));
        let service = settings.service_config(&globals).unwrap();
        service.validate().unwrap();
        service
            .validate_paths(r"C:\ProgramData\VirtioMemService\config.json")
            .unwrap();
        assert_eq!(service.demand_report_path, CONTROL_PATH);
        assert!(service.telemetry_path.is_none());
        let host: HostList =
            toml::from_str("[[vm]]\nvm_name='guest'\nalias='memory0'\npriority=1").unwrap();
        host.validate().unwrap();
        assert!(host.vms[0].telemetry_path.is_none());
        let mut host = host;
        host.vms[0].telemetry_path = Some("/tmp/virtio-diagnostic.json".to_owned());
        host.validate().unwrap();
        let mut other = host.vms[0].clone();
        other.vm_name = "other".to_owned();
        other.telemetry_path = Some("/tmp/./virtio-diagnostic.json".to_owned());
        host.vms.push(other);
        assert!(host.validate().is_err(), "outputs cannot alias across VMs");
    }

    #[test]
    fn a_linux_guest_has_its_own_identity_and_rejects_windows_settings() {
        let linux: VmSettings = toml::from_str(
            "min_ram='2GB'\nmax_ram='5GB'\nfree_ram='512MB'\nguest_os='linux'\nvm_name='debian'",
        )
        .unwrap();
        linux.validate().unwrap();
        assert_eq!(
            toml::from_str::<VmSettings>(&toml::to_string(&linux).unwrap()).unwrap(),
            linux
        );
        let globals = GlobalSettings::default();
        let service = linux.linux_service_config(&globals).unwrap();
        service.validate().unwrap();
        assert_eq!(service.service_name, GuestOs::Linux.service_name());
        assert_eq!(service.control_path, GuestOs::Linux.control_path());
        assert!(linux.service_config(&globals).is_err());
        assert!(vm("guest").linux_service_config(&globals).is_err());

        let mut mixed = linux.clone();
        mixed.service_account = Some("root".to_owned());
        assert!(mixed.validate().is_err());
        let mut windows = vm("guest");
        windows.install_path = None;
        assert!(
            windows.validate().is_err(),
            "Windows needs its install path"
        );
    }

    #[test]
    fn every_policy_size_comes_from_the_vm_free_ram_setting() {
        let globals = GlobalSettings::default();
        let mut windows = vm("guest");
        let policy = windows.guest_policy(&globals);
        policy.validate().unwrap();
        assert_eq!(policy.target.physical_reserve_bytes, 2 * GIB);
        assert_eq!(policy.target.commit_reserve_bytes, 2 * GIB);
        assert_eq!(policy.target.safe_floor_physical_reserve_bytes, GIB);
        assert_eq!(policy.target.downward_hysteresis_bytes, GIB / 4);
        assert_eq!(policy.grow_step_bytes, GIB);
        assert_eq!(policy.shrink_step_bytes, 3 * GIB);

        let linux: VmSettings = toml::from_str(
            "min_ram='2GB'\nmax_ram='5GB'\nfree_ram='512MB'\nguest_os='linux'\nvm_name='debian'",
        )
        .unwrap();
        let policy = linux.guest_policy(&globals);
        policy.validate().unwrap();
        assert_eq!(policy.target.physical_reserve_bytes, GIB / 2);
        assert_eq!(policy.target.safe_floor_physical_reserve_bytes, GIB / 4);
        assert_eq!(policy.target.downward_hysteresis_bytes, GIB / 16);
        assert_eq!(
            policy.shrink_step_bytes,
            5 * GIB,
            "Linux returns its surplus at once"
        );

        for invalid in [0, GIB / 2 + 1, 16 * GIB] {
            windows.free_ram = invalid;
            assert!(windows.validate().is_err(), "{invalid}");
        }
        assert!(
            toml::from_str::<VmSettings>("min_ram='2GB'\nmax_ram='5GB'\nvm_name='debian'").is_err(),
            "free_ram has no built-in default"
        );
    }

    #[test]
    fn validates_ram_limits_optional_outputs_and_rejects_obsolete_keys() {
        let mut settings = vm("guest");
        for (min, max) in [(0, GIB), (4 * GIB, GIB), (GIB + 1, 4 * GIB)] {
            settings.min_ram = min;
            settings.max_ram = max;
            assert!(settings.validate().is_err());
        }
        settings = vm("guest");
        for path in [
            "",
            "relative.json",
            CONTROL_PATH,
            r"C:\ProgramData\VirtioMemService\telemetry.json.requests",
            r"C:\ProgramData\VirtioMemService\config.json",
        ] {
            settings.telemetry_path = Some(path.to_owned());
            assert!(settings.validate().is_err(), "{path}");
        }
        settings.telemetry_path =
            Some(r"C:\ProgramData\VirtioMemService\diagnostic.json".to_owned());
        settings.validate().unwrap();
        assert!(toml::from_str::<VmSettings>(&format!(
            "{}\npoll_interval_millis=5000",
            toml::to_string(&settings).unwrap()
        ))
        .is_err());
        assert!(toml::from_str::<GlobalSettings>("virsh_binary='virsh'").is_err());
        assert!(
            toml::from_str::<HostList>("[[vm]]\nname='old'\nalias='memory0'\npriority=1").is_err()
        );
        let invalid = GlobalSettings {
            poll_interval_seconds: 0,
            ..GlobalSettings::default()
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn independent_vm_files_are_loaded_relative_to_list_and_identity_checked() {
        let root = std::env::temp_dir().join(format!("virtio-mem-settings-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(HOST_LIST_FILE);
        let mut list = HostList {
            vms: vec![HostVm {
                vm_name: "alpha".to_owned(),
                alias: "memory0".to_owned(),
                priority: 2,
                telemetry_path: None,
            }],
        };
        let write = |path: &Path, text: String| std::fs::write(path, text).unwrap();
        write(&path, toml::to_string(&list).unwrap());
        write(
            &root.join("alpha_settings.toml"),
            toml::to_string(&vm("alpha")).unwrap(),
        );
        // Missing globals use defaults and missing telemetry requires no stand-ins.
        let (_, _, loaded) = load_configuration(&path).unwrap();
        assert_eq!(loaded[0], vm("alpha"));
        let mut beta = vm("beta");
        beta.min_ram = 8 * GIB;
        list.vms.push(HostVm {
            vm_name: "beta".to_owned(),
            alias: "memory0".to_owned(),
            priority: 1,
            telemetry_path: None,
        });
        write(&path, toml::to_string(&list).unwrap());
        write(
            &root.join("beta_settings.toml"),
            toml::to_string(&beta).unwrap(),
        );
        assert!(load_configuration(&path)
            .unwrap_err()
            .contains("pool_bytes"));
        write(&root.join(GLOBAL_FILE), "pool_bytes='32GB'".to_owned());
        let (_, _, loaded) = load_configuration(&path).unwrap();
        assert_eq!(loaded, [vm("alpha"), beta]);
        write(
            &root.join("beta_settings.toml"),
            toml::to_string(&vm("alpha")).unwrap(),
        );
        assert!(load_configuration(&path).is_err());
        std::fs::remove_file(root.join("beta_settings.toml")).unwrap();
        assert!(load_configuration(&path).is_err());
        list.vms[1].vm_name = "alpha".to_owned();
        assert!(list.validate().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
