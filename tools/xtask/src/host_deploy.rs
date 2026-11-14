use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rampire_host::attestation::{AttestedCompatibilitySource, CompatibilityAttestation};
use rampire_host::compatibility_source::CompatibilitySource;
use rampire_host::config::{HostConfig, HostSettings};
use rampire_host::virsh::Virsh;
use serde::Serialize;

use rampire_ops::process;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    config: PathBuf,
    /// One reviewed source attestation per configured VM.
    attestations: BTreeMap<String, PathBuf>,
    /// A folder holding `<vm_name>.attestation.json` for each configured VM
    /// that `attestations` does not name.
    attestation_dir: Option<PathBuf>,
    output: PathBuf,
    timeout: Duration,
    apply: bool,
    elevate: bool,
    elevated_child: bool,
}

#[derive(Debug, Serialize)]
struct Evidence {
    schema_version: u32,
    captured_unix_millis: u64,
    applied: bool,
    unit: String,
    candidate_binary_sha256: String,
    installed_binary_sha256: Option<String>,
    config_sha256: String,
    attestations: Vec<AttestationEvidence>,
    unit_sha256: String,
    libvirt_policy_sha256: String,
    active_state: String,
    unit_file_state: String,
    restart_policy: String,
    backup_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttestationEvidence {
    vm_name: String,
    installed_path: String,
    sha256: String,
}

/// Where host-deploy installs the configuration, with the attestations in
/// their own folder inside it; the unit's ExecStart names this directory.
const CONFIG_DIRECTORY: &str = "/etc/rampire";

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut config = None;
    let mut attestations = BTreeMap::new();
    let mut attestation_dir = None;
    let mut output = None;
    let mut timeout = None;
    let mut apply = false;
    let mut elevate = false;
    let mut elevated_child = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--apply" if !apply => apply = true,
            "--elevate" if !elevate => elevate = true,
            "--elevated-child" if !elevated_child => elevated_child = true,
            option @ ("--config"
            | "--attestation"
            | "--attestation-dir"
            | "--output"
            | "--command-timeout-seconds") => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{option} requires a value"))?;
                match option {
                    "--config" if config.is_none() => config = Some(PathBuf::from(value)),
                    "--attestation" => {
                        let (vm, path) = value
                            .split_once('=')
                            .filter(|(vm, path)| !vm.is_empty() && !path.is_empty())
                            .ok_or("--attestation requires VM=FILE")?;
                        if attestations
                            .insert(vm.to_owned(), PathBuf::from(path))
                            .is_some()
                        {
                            return Err(format!("duplicate --attestation for VM {vm}"));
                        }
                    }
                    "--attestation-dir" if attestation_dir.is_none() => {
                        attestation_dir = Some(PathBuf::from(value))
                    }
                    "--output" if output.is_none() => output = Some(PathBuf::from(value)),
                    "--command-timeout-seconds" if timeout.is_none() => {
                        timeout = Some(Duration::from_secs(
                            value
                                .parse::<u64>()
                                .ok()
                                .filter(|v| *v > 0)
                                .ok_or_else(|| format!("{option} requires a positive integer"))?,
                        ));
                    }
                    _ => return Err(format!("duplicate host-deploy option: {option}")),
                }
            }
            option => return Err(format!("unknown or duplicate host-deploy option: {option}")),
        }
        index += 1;
    }
    if elevate && elevated_child {
        return Err("--elevate and --elevated-child are mutually exclusive".to_owned());
    }
    if (elevate || elevated_child) && !apply {
        return Err("host-deploy elevation requires --apply".to_owned());
    }
    Ok(Command {
        config: config.ok_or_else(|| "host-deploy requires --config".to_owned())?,
        attestations,
        attestation_dir,
        output: output.ok_or_else(|| "host-deploy requires --output".to_owned())?,
        timeout: timeout
            .ok_or_else(|| "host-deploy requires --command-timeout-seconds".to_owned())?,
        apply,
        elevate,
        elevated_child,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    if command.elevate {
        let json = run_elevated(command, repo)?;
        persist(&resolve(repo, &command.output), json.as_bytes())?;
        println!(
            "Host deployment evidence written to {}",
            resolve(repo, &command.output).display()
        );
        return Ok(());
    }
    let evidence = collect(command, repo)?;
    let json = serde_json::to_string_pretty(&evidence)
        .map(|json| format!("{json}\n"))
        .map_err(|error| format!("serialize host deployment evidence: {error}"))?;
    if command.elevated_child {
        print!("{json}");
    } else {
        persist(&resolve(repo, &command.output), json.as_bytes())?;
        println!(
            "Host deployment dry-run evidence written to {}",
            resolve(repo, &command.output).display()
        );
    }
    Ok(())
}

/// The configuration's files, relative to its directory: `global.toml`
/// and each VM's `vms.d/NAME.toml`.
fn configuration_files(directory: &Path) -> Result<Vec<String>, String> {
    use rampire_core::settings::{self, GLOBAL_FILE, VMS_DIRECTORY};
    let (_, vms) = settings::load_configuration(directory)?;
    let mut files = vec![GLOBAL_FILE.to_owned()];
    for vm in &vms {
        files.push(format!(
            "{VMS_DIRECTORY}/{}",
            settings::vm_filename(&vm.vm_name)?
        ));
    }
    Ok(files)
}

/// Copies the configuration files byte for byte, so what is installed is what
/// the administrator wrote: their comments and sizes such as `"3GB"` stay as
/// they are. The caller validates the copy, not the source.
fn snapshot_configuration(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    use rampire_core::settings::{GLOBAL_FILE, VMS_DIRECTORY};
    std::fs::create_dir_all(directory.join(VMS_DIRECTORY))
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    for name in configuration_files(source)? {
        let from = source.join(&name);
        if name == GLOBAL_FILE && !from.exists() {
            persist(
                &directory.join(GLOBAL_FILE),
                b"# Every server-wide setting is at its built-in default.\n",
            )?;
            continue;
        }
        let bytes =
            std::fs::read(&from).map_err(|error| format!("read {}: {error}", from.display()))?;
        persist(&directory.join(&name), &bytes)?;
    }
    Ok(directory.to_path_buf())
}

/// VM files installed by an earlier deployment that the new configuration
/// doesn't have. They are backed up and removed, or the controller would
/// still manage those VMs.
fn stale_vm_files(installed: &Path, keep: &[String]) -> Result<Vec<PathBuf>, String> {
    use rampire_core::settings::VMS_DIRECTORY;
    let directory = installed.join(VMS_DIRECTORY);
    let entries = match std::fs::read_dir(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.map_err(|error| format!("read {}: {error}", directory.display()))?,
    };
    let mut stale = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !keep.contains(&format!("{VMS_DIRECTORY}/{name}")) {
            stale.push(path);
        }
    }
    stale.sort();
    Ok(stale)
}

fn collect(command: &Command, repo: &Path) -> Result<Evidence, String> {
    let config_source = resolve(repo, &command.config);
    let candidate = repo.join("target/release/rampire");
    let unit_source = repo.join("host/systemd/rampire.service");
    let policy_source = repo.join("host/systemd/50-rampire.rules");
    for path in [&config_source, &candidate, &unit_source, &policy_source] {
        if !path.is_file() {
            return Err(format!(
                "required host deployment input is missing: {}",
                path.display()
            ));
        }
    }
    // Snapshot all three formats together; validation, hashing and installation
    // consume the same bytes even if the administrator edits the source later.
    let config_source = snapshot_configuration(
        &config_source,
        &repo.join(format!(
            ".artifacts/deploy/{}-{}-config",
            now_millis()?,
            std::process::id()
        )),
    )?;
    let settings = HostSettings::load(&config_source)
        .map_err(|error| format!("validate host configuration: {error}"))?;
    let mut attestations = pair_attestations(
        &command.attestations,
        command.attestation_dir.as_deref(),
        repo,
        &settings.vms,
    )?;
    // Snapshot the attestations with the settings, so an install from the
    // installed directory itself never copies a file onto itself.
    let attestation_snapshot = config_source.join(rampire_host::config::ATTESTATION_DIRECTORY);
    for attestation in &mut attestations {
        let bytes = std::fs::read(&attestation.source).map_err(|error| {
            format!(
                "read compatibility attestation {}: {error}",
                attestation.source.display()
            )
        })?;
        let copy = attestation_snapshot.join(format!("{}.attestation.json", attestation.vm_name));
        persist(&copy, &bytes)?;
        attestation.source = copy;
    }
    for (attestation, config) in attestations.iter().zip(&settings.vms) {
        let text = std::fs::read_to_string(&attestation.source)
            .map_err(|error| format!("read compatibility attestation: {error}"))?;
        CompatibilityAttestation::parse(&text)?;
        validate_live_attestation(config, &attestation.source, command.timeout)?;
    }
    let unit = rampire_ops::units::CONTROLLER_UNIT;
    let before = properties(unit, repo, command.timeout)?;
    require_inactive(&before, unit)?;

    let candidate_hash = hash(&candidate, repo, command.timeout)?;
    let mut config_index = String::new();
    for name in configuration_files(&config_source)? {
        config_index.push_str(&format!(
            "{}  {name}\n",
            hash(&config_source.join(&name), repo, command.timeout)?
        ));
    }
    let index_path = config_source.join("config-index.sha256");
    persist(&index_path, config_index.as_bytes())?;
    let config_hash = hash(&index_path, repo, command.timeout)?;
    let attestation_evidence = attestations
        .iter()
        .map(|attestation| {
            Ok(AttestationEvidence {
                vm_name: attestation.vm_name.clone(),
                installed_path: attestation.target.display().to_string(),
                sha256: hash(&attestation.source, repo, command.timeout)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let unit_hash = hash(&unit_source, repo, command.timeout)?;
    let policy_hash = hash(&policy_source, repo, command.timeout)?;
    let mut backup_path = None;
    if command.apply {
        if !command.elevated_child || unsafe { libc_geteuid() } != 0 {
            return Err("host-deploy --apply must run through --elevate".to_owned());
        }
        backup_path = Some(install(
            command,
            repo,
            &config_source,
            &attestations,
            &candidate,
            &unit_source,
            &before,
        )?);
    }
    let after = properties(unit, repo, command.timeout)?;
    require_inactive(&after, unit)?;
    let restart_policy = one(&after, "Restart")?;
    if command.apply && restart_policy != "no" {
        return Err(format!(
            "installed unit is not fail-stop: Restart={restart_policy}"
        ));
    }
    Ok(Evidence {
        schema_version: 4,
        captured_unix_millis: now_millis()?,
        applied: command.apply,
        unit: unit.to_owned(),
        candidate_binary_sha256: candidate_hash,
        installed_binary_sha256: command
            .apply
            .then(|| hash(Path::new("/usr/local/bin/rampire"), repo, command.timeout))
            .transpose()?,
        config_sha256: config_hash,
        attestations: attestation_evidence,
        unit_sha256: unit_hash,
        libvirt_policy_sha256: policy_hash,
        active_state: one(&after, "ActiveState")?,
        unit_file_state: one(&after, "UnitFileState")?,
        restart_policy,
        backup_path,
    })
}

struct DeployedAttestation {
    vm_name: String,
    source: PathBuf,
    target: PathBuf,
}

/// Pairs every configured VM with exactly one reviewed source attestation and
/// the path its config names, before anything is read or validated live. A VM
/// not named explicitly takes `<vm_name>.attestation.json` from `directory`.
fn pair_attestations(
    attestations: &BTreeMap<String, PathBuf>,
    directory: Option<&Path>,
    repo: &Path,
    vms: &[HostConfig],
) -> Result<Vec<DeployedAttestation>, String> {
    if let Some(extra) = attestations
        .keys()
        .find(|vm| !vms.iter().any(|config| &config.vm_name == *vm))
    {
        return Err(format!(
            "--attestation names VM {extra}, which is not configured"
        ));
    }
    vms.iter()
        .map(|config| {
            let source = match (attestations.get(&config.vm_name), directory) {
                (Some(path), _) => resolve(repo, path),
                (None, Some(directory)) => {
                    let path = resolve(
                        repo,
                        &directory.join(format!("{}.attestation.json", config.vm_name)),
                    );
                    if !path.is_file() {
                        return Err(format!(
                            "no attestation for VM {}: expected {}",
                            config.vm_name,
                            path.display()
                        ));
                    }
                    path
                }
                (None, None) => {
                    return Err(format!(
                        "host-deploy requires --attestation {}=FILE or --attestation-dir DIR",
                        config.vm_name
                    ))
                }
            };
            let target = rampire_host::config::attestation_path(
                Path::new(CONFIG_DIRECTORY),
                &config.vm_name,
            );
            Ok(DeployedAttestation {
                vm_name: config.vm_name.clone(),
                source,
                target,
            })
        })
        .collect()
}

fn install(
    command: &Command,
    repo: &Path,
    config: &Path,
    attestations: &[DeployedAttestation],
    candidate: &Path,
    unit: &Path,
    before: &BTreeMap<String, Vec<String>>,
) -> Result<String, String> {
    install_directory(
        "/var/lib/rampire",
        "root",
        "root",
        "0755",
        repo,
        command.timeout,
    )?;
    install_directory(
        "/var/lib/rampire/deployment-backups",
        "root",
        "root",
        "0700",
        repo,
        command.timeout,
    )?;
    install_directory(
        "/var/lib/rampire/state",
        "rampire",
        "rampire",
        "0700",
        repo,
        command.timeout,
    )?;
    let backup = PathBuf::from(format!(
        "/var/lib/rampire/deployment-backups/{}",
        now_millis()?
    ));
    std::fs::create_dir_all(&backup)
        .map_err(|error| format!("create host deployment backup: {error}"))?;
    process::bounded_text(
        "chmod",
        &[process::os("0700"), backup.as_os_str().to_owned()],
        repo,
        command.timeout,
    )?;
    let config_files = configuration_files(config)?;
    let stale = stale_vm_files(Path::new(CONFIG_DIRECTORY), &config_files)?;
    let targets = [
        PathBuf::from("/usr/local/bin/rampire"),
        PathBuf::from("/etc/systemd/system/rampire.service"),
        PathBuf::from("/etc/polkit-1/rules.d/50-rampire.rules"),
    ]
    .into_iter()
    .chain(
        config_files
            .iter()
            .map(|name| Path::new(CONFIG_DIRECTORY).join(name)),
    )
    .chain(stale.iter().cloned())
    .chain(
        attestations
            .iter()
            .map(|attestation| attestation.target.clone()),
    );
    for target in targets {
        let Some(name) = target.file_name() else {
            return Err(format!(
                "backup target has no file name: {}",
                target.display()
            ));
        };
        if target.exists() {
            std::fs::copy(&target, backup.join(name))
                .map_err(|error| format!("backup {}: {error}", target.display()))?;
        }
    }
    install_directory(
        CONFIG_DIRECTORY,
        "root",
        "root",
        "0755",
        repo,
        command.timeout,
    )?;
    install_directory(
        &Path::new(CONFIG_DIRECTORY)
            .join(rampire_core::settings::VMS_DIRECTORY)
            .display()
            .to_string(),
        "root",
        "rampire",
        "0750",
        repo,
        command.timeout,
    )?;
    install_directory(
        &Path::new(CONFIG_DIRECTORY)
            .join(rampire_host::config::ATTESTATION_DIRECTORY)
            .display()
            .to_string(),
        "root",
        "rampire",
        "0750",
        repo,
        command.timeout,
    )?;
    install_file(
        candidate,
        "/usr/local/bin/rampire",
        "root",
        "root",
        "0755",
        repo,
        command.timeout,
    )?;
    install_file(
        unit,
        "/etc/systemd/system/rampire.service",
        "root",
        "root",
        "0644",
        repo,
        command.timeout,
    )?;
    install_file(
        &repo.join("host/systemd/50-rampire.rules"),
        "/etc/polkit-1/rules.d/50-rampire.rules",
        "root",
        "root",
        "0644",
        repo,
        command.timeout,
    )?;
    for name in &config_files {
        install_file(
            &config.join(name),
            &format!("{CONFIG_DIRECTORY}/{name}"),
            "root",
            "rampire",
            "0640",
            repo,
            command.timeout,
        )?;
    }
    for attestation in attestations {
        install_file(
            &attestation.source,
            attestation
                .target
                .to_str()
                .ok_or("attestation target is not UTF-8")?,
            "root",
            "rampire",
            "0640",
            repo,
            command.timeout,
        )?;
    }
    for path in &stale {
        std::fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display()))?;
    }
    process::bounded_text(
        "systemctl",
        &[process::os("daemon-reload")],
        repo,
        command.timeout,
    )?;
    process::bounded_text(
        "systemctl",
        &[
            process::os("disable"),
            process::os(rampire_ops::units::CONTROLLER_UNIT),
        ],
        repo,
        command.timeout,
    )?;
    if one(before, "UnitFileState")? == "enabled" {
        std::fs::write(backup.join("was-enabled"), b"true\n")
            .map_err(|error| format!("record prior enablement: {error}"))?;
    }
    Ok(backup.display().to_string())
}

fn install_file(
    source: &Path,
    target: &str,
    owner: &str,
    group: &str,
    mode: &str,
    repo: &Path,
    timeout: Duration,
) -> Result<(), String> {
    process::bounded_text(
        "install",
        &[
            process::os("-o"),
            process::os(owner),
            process::os("-g"),
            process::os(group),
            process::os("-m"),
            process::os(mode),
            process::os("--"),
            source.as_os_str().to_owned(),
            process::os(target),
        ],
        repo,
        timeout,
    )
    .map(|_| ())
}

fn install_directory(
    target: &str,
    owner: &str,
    group: &str,
    mode: &str,
    repo: &Path,
    timeout: Duration,
) -> Result<(), String> {
    process::bounded_text(
        "install",
        &[
            process::os("-d"),
            process::os("-o"),
            process::os(owner),
            process::os("-g"),
            process::os(group),
            process::os("-m"),
            process::os(mode),
            process::os("--"),
            process::os(target),
        ],
        repo,
        timeout,
    )
    .map(|_| ())
}

fn validate_live_attestation(
    config: &HostConfig,
    path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    AttestedCompatibilitySource::new(
        Virsh::with_connection(&config.virsh_binary, timeout, &config.libvirt_uri),
        &config.vm_name,
        &config.alias,
        config.guest_os,
        path,
    )
    .compatibility()?
    .validate_for_resize()
    .map_err(|error| format!("validate live compatibility attestation: {error}"))
}

fn properties(
    unit: &str,
    repo: &Path,
    timeout: Duration,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let text = process::bounded_text(
        "systemctl",
        &[
            process::os("show"),
            process::os(unit),
            process::os("--property=ActiveState,SubState,UnitFileState,Restart,DropInPaths"),
            process::os("--no-pager"),
        ],
        repo,
        timeout,
    )?;
    let mut result = BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed systemctl property: {line}"))?;
        result
            .entry(key.to_owned())
            .or_insert_with(Vec::new)
            .push(value.to_owned());
    }
    Ok(result)
}

fn one(values: &BTreeMap<String, Vec<String>>, key: &str) -> Result<String, String> {
    let found = values
        .get(key)
        .ok_or_else(|| format!("systemctl omitted {key}"))?;
    if found.len() != 1 {
        return Err(format!("systemctl returned repeated {key}"));
    }
    Ok(found[0].clone())
}

fn require_inactive(values: &BTreeMap<String, Vec<String>>, unit: &str) -> Result<(), String> {
    let state = one(values, "ActiveState")?;
    if state == "inactive" {
        Ok(())
    } else {
        Err(format!(
            "refusing host deployment because {unit} is {state}"
        ))
    }
}

fn hash(path: &Path, repo: &Path, timeout: Duration) -> Result<String, String> {
    process::bounded_text(
        "sha256sum",
        &[process::os("--"), path.as_os_str().to_owned()],
        repo,
        timeout,
    )?
    .split_whitespace()
    .next()
    .filter(|value| value.len() == 64)
    .map(str::to_owned)
    .ok_or_else(|| format!("invalid SHA-256 for {}", path.display()))
}

fn run_elevated(command: &Command, repo: &Path) -> Result<String, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("resolve xtask executable: {error}"))?;
    let mut args = vec![process::os("--"), executable.into_os_string()];
    args.extend(elevated_arguments(command));
    process::elevated_text(
        &args,
        repo,
        command
            .timeout
            .checked_mul(4)
            .ok_or("elevated workflow timeout overflowed")?,
    )
}

fn elevated_arguments(command: &Command) -> Vec<OsString> {
    let mut arguments = vec![
        process::os("host-deploy"),
        process::os("--config"),
        command.config.as_os_str().to_owned(),
    ];
    for (vm, path) in &command.attestations {
        let mut value = OsString::from(format!("{vm}="));
        value.push(path.as_os_str());
        arguments.push(process::os("--attestation"));
        arguments.push(value);
    }
    if let Some(directory) = &command.attestation_dir {
        arguments.push(process::os("--attestation-dir"));
        arguments.push(directory.as_os_str().to_owned());
    }
    arguments.extend([
        process::os("--output"),
        command.output.as_os_str().to_owned(),
        process::os("--command-timeout-seconds"),
        process::os(command.timeout.as_secs().to_string()),
        process::os("--apply"),
        process::os("--elevated-child"),
    ]);
    arguments
}

fn persist(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "host deployment output has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create host deployment evidence directory: {error}"))?;
    let temp = parent.join(format!(".host-deployment-{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)
        .map_err(|error| format!("write host deployment evidence: {error}"))?;
    std::fs::rename(&temp, path)
        .map_err(|error| format!("commit host deployment evidence: {error}"))
}

fn resolve(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}
use rampire_ops::process::unix_millis as now_millis;

unsafe extern "C" {
    fn geteuid() -> u32;
}
unsafe fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_explicit_apply_and_elevation() {
        let args = [
            "--config",
            "host.toml",
            "--attestation",
            "guest=attestation.json",
            "--attestation",
            "other=other.json",
            "--output",
            "evidence.json",
            "--command-timeout-seconds",
            "30",
            "--apply",
            "--elevate",
        ]
        .map(str::to_owned);
        let command = parse(&args).expect("valid command");
        assert!(command.apply && command.elevate);
        assert_eq!(command.attestations.len(), 2);
        assert_eq!(
            parse(&args).map(|command| elevated_arguments(&command).len()),
            Ok(13)
        );
        let mut duplicate = args.to_vec();
        duplicate[5] = "guest=again.json".to_owned();
        assert!(parse(&duplicate).is_err());

        let mut with_folder = args.to_vec();
        with_folder.splice(
            2..6,
            [
                "--attestation-dir".to_owned(),
                "site/attestations".to_owned(),
            ],
        );
        let command = parse(&with_folder).expect("folder");
        assert_eq!(
            command.attestation_dir,
            Some(PathBuf::from("site/attestations"))
        );
        let forwarded = elevated_arguments(&command);
        assert!(forwarded
            .windows(2)
            .any(|pair| pair[0] == "--attestation-dir" && pair[1] == "site/attestations"));
    }

    #[test]
    fn every_configured_vm_needs_exactly_one_attestation_in_the_config_directory() {
        let directory =
            std::env::temp_dir().join(format!("rampire-deploy-test-{}", std::process::id()));
        let source = crate::repo_root().expect("root").join("fixtures/config");
        let snapshot = snapshot_configuration(&source, &directory).expect("snapshot");
        let original = HostSettings::load(&snapshot).expect("settings");
        let mut settings = original;
        settings.vms[0].vm_name = "a".to_owned();
        let mut other = settings.vms[0].clone();
        other.vm_name = "b".to_owned();
        settings.vms.push(other);
        std::fs::remove_dir_all(directory).expect("remove fixture");
        let repo = Path::new("/repo");
        let both = BTreeMap::from([
            ("a".to_owned(), PathBuf::from("a.json")),
            ("b".to_owned(), PathBuf::from("/abs/b.json")),
        ]);
        let paired = pair_attestations(&both, None, repo, &settings.vms).expect("paired");
        assert_eq!(paired[0].source, Path::new("/repo/a.json"));
        // Installed in their own folder, apart from the settings files.
        assert_eq!(
            paired[1].target,
            Path::new("/etc/rampire/attestations/b.attestation.json")
        );

        let mut missing = both.clone();
        missing.remove("b");
        assert!(pair_attestations(&missing, None, repo, &settings.vms).is_err());
        let mut extra = both.clone();
        extra.insert("c".to_owned(), PathBuf::from("c.json"));
        assert!(pair_attestations(&extra, None, repo, &settings.vms).is_err());
    }

    #[test]
    fn an_attestation_folder_supplies_each_vm_not_named_explicitly() {
        let directory =
            std::env::temp_dir().join(format!("rampire-deploy-dir-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("folder");
        std::fs::write(directory.join("a.attestation.json"), b"{}").expect("a");
        let source = crate::repo_root().expect("root").join("fixtures/config");
        let mut vms = HostSettings::load(&source).expect("settings").vms;
        vms[0].vm_name = "a".to_owned();
        let mut other = vms[0].clone();
        other.vm_name = "b".to_owned();
        vms.push(other);
        let repo = Path::new("/repo");
        let named = BTreeMap::from([("b".to_owned(), PathBuf::from("/abs/b.json"))]);
        let paired = pair_attestations(&named, Some(&directory), repo, &vms);
        let missing = pair_attestations(&BTreeMap::new(), Some(&directory), repo, &vms);
        std::fs::remove_dir_all(&directory).expect("remove folder");
        let paired = paired.expect("paired");
        assert_eq!(paired[0].source, directory.join("a.attestation.json"));
        assert_eq!(paired[1].source, Path::new("/abs/b.json"));
        // b is in neither the folder nor the explicit list.
        assert!(missing.is_err_and(|error| error.contains("no attestation for VM b")));
    }

    #[test]
    fn the_installed_configuration_keeps_comments_and_written_sizes() {
        let root = std::env::temp_dir().join(format!("rampire-deploy-copy-{}", std::process::id()));
        let source = root.join("source");
        std::fs::create_dir_all(source.join("vms.d")).expect("source");
        let globals = "# The pool every VM shares.\npool = \"3GB\"\n";
        let vm = "# Web first, small at idle.\nalias = \"ua-virtiomem0\"\npriority = 100\nguest_os = \"linux\"\nmin_ram = \"512MB\"\nmax_ram = \"2GiB\"\nfree_ram = \"128MiB\"\n";
        std::fs::write(source.join("global.toml"), globals).expect("globals");
        std::fs::write(source.join("vms.d/web.toml"), vm).expect("vm");
        std::fs::write(source.join("vms.d/web.toml~"), "backup").expect("backup");
        let snapshot = snapshot_configuration(&source, &root.join("copy"));
        let read = |name: &str| std::fs::read_to_string(root.join("copy").join(name));
        let copied = (read("global.toml"), read("vms.d/web.toml"));
        let backup_copied = root.join("copy/vms.d/web.toml~").exists();
        let loaded = snapshot.as_ref().map(|path| HostSettings::load(path));
        std::fs::remove_dir_all(&root).expect("remove");
        assert!(snapshot.is_ok(), "{snapshot:?}");
        assert_eq!(copied.0.expect("globals"), globals);
        assert_eq!(copied.1.expect("vm"), vm);
        assert!(!backup_copied, "only the configuration is installed");
        let vms = loaded.expect("snapshot path").expect("valid copy").vms;
        assert_eq!(vms[0].max_ram, 2 << 30);
    }

    #[test]
    fn vm_files_the_new_configuration_drops_are_stale() {
        let root =
            std::env::temp_dir().join(format!("rampire-deploy-stale-{}", std::process::id()));
        std::fs::create_dir_all(root.join("vms.d")).expect("folders");
        for name in ["vms.d/web.toml", "vms.d/old.toml", "global.toml"] {
            std::fs::write(root.join(name), b"").expect("file");
        }
        let keep = ["global.toml".to_owned(), "vms.d/web.toml".to_owned()];
        let found = stale_vm_files(&root, &keep);
        let absent = stale_vm_files(&root.join("absent"), &keep);
        std::fs::remove_dir_all(&root).expect("remove");
        assert_eq!(found, Ok(vec![root.join("vms.d/old.toml")]));
        assert_eq!(absent, Ok(Vec::new()));
    }
}
