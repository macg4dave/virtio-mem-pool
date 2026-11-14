//! The documents `rampire COMMAND --json` writes (roadmap O4.1). Each names
//! its schema and version; docs/json.md describes them, and the tests below
//! hold each to its example in `fixtures/json/`.

use serde::{Deserialize, Serialize};

use rampire_core::ControllerStatusSnapshot;

use crate::doctor::Finding;

/// The version of every report below.
pub const REPORT_VERSION: u16 = 1;

rampire_core::schema_name! {
    /// `rampire status --json`.
    pub struct StatusReportSchema = "rampire.status-report";
}

rampire_core::schema_name! {
    /// `rampire check --json`.
    pub struct CheckSchema = "rampire.check";
}

rampire_core::schema_name! {
    /// `rampire doctor --json`.
    pub struct DoctorSchema = "rampire.doctor";
}

/// `rampire status --json`: every VM's controller status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub schema: StatusReportSchema,
    pub version: u16,
    pub vms: Vec<ControllerStatusSnapshot>,
}

/// `rampire check --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    pub schema: CheckSchema,
    pub version: u16,
    pub passed: bool,
    pub vms: Vec<CheckedVm>,
}

/// One VM in a [`CheckReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckedVm {
    pub vm_name: String,
    /// `None` when the VM passed.
    pub problem: Option<String>,
}

/// `rampire doctor --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorReport {
    pub schema: DoctorSchema,
    pub version: u16,
    pub passed: bool,
    pub findings: Vec<Finding>,
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rampire_core::{
        CommandOwnership, ControlHealth, GuestInputStatus, RecoveryState, StatusSchema,
        CONTROLLER_STATUS_VERSION,
    };
    use serde::de::DeserializeOwned;

    use super::*;

    const GIB: u64 = 1 << 30;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/json")
    }

    fn status_report() -> StatusReport {
        StatusReport {
            schema: StatusReportSchema,
            version: REPORT_VERSION,
            vms: vec![ControllerStatusSnapshot {
                schema: StatusSchema,
                version: CONTROLLER_STATUS_VERSION,
                observed_unix_millis: 1_794_600_000_000,
                vm_name: "web".to_owned(),
                device_alias: "ua-virtiomem0".to_owned(),
                device_size_bytes: 15 * GIB,
                block_size_bytes: 2 << 20,
                maximum_target_bytes: 15 * GIB,
                requested_bytes: GIB,
                current_bytes: GIB,
                observe_only: false,
                guest_input: GuestInputStatus {
                    error: None,
                    observed_unix_millis: Some(1_794_599_999_500),
                    request: None,
                },
                admitted_request: None,
                result: None,
                command_ownership: CommandOwnership::None,
                command: None,
                control_health: ControlHealth::Converged,
                actuation_latched: false,
                latch_reason: None,
                recovery_state: RecoveryState::NotRequired,
                recovery_reason: None,
                last_latch_clear_reason: None,
                compatibility_fingerprint_sha256: "e".repeat(64),
            }],
        }
    }

    fn check_report() -> CheckReport {
        CheckReport {
            schema: CheckSchema,
            version: REPORT_VERSION,
            passed: false,
            vms: vec![
                CheckedVm {
                    vm_name: "web".to_owned(),
                    problem: None,
                },
                CheckedVm {
                    vm_name: "db".to_owned(),
                    problem: Some("live compatibility: drifted: domain_xml_sha256".to_owned()),
                },
            ],
        }
    }

    fn doctor_report() -> DoctorReport {
        DoctorReport {
            schema: DoctorSchema,
            version: REPORT_VERSION,
            passed: false,
            findings: vec![
                Finding {
                    check: "QEMU".to_owned(),
                    ok: true,
                    detail: "10.1.0".to_owned(),
                    fix: None,
                },
                Finding {
                    check: "service account".to_owned(),
                    ok: false,
                    detail: "there is no user rampire".to_owned(),
                    fix: Some(
                        "useradd --system --no-create-home --shell /sbin/nologin rampire"
                            .to_owned(),
                    ),
                },
            ],
        }
    }

    fn json<T: Serialize>(document: &T) -> String {
        format!("{}\n", serde_json::to_string_pretty(document).unwrap())
    }

    fn matches_fixture<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
        name: &str,
        document: &T,
    ) {
        let path = fixtures().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}; write it with --ignored", path.display()));
        assert_eq!(
            text,
            json(document),
            "{name} changed: update docs/json.md too"
        );
        assert_eq!(&serde_json::from_str::<T>(&text).unwrap(), document);
    }

    #[test]
    fn every_report_matches_its_documented_example() {
        matches_fixture("status-report.json", &status_report());
        matches_fixture("check.json", &check_report());
        matches_fixture("doctor.json", &doctor_report());
    }

    #[test]
    fn a_report_of_another_schema_is_refused() {
        let text = json(&check_report()).replace("rampire.check", "rampire.doctor");
        assert!(serde_json::from_str::<CheckReport>(&text).is_err());
    }

    /// Write the examples after a deliberate change to a report.
    #[test]
    #[ignore]
    fn write_the_examples() {
        std::fs::create_dir_all(fixtures()).unwrap();
        for (name, text) in [
            ("status-report.json", json(&status_report())),
            ("check.json", json(&check_report())),
            ("doctor.json", json(&doctor_report())),
        ] {
            std::fs::write(fixtures().join(name), text).unwrap();
        }
    }
}
