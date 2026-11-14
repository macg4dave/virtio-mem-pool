//! Process-lifetime ownership. Never unlink a locked file: replacement would
//! allow a second inode to be locked while the first writer is still alive.
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

pub fn state_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("policy state path must be absolute and contain no '..' components".to_owned());
    }
    let mut resolved = PathBuf::new();
    for part in path.components() {
        if part != Component::CurDir {
            resolved.push(part);
        }
    }
    Ok(resolved)
}

pub struct CheckpointOwner(File);

impl CheckpointOwner {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let path = state_path(path)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                return Err("checkpoint cannot be a symbolic link".to_owned());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err("checkpoint cannot have hard-link aliases".to_owned());
                }
            }
        }
        let parent = path.parent().ok_or("policy state path has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create state directory: {error}"))?;
        // Canonicalising the directory makes symlink aliases share the lock.
        let parent = parent.canonicalize().map_err(|error| error.to_string())?;
        let name = path
            .file_name()
            .ok_or("policy state path has no filename")?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join(lock_name))
            .map_err(|error| format!("open checkpoint lock: {error}"))?;
        file.try_lock()
            .map_err(|error| format!("checkpoint already owned or cannot be locked: {error}"))?;
        Ok(Self(file))
    }
}

impl Drop for CheckpointOwner {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Linux abstract sockets provide one machine-wide name across service/root
/// accounts without lock-directory ownership or stale-file recovery problems.
#[cfg(target_os = "linux")]
pub struct DeviceOwner {
    _listener: std::os::unix::net::UnixListener,
}

#[cfg(target_os = "linux")]
impl DeviceOwner {
    pub fn for_domain(command: &impl crate::virsh::VirshCommand, vm: &str) -> Result<Self, String> {
        let uuid = command
            .run(&["domuuid".to_owned(), vm.to_owned()])
            .map_err(|error| error.to_string())?;
        let uuid = uuid.trim();
        if uuid.len() != 36
            || !uuid.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        {
            return Err("libvirt returned an invalid domain UUID".to_owned());
        }
        Self::acquire(&uuid.to_ascii_lowercase())
    }

    pub fn acquire(vm: &str) -> Result<Self, String> {
        use sha2::{Digest, Sha256};
        use std::os::linux::net::SocketAddrExt;
        let name = format!("rampire-{:x}", Sha256::digest(vm.as_bytes()));
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .map_err(|error| error.to_string())?;
        std::os::unix::net::UnixListener::bind_addr(&address)
            .map(|listener| Self {
                _listener: listener,
            })
            .map_err(|error| format!("VM {vm} already owned or cannot be locked: {error}"))
    }
}

#[cfg(not(target_os = "linux"))]
pub struct DeviceOwner;
#[cfg(not(target_os = "linux"))]
impl DeviceOwner {
    pub fn for_domain(_: &impl crate::virsh::VirshCommand, _: &str) -> Result<Self, String> {
        Err("controller ownership requires Linux".to_owned())
    }
    pub fn acquire(_: &str) -> Result<Self, String> {
        Err("controller ownership requires Linux".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_aliases_contend_and_release_on_drop() {
        let root = std::env::temp_dir().join(format!("virtio-owner-{}", std::process::id()));
        let path = root.join("state.json");
        let first = CheckpointOwner::acquire(&path).unwrap();
        assert!(CheckpointOwner::acquire(&root.join("./state.json")).is_err());
        #[cfg(unix)]
        {
            let alias = root.with_extension("alias");
            let _ = std::fs::remove_file(&alias);
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert!(CheckpointOwner::acquire(&alias.join("state.json")).is_err());
            std::fs::remove_file(alias).unwrap();
        }
        drop(first);
        assert!(CheckpointOwner::acquire(&path).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn locks_are_exclusive_across_processes() {
        const PATH_ENV: &str = "RAMPIRE_OWNERSHIP_FIXTURE_PATH";
        const VM_ENV: &str = "RAMPIRE_OWNERSHIP_FIXTURE_VM";
        if let Some(path) = std::env::var_os(PATH_ENV) {
            assert!(CheckpointOwner::acquire(Path::new(&path)).is_err());
            assert!(DeviceOwner::acquire(&std::env::var(VM_ENV).unwrap()).is_err());
            return;
        }
        let root =
            std::env::temp_dir().join(format!("virtio-owner-process-{}", std::process::id()));
        let path = root.join("state.json");
        let vm = format!("process-lock-{}", std::process::id());
        let checkpoint = CheckpointOwner::acquire(&path).unwrap();
        let device = DeviceOwner::acquire(&vm).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ownership::tests::locks_are_exclusive_across_processes",
            ])
            .env(PATH_ENV, &path)
            .env(VM_ENV, &vm)
            .status()
            .unwrap();
        assert!(status.success());
        drop((checkpoint, device));
        assert!(CheckpointOwner::acquire(&path).is_ok());
        assert!(DeviceOwner::acquire(&vm).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn different_configs_for_the_same_vm_contend() {
        let vm = format!("ownership-test-{}", std::process::id());
        let first = DeviceOwner::acquire(&vm).unwrap();
        assert!(DeviceOwner::acquire(&vm).is_err());
        drop(first);
        assert!(DeviceOwner::acquire(&vm).is_ok());
    }
}
