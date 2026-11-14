//! Identity of a Windows service build, from synchronised source to the
//! executable installed on the guest.
//!
//! `sync` stamps the remote workspace with a [`BuildIdentity`]; `build` compiles
//! its `build_id` into the service (`rampire-guest version`); `fetch`
//! records the verified executables as a [`BuildRecord`]. The installed
//! service is compared with that record by SHA-256, the authority for "which
//! binary is this", with the self-reported build ID as the readable label.

use serde::{Deserialize, Serialize};

/// Remote workspace file holding the identity of the last synchronised source.
pub const STAMP_FILE: &str = "rampire-build.json";
/// Local record of the last fetched, verified candidate.
pub const RECORD_FILE: &str = "build.json";
const RECORD_SCHEMA_VERSION: u32 = 1;
const SHORT_COMMIT_CHARS: usize = 12;
const SHORT_DIGEST_CHARS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    pub build_id: String,
    pub commit: String,
    pub dirty: bool,
    pub source_sha256: String,
}

impl BuildIdentity {
    /// A clean tree is identified by its commit; a dirty tree also by the
    /// digest of the exact files synchronised.
    pub fn new(commit: &str, dirty: bool, source_sha256: &str) -> Result<Self, String> {
        if !is_hex(commit, 40) {
            return Err(format!("expected a full Git commit ID, got {commit:?}"));
        }
        if !is_hex(source_sha256, 64) {
            return Err("source digest must be a SHA-256 value".to_owned());
        }
        let short_commit = &commit[..SHORT_COMMIT_CHARS];
        let build_id = if dirty {
            format!(
                "{short_commit}-dirty.{}",
                &source_sha256[..SHORT_DIGEST_CHARS]
            )
        } else {
            short_commit.to_owned()
        };
        Ok(Self {
            build_id,
            commit: commit.to_owned(),
            dirty,
            source_sha256: source_sha256.to_owned(),
        })
    }

    /// Parses and re-derives a stamp read back from the remote workspace, so
    /// a damaged or hand-edited stamp cannot inject text into the build
    /// command or claim an identity its fields do not support.
    pub fn parse_stamp(text: &str) -> Result<Self, String> {
        let stamp: Self = serde_json::from_str(text.trim())
            .map_err(|error| format!("parse remote build stamp {STAMP_FILE}: {error}"))?;
        let derived = Self::new(&stamp.commit, stamp.dirty, &stamp.source_sha256)?;
        if derived != stamp {
            return Err(format!(
                "remote build stamp {STAMP_FILE} is inconsistent; run windows sync again"
            ));
        }
        Ok(derived)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecord {
    pub schema_version: u32,
    #[serde(flatten)]
    pub identity: BuildIdentity,
    pub service_sha256: String,
    pub workload_sha256: String,
    pub fetched_unix_millis: u64,
}

impl BuildRecord {
    pub fn new(
        identity: BuildIdentity,
        service_sha256: String,
        workload_sha256: String,
        fetched_unix_millis: u64,
    ) -> Self {
        Self {
            schema_version: RECORD_SCHEMA_VERSION,
            identity,
            service_sha256,
            workload_sha256,
            fetched_unix_millis,
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let record: Self = serde_json::from_str(text)
            .map_err(|error| format!("parse build record {RECORD_FILE}: {error}"))?;
        if record.schema_version != RECORD_SCHEMA_VERSION {
            return Err(format!(
                "unsupported build record schema version: {}",
                record.schema_version
            ));
        }
        if BuildIdentity::new(
            &record.identity.commit,
            record.identity.dirty,
            &record.identity.source_sha256,
        )? != record.identity
            || !is_hex(&record.service_sha256, 64)
            || !is_hex(&record.workload_sha256, 64)
        {
            return Err(format!("build record {RECORD_FILE} is inconsistent"));
        }
        Ok(record)
    }
}

/// The service binary the SCM currently points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledBuild {
    pub path: String,
    pub sha256: String,
    /// `None` for binaries that predate `version`, or that did not answer it.
    pub build_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No service registration exists; the candidate can be deployed.
    NotInstalled,
    /// The installed binary is exactly the fetched candidate.
    Current,
    /// The installed binary differs from the fetched candidate.
    UpdateAvailable,
    /// Nothing has been fetched to compare against.
    NoCandidate,
}

impl Verdict {
    pub fn of(installed: Option<&InstalledBuild>, candidate: Option<&BuildRecord>) -> Self {
        match (installed, candidate) {
            (_, None) => Self::NoCandidate,
            (None, Some(_)) => Self::NotInstalled,
            (Some(installed), Some(candidate)) if installed.sha256 == candidate.service_sha256 => {
                Self::Current
            }
            (Some(_), Some(_)) => Self::UpdateAvailable,
        }
    }
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .chars()
            .all(|character| matches!(character, '0'..='9' | 'a'..='f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "c211d98b6ab8c29fb2a2df0f3c3da846538f144c";
    const DIGEST: &str = "1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b";

    fn record(service_sha256: &str) -> BuildRecord {
        BuildRecord::new(
            BuildIdentity::new(COMMIT, false, DIGEST).expect("identity"),
            service_sha256.to_owned(),
            DIGEST.to_owned(),
            1,
        )
    }

    fn installed(sha256: &str) -> InstalledBuild {
        InstalledBuild {
            path: r"C:\Program Files\RAMpire\rampire-guest.exe".to_owned(),
            sha256: sha256.to_owned(),
            build_id: None,
        }
    }

    #[test]
    fn build_id_names_the_commit_and_marks_unsynchronised_edits() {
        assert_eq!(
            BuildIdentity::new(COMMIT, false, DIGEST)
                .expect("clean")
                .build_id,
            "c211d98b6ab8"
        );
        assert_eq!(
            BuildIdentity::new(COMMIT, true, DIGEST)
                .expect("dirty")
                .build_id,
            "c211d98b6ab8-dirty.1a2b3c4d5e6f"
        );
        assert!(BuildIdentity::new("c211d98", false, DIGEST).is_err());
        assert!(BuildIdentity::new(COMMIT, false, "not-a-digest").is_err());
    }

    #[test]
    fn stamps_must_round_trip_and_cannot_carry_another_build_id() {
        let identity = BuildIdentity::new(COMMIT, true, DIGEST).expect("identity");
        let stamp = serde_json::to_string(&identity).expect("encode");
        assert_eq!(BuildIdentity::parse_stamp(&stamp), Ok(identity.clone()));

        let forged = BuildIdentity {
            build_id: "x\" & del *".to_owned(),
            ..identity
        };
        let forged = serde_json::to_string(&forged).expect("encode");
        assert!(BuildIdentity::parse_stamp(&forged).is_err());
        assert!(BuildIdentity::parse_stamp("{}").is_err());
    }

    #[test]
    fn records_round_trip_and_reject_damage() {
        let original = record(DIGEST);
        let text = serde_json::to_string_pretty(&original).expect("encode");
        assert_eq!(BuildRecord::parse(&text), Ok(original.clone()));

        let damaged = BuildRecord {
            service_sha256: "ABC".to_owned(),
            ..original
        };
        let text = serde_json::to_string(&damaged).expect("encode");
        assert!(BuildRecord::parse(&text).is_err());
    }

    #[test]
    fn verdict_compares_the_installed_binary_with_the_fetched_candidate() {
        let candidate = record(DIGEST);
        let other = "f".repeat(64);
        assert_eq!(
            Verdict::of(Some(&installed(DIGEST)), None),
            Verdict::NoCandidate
        );
        assert_eq!(Verdict::of(None, Some(&candidate)), Verdict::NotInstalled);
        assert_eq!(
            Verdict::of(Some(&installed(DIGEST)), Some(&candidate)),
            Verdict::Current
        );
        assert_eq!(
            Verdict::of(Some(&installed(&other)), Some(&candidate)),
            Verdict::UpdateAvailable
        );
    }
}
