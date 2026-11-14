//! `rampire set`: change settings in the draft, keeping the file's comments
//! and layout. Nothing is written unless the changed file is valid and the
//! whole configuration still loads.

use std::path::{Path, PathBuf};

use rampire_core::settings::{self, GlobalSettings, VmSettings, GLOBAL_FILE, VMS_DIRECTORY};
use toml_edit::{DocumentMut, Item, Value};

/// What `set` changes: `global.toml`, or one VM's file.
pub const GLOBAL: &str = settings::RESERVED_VM_NAME;

/// One `KEY=VALUE` from the command line.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub key: String,
    pub value: Value,
}

/// Read `KEY=VALUE`. A whole number or `true`/`false` is written as one;
/// anything else, such as `12GB`, as text.
pub fn parse_assignment(text: &str) -> Result<Assignment, String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not KEY=VALUE"))?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!("{key:?} is not a setting name"));
    }
    let value = value.trim();
    let value = if let Ok(number) = value.parse::<i64>() {
        Value::from(number)
    } else if value == "true" || value == "false" {
        Value::from(value == "true")
    } else {
        Value::from(value)
    };
    Ok(Assignment {
        key: key.to_owned(),
        value,
    })
}

/// Apply `assignments` to `target` (a VM's name, or [`GLOBAL`]) in the
/// configuration in `directory`. Returns the file changed.
pub fn set(directory: &Path, target: &str, assignments: &[Assignment]) -> Result<PathBuf, String> {
    if assignments.is_empty() {
        return Err("set needs at least one KEY=VALUE".to_owned());
    }
    let path = if target == GLOBAL {
        directory.join(GLOBAL_FILE)
    } else {
        directory
            .join(VMS_DIRECTORY)
            .join(settings::vm_filename(target)?)
    };
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && target == GLOBAL => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the configuration has no VM {target}: {} doesn't exist",
                path.display()
            ))
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut document: DocumentMut = original
        .as_deref()
        .unwrap_or("")
        .parse()
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    for assignment in assignments {
        match document.get_mut(&assignment.key) {
            Some(Item::Value(value)) => {
                // Keep the comment before the line and anything after it.
                let decor = value.decor().clone();
                *value = assignment.value.clone();
                *value.decor_mut() = decor;
            }
            _ => document[&assignment.key] = Item::Value(assignment.value.clone()),
        }
    }
    let text = document.to_string();
    if target == GLOBAL {
        toml::from_str::<GlobalSettings>(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .validate()?;
    } else {
        let mut vm: VmSettings =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        vm.vm_name = target.to_owned();
        vm.validate()?;
    }
    replace(&path, text.as_bytes())?;
    // The file is valid alone; the configuration must still load as a whole
    // (a pool is needed with several VMs), or the change is undone.
    if let Err(error) = settings::load_configuration(directory) {
        match &original {
            Some(original) => replace(&path, original.as_bytes())?,
            None => std::fs::remove_file(&path)
                .map_err(|remove| format!("{error}; and remove {}: {remove}", path.display()))?,
        }
        return Err(format!("{error}; nothing changed"));
    }
    Ok(path)
}

/// Replace `path` in one step, keeping its owner and mode.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("toml.rampire-set");
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temporary, metadata.permissions())
            .map_err(|error| format!("set mode of {}: {error}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::os::unix::fs::chown(&temporary, Some(metadata.uid()), Some(metadata.gid()))
                .map_err(|error| format!("set owner of {}: {error}", temporary.display()))?;
        }
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rampire-set-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(VMS_DIRECTORY)).unwrap();
        std::fs::write(
            root.join("vms.d/web.toml"),
            "# The web server.\nalias = \"m\"\n# Served first.\npriority = 100 # high\nguest_os = \"linux\"\nmin_ram = \"1GB\"\nmax_ram = \"4GB\"\nfree_ram = \"512MB\"\n",
        )
        .unwrap();
        root
    }

    fn assignments(texts: &[&str]) -> Vec<Assignment> {
        texts
            .iter()
            .map(|text| parse_assignment(text).unwrap())
            .collect()
    }

    #[test]
    fn values_are_numbers_booleans_or_text() {
        assert_eq!(
            parse_assignment("priority=150").unwrap().value.as_integer(),
            Some(150)
        );
        assert_eq!(
            parse_assignment("observe_only=true")
                .unwrap()
                .value
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            parse_assignment("max_ram = 12GB").unwrap().value.as_str(),
            Some("12GB")
        );
        for bad in ["priority", "=1", "Max=1", "a.b=1"] {
            assert!(parse_assignment(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_vm_setting_changes_and_its_comments_stay() {
        let root = configuration("vm");
        let path = set(
            &root,
            "web",
            &assignments(&["priority=150", "max_ram=12GB"]),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(text.contains("# The web server.\n"));
        assert!(
            text.contains("# Served first.\npriority = 150 # high\n"),
            "{text}"
        );
        assert!(text.contains("max_ram = \"12GB\"\n"), "{text}");
    }

    #[test]
    fn an_invalid_change_writes_nothing() {
        let root = configuration("invalid");
        let before = std::fs::read_to_string(root.join("vms.d/web.toml")).unwrap();
        for bad in [
            &["priority=0"][..],
            &["max_ram=nonsense"],
            &["vm_name=web"],
            &["colour=red"],
        ] {
            assert!(set(&root, "web", &assignments(bad)).is_err(), "{bad:?}");
        }
        assert!(set(&root, "other", &assignments(&["priority=1"])).is_err());
        let after = std::fs::read_to_string(root.join("vms.d/web.toml")).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn global_settings_are_created_and_checked_as_a_whole() {
        let root = configuration("global");
        set(&root, GLOBAL, &assignments(&["host_reserve=6GB"])).unwrap();
        let text = std::fs::read_to_string(root.join(GLOBAL_FILE)).unwrap();
        assert_eq!(text, "host_reserve = \"6GB\"\n");
        // With a second VM and no pool the configuration doesn't load, so a
        // change that is valid on its own is undone.
        std::fs::copy(root.join("vms.d/web.toml"), root.join("vms.d/db.toml")).unwrap();
        let error = set(&root, GLOBAL, &assignments(&["host_reserve=8GB"])).unwrap_err();
        assert!(
            error.contains("pool") && error.contains("nothing changed"),
            "{error}"
        );
        let after = std::fs::read_to_string(root.join(GLOBAL_FILE)).unwrap();
        assert_eq!(
            after,
            "host_reserve = \"6GB\"
"
        );
        set(&root, GLOBAL, &assignments(&["pool=8GB"])).expect("a pool lets two VMs load");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
