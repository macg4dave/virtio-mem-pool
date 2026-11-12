//! Linux guest tooling: generates the guest service's runtime configuration
//! from the administrator settings, as Windows deployment does for its guest.

use std::path::Path;

use virtio_mem_core::settings;

pub fn run(arguments: &[String], repo: &Path) -> Result<(), String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        ["config", manifest] => {
            println!("{}", service_config_json(&repo.join(manifest))?);
            Ok(())
        }
        _ => Err("linux requires: config VM_SETTINGS".to_owned()),
    }
}

/// The `/etc/virtio-mem-guest/config.json` for one Linux VM's settings file
/// and its sibling global settings.
fn service_config_json(manifest: &Path) -> Result<String, String> {
    let vm = settings::load_vm(manifest)?;
    let globals = settings::load_globals(manifest.parent().unwrap_or(Path::new(".")))?;
    let config = vm.linux_service_config(&globals)?;
    config.validate()?;
    serde_json::to_string_pretty(&config)
        .map_err(|error| format!("encode service configuration: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_mem_core::settings::GuestOs;
    use virtio_mem_core::LinuxServiceConfigFile;

    fn tracked(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/config_files")
            .join(name)
    }

    #[test]
    fn generates_the_tracked_linux_guest_configuration_and_refuses_a_windows_guest() {
        let json = service_config_json(&tracked("debian13_settings.toml")).unwrap();
        let config: LinuxServiceConfigFile = serde_json::from_str(&json).unwrap();
        config.validate().unwrap();
        assert_eq!(config.vm_name, "debian13");
        assert_eq!(config.control_path, GuestOs::Linux.control_path());
        assert_eq!((config.min_ram, config.max_ram), (2 << 30, 5 << 30));
        assert!(service_config_json(&tracked("win11_gpu_settings.toml"))
            .unwrap_err()
            .contains("not a Linux guest"));
    }
}
