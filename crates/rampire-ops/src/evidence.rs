//! What each `rampire apply` and `rampire rollback` did, kept on the host
//! without anyone asking (principle 4): one JSON record per change under
//! [`EVIDENCE_ROOT`].

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Where the records are kept.
pub const EVIDENCE_ROOT: &str = "/var/lib/rampire/evidence";

rampire_core::schema_name! {
    /// The `schema` of an apply or rollback record.
    pub struct ApplyEvidenceSchema = "rampire.apply-evidence";
}

/// One change to the applied configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyEvidence {
    pub schema: ApplyEvidenceSchema,
    pub version: u16,
    /// `apply` or `rollback`.
    pub action: String,
    pub unix_millis: u64,
    /// `rampire --version` of the binary that made the change.
    pub build: String,
    /// The draft applied, for `apply`.
    pub draft: Option<String>,
    /// The copy the controller ran before, if any.
    pub from: Option<String>,
    /// The copy it runs now.
    pub to: String,
    /// Every file of the new copy and its SHA-256.
    pub files: Vec<FileDigest>,
    /// Each VM `check` passed.
    pub checked_vms: Vec<String>,
    /// The controller unit, and whether it was restarted on the new copy.
    pub unit: String,
    pub restarted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDigest {
    pub path: String,
    pub sha256: String,
}

/// The SHA-256 of every file below `root`, relative to it, in name order.
pub fn digests(root: &Path) -> Result<Vec<FileDigest>, String> {
    crate::applied::files_below(root)?
        .into_iter()
        .map(|name| {
            let bytes = std::fs::read(root.join(&name))
                .map_err(|error| format!("read {}: {error}", root.join(&name).display()))?;
            Ok(FileDigest {
                path: name.display().to_string(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            })
        })
        .collect()
}

/// Write `evidence` as a new record in `root`. Returns its path.
pub fn record(root: &Path, evidence: &ApplyEvidence) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|error| format!("create {}: {error}", root.display()))?;
    let mut millis = evidence.unix_millis;
    let path = loop {
        let path = root.join(format!("{millis:016}-{}.json", evidence.action));
        if !path.exists() {
            break path;
        }
        millis += 1;
    };
    let text = serde_json::to_string_pretty(evidence).map_err(|error| error.to_string())?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// Remove all but the newest `keep` records in `root`. Returns what was
/// removed.
pub fn prune(root: &Path, keep: usize) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.map_err(|error| format!("read {}: {error}", root.display()))?,
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", root.display()))?
            .path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.len() > 17
            && name[..16].bytes().all(|b| b.is_ascii_digit())
            && name.ends_with(".json")
        {
            records.push(path);
        }
    }
    records.sort();
    let excess = records.len().saturating_sub(keep);
    let removed: Vec<_> = records.into_iter().take(excess).collect();
    for path in &removed {
        std::fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display()))?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(millis: u64) -> ApplyEvidence {
        ApplyEvidence {
            schema: ApplyEvidenceSchema,
            version: 1,
            action: "apply".to_owned(),
            unix_millis: millis,
            build: "0.1.0 (build test)".to_owned(),
            draft: Some("/etc/rampire".to_owned()),
            from: None,
            to: "/var/lib/rampire/applied/0000000000000001".to_owned(),
            files: vec![],
            checked_vms: vec![],
            unit: "rampire.service".to_owned(),
            restarted: false,
        }
    }

    #[test]
    fn records_are_named_by_time_and_pruned_oldest_first() {
        let root = std::env::temp_dir().join(format!("rampire-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = record(&root, &evidence(5)).unwrap();
        let second = record(&root, &evidence(5)).unwrap();
        let third = record(&root, &evidence(9)).unwrap();
        std::fs::write(root.join("notes.txt"), "kept").unwrap();
        assert_ne!(
            first, second,
            "the same millisecond still gets its own record"
        );
        let text = std::fs::read_to_string(&third).unwrap();
        assert!(text.starts_with("{\n  \"schema\": \"rampire.apply-evidence\",\n  \"version\": 1,"));
        assert_eq!(prune(&root, 2).unwrap(), [first]);
        assert_eq!(prune(&root, 2).unwrap(), Vec::<PathBuf>::new());
        assert!(root.join("notes.txt").exists());
        assert!(second.exists() && third.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
