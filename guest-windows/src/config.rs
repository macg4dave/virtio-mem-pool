use std::io::Read;
use std::time::Duration;

use virtio_mem_core::{WindowsServiceConfigFile, WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION};

use crate::error::ConfigurationError;

pub const DEFAULT_CONFIG_PATH: &str = r"C:\ProgramData\VirtioMemService\config.json";

/// The deployed configuration file and where it was loaded from. It reads as
/// the file's own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    pub file: WindowsServiceConfigFile,
    pub config_path: String,
}

impl std::ops::Deref for ServiceConfig {
    type Target = WindowsServiceConfigFile;

    fn deref(&self) -> &WindowsServiceConfigFile {
        &self.file
    }
}

impl ServiceConfig {
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.file.shutdown_timeout_millis)
    }

    pub fn validate(&self) -> Result<(), ConfigurationError> {
        self.file
            .validate()
            .and_then(|()| self.file.validate_paths(&self.config_path))
            .map_err(ConfigurationError::InvalidFile)?;
        #[cfg(windows)]
        validate_file_identity(&self.config_path, virtio_mem_core::settings::CONTROL_PATH)?;
        Ok(())
    }

    pub fn load_default() -> Result<Self, ConfigurationError> {
        Self::load_from_path(DEFAULT_CONFIG_PATH)
    }

    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigurationError> {
        const MAX_CONFIG_BYTES: u64 = 64 * 1024;
        let path = path.as_ref();
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigurationError::MissingFile(
                    path.to_string_lossy().into_owned(),
                ))
            }
            Err(error) => return Err(ConfigurationError::FileIo(error.to_string())),
        };
        let mut contents = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|error| ConfigurationError::FileIo(error.to_string()))?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigurationError::InvalidFile(
                "configuration exceeds 64 KiB".to_owned(),
            ));
        }
        let file: WindowsServiceConfigFile = serde_json::from_str(&contents)
            .map_err(|error| ConfigurationError::InvalidFile(error.to_string()))?;
        if file.schema_version != WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION {
            return Err(ConfigurationError::UnsupportedSchemaVersion(
                file.schema_version,
            ));
        }
        let config = Self {
            file,
            config_path: path.to_string_lossy().into_owned(),
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(windows)]
pub(crate) fn validate_file_identity(config: &str, report: &str) -> Result<(), ConfigurationError> {
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let identify = |path: &str| -> Result<Option<(u32, u32, u32)>, ConfigurationError> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ConfigurationError::FileIo(error.to_string())),
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
            return Err(ConfigurationError::FileIo(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        Ok(Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        )))
    };
    if let Some(config_id) = identify(config)? {
        for path in [
            report.to_owned(),
            virtio_mem_core::host_status_path(report),
            virtio_mem_core::request_state_path(report),
        ] {
            if identify(&path)? == Some(config_id) {
                return Err(ConfigurationError::InvalidFile(
                    "configuration shares a file identity with a publication file".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// A valid configuration shared by this crate's tests.
#[cfg(test)]
pub(crate) fn test_config() -> ServiceConfig {
    ServiceConfig {
        file: WindowsServiceConfigFile {
            schema_version: WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION,
            service_name: "TestService".to_owned(),
            display_name: "Test service".to_owned(),
            description: "Test configuration".to_owned(),
            service_account: r"NT AUTHORITY\LocalService".to_owned(),
            shutdown_timeout_millis: 30,
        },
        config_path: DEFAULT_CONFIG_PATH.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "virtio-mem-service-{name}-{}.json",
            std::process::id()
        ))
    }

    #[test]
    fn explicit_configuration_is_valid() {
        assert!(test_config().validate().is_ok());
    }

    #[test]
    fn rejects_empty_identity_and_zero_timings() {
        let mut config = test_config();
        config.file.service_name.clear();
        assert!(config.validate().is_err());
        let mut config = test_config();
        config.file.shutdown_timeout_millis = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_path_collisions_and_control_characters() {
        let mut config = test_config();
        config.config_path = virtio_mem_core::settings::CONTROL_PATH.to_uppercase();
        assert!(config.validate().is_err());
        config.config_path =
            virtio_mem_core::request_state_path(virtio_mem_core::settings::CONTROL_PATH);
        assert!(config.validate().is_err());
        let mut config = test_config();
        config.file.description.push('\0');
        assert!(config.validate().is_err());
    }

    #[test]
    fn loads_the_versioned_file_with_its_path() {
        let path = test_path("config-round-trip");
        let expected = ServiceConfig {
            config_path: path.to_string_lossy().into_owned(),
            ..test_config()
        };
        std::fs::write(&path, serde_json::to_vec(&expected.file).unwrap()).unwrap();
        let loaded = ServiceConfig::load_from_path(&path).expect("configuration should load");
        assert_eq!(loaded, expected);
        assert_eq!(loaded.shutdown_timeout(), Duration::from_millis(30));

        let mut old = expected.file.clone();
        old.schema_version = 99;
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(
            ServiceConfig::load_from_path(&path),
            Err(ConfigurationError::UnsupportedSchemaVersion(99))
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            ServiceConfig::load_from_path(&path),
            Err(ConfigurationError::MissingFile(
                path.to_string_lossy().into_owned()
            ))
        );
    }
}
