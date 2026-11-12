//! Loads the generated service configuration.

use std::path::Path;

use virtio_mem_core::LinuxServiceConfigFile;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/virtio-mem-guest/config.json";
const MAX_CONFIG_BYTES: usize = 64 * 1024;

pub fn load(path: &Path) -> Result<LinuxServiceConfigFile, String> {
    let bytes = crate::files::read_bounded(path, MAX_CONFIG_BYTES)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .ok_or_else(|| format!("service configuration file is required: {}", path.display()))?;
    let config: LinuxServiceConfigFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is invalid: {error}", path.display()))?;
    config
        .validate()
        .map_err(|error| format!("{} is invalid: {error}", path.display()))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_mem_core::settings::{GlobalSettings, VmSettings};

    #[test]
    fn loads_the_configuration_the_tooling_generates_and_fails_closed_otherwise() {
        let directory =
            std::env::temp_dir().join(format!("virtio-mem-guest-config-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        assert!(load(&path).unwrap_err().contains("required"));

        let vm: VmSettings =
            toml::from_str("min_ram='2GB'\nmax_ram='5GB'\nguest_os='linux'\nvm_name='debian'")
                .unwrap();
        let generated = vm.linux_service_config(&GlobalSettings::default()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&generated).unwrap()).unwrap();
        assert_eq!(load(&path).unwrap(), generated);

        let mut invalid = generated;
        invalid.min_ram = invalid.max_ram + (1 << 30);
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, b"{}").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
