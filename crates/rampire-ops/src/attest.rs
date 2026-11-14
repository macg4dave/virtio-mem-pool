//! Making attestations (roadmap O3.5, O3.6). An attestation binds a VM's
//! review profile to its live evidence; it lands in the draft's
//! `attestations/` folder, so the next applied copy carries it.
//!
//! A VM with no attestation yet is attested by `apply` when its file names a
//! review profile and it is running. A VM whose live evidence has drifted
//! from its attestation needs a person: `rampire vm attest` (decision D11).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rampire_core::settings::{self, VmSettings, VMS_DIRECTORY};
use rampire_host::attestation::{
    read_review, CompatibilityAttestation, VirshCompatibilityEvidenceSource,
};
use rampire_host::config::{attestation_path, ATTESTATION_DIRECTORY};
use rampire_host::virsh::Virsh;

use crate::applied::REVIEW_DIRECTORY;
use crate::process;

/// What the commands need to reach libvirt.
#[derive(Debug, Clone)]
pub struct Libvirt {
    pub connection: String,
    pub timeout: Duration,
}

impl Libvirt {
    /// Run `virsh` on this connection.
    pub fn virsh(&self, args: &[&str]) -> Result<String, String> {
        let mut arguments = vec![process::os("-c"), process::os(&self.connection)];
        arguments.extend(args.iter().map(process::os));
        process::bounded_text("virsh", &arguments, Path::new("/"), self.timeout)
    }

    /// The VM's state as `virsh domstate` names it: "running", "shut off".
    pub fn state(&self, vm: &str) -> Result<String, String> {
        Ok(self.virsh(&["domstate", vm])?.trim().to_owned())
    }
}

/// The VM's settings file in the draft.
pub fn vm_settings(directory: &Path, vm: &str) -> Result<VmSettings, String> {
    settings::load_vm(
        &directory
            .join(VMS_DIRECTORY)
            .join(settings::vm_filename(vm)?),
    )
}

/// Attest the running VM `vm` from its review profile. Returns where the
/// attestation was written.
pub fn attest(directory: &Path, vm: &str, libvirt: &Libvirt) -> Result<PathBuf, String> {
    let settings = vm_settings(directory, vm)?;
    let profile = settings.review.as_deref().ok_or_else(|| {
        format!("{vm} names no review profile: add review = \"PROFILE\" to its file")
    })?;
    let review = read_review(
        &directory
            .join(REVIEW_DIRECTORY)
            .join(format!("{profile}.toml")),
    )?;
    if review.guest_os != settings.guest_os {
        return Err(format!(
            "review profile {profile} is for a {:?} guest, but {vm} is {:?}",
            review.guest_os, settings.guest_os
        ));
    }
    let state = libvirt.state(vm)?;
    if state != "running" {
        return Err(format!("{vm} is {state}: a VM is attested while it runs"));
    }
    let virsh = Virsh::with_connection("virsh", libvirt.timeout, &libvirt.connection);
    let live = VirshCompatibilityEvidenceSource::new(virsh, vm, &settings.alias).collect()?;
    let document = CompatibilityAttestation::new(live, review)?;
    let path = attestation_path(directory, vm);
    write_atomically(
        &path,
        format!("{}\n", document.to_pretty_json()?).as_bytes(),
    )?;
    Ok(path)
}

/// Attest every VM that has no attestation yet, names a review profile and
/// is running. Returns the VMs attested.
pub fn attest_missing(directory: &Path, libvirt: &Libvirt) -> Result<Vec<String>, String> {
    let (_, vms) = settings::load_configuration(directory)?;
    let mut attested = Vec::new();
    for vm in vms {
        if vm.review.is_none() || attestation_path(directory, &vm.vm_name).exists() {
            continue;
        }
        if libvirt.state(&vm.vm_name).ok().as_deref() != Some("running") {
            continue;
        }
        attest(directory, &vm.vm_name, libvirt)?;
        attested.push(vm.vm_name);
    }
    Ok(attested)
}

/// The VMs whose live evidence no longer matches their attestation, each
/// with what changed. VMs that aren't running can't be compared and are
/// left out.
pub fn drifted(directory: &Path, libvirt: &Libvirt) -> Result<Vec<(String, String)>, String> {
    let (_, vms) = settings::load_configuration(directory)?;
    let mut found = Vec::new();
    for vm in vms {
        let path = attestation_path(directory, &vm.vm_name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if libvirt.state(&vm.vm_name).ok().as_deref() != Some("running") {
            continue;
        }
        let attestation = CompatibilityAttestation::parse(&text)?;
        let virsh = Virsh::with_connection("virsh", libvirt.timeout, &libvirt.connection);
        let live =
            VirshCompatibilityEvidenceSource::new(virsh, &vm.vm_name, &vm.alias).collect()?;
        if let Err(change) = attestation.validate_live(&live, vm.guest_os) {
            found.push((vm.vm_name, change));
        }
    }
    Ok(found)
}

/// Shut the VM down, wait for it to stop, and start it again, so its saved
/// definition takes effect.
pub fn cold_restart(vm: &str, libvirt: &Libvirt, shutdown_timeout: Duration) -> Result<(), String> {
    if libvirt.state(vm)? != "shut off" {
        libvirt.virsh(&["shutdown", vm])?;
        let started = Instant::now();
        while libvirt.state(vm)? != "shut off" {
            if started.elapsed() > shutdown_timeout {
                return Err(format!(
                    "{vm} did not shut down within {} s; nothing more was done",
                    shutdown_timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    libvirt.virsh(&["start", vm])?;
    Ok(())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".attestation-{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("write {}: {error}", path.display()))
}

/// The attestations folder, for messages.
pub fn folder(directory: &Path) -> PathBuf {
    directory.join(ATTESTATION_DIRECTORY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(review: Option<&str>) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rampire-attest-{}-{}",
            review.unwrap_or("none"),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vms.d")).unwrap();
        let review = review
            .map(|name| format!("review = \"{name}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            root.join("vms.d/web.toml"),
            format!("alias = \"m\"\npriority = 1\nguest_os = \"linux\"\nmin_ram = \"1GB\"\nmax_ram = \"4GB\"\nfree_ram = \"512MB\"\n{review}"),
        )
        .unwrap();
        root
    }

    fn unreachable() -> Libvirt {
        Libvirt {
            connection: "test:///no-such-connection".to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn a_vm_without_a_review_profile_cannot_be_attested() {
        let root = configuration(None);
        let error = attest(&root, "web", &unreachable()).unwrap_err();
        let missing = attest_missing(&root, &unreachable());
        std::fs::remove_dir_all(&root).unwrap();
        assert!(error.contains("review profile"), "{error}");
        assert_eq!(missing, Ok(vec![]), "nothing to attest without a profile");
    }

    #[test]
    fn a_missing_profile_is_reported_before_libvirt_is_asked() {
        let root = configuration(Some("absent"));
        let error = attest(&root, "web", &unreachable()).unwrap_err();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(error.contains("absent.toml"), "{error}");
    }
}
