//! The Linux guest service's configuration file format. It lives here so the
//! service and the deployment tooling share one serialization.

use serde::{Deserialize, Serialize};

use crate::GuestMemoryPolicy;

pub const LINUX_SERVICE_CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxServiceConfigFile {
    pub schema_version: u32,
    pub vm_name: String,
    pub service_name: String,
    /// The control publication. Host status and durable request state are
    /// `<path>.<suffix>` beside it.
    pub control_path: String,
    pub poll_interval_millis: u64,
    pub policy: GuestMemoryPolicy,
    pub min_ram: u64,
    pub max_ram: u64,
}

impl LinuxServiceConfigFile {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != LINUX_SERVICE_CONFIG_SCHEMA_VERSION {
            return Err(format!(
                "unsupported configuration schema version: {}",
                self.schema_version
            ));
        }
        if !crate::memory_request::valid_identity(&self.vm_name)
            || !crate::memory_request::valid_identity(&self.service_name)
            || self.service_name.contains('/')
        {
            return Err("VM/service identity must be ASCII of at most 128 bytes; service names cannot contain slashes".to_owned());
        }
        // The host reads this path through the guest agent, so it must mean
        // the same file whatever the reader's working directory.
        if !self.control_path.starts_with('/')
            || self.control_path.ends_with('/')
            || self.control_path.chars().any(char::is_control)
            || self
                .control_path
                .split('/')
                .any(|part| part == "." || part == "..")
        {
            return Err(
                "control_path must be an absolute file path without '.' or '..' components"
                    .to_owned(),
            );
        }
        if self.poll_interval_millis == 0 || self.poll_interval_millis >= u64::from(u32::MAX) {
            return Err("configuration timing is zero or too large".to_owned());
        }
        crate::settings::validate_ram_limits(self.min_ram, self.max_ram)?;
        self.policy.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{GlobalSettings, GuestOs};

    fn config() -> LinuxServiceConfigFile {
        LinuxServiceConfigFile {
            schema_version: LINUX_SERVICE_CONFIG_SCHEMA_VERSION,
            vm_name: "debian".to_owned(),
            service_name: GuestOs::Linux.service_name().to_owned(),
            control_path: GuestOs::Linux.control_path().to_owned(),
            poll_interval_millis: 1000,
            policy: GlobalSettings::default().guest_policy(),
            min_ram: 2 << 30,
            max_ram: 5 << 30,
        }
    }

    #[test]
    fn round_trips_and_rejects_ambiguous_paths_and_unknown_fields() {
        let valid = config();
        valid.validate().unwrap();
        let text = serde_json::to_string(&valid).unwrap();
        assert_eq!(
            serde_json::from_str::<LinuxServiceConfigFile>(&text).unwrap(),
            valid
        );
        for path in ["", "control.json", "/var/lib/../control.json", "/var/lib/"] {
            let mut invalid = config();
            invalid.control_path = path.to_owned();
            assert!(invalid.validate().is_err(), "{path}");
        }
        let mut invalid = config();
        invalid.schema_version += 1;
        assert!(invalid.validate().is_err());
        let mut extended = serde_json::to_value(&valid).unwrap();
        extended["service_account"] = serde_json::json!("root");
        assert!(serde_json::from_value::<LinuxServiceConfigFile>(extended).is_err());
    }
}
