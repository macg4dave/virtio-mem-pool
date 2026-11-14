//! The applied configuration (decision D10). `/etc/rampire` is a draft that
//! people and tools edit; the controller runs only a copy that
//! `rampire apply` checked. Each applied copy is a snapshot directory under
//! [`APPLIED_ROOT`] that is never changed once written, and the `current`
//! link names the one the controller runs.

use std::path::{Path, PathBuf};

use rampire_core::settings::{self, GLOBAL_FILE, VMS_DIRECTORY};
use rampire_host::config::ATTESTATION_DIRECTORY;

/// Where applied copies are kept.
pub const APPLIED_ROOT: &str = "/var/lib/rampire/applied";
/// The link, inside [`APPLIED_ROOT`], to the copy the controller runs.
pub const CURRENT: &str = "current";
/// Review profiles, inside a configuration directory.
pub const REVIEW_DIRECTORY: &str = "reviews";
/// Written in place of a missing `global.toml`, so a copy is complete.
const DEFAULT_GLOBALS: &[u8] = b"# Every server-wide setting is at its built-in default.\n";

/// The files of the configuration in `directory` that an applied copy
/// holds, relative to it: `global.toml`, each `vms.d/NAME.toml`, each VM's
/// attestation and the review profiles the VMs name. The configuration
/// must load.
pub fn configuration_files(directory: &Path) -> Result<Vec<String>, String> {
    let (_, vms) = settings::load_configuration(directory)?;
    let mut files = vec![GLOBAL_FILE.to_owned()];
    let mut reviews = Vec::new();
    for vm in &vms {
        files.push(format!(
            "{VMS_DIRECTORY}/{}",
            settings::vm_filename(&vm.vm_name)?
        ));
        files.push(format!(
            "{ATTESTATION_DIRECTORY}/{}.attestation.json",
            vm.vm_name
        ));
        if let Some(review) = &vm.review {
            let file = format!("{REVIEW_DIRECTORY}/{review}.toml");
            if !reviews.contains(&file) {
                reviews.push(file);
            }
        }
    }
    files.extend(reviews);
    Ok(files)
}

/// Copy the configuration in `source` into the new directory `target`, byte
/// for byte, so comments and sizes stay as written.
pub fn snapshot(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }
    let files = configuration_files(source)?;
    create_dir(&target.join(VMS_DIRECTORY))?;
    for name in &files {
        let from = source.join(name);
        let to = target.join(name);
        if let Some(parent) = to.parent() {
            create_dir(parent)?;
        }
        if name == GLOBAL_FILE && !from.exists() {
            write(&to, DEFAULT_GLOBALS)?;
            continue;
        }
        let bytes =
            std::fs::read(&from).map_err(|error| format!("read {}: {error}", from.display()))?;
        write(&to, &bytes)?;
    }
    Ok(())
}

/// Whether two configuration directories hold the same files with the
/// same bytes.
pub fn same(a: &Path, b: &Path) -> Result<bool, String> {
    let files = files_below(a)?;
    if files != files_below(b)? {
        return Ok(false);
    }
    for name in &files {
        let read = |root: &Path| {
            std::fs::read(root.join(name))
                .map_err(|error| format!("read {}: {error}", root.join(name).display()))
        };
        if read(a)? != read(b)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every regular file below `root`, relative to it, sorted.
fn files_below(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
            let kind = entry
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
            let name = relative.join(entry.file_name());
            if kind.is_dir() {
                pending.push(name);
            } else {
                found.push(name);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The applied copy the controller runs, or `None` before the first apply.
pub fn current(root: &Path) -> Result<Option<PathBuf>, String> {
    let link = root.join(CURRENT);
    match std::fs::read_link(&link) {
        Ok(target) => Ok(Some(root.join(target))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", link.display())),
    }
}

/// Every applied copy, oldest first.
pub fn copies(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.map_err(|error| format!("read {}: {error}", root.display()))?,
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", root.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_copy_name(&name) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Applied copies are named by the time they were made, zero-padded so that
/// name order is time order.
fn is_copy_name(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_digit())
}

/// A new, empty place to stage a copy in, inside `root`.
pub fn staging(root: &Path) -> Result<PathBuf, String> {
    create_dir(root)?;
    let staging = root.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| format!("remove {}: {error}", staging.display()))?;
    }
    Ok(staging)
}

/// Keep the staged copy as a new applied copy, named by `unix_millis`, and
/// make it current. Returns where it now is.
pub fn install(root: &Path, staged: &Path, unix_millis: u64) -> Result<PathBuf, String> {
    let mut millis = unix_millis;
    let target = loop {
        let target = root.join(format!("{millis:016}"));
        if !target.exists() {
            break target;
        }
        millis += 1;
    };
    sync_tree(staged)?;
    std::fs::rename(staged, &target)
        .map_err(|error| format!("keep {}: {error}", target.display()))?;
    make_current(root, &target)?;
    Ok(target)
}

/// Point `current` at `target` in one step: a reader sees the old copy or
/// the new one, never neither.
pub fn make_current(root: &Path, target: &Path) -> Result<(), String> {
    let name = target
        .file_name()
        .ok_or_else(|| format!("{} has no name", target.display()))?;
    let next = root.join(format!(".{CURRENT}.next"));
    let _ = std::fs::remove_file(&next);
    symlink(Path::new(name), &next)?;
    std::fs::rename(&next, root.join(CURRENT))
        .map_err(|error| format!("switch {}: {error}", root.join(CURRENT).display()))?;
    sync_directory(root)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|error| format!("link {}: {error}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<(), String> {
    Err(format!(
        "link {}: applied copies need a Unix host",
        link.display()
    ))
}

fn create_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))
}

/// Flush every file and directory of a staged copy before it is kept.
fn sync_tree(root: &Path) -> Result<(), String> {
    for name in files_below(root)? {
        let path = root.join(name);
        std::fs::File::open(&path)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("flush {}: {error}", path.display()))?;
    }
    for directory in [root.join(VMS_DIRECTORY), root.to_path_buf()] {
        sync_directory(&directory)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("flush {}: {error}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rampire-applied-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn draft(root: &Path) -> PathBuf {
        let draft = root.join("draft");
        std::fs::create_dir_all(draft.join("vms.d")).unwrap();
        std::fs::create_dir_all(draft.join("attestations")).unwrap();
        std::fs::create_dir_all(draft.join("reviews")).unwrap();
        std::fs::write(
            draft.join("vms.d/web.toml"),
            "# Web.\nalias = \"m\"\npriority = 1\nguest_os = \"linux\"\nmin_ram = \"1GB\"\nmax_ram = \"4GB\"\nfree_ram = \"512MB\"\nreview = \"std\"\n",
        )
        .unwrap();
        std::fs::write(draft.join("vms.d/web.toml~"), "editor backup").unwrap();
        std::fs::write(draft.join("attestations/web.attestation.json"), "{}").unwrap();
        std::fs::write(draft.join("reviews/std.toml"), "version = 1\n").unwrap();
        std::fs::write(draft.join("reviews/unused.toml"), "version = 1\n").unwrap();
        draft
    }

    #[test]
    fn a_snapshot_holds_exactly_the_configuration() {
        let root = temp("snapshot");
        let draft = draft(&root);
        let copy = root.join("copy");
        snapshot(&draft, &copy).unwrap();
        let files = files_below(&copy).unwrap();
        assert_eq!(
            files,
            [
                "attestations/web.attestation.json",
                "global.toml",
                "reviews/std.toml",
                "vms.d/web.toml"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            std::fs::read(copy.join("global.toml")).unwrap(),
            DEFAULT_GLOBALS
        );
        assert!(
            snapshot(&draft, &copy).is_err(),
            "a copy is never overwritten"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_makes_the_copy_current_and_keeps_the_last() {
        let root = temp("install");
        let draft = draft(&root);
        let applied = root.join("applied");
        assert_eq!(current(&applied).unwrap(), None);

        let first = staging(&applied).unwrap();
        snapshot(&draft, &first).unwrap();
        let first = install(&applied, &first, 1_000).unwrap();
        assert_eq!(current(&applied).unwrap(), Some(first.clone()));

        let second = staging(&applied).unwrap();
        snapshot(&draft, &second).unwrap();
        assert!(same(&first, &second).unwrap());
        std::fs::write(second.join("global.toml"), "pool = \"8GB\"\n").unwrap();
        assert!(!same(&first, &second).unwrap());
        // The same millisecond again still gets its own name.
        let second = install(&applied, &second, 1_000).unwrap();
        assert_ne!(first, second);
        assert_eq!(copies(&applied).unwrap(), [first.clone(), second.clone()]);
        assert_eq!(current(&applied).unwrap(), Some(second));

        make_current(&applied, &first).unwrap();
        assert_eq!(current(&applied).unwrap(), Some(first));
        std::fs::remove_dir_all(root).unwrap();
    }
}
