//! `rampire check`: validate the configuration and every VM's attestation
//! against the live VM. It reads; it changes nothing.

use std::path::Path;

use rampire_host::attestation::{AttestedCompatibilitySource, CompatibilityAttestation};
use rampire_host::compatibility_source::CompatibilitySource;
use rampire_host::config::{HostConfig, HostSettings};
use rampire_host::virsh::Virsh;

/// What `check` found for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmCheck {
    pub vm_name: String,
    /// `None` when the VM passed.
    pub problem: Option<String>,
}

/// What `check` found for the whole configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub vms: Vec<VmCheck>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.vms.iter().all(|vm| vm.problem.is_none())
    }
}

/// Load the configuration in `directory` and check every VM. A
/// configuration that doesn't load is an error; a VM that fails is a
/// problem in the report, so every VM is checked.
pub fn run(directory: &Path) -> Result<Report, String> {
    let settings = HostSettings::load(directory).map_err(|error| error.to_string())?;
    let vms = settings
        .vms
        .iter()
        .map(|config| VmCheck {
            vm_name: config.vm_name.clone(),
            problem: check_vm(config).err(),
        })
        .collect();
    Ok(Report { vms })
}

fn check_vm(config: &HostConfig) -> Result<(), String> {
    if !domain_exists(config)? {
        return Err(format!(
            "libvirt has no domain {name}; stop managing it with: rampire vm remove {name}",
            name = config.vm_name
        ));
    }
    let path = Path::new(&config.compatibility_attestation_path);
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read attestation {}: {error}", path.display()))?;
    CompatibilityAttestation::parse(&text)?;
    AttestedCompatibilitySource::new(
        Virsh::with_connection(
            &config.virsh_binary,
            config.command_timeout,
            &config.libvirt_uri,
        ),
        &config.vm_name,
        &config.alias,
        config.guest_os,
        path,
    )
    .compatibility()?
    .validate_for_resize()
    .map_err(|error| format!("live compatibility: {error}"))
}

/// Whether libvirt knows the VM. Any other failure to ask is an error.
fn domain_exists(config: &HostConfig) -> Result<bool, String> {
    let output = std::process::Command::new(&config.virsh_binary)
        .args(["-c", &config.libvirt_uri, "domstate", &config.vm_name])
        .output()
        .map_err(|error| format!("run virsh: {error}"))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_missing_domain(&stderr) {
        Ok(false)
    } else {
        Err(format!("ask libvirt about the domain: {}", stderr.trim()))
    }
}

/// libvirt's answer for a name it doesn't know.
fn is_missing_domain(stderr: &str) -> bool {
    stderr.contains("failed to get domain") || stderr.contains("Domain not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_libvirts_answer_for_an_unknown_domain() {
        assert!(is_missing_domain(
            "error: failed to get domain 'gone'
error: Domain not found: no domain with matching name 'gone'"
        ));
        assert!(!is_missing_domain(
            "error: failed to connect to the hypervisor"
        ));
    }

    #[test]
    fn an_empty_configuration_passes() {
        let root = std::env::temp_dir().join(format!("rampire-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vms.d")).unwrap();
        let report = run(&root);
        std::fs::remove_dir_all(&root).unwrap();
        let report = report.expect("loads");
        assert!(report.vms.is_empty());
        assert!(report.passed());
    }

    #[test]
    fn a_vm_that_fails_is_a_problem_not_an_error() {
        // The fixture VM isn't a libvirt domain here and has no attestation:
        // whichever check meets it first, it fails as a problem in the report.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/config");
        let report = run(&fixture).expect("fixture loads");
        assert_eq!(report.vms.len(), 1);
        assert!(!report.passed());
        assert!(report.vms[0].problem.is_some());
    }
}
