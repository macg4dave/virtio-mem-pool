//! The Windows memory service's configuration file. It holds only what
//! installing the service needs: its SCM identity, the account it runs as and
//! how long it may take to stop. Like the Linux guest, the service takes its
//! VM name, limits, policy and poll interval from the host at run time.

use serde::{Deserialize, Serialize};

/// Version 9 removed everything the host now publishes.
pub const WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION: u32 = 9;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsServiceConfigFile {
    pub schema_version: u32,
    pub service_name: String,
    pub display_name: String,
    pub description: String,
    pub service_account: String,
    pub shutdown_timeout_millis: u64,
}

impl WindowsServiceConfigFile {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION {
            return Err(format!(
                "unsupported configuration schema version: {}",
                self.schema_version
            ));
        }
        for value in [
            &self.service_name,
            &self.display_name,
            &self.description,
            &self.service_account,
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                return Err(
                    "configuration strings must be nonempty and contain no control characters"
                        .to_owned(),
                );
            }
        }
        if !crate::memory_request::valid_identity(&self.service_name)
            || self.service_name.contains(['/', '\\'])
        {
            return Err(
                "the service name must be ASCII of at most 128 bytes, without slashes".to_owned(),
            );
        }
        if self.shutdown_timeout_millis == 0 || self.shutdown_timeout_millis >= u64::from(u32::MAX)
        {
            return Err("shutdown_timeout_millis is zero or too large".to_owned());
        }
        Ok(())
    }

    /// The control publication also names its host status and durable
    /// request state as `<path>.<suffix>`; the configuration must be none of
    /// them.
    pub fn validate_paths(&self, config_path: &str) -> Result<(), String> {
        let config = normalize_windows_path(config_path)?;
        let report = normalize_windows_path(crate::settings::CONTROL_PATH)?;
        if config == report || config.starts_with(&format!("{report}.")) {
            return Err(
                "configuration path collides with the control publication or its derived files"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// Compare Windows paths consistently even when deployment runs on Linux.
/// Reject ambiguous Win32 forms rather than guessing their native expansion.
pub fn normalize_windows_path(value: &str) -> Result<String, String> {
    let path = value.replace('/', "\\");
    let bytes = path.as_bytes();
    if value.chars().any(char::is_control)
        || bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
    {
        return Err(
            "path must be an absolute Windows drive path without control characters".to_owned(),
        );
    }
    let mut parts = Vec::new();
    for part in path[3..].split('\\') {
        if part == "." || part.is_empty() {
            continue;
        }
        if part == ".."
            || part.ends_with(['.', ' '])
            || part.contains([':', '"', '<', '>', '|', '?', '*'])
            || matches!(
                part.split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
        {
            return Err("path contains an ambiguous or reserved Windows component".to_owned());
        }
        parts.push(part.to_lowercase());
    }
    if parts.is_empty() {
        return Err("path must name a file".to_owned());
    }
    Ok(format!(
        "{}:\\{}",
        char::from(bytes[0]).to_ascii_lowercase(),
        parts.join("\\")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_aliases_are_compared_on_every_platform() {
        assert_eq!(
            normalize_windows_path(r"C:\Data\.\CONFIG.json").unwrap(),
            normalize_windows_path("c:/data/config.json").unwrap()
        );
        for path in [
            "relative.json",
            r"C:\data\..\config.json",
            "C:\\data\\name. ",
            "C:\\data\\nul.json",
            "C:\\data\\file:stream",
            "C:\\data\\bad\0.json",
        ] {
            assert!(normalize_windows_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn configuration_cannot_alias_any_derived_publication_file() {
        let config = WindowsServiceConfigFile {
            schema_version: WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION,
            service_name: "VirtioMemService".to_owned(),
            display_name: "Virtio memory".to_owned(),
            description: "Virtio memory".to_owned(),
            service_account: r"NT AUTHORITY\LocalService".to_owned(),
            shutdown_timeout_millis: 1000,
        };
        config.validate().unwrap();
        config
            .validate_paths(r"C:\ProgramData\VirtioMemService\config.json")
            .unwrap();
        let control = crate::settings::CONTROL_PATH;
        for path in [
            crate::host_status_path(&control.to_uppercase()),
            crate::request_state_path(control),
            format!("{control}.tmp-12"),
            control.to_owned(),
        ] {
            assert!(config.validate_paths(&path).is_err(), "{path}");
        }
        let mut old = serde_json::to_value(&config).unwrap();
        old["policy"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<WindowsServiceConfigFile>(old).is_err(),
            "an older configuration is refused, not half read"
        );
    }
}
