//! `rampire doctor`: check what the host needs for RAMpire, and say how to
//! fix each problem found. It reads; it changes nothing.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::process;

/// libvirt's first release with `dynamicMemslots` for virtio-mem.
pub const MIN_LIBVIRT: (u32, u32) = (10, 1);
/// QEMU's first release with `dynamic-memslots` on virtio-mem-pci.
pub const MIN_QEMU: (u32, u32) = (8, 2);
/// The virtio-mem device options attestation requires.
pub const DEVICE_OPTIONS: [&str; 2] = ["dynamic-memslots", "unplugged-inaccessible"];
/// Where the installed unit may be.
const UNIT_PATHS: [&str; 2] = [
    "/etc/systemd/system/rampire.service",
    "/usr/lib/systemd/system/rampire.service",
];

/// A version as major, minor and micro.
pub type Version = (u32, u32, u32);

/// One thing `doctor` checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub check: String,
    pub ok: bool,
    /// What was found.
    pub detail: String,
    /// How to fix it, when it isn't ok.
    pub fix: Option<String>,
}

impl Finding {
    fn ok(check: &str, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            ok: true,
            detail: detail.into(),
            fix: None,
        }
    }

    fn problem(check: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            ok: false,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Run every check against libvirt at `connection`, for the service
/// account `account`, with the applied copies in `applied_root`.
pub fn run(connection: &str, account: &str, applied_root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let virsh = |args: &[&str]| {
        let mut arguments = vec![process::os("-c"), process::os(connection)];
        arguments.extend(args.iter().map(process::os));
        process::bounded_text("virsh", &arguments, Path::new("/"), TIMEOUT)
    };

    match virsh(&["version"]) {
        Err(error) => findings.push(Finding::problem(
            "libvirt",
            format!("virsh could not reach {connection}: {error}"),
            "install libvirt's client and daemon, and start libvirt (virtqemud or libvirtd)",
        )),
        Ok(text) => {
            let (libvirt, qemu) = versions(&text);
            findings.push(minimum(
                "libvirt",
                libvirt,
                MIN_LIBVIRT,
                "upgrade libvirt; RAMpire needs dynamicMemslots for virtio-mem",
            ));
            findings.push(minimum(
                "QEMU",
                qemu,
                MIN_QEMU,
                "upgrade QEMU; RAMpire needs dynamic-memslots on virtio-mem-pci",
            ));
        }
    }

    findings.push(
        match virsh(&["domcapabilities"]).map(|xml| emulator(&xml)) {
            Ok(Some(path)) => device_options(&path),
            Ok(None) => Finding::problem(
                "virtio-mem device",
                "libvirt's domain capabilities name no emulator",
                "install QEMU for this host's architecture",
            ),
            Err(error) => Finding::problem(
                "virtio-mem device",
                format!("read libvirt's domain capabilities: {error}"),
                "check that libvirt can run QEMU guests",
            ),
        },
    );

    let account_exists = getent("passwd", account).is_ok();
    findings.push(if account_exists {
        Finding::ok("service account", format!("{account} exists"))
    } else {
        Finding::problem(
            "service account",
            format!("there is no user {account}"),
            format!("useradd --system --no-create-home --shell /sbin/nologin {account}"),
        )
    });

    findings.push(socket_groups());

    if account_exists {
        findings.push(management_access(connection, account));
    }

    let current = applied_root.join(crate::applied::CURRENT);
    findings.push(if current.exists() {
        Finding::ok(
            "applied configuration",
            format!("{} exists", current.display()),
        )
    } else {
        Finding::problem(
            "applied configuration",
            "nothing has been applied, so the controller has nothing to run",
            "rampire init on a new host, or rampire apply",
        )
    });
    findings
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn minimum(check: &str, found: Option<Version>, needed: (u32, u32), fix: &str) -> Finding {
    match found {
        None => Finding::problem(check, "its version is unknown", fix),
        Some(version) if (version.0, version.1) >= needed => {
            Finding::ok(check, format!("{}.{}.{}", version.0, version.1, version.2))
        }
        Some(version) => Finding::problem(
            check,
            format!(
                "{}.{}.{} is older than {}.{}",
                version.0, version.1, version.2, needed.0, needed.1
            ),
            fix,
        ),
    }
}

/// libvirt's and QEMU's versions from `virsh version`.
pub fn versions(text: &str) -> (Option<Version>, Option<Version>) {
    let mut libvirt = None;
    let mut qemu = None;
    for line in text.lines() {
        let version = line.split_whitespace().last().and_then(parse_version);
        // The daemon's version, or the library's when there is no daemon line.
        if line.starts_with("Running against daemon")
            || (line.starts_with("Using library") && libvirt.is_none())
        {
            libvirt = version;
        } else if line.starts_with("Running hypervisor") && line.contains("QEMU") {
            qemu = version;
        }
    }
    (libvirt, qemu)
}

fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.split('.').map(|part| part.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let micro = parts.next().flatten().unwrap_or(0);
    Some((major, minor, micro))
}

/// The emulator in libvirt's domain capabilities.
pub fn emulator(xml: &str) -> Option<String> {
    let start = xml.find("<path>")? + "<path>".len();
    let end = xml[start..].find("</path>")? + start;
    Some(xml[start..end].trim().to_owned())
}

fn device_options(emulator: &str) -> Finding {
    let check = "virtio-mem device";
    let help = process::bounded_text(
        emulator,
        &[process::os("-device"), process::os("virtio-mem-pci,help")],
        Path::new("/"),
        TIMEOUT,
    );
    match help {
        Err(error) => Finding::problem(
            check,
            format!("{emulator} could not describe virtio-mem-pci: {error}"),
            "install a QEMU with virtio-mem support",
        ),
        Ok(text) => {
            let missing: Vec<_> = DEVICE_OPTIONS
                .iter()
                .filter(|option| !text.contains(*option))
                .collect();
            if missing.is_empty() {
                Finding::ok(
                    check,
                    format!(
                        "{emulator} has virtio-mem-pci with {}",
                        DEVICE_OPTIONS.join(" and ")
                    ),
                )
            } else {
                Finding::problem(
                    check,
                    format!("{emulator}'s virtio-mem-pci lacks {missing:?}"),
                    "upgrade QEMU to 8.2 or later",
                )
            }
        }
    }
}

/// The groups the unit adds for libvirt's channel sockets.
pub fn supplementary_groups(unit: &str) -> Vec<String> {
    unit.lines()
        .filter_map(|line| line.trim().strip_prefix("SupplementaryGroups="))
        .flat_map(|groups| groups.split_whitespace().map(str::to_owned))
        .collect()
}

fn socket_groups() -> Finding {
    let check = "socket group";
    let Some(unit) = UNIT_PATHS
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
    else {
        return Finding::problem(
            check,
            "rampire.service is not installed",
            "install RAMpire (its package, or a development build)",
        );
    };
    let groups = supplementary_groups(&unit);
    let missing: Vec<_> = groups
        .iter()
        .filter(|group| getent("group", group).is_err())
        .collect();
    if groups.is_empty() {
        Finding::problem(
            check,
            "rampire.service adds no group for libvirt's channel sockets",
            "set SupplementaryGroups= to the group that owns them: qemu on the RHEL family, libvirt-qemu on Debian and Ubuntu",
        )
    } else if missing.is_empty() {
        Finding::ok(check, format!("rampire.service adds {}", groups.join(" ")))
    } else {
        Finding::problem(
            check,
            format!("rampire.service adds {missing:?}, which this host doesn't have"),
            "override SupplementaryGroups= in a drop-in (systemctl edit rampire): qemu on the RHEL family, libvirt-qemu on Debian and Ubuntu",
        )
    }
}

fn management_access(connection: &str, account: &str) -> Finding {
    let check = "libvirt access";
    let result = process::bounded_text(
        "runuser",
        &[
            process::os("-u"),
            process::os(account),
            process::os("--"),
            process::os("virsh"),
            process::os("-c"),
            process::os(connection),
            process::os("list"),
            process::os("--all"),
            process::os("--name"),
        ],
        Path::new("/"),
        TIMEOUT,
    );
    match result {
        Ok(_) => Finding::ok(check, format!("{account} manages {connection} without a password")),
        Err(error) => Finding::problem(
            check,
            format!("{account} cannot manage {connection}: {error}"),
            "install the polkit rule granting it org.libvirt.unix.manage (50-rampire.rules in /etc/polkit-1/rules.d)",
        ),
    }
}

fn getent(database: &str, key: &str) -> Result<String, String> {
    process::bounded_text(
        "getent",
        &[process::os(database), process::os(key)],
        Path::new("/"),
        TIMEOUT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_libvirt_and_qemu_versions() {
        let text = "Compiled against library: libvirt 11.10.0\nUsing library: libvirt 11.10.0\n\
                    Using API: QEMU 11.10.0\nRunning hypervisor: QEMU 10.1.0\n\
                    Running against daemon: 11.10.0\n";
        assert_eq!(versions(text), (Some((11, 10, 0)), Some((10, 1, 0))));
        assert_eq!(
            versions("Using library: libvirt 9.0.0\n"),
            (Some((9, 0, 0)), None)
        );
        let old = minimum("QEMU", Some((8, 1, 5)), MIN_QEMU, "upgrade");
        assert!(!old.ok && old.detail.contains("8.1.5"));
        assert!(minimum("QEMU", Some((8, 2, 0)), MIN_QEMU, "upgrade").ok);
        assert!(!minimum("QEMU", None, MIN_QEMU, "upgrade").ok);
    }

    #[test]
    fn reads_the_emulator_and_the_unit_groups() {
        let xml =
            "<domainCapabilities>\n  <path>/usr/libexec/qemu-kvm</path>\n</domainCapabilities>";
        assert_eq!(emulator(xml).as_deref(), Some("/usr/libexec/qemu-kvm"));
        assert_eq!(emulator("<domainCapabilities/>"), None);
        let unit = "[Service]\nUser=rampire\nSupplementaryGroups=qemu kvm\n";
        assert_eq!(supplementary_groups(unit), ["qemu", "kvm"]);
        assert!(supplementary_groups("[Service]\n").is_empty());
    }
}
