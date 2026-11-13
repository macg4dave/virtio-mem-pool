//! Linux guest tooling: generates the guest service's runtime configuration
//! from the administrator settings, as Windows deployment does for its guest,
//! and builds the guest service stamped with the identity of its source.

use std::path::Path;
use std::process::Command;

use virtio_mem_core::settings;

pub fn run(arguments: &[String], repo: &Path) -> Result<(), String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        ["config", manifest] => {
            println!("{}", service_config_json(&repo.join(manifest))?);
            Ok(())
        }
        ["build"] => build(repo),
        _ => Err("linux requires: config VM_SETTINGS | build".to_owned()),
    }
}

const GUEST_PACKAGE: &str = "virtio-mem-guest-linux";
const GUEST_BINARY: &str = "virtio-mem-guest";

/// Release-build the guest service with the checkout's build identity
/// compiled in, so `virtio-mem-guest version` and every publication name the
/// exact source that produced the binary.
fn build(repo: &Path) -> Result<(), String> {
    let identity = crate::windows::checkout_identity(repo)?;
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked", "-p", GUEST_PACKAGE])
        .env("VIRTIO_MEM_BUILD_ID", &identity.build_id)
        .current_dir(repo)
        .status()
        .map_err(|error| format!("run cargo: {error}"))?;
    if !status.success() {
        return Err(format!("cargo build -p {GUEST_PACKAGE} failed: {status}"));
    }
    let binary = repo.join("target").join("release").join(GUEST_BINARY);
    let reported = Command::new(&binary)
        .arg("version")
        .output()
        .map_err(|error| format!("run {}: {error}", binary.display()))?;
    let reported = String::from_utf8_lossy(&reported.stdout);
    if reported.trim() != identity.build_id {
        return Err(format!(
            "{} reports build {:?}, expected {}",
            binary.display(),
            reported.trim(),
            identity.build_id
        ));
    }
    println!("Built {} as build {}", binary.display(), identity.build_id);
    Ok(())
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
