//! Linux guest tooling: builds the guest service stamped with the identity
//! of its source, and installs, inspects and removes it on a development
//! guest over SSH, recording what it did. A Linux guest has no generated
//! configuration; the host publishes its limits and policy.
//!
//! The guest is named by an SSH destination the caller supplies, so nothing
//! here knows any site's machines. The remote account needs passwordless
//! `sudo`. `package` builds the Debian package production installs use.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::process;

const GUEST_PACKAGE: &str = "rampire-guest-linux";
const GUEST_BINARY: &str = "rampire-guest";
const UNIT_SOURCE: &str = "guest-linux/systemd/rampire-guest.service";
const MONITOR_SOURCE: &str = "tools/linux/watch-ram.sh";

const REMOTE_BINARY: &str = "/usr/local/libexec/rampire-guest";
const REMOTE_UNIT: &str = "/etc/systemd/system/rampire-guest.service";
const REMOTE_MONITOR: &str = "/usr/local/bin/rampire-watch-ram";
const WORKLOAD_BINARY: &str = "rampire-workload";
const REMOTE_WORKLOAD: &str = "/usr/local/bin/rampire-workload";
const UNIT_NAME: &str = "rampire-guest.service";
/// The service's state directory; with `DynamicUser=` systemd keeps it under
/// `/var/lib/private` and links it here.
const REMOTE_STATE: &str = "/var/lib/rampire-guest";
const REMOTE_PRIVATE_STATE: &str = "/var/lib/private/rampire-guest";
const EVIDENCE_SCHEMA_VERSION: u32 = 1;

const HELP: &str = "linux requires one of:
  build
  package --output DIRECTORY
  deploy SSH_DESTINATION --output PATH [--apply]
  status SSH_DESTINATION
  uninstall SSH_DESTINATION --output PATH [--apply] [--discard-state]";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Build,
    Package {
        output: String,
    },
    Deploy {
        target: String,
        output: String,
        apply: bool,
    },
    Status {
        target: String,
    },
    Uninstall {
        target: String,
        output: String,
        apply: bool,
        discard_state: bool,
    },
}

fn parse(arguments: &[&str]) -> Result<Action, String> {
    let (command, target, options) = match arguments {
        ["build"] => return Ok(Action::Build),
        ["package", "--output", output] if !output.is_empty() => {
            return Ok(Action::Package {
                output: (*output).to_owned(),
            })
        }
        [command @ ("deploy" | "status" | "uninstall"), target, options @ ..]
            if valid_destination(target) =>
        {
            (*command, (*target).to_owned(), options)
        }
        _ => return Err(HELP.to_owned()),
    };
    let mut output = None;
    let mut apply = false;
    let mut discard_state = false;
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        match (*option, command) {
            ("--output", "deploy" | "uninstall") if output.is_none() => {
                output = Some(
                    rest.next()
                        .filter(|path| !path.is_empty())
                        .ok_or("--output needs a path")?
                        .to_string(),
                );
            }
            ("--apply", "deploy" | "uninstall") if !apply => apply = true,
            ("--discard-state", "uninstall") if !discard_state => discard_state = true,
            _ => return Err(HELP.to_owned()),
        }
    }
    match command {
        "status" => Ok(Action::Status { target }),
        "deploy" => Ok(Action::Deploy {
            target,
            output: output.ok_or("deploy requires --output PATH")?,
            apply,
        }),
        _ => Ok(Action::Uninstall {
            target,
            output: output.ok_or("uninstall requires --output PATH")?,
            apply,
            discard_state,
        }),
    }
}

/// An SSH destination as `ssh` takes it, refusing anything `ssh` could read
/// as an option or a shell could split.
fn valid_destination(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('-')
        && target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-@:[]".contains(&byte))
}

pub fn run(arguments: &[String], repo: &Path) -> Result<(), String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match parse(&arguments)? {
        Action::Build => build(repo).map(|_| ()),
        Action::Package { output } => package(&repo.join(output), repo),
        Action::Status { target } => {
            let installed = remote_status(&target, repo)?;
            for (key, value) in &installed {
                println!("{key}: {value}");
            }
            Ok(())
        }
        Action::Deploy {
            target,
            output,
            apply,
        } => deploy(&target, &repo.join(output), apply, repo),
        Action::Uninstall {
            target,
            output,
            apply,
            discard_state,
        } => uninstall(&target, &repo.join(output), apply, discard_state, repo),
    }
}

/// Release-build the guest service with the checkout's build identity
/// compiled in, so `rampire-guest version` and every publication name the
/// exact source that produced the binary. Returns that build identity.
fn build(repo: &Path) -> Result<String, String> {
    let identity = crate::windows::checkout_identity(repo)?;
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked", "-p", GUEST_PACKAGE])
        .env("RAMPIRE_BUILD_ID", &identity.build_id)
        .current_dir(repo)
        .status()
        .map_err(|error| format!("run cargo: {error}"))?;
    if !status.success() {
        return Err(format!("cargo build -p {GUEST_PACKAGE} failed: {status}"));
    }
    let binary = release_binary(repo, GUEST_BINARY);
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
    Ok(identity.build_id)
}

/// Build the stamped guest and its Debian package in `output`.
fn package(output: &Path, repo: &Path) -> Result<(), String> {
    let build_id = build(repo)?;
    let binary = read_binary(repo, GUEST_BINARY)?;
    let unit = read_source(repo, UNIT_SOURCE)?;
    let monitor = unix_text(&read_source(repo, MONITOR_SOURCE)?);
    std::fs::create_dir_all(output)
        .map_err(|error| format!("create {}: {error}", output.display()))?;
    let path = crate::debian::build(
        &crate::debian::Contents {
            build_id: &build_id,
            cargo_version: env!("CARGO_PKG_VERSION"),
            architecture: crate::debian::architecture()?,
            maintainer: &maintainer(repo)?,
            binary: &binary,
            unit: &unit,
            monitor: &monitor,
        },
        output,
        repo,
    )?;
    let bytes =
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    println!(
        "Built {} (build {build_id}, sha256 {})",
        path.display(),
        sha256(&bytes, repo)?
    );
    Ok(())
}

/// Who built the package: `DEBFULLNAME` and `DEBEMAIL` as Debian tools take
/// them, else the checkout's Git identity.
fn maintainer(repo: &Path) -> Result<String, String> {
    let setting = |variable: &str, git_key: &str| -> Result<String, String> {
        match std::env::var(variable) {
            Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
            _ => process::checked_text("git", &[process::os("config"), process::os(git_key)], repo)
                .map(|value| value.trim().to_owned())
                .map_err(|_| {
                    format!("set {variable} or git {git_key} to name the package's maintainer")
                }),
        }
    };
    let name = setting("DEBFULLNAME", "user.name")?;
    let email = setting("DEBEMAIL", "user.email")?;
    if [&name, &email]
        .iter()
        .any(|value| value.contains(['<', '>', '\n']))
    {
        return Err("maintainer name and email must not contain <, > or newlines".to_owned());
    }
    Ok(format!("{name} <{email}>"))
}

/// Prints `key=value` lines describing the installation. Every probe is
/// allowed to fail: a missing piece reads as empty.
fn status_script() -> String {
    format!(
        "b={REMOTE_BINARY}; u={REMOTE_UNIT}
echo \"binary_sha256=$(sha256sum \"$b\" 2>/dev/null | cut -d' ' -f1)\"
echo \"build_id=$(\"$b\" version 2>/dev/null)\"
echo \"unit_sha256=$(sha256sum \"$u\" 2>/dev/null | cut -d' ' -f1)\"
echo \"active=$(systemctl is-active {UNIT_NAME} 2>/dev/null)\"
echo \"enabled=$(systemctl is-enabled {UNIT_NAME} 2>/dev/null)\"
echo \"kernel=$(uname -r)\"
if test -e /dev/virtio-ports/org.rampire.control.0; then echo control_channel=present; else echo control_channel=absent; fi
if sudo -n test -e {REMOTE_STATE}/control.json.request-state; then echo request_state=present; else echo request_state=absent; fi
"
    )
}

fn parse_status(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// Run `script` on `target` through `sh`, feeding it on standard input so
/// nothing in it is reinterpreted by the local or remote command line.
fn remote(target: &str, script: &str, repo: &Path) -> Result<String, String> {
    remote_with_input(target, script.as_bytes(), "sh -s", repo)
}

/// Run `command` on `target` with `input` on its standard input.
fn remote_with_input(
    target: &str,
    input: &[u8],
    command: &str,
    repo: &Path,
) -> Result<String, String> {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", target, command])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run ssh: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("ssh has no standard input")?
        .write_all(input)
        .map_err(|error| format!("send to {target}: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for ssh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{target}: `{command}` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{target} returned invalid UTF-8"))
}

fn remote_status(target: &str, repo: &Path) -> Result<BTreeMap<String, String>, String> {
    Ok(parse_status(&remote(target, &status_script(), repo)?))
}

/// Install `bytes` at `path` on `target` with `mode`, through a temporary
/// file so a failed transfer never leaves a partial file in place.
fn install_file(
    target: &str,
    bytes: &[u8],
    path: &str,
    mode: &str,
    repo: &Path,
) -> Result<(), String> {
    let staged = format!("/tmp/.rampire-deploy-{}", std::process::id());
    remote_with_input(
        target,
        bytes,
        &format!("cat > {staged} && sudo -n install -D -m {mode} {staged} {path}; status=$?; rm -f {staged}; exit $status"),
        repo,
    )
    .map(|_| ())
}

fn sha256(bytes: &[u8], repo: &Path) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run sha256sum: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("sha256sum has no standard input")?
        .write_all(bytes)
        .map_err(|error| format!("hash: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("hash: {error}"))?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .filter(|digest| digest.len() == 64)
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum returned no digest".to_owned())
}

/// A unit or script as the guest must receive it: LF line endings, whatever
/// the checkout uses.
fn unix_text(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .copied()
        .filter(|byte| *byte != b'\r')
        .collect()
}

/// A release binary where Cargo put it: `CARGO_TARGET_DIR` (relative to the
/// checkout when relative) or the checkout's `target/`.
fn release_binary(repo: &Path, name: &str) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .filter(|directory| !directory.is_empty())
        .map_or_else(|| repo.join("target"), |directory| repo.join(directory))
        .join("release")
        .join(name)
}

fn read_binary(repo: &Path, name: &str) -> Result<Vec<u8>, String> {
    let path = release_binary(repo, name);
    std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn read_source(repo: &Path, path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(repo.join(path)).map_err(|error| format!("read {path}: {error}"))
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    action: &'static str,
    applied: bool,
    ssh_destination: String,
    captured_unix_millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_discarded: Option<bool>,
    before: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<BTreeMap<String, String>>,
}

fn persist(path: &Path, evidence: &Evidence) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let mut bytes =
        serde_json::to_vec_pretty(evidence).map_err(|error| format!("encode evidence: {error}"))?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    println!("Evidence written to {}", path.display());
    Ok(())
}

/// The installation matches the candidate and is running.
fn verify_installed(
    after: &BTreeMap<String, String>,
    build_id: &str,
    binary_sha256: &str,
    unit_sha256: &str,
) -> Result<(), String> {
    let field = |key: &str| after.get(key).map(String::as_str).unwrap_or("");
    for (key, expected) in [
        ("build_id", build_id),
        ("binary_sha256", binary_sha256),
        ("unit_sha256", unit_sha256),
        ("active", "active"),
        ("enabled", "enabled"),
    ] {
        if field(key) != expected {
            return Err(format!(
                "installed {key} is {:?}, expected {expected:?}",
                field(key)
            ));
        }
    }
    Ok(())
}

fn deploy(target: &str, output: &Path, apply: bool, repo: &Path) -> Result<(), String> {
    let build_id = build(repo)?;
    let binary = read_binary(repo, GUEST_BINARY)?;
    let unit = unix_text(&read_source(repo, UNIT_SOURCE)?);
    let monitor = unix_text(&read_source(repo, MONITOR_SOURCE)?);
    let workload = read_binary(repo, WORKLOAD_BINARY)?;
    let binary_sha256 = sha256(&binary, repo)?;
    let unit_sha256 = sha256(&unit, repo)?;
    let before = remote_status(target, repo)?;
    let mut evidence = Evidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        action: "deploy",
        applied: apply,
        ssh_destination: target.to_owned(),
        captured_unix_millis: process::unix_millis()?,
        build_id: Some(build_id.clone()),
        binary_sha256: Some(binary_sha256.clone()),
        unit_sha256: Some(unit_sha256.clone()),
        state_discarded: None,
        before,
        after: None,
    };
    if !apply {
        println!("Dry run: would install build {build_id} on {target}; pass --apply to install");
        return persist(output, &evidence);
    }
    let result = (|| {
        install_file(target, &binary, REMOTE_BINARY, "0755", repo)?;
        install_file(target, &unit, REMOTE_UNIT, "0644", repo)?;
        install_file(target, &monitor, REMOTE_MONITOR, "0755", repo)?;
        install_file(target, &workload, REMOTE_WORKLOAD, "0755", repo)?;
        remote(
            target,
            &format!(
                "set -e; sudo -n systemctl daemon-reload; sudo -n systemctl enable {UNIT_NAME} >/dev/null 2>&1; sudo -n systemctl restart {UNIT_NAME}; sleep 2"
            ),
            repo,
        )?;
        Ok::<_, String>(())
    })();
    // Whatever happened, record what is there now.
    evidence.after = remote_status(target, repo).ok();
    persist(output, &evidence)?;
    result?;
    verify_installed(
        evidence
            .after
            .as_ref()
            .ok_or("could not read the installation back")?,
        &build_id,
        &binary_sha256,
        &unit_sha256,
    )?;
    println!("Installed build {build_id} on {target}; {UNIT_NAME} is active");
    Ok(())
}

fn uninstall(
    target: &str,
    output: &Path,
    apply: bool,
    discard_state: bool,
    repo: &Path,
) -> Result<(), String> {
    let before = remote_status(target, repo)?;
    let mut evidence = Evidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        action: "uninstall",
        applied: apply,
        ssh_destination: target.to_owned(),
        captured_unix_millis: process::unix_millis()?,
        build_id: None,
        binary_sha256: None,
        unit_sha256: None,
        state_discarded: Some(discard_state),
        before,
        after: None,
    };
    if !apply {
        println!(
            "Dry run: would remove {UNIT_NAME} from {target}{}; pass --apply to remove",
            if discard_state {
                " and discard its request state"
            } else {
                ", keeping its request state"
            }
        );
        return persist(output, &evidence);
    }
    let mut script = format!(
        "sudo -n systemctl disable --now {UNIT_NAME} >/dev/null 2>&1 || true
sudo -n rm -f {REMOTE_UNIT} {REMOTE_BINARY} {REMOTE_MONITOR} {REMOTE_WORKLOAD}
sudo -n systemctl daemon-reload
"
    );
    if discard_state {
        script.push_str(&format!(
            "sudo -n rm -rf --one-file-system {REMOTE_PRIVATE_STATE} {REMOTE_STATE}\n"
        ));
    }
    let result = remote(target, &script, repo);
    evidence.after = remote_status(target, repo).ok();
    persist(output, &evidence)?;
    result?;
    if !discard_state {
        // A request the host may already have admitted is kept, so a later
        // install resumes it rather than losing it.
        println!("Request state kept in {REMOTE_STATE}; pass --discard-state to remove it");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_need_a_destination_and_reject_unsafe_ones() {
        assert_eq!(parse(&["build"]), Ok(Action::Build));
        assert_eq!(
            parse(&["package", "--output", "dist"]),
            Ok(Action::Package {
                output: "dist".to_owned()
            })
        );
        assert_eq!(
            parse(&["status", "user@guest.lan"]),
            Ok(Action::Status {
                target: "user@guest.lan".to_owned()
            })
        );
        assert_eq!(
            parse(&["deploy", "guest", "--apply", "--output", "e.json"]),
            Ok(Action::Deploy {
                target: "guest".to_owned(),
                output: "e.json".to_owned(),
                apply: true
            })
        );
        assert_eq!(
            parse(&[
                "uninstall",
                "guest",
                "--output",
                "e.json",
                "--discard-state"
            ]),
            Ok(Action::Uninstall {
                target: "guest".to_owned(),
                output: "e.json".to_owned(),
                apply: false,
                discard_state: true
            })
        );
        for bad in [
            &["config", "x.toml"][..],
            &["package"],
            &["package", "--output", ""],
            &["deploy", "guest"],
            &["deploy", "-oProxyCommand=x", "--output", "e.json"],
            &["deploy", "guest; reboot", "--output", "e.json"],
            &["status", "guest", "--apply"],
            &["deploy", "guest", "--output", "e.json", "--discard-state"],
            &["deploy", "guest", "--output"],
            &["uninstall", "guest", "--output", "a", "--output", "b"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn status_lines_become_fields_and_an_installation_is_checked_against_them() {
        let installed = parse_status(
            "binary_sha256=abc\nbuild_id=1234-dirty.5678\nunit_sha256=def\n\
             active=active\nenabled=enabled\nkernel=6.12\nnoise without a separator\n",
        );
        assert_eq!(installed["kernel"], "6.12");
        assert_eq!(
            verify_installed(&installed, "1234-dirty.5678", "abc", "def"),
            Ok(())
        );
        assert!(verify_installed(&installed, "other", "abc", "def")
            .unwrap_err()
            .contains("build_id"));
        let mut stopped = installed.clone();
        stopped.insert("active".to_owned(), "failed".to_owned());
        assert!(verify_installed(&stopped, "1234-dirty.5678", "abc", "def").is_err());
        let mut missing = installed;
        missing.remove("unit_sha256");
        assert!(verify_installed(&missing, "1234-dirty.5678", "abc", "def").is_err());
    }

    #[test]
    fn units_and_scripts_reach_the_guest_with_unix_line_endings() {
        assert_eq!(unix_text(b"[Unit]\r\nA=1\r\n"), b"[Unit]\nA=1\n");
        assert!(status_script().contains(REMOTE_BINARY));
    }
}
