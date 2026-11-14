//! Administrator-owned TOML settings: `global.toml` and one
//! `vms.d/NAME.toml` per VM, in one configuration directory (normally
//! `/etc/rampire`). Runtime wire files are generated from these, never a
//! second source of administrator settings.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    GuestMemoryPolicy, GuestSettings, TargetPolicyConfig, WindowsConfigSchema,
    WindowsServiceConfigFile, WINDOWS_SERVICE_CONFIG_VERSION,
};

pub const SERVICE_NAME: &str = "RampireGuest";
// The Windows guest's control publication; host status and durable request
// state are derived beside it, as on Linux.
pub const CONTROL_PATH: &str = r"C:\ProgramData\RAMpire\telemetry.json";
/// The Linux guest service's wire identity and systemd unit name.
pub const LINUX_SERVICE_NAME: &str = "rampire-guest";
/// The Linux guest's control publication; host status and durable request
/// state are derived beside it, as on Windows.
pub const LINUX_CONTROL_PATH: &str = "/var/lib/rampire-guest/control.json";

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

    /// How long a removal may stop moving before the host settles it at what
    /// the guest returned.
    ///
    /// Linux: the guest driver retries a busy removal on its own schedule
    /// (see `linux_removal_retry_after_seconds`), so the host waits for
    /// `LINUX_REMOVAL_RETRIES_AWAITED` retries from a fresh stall plus
    /// `LINUX_REMOVAL_SETTLE_MARGIN_SECONDS`. The driver's wait never
    /// exceeds its 300 s maximum, so the same window also covers at least one
    /// retry after any later movement. Windows uses the global
    /// `convergence_timeout_seconds`.
    pub fn removal_settle_seconds(self, convergence_timeout_seconds: u64) -> u64 {
        match self {
            Self::Windows => convergence_timeout_seconds,
            Self::Linux => {
                linux_removal_retry_after_seconds(LINUX_REMOVAL_RETRIES_AWAITED)
                    + LINUX_REMOVAL_SETTLE_MARGIN_SECONDS
            }
        }
    }
}

/// The Linux driver's first busy-removal retry, `VIRTIO_MEM_RETRY_TIMER_MIN_MS`
/// in `drivers/virtio/virtio_mem.c`.
const LINUX_REMOVAL_RETRY_MIN_SECONDS: u64 = 50;
/// The longest wait between Linux retries, `VIRTIO_MEM_RETRY_TIMER_MAX_MS`.
const LINUX_REMOVAL_RETRY_MAX_SECONDS: u64 = 300;
/// Busy-removal retries a Linux guest gets before its removal is settled.
const LINUX_REMOVAL_RETRIES_AWAITED: u32 = 3;
/// Allowance for the awaited retry's own offlining work and the next host
/// poll (at most 10 s) to show its movement. Provisional until the recorded
/// late-retry run (roadmap P1).
const LINUX_REMOVAL_SETTLE_MARGIN_SECONDS: u64 = 30;

/// Seconds from a Linux removal's first busy failure to its `retries`-th
/// retry. The driver's wait starts at 50 s and doubles after each retry up to
/// 300 s; only a run that completes the whole request resets it, so partial
/// progress does not: retries come at +50, +150, +350, +650 s and so on.
fn linux_removal_retry_after_seconds(retries: u32) -> u64 {
    let mut wait = LINUX_REMOVAL_RETRY_MIN_SECONDS;
    let mut elapsed = 0;
    for _ in 0..retries {
        elapsed += wait;
        wait = (wait * 2).min(LINUX_REMOVAL_RETRY_MAX_SECONDS);
    }
    elapsed
}

/// The global settings, in the configuration directory. It may be absent.
pub const GLOBAL_FILE: &str = "global.toml";
/// The folder of VM settings files, in the configuration directory.
pub const VMS_DIRECTORY: &str = "vms.d";
const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettings {
    #[serde(
        default,
        deserialize_with = "crate::units::byte_size::option::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub pool: Option<u64>,
    /// Host memory kept free for the host itself: growth stops above it.
    #[serde(with = "crate::units::byte_size")]
    pub host_reserve: u64,
    pub observe_only: bool,
    pub poll_interval_seconds: u64,
    pub command_timeout_seconds: u64,
    pub convergence_timeout_seconds: u64,
    /// The libvirt connection the controller manages its VMs through.
    pub libvirt_uri: String,
    /// How many applied copies, and records of what each apply did, are
    /// kept. At least 2, so there is always a copy to roll back to.
    pub retention: u32,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            pool: None,
            host_reserve: 4 * GIB,
            observe_only: false,
            poll_interval_seconds: 5,
            command_timeout_seconds: 10,
            convergence_timeout_seconds: 300,
            libvirt_uri: "qemu:///system".to_owned(),
            retention: 10,
        }
    }
}

impl GlobalSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.pool == Some(0) || self.host_reserve == 0 {
            return Err("pool and host_reserve must be positive".to_owned());
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
        if self.libvirt_uri.trim().is_empty()
            || self.libvirt_uri.chars().any(char::is_whitespace)
            || !self.libvirt_uri.contains(":/")
        {
            return Err("libvirt_uri must be a libvirt connection URI".to_owned());
        }
        if !(2..=1000).contains(&self.retention) {
            return Err("retention must be 2..=1000".to_owned());
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

/// One VM's settings: the file `vms.d/NAME.toml`, where NAME is the libvirt
/// domain. The name comes from the file, so it isn't repeated inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSettings {
    /// The libvirt domain: the file's name without `.toml`.
    #[serde(skip)]
    pub vm_name: String,
    /// The libvirt alias of the VM's virtio-mem device.
    pub alias: String,
    /// Pool priority; larger is served first when VMs contend.
    pub priority: u32,
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
    /// Optional host-local diagnostic copy of each guest publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_path: Option<String>,
    /// The review profile, `reviews/NAME.toml`, the VM is attested with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
}

impl VmSettings {
    pub fn validate(&self) -> Result<(), String> {
        validate_vm_name(&self.vm_name)?;
        if self.priority == 0 || !valid_alias(&self.alias) {
            return Err(
                "a VM needs a positive priority and a valid libvirt device alias".to_owned(),
            );
        }
        if let Some(path) = &self.telemetry_path {
            validate_telemetry_path(path)?;
        }
        if let Some(review) = &self.review {
            validate_name("review profile", review)?;
        }
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
                if self.install_path.is_some() || self.service_account.is_some() {
                    return Err("install_path and service_account are Windows settings; omit them for a Linux guest".to_owned());
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
        Ok(())
    }

    /// This VM's guest policy. The Windows `viomem` driver removes memory
    /// in bounded steps; a Linux guest may return its whole surplus at once.
    /// PAUSED (viomem): the Windows step stays until Hyper-V Dynamic Memory
    /// replaces viomem (docs/roadmap.md, milestone W), whose own limit is
    /// about 2.2 GiB per removal.
    pub fn guest_policy(&self, globals: &GlobalSettings) -> GuestMemoryPolicy {
        let removal_step = match self.guest_os {
            GuestOs::Windows => self.free_ram + self.free_ram / 2,
            GuestOs::Linux => self.max_ram,
        };
        globals.guest_policy(self.free_ram, removal_step)
    }

    /// The Windows service's install configuration. Its limits and policy
    /// are not in it: the host publishes them, as for a Linux guest.
    pub fn service_config(&self) -> Result<WindowsServiceConfigFile, String> {
        let (_, service_account) = self.windows_install()?;
        Ok(WindowsServiceConfigFile {
            schema: WindowsConfigSchema,
            version: WINDOWS_SERVICE_CONFIG_VERSION,
            service_name: SERVICE_NAME.to_owned(),
            display_name: "RAMpire guest service".to_owned(),
            description: "RAMpire guest memory requests.".to_owned(),
            service_account: service_account.to_owned(),
            shutdown_timeout_millis: 30_000,
        })
    }

    /// What the host publishes to this VM's guest: the administrator's
    /// limits and the policy derived from them.
    pub fn guest_settings(&self, globals: &GlobalSettings) -> GuestSettings {
        GuestSettings {
            policy: self.guest_policy(globals),
            min_ram: self.min_ram,
            max_ram: self.max_ram,
            poll_interval_millis: globals.poll_interval_seconds * 1000,
        }
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
/// VM and review profile names follow it, since each names a file.
fn validate_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(format!("{kind} name {name:?} must be 1..=64 lower-case ASCII letters, digits, '-' or '_', starting with a letter or digit"));
    }
    Ok(())
}

/// A name the command line uses for `global.toml`, so no VM may have it.
pub const RESERVED_VM_NAME: &str = "global";

fn validate_vm_name(name: &str) -> Result<(), String> {
    validate_name("VM", name)?;
    if name == RESERVED_VM_NAME {
        return Err(format!(
            "a VM can't be named {RESERVED_VM_NAME:?}: rampire set uses that name for global.toml"
        ));
    }
    Ok(())
}

/// The settings file of the VM `name`, inside `vms.d`.
pub fn vm_filename(name: &str) -> Result<String, String> {
    validate_vm_name(name)?;
    Ok(format!("{name}.toml"))
}

/// A diagnostic copy goes to an absolute host file outside the controller's
/// own directories.
fn validate_telemetry_path(path: &str) -> Result<(), String> {
    let file = Path::new(path);
    if file
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
        || file.file_name().is_none()
        || file.starts_with("/var/lib/rampire")
        || file.starts_with("/etc/rampire")
        // Host paths are Linux paths, whatever platform checks them.
        || !path.starts_with('/')
        || path.chars().any(char::is_control)
    {
        return Err("telemetry_path must be an absolute host file path outside the controller's state and configuration directories".to_owned());
    }
    Ok(())
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
    toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
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

/// One VM's settings file. The VM's name is the file's name.
pub fn load_vm(path: &Path) -> Result<VmSettings, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".toml"))
        .ok_or_else(|| format!("{}: a VM settings file is named NAME.toml", path.display()))?;
    let mut vm: VmSettings = read_toml(path)?;
    vm.vm_name = name.to_owned();
    vm.validate()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(vm)
}

/// Load and validate the whole configuration in `directory` before any
/// consumer can act: `global.toml`, which may be absent, and every
/// `vms.d/NAME.toml`, in name order. Other files in `vms.d`, such as editor
/// backups, hidden files and a package's `NAME.toml.rpmnew`, are ignored. An
/// empty `vms.d` is valid: the controller then manages no VMs (decision D13).
pub fn load_configuration(directory: &Path) -> Result<(GlobalSettings, Vec<VmSettings>), String> {
    let globals = load_globals(directory)?;
    let vms_directory = directory.join(VMS_DIRECTORY);
    let read_error = |error: std::io::Error| format!("read {}: {error}", vms_directory.display());
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&vms_directory).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with('.') && name.ends_with(".toml") {
            paths.push(path);
        }
    }
    paths.sort();
    let vms = paths
        .iter()
        .map(|path| load_vm(path))
        .collect::<Result<Vec<_>, String>>()?;
    if vms.len() > 1 && globals.pool.is_none() {
        return Err(format!(
            "set pool in {GLOBAL_FILE} when managing more than one VM"
        ));
    }
    let mut outputs = BTreeSet::new();
    for vm in &vms {
        if let Some(path) = &vm.telemetry_path {
            if !outputs.insert(Path::new(path)) {
                return Err(format!(
                    "VM {} shares its telemetry_path with another VM",
                    vm.vm_name
                ));
            }
        }
    }
    Ok((globals, vms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_removals_settle_after_the_drivers_third_busy_retry() {
        let retries: Vec<_> = (1..=5).map(linux_removal_retry_after_seconds).collect();
        assert_eq!(retries, [50, 150, 350, 650, 950]);
        let linux = GuestOs::Linux.removal_settle_seconds(300);
        assert!(linux > linux_removal_retry_after_seconds(3));
        assert!(linux < linux_removal_retry_after_seconds(4));
        // After movement at any retry the next one is at most 300 s away.
        assert!(linux > LINUX_REMOVAL_RETRY_MAX_SECONDS);
        // Linux ignores the global value; Windows keeps it.
        assert_eq!(GuestOs::Linux.removal_settle_seconds(60), linux);
        assert_eq!(GuestOs::Windows.removal_settle_seconds(60), 60);
    }

    fn vm(name: &str) -> VmSettings {
        VmSettings {
            alias: "memory0".to_owned(),
            priority: 1,
            telemetry_path: None,
            review: None,
            min_ram: 4 * GIB,
            max_ram: 16 * GIB,
            free_ram: 2 * GIB,
            vm_name: name.to_owned(),
            guest_os: GuestOs::Windows,
            install_path: Some(r"C:\Program Files\RAMpire\rampire-guest.exe".to_owned()),
            service_account: Some(r"NT AUTHORITY\LocalService".to_owned()),
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
        assert_eq!(vm_filename("guest").unwrap(), "guest.toml");
        assert!(vm_filename("global").is_err());
    }

    #[test]
    fn minimal_settings_round_trip_and_generate_a_complete_control_config() {
        let settings = vm("guest");
        let text = toml::to_string(&settings).unwrap();
        assert!(!text.contains("telemetry"));
        assert!(!text.contains("vm_name"), "the name is the file's: {text}");
        let parsed = VmSettings {
            vm_name: "guest".to_owned(),
            ..toml::from_str::<VmSettings>(&text).unwrap()
        };
        assert_eq!(parsed, settings);
        let globals: GlobalSettings = toml::from_str("").unwrap();
        globals.validate().unwrap();
        assert!(!text.contains("guest_os"));
        let service = settings.service_config().unwrap();
        service.validate().unwrap();
        service
            .validate_paths(r"C:\ProgramData\RAMpire\config.json")
            .unwrap();
        assert_eq!(service.service_account, r"NT AUTHORITY\LocalService");
        let mut traced = settings.clone();
        traced.telemetry_path = Some("/tmp/virtio-diagnostic.json".to_owned());
        traced.validate().unwrap();
        for path in [
            r"C:\x.json",
            "relative.json",
            "/etc/rampire/x.json",
            "/tmp/../x",
        ] {
            traced.telemetry_path = Some(path.to_owned());
            assert!(traced.validate().is_err(), "{path}");
        }
        traced.telemetry_path = None;
        traced.review = Some("linux-standard".to_owned());
        traced.validate().unwrap();
        traced.review = Some("../other".to_owned());
        assert!(traced.validate().is_err());
    }

    #[test]
    fn a_linux_guest_has_its_own_identity_and_rejects_windows_settings() {
        let linux: VmSettings = toml::from_str(
            "min_ram='2GB'\nmax_ram='5GB'\nfree_ram='512MB'\nguest_os='linux'\nalias='memory0'\npriority=1",
        )
        .unwrap();
        let linux = VmSettings {
            vm_name: "debian".to_owned(),
            ..linux
        };
        linux.validate().unwrap();
        let round_trip = VmSettings {
            vm_name: "debian".to_owned(),
            ..toml::from_str::<VmSettings>(&toml::to_string(&linux).unwrap()).unwrap()
        };
        assert_eq!(round_trip, linux);
        let globals = GlobalSettings::default();
        let published = linux.guest_settings(&globals);
        published.validate().unwrap();
        assert_eq!((published.min_ram, published.max_ram), (2 * GIB, 5 * GIB));
        assert_eq!(published.policy, linux.guest_policy(&globals));
        assert_eq!(published.poll_interval_millis, 5000);
        assert!(linux.service_config().is_err());

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
            "min_ram='2GB'\nmax_ram='5GB'\nfree_ram='512MB'\nguest_os='linux'\nalias='memory0'\npriority=1",
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
            toml::from_str::<VmSettings>("min_ram='2GB'\nmax_ram='5GB'\nalias='m'\npriority=1")
                .is_err(),
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
        assert!(
            toml::from_str::<VmSettings>(&format!(
                "{}\nvm_name='guest'",
                toml::to_string(&settings).unwrap()
            ))
            .is_err(),
            "the name is the file's, never a setting"
        );
        assert!(toml::from_str::<VmSettings>(&format!(
            "{}\npoll_interval_millis=5000",
            toml::to_string(&settings).unwrap()
        ))
        .is_err());
        assert!(toml::from_str::<GlobalSettings>("virsh_binary='virsh'").is_err());
        for old in ["pool_bytes='4GB'", "host_min_headroom_bytes='4GB'"] {
            assert!(toml::from_str::<GlobalSettings>(old).is_err(), "{old}");
        }
        let invalid = GlobalSettings {
            poll_interval_seconds: 0,
            ..GlobalSettings::default()
        };
        assert!(invalid.validate().is_err());
        let remote: GlobalSettings =
            toml::from_str("libvirt_uri='qemu+ssh://kvm.example/system'").unwrap();
        assert_eq!(remote.validate(), Ok(()));
        for uri in ["", "qemu system", "system"] {
            let invalid = GlobalSettings {
                libvirt_uri: uri.to_owned(),
                ..GlobalSettings::default()
            };
            assert!(invalid.validate().is_err(), "{uri}");
        }
    }

    #[test]
    fn vm_files_in_vms_d_make_the_configuration() {
        let root = std::env::temp_dir().join(format!("rampire-settings-{}", std::process::id()));
        let vms = root.join(VMS_DIRECTORY);
        let write = |path: &Path, text: String| std::fs::write(path, text).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            load_configuration(&root).unwrap_err().contains("vms.d"),
            "a directory without vms.d is not a configuration"
        );
        std::fs::create_dir(&vms).unwrap();
        // No VMs and no global settings: valid, with every default.
        let (globals, loaded) = load_configuration(&root).unwrap();
        assert_eq!((globals, loaded), (GlobalSettings::default(), vec![]));

        write(
            &vms.join("alpha.toml"),
            toml::to_string(&vm("alpha")).unwrap(),
        );
        for ignored in ["alpha.toml.rpmnew", "alpha.toml~", ".beta.toml", "README"] {
            write(&vms.join(ignored), "not settings".to_owned());
        }
        let (_, loaded) = load_configuration(&root).unwrap();
        assert_eq!(loaded, [vm("alpha")]);

        let mut beta = vm("beta");
        beta.min_ram = 8 * GIB;
        write(&vms.join("beta.toml"), toml::to_string(&beta).unwrap());
        assert!(load_configuration(&root).unwrap_err().contains("pool"));
        write(&root.join(GLOBAL_FILE), "pool='32GB'".to_owned());
        let (_, loaded) = load_configuration(&root).unwrap();
        assert_eq!(loaded, [vm("alpha"), beta.clone()]);

        let mut shared = beta;
        shared.telemetry_path = Some("/tmp/./diagnostic.json".to_owned());
        write(&vms.join("beta.toml"), toml::to_string(&shared).unwrap());
        let mut alpha = vm("alpha");
        alpha.telemetry_path = Some("/tmp/diagnostic.json".to_owned());
        write(&vms.join("alpha.toml"), toml::to_string(&alpha).unwrap());
        assert!(
            load_configuration(&root).is_err(),
            "outputs cannot alias across VMs"
        );

        write(
            &vms.join("Bad Name.toml"),
            toml::to_string(&vm("x")).unwrap(),
        );
        assert!(load_configuration(&root).unwrap_err().contains("Bad Name"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
