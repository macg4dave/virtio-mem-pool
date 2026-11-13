use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use virtio_mem_host::attestation::{AttestedCompatibilitySource, CompatibilityAttestation};
use virtio_mem_host::compatibility_source::CompatibilitySource;
use virtio_mem_host::config::{HostConfig, HostSettings};
use virtio_mem_host::virsh::Virsh;

use crate::process;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    instance: String,
    config: PathBuf,
    /// One reviewed source attestation per configured VM.
    attestations: BTreeMap<String, PathBuf>,
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
    instance: String,
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

/// Where host-deploy installs the configuration and attestations; the unit's
/// ExecStart names the host list in it.
const CONFIG_DIRECTORY: &str = "/etc/virtio-mem-host";

/// The deployment layout: unit `virtio-mem-host@INSTANCE.service` runs the
/// config `/etc/virtio-mem-host/host_list.toml` and its sibling settings files.
/// Returns INSTANCE for a controller unit name.
pub fn controller_instance(unit: &str) -> Result<&str, String> {
    unit.strip_prefix("virtio-mem-host@")
        .and_then(|rest| rest.strip_suffix(".service"))
        .filter(|instance| validate_identity(instance).is_ok())
        .ok_or_else(|| format!("{unit} is not a virtio-mem-host@INSTANCE.service unit"))
}

/// The controller unit to observe when none was named: the one
/// `virtio-mem-host@INSTANCE` unit systemd knows on this host, since one
/// controller manages every VM in its configuration. With none or several,
/// `fallback` (the unit named after the VM) is kept.
pub fn installed_controller_unit(fallback: &str, repo: &Path) -> String {
    let listing = process::bounded_text(
        "systemctl",
        &[
            process::os("list-units"),
            process::os("--all"),
            process::os("--plain"),
            process::os("--no-legend"),
            process::os("virtio-mem-host@*.service"),
        ],
        repo,
        Duration::from_secs(10),
    );
    // Best effort: without systemctl the fallback is what was used before.
    listing.map_or_else(
        |_| fallback.to_owned(),
        |text| only_controller_unit(&text).unwrap_or_else(|| fallback.to_owned()),
    )
}

/// The single controller unit in a `systemctl list-units --plain` listing.
fn only_controller_unit(listing: &str) -> Option<String> {
    let mut units = listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| controller_instance(unit).is_ok());
    let unit = units.next()?;
    units.next().is_none().then(|| unit.to_owned())
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let instance = args
        .first()
        .ok_or_else(|| "host-deploy requires INSTANCE and explicit options".to_owned())?;
    validate_identity(instance)?;
    let mut config = None;
    let mut attestations = BTreeMap::new();
    let mut output = None;
    let mut timeout = None;
    let mut apply = false;
    let mut elevate = false;
    let mut elevated_child = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--apply" if !apply => apply = true,
            "--elevate" if !elevate => elevate = true,
            "--elevated-child" if !elevated_child => elevated_child = true,
            option @ ("--config" | "--attestation" | "--output" | "--command-timeout-seconds") => {
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
        instance: instance.clone(),
        config: config.ok_or_else(|| "host-deploy requires --config".to_owned())?,
        attestations,
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

fn snapshot_configuration(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    use virtio_mem_core::settings::{self, GLOBAL_FILE, HOST_LIST_FILE};
    let (globals, list, vms) = settings::load_configuration(source)?;
    let serialize = |result: Result<String, toml::ser::Error>| {
        result.map_err(|error| format!("serialize configuration: {error}"))
    };
    persist(
        &directory.join(GLOBAL_FILE),
        serialize(toml::to_string(&globals))?.as_bytes(),
    )?;
    persist(
        &directory.join(HOST_LIST_FILE),
        serialize(toml::to_string(&list))?.as_bytes(),
    )?;
    for vm in &vms {
        persist(
            &directory.join(settings::vm_filename(&vm.vm_name)?),
            serialize(toml::to_string(vm))?.as_bytes(),
        )?;
    }
    Ok(directory.join(HOST_LIST_FILE))
}

fn configuration_paths(list_path: &Path) -> Result<Vec<PathBuf>, String> {
    use virtio_mem_core::settings::{self, GLOBAL_FILE};
    let (_, _, vms) = settings::load_configuration(list_path)?;
    let directory = list_path
        .parent()
        .ok_or("host list has no parent directory")?;
    let mut paths = vec![directory.join(GLOBAL_FILE), list_path.to_path_buf()];
    for vm in &vms {
        paths.push(directory.join(settings::vm_filename(&vm.vm_name)?));
    }
    Ok(paths)
}

fn collect(command: &Command, repo: &Path) -> Result<Evidence, String> {
    let config_source = resolve(repo, &command.config);
    let candidate = repo.join("target/release/virtio-mem-host");
    let unit_source = repo.join("host/systemd/virtio-mem-host@.service");
    let policy_source = repo.join("host/systemd/50-virtio-mem-host.rules");
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
            ".artifacts/deploy/{}-{}-{}-config",
            command.instance,
            now_millis()?,
            std::process::id()
        )),
    )?;
    let settings = HostSettings::load(&config_source)
        .map_err(|error| format!("validate host configuration: {error}"))?;
    let attestations = pair_attestations(&command.attestations, repo, &settings.vms)?;
    for (attestation, config) in attestations.iter().zip(&settings.vms) {
        let text = std::fs::read_to_string(&attestation.source)
            .map_err(|error| format!("read compatibility attestation: {error}"))?;
        CompatibilityAttestation::parse(&text)?;
        validate_live_attestation(config, &attestation.source, command.timeout)?;
    }
    let unit = format!("virtio-mem-host@{}.service", command.instance);
    let before = properties(&unit, repo, command.timeout)?;
    require_inactive(&before, &unit)?;

    let candidate_hash = hash(&candidate, repo, command.timeout)?;
    let config_files = configuration_paths(&config_source)?;
    let mut config_index = String::new();
    for path in &config_files {
        config_index.push_str(&format!(
            "{}  {}\n",
            hash(path, repo, command.timeout)?,
            path.file_name()
                .and_then(|name| name.to_str())
                .ok_or("configuration filename is not UTF-8")?
        ));
    }
    let index_path = config_source.with_file_name("config-index.sha256");
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
    let after = properties(&unit, repo, command.timeout)?;
    require_inactive(&after, &unit)?;
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
        instance: command.instance.clone(),
        unit,
        candidate_binary_sha256: candidate_hash,
        installed_binary_sha256: command
            .apply
            .then(|| {
                hash(
                    Path::new("/usr/local/libexec/virtio-mem-host"),
                    repo,
                    command.timeout,
                )
            })
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
/// the path its config names, before anything is read or validated live.
fn pair_attestations(
    attestations: &BTreeMap<String, PathBuf>,
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
            let source = attestations
                .get(&config.vm_name)
                .map(|path| resolve(repo, path))
                .ok_or_else(|| {
                    format!("host-deploy requires --attestation {}=FILE", config.vm_name)
                })?;
            let target = virtio_mem_host::config::attestation_path(
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
        "/var/lib/virtio-mem-host",
        "root",
        "root",
        "0755",
        repo,
        command.timeout,
    )?;
    install_directory(
        "/var/lib/virtio-mem-host/deployment-backups",
        "root",
        "root",
        "0700",
        repo,
        command.timeout,
    )?;
    install_directory(
        "/var/lib/virtio-mem-host/state",
        "virtio-mem-host",
        "virtio-mem-host",
        "0700",
        repo,
        command.timeout,
    )?;
    let backup = PathBuf::from(format!(
        "/var/lib/virtio-mem-host/deployment-backups/{}",
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
    let config_files = configuration_paths(config)?;
    let targets = [
        PathBuf::from("/usr/local/libexec/virtio-mem-host"),
        PathBuf::from("/etc/systemd/system/virtio-mem-host@.service"),
        PathBuf::from("/etc/polkit-1/rules.d/50-virtio-mem-host.rules"),
    ]
    .into_iter()
    .chain(
        config_files
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| Path::new("/etc/virtio-mem-host").join(name)),
    )
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
    install_file(
        candidate,
        "/usr/local/libexec/virtio-mem-host",
        "root",
        "root",
        "0755",
        repo,
        command.timeout,
    )?;
    install_file(
        unit,
        "/etc/systemd/system/virtio-mem-host@.service",
        "root",
        "root",
        "0644",
        repo,
        command.timeout,
    )?;
    install_file(
        &repo.join("host/systemd/50-virtio-mem-host.rules"),
        "/etc/polkit-1/rules.d/50-virtio-mem-host.rules",
        "root",
        "root",
        "0644",
        repo,
        command.timeout,
    )?;
    for source in &config_files {
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid configuration filename")?;
        install_file(
            source,
            &format!("/etc/virtio-mem-host/{name}"),
            "root",
            "virtio-mem-host",
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
            "virtio-mem-host",
            "0640",
            repo,
            command.timeout,
        )?;
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
            process::os(format!("virtio-mem-host@{}.service", command.instance)),
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
        process::os(&command.instance),
        process::os("--config"),
        command.config.as_os_str().to_owned(),
    ];
    for (vm, path) in &command.attestations {
        let mut value = OsString::from(format!("{vm}="));
        value.push(path.as_os_str());
        arguments.push(process::os("--attestation"));
        arguments.push(value);
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
fn validate_identity(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        Ok(())
    } else {
        Err("host-deploy INSTANCE contains unsafe characters".to_owned())
    }
}
use crate::process::unix_millis as now_millis;

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
            "guest",
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
            Ok(14)
        );
        let mut duplicate = args.to_vec();
        duplicate[6] = "guest=again.json".to_owned();
        assert!(parse(&duplicate).is_err());
    }

    #[test]
    fn every_configured_vm_needs_exactly_one_attestation_in_the_config_directory() {
        let directory =
            std::env::temp_dir().join(format!("virtio-mem-deploy-test-{}", std::process::id()));
        let source = crate::repo_root()
            .expect("root")
            .join("fixtures/config/host_list.toml");
        let snapshot = snapshot_configuration(&source, &directory).expect("snapshot");
        let original = HostSettings::load(&snapshot).expect("settings");
        let mut settings = original;
        settings.vms[0].vm_name = "a".to_owned();
        settings.vms[0].compatibility_attestation_path =
            "/etc/virtio-mem-host/a.attestation.json".to_owned();
        let mut other = settings.vms[0].clone();
        other.vm_name = "b".to_owned();
        other.compatibility_attestation_path = "/etc/virtio-mem-host/b.attestation.json".to_owned();
        settings.vms.push(other);
        std::fs::remove_dir_all(directory).expect("remove fixture");
        let repo = Path::new("/repo");
        let both = BTreeMap::from([
            ("a".to_owned(), PathBuf::from("a.json")),
            ("b".to_owned(), PathBuf::from("/abs/b.json")),
        ]);
        let paired = pair_attestations(&both, repo, &settings.vms).expect("paired");
        assert_eq!(paired[0].source, Path::new("/repo/a.json"));
        assert_eq!(
            paired[1].target,
            Path::new("/etc/virtio-mem-host/b.attestation.json")
        );

        let mut missing = both.clone();
        missing.remove("b");
        assert!(pair_attestations(&missing, repo, &settings.vms).is_err());
        let mut extra = both.clone();
        extra.insert("c".to_owned(), PathBuf::from("c.json"));
        assert!(pair_attestations(&extra, repo, &settings.vms).is_err());
    }

    #[test]
    fn controller_units_name_a_deployment_instance() {
        assert_eq!(
            controller_instance("virtio-mem-host@pool.service"),
            Ok("pool")
        );
        for unit in [
            "virtio-mem-host@.service",
            "virtio-mem-host@a/b.service",
            "other@pool.service",
            "virtio-mem-host@pool",
        ] {
            assert!(controller_instance(unit).is_err(), "{unit}");
        }
    }

    #[test]
    fn the_only_installed_controller_is_the_default_one() {
        let one =
            "virtio-mem-host@main.service loaded active running Virtio-mem controller for main\n";
        assert_eq!(
            only_controller_unit(one).as_deref(),
            Some("virtio-mem-host@main.service")
        );
        // Several controllers, or none, leave the choice to the caller.
        let two = format!("{one}virtio-mem-host@lab.service loaded inactive dead x\n");
        assert_eq!(only_controller_unit(&two), None);
        assert_eq!(only_controller_unit(""), None);
        assert_eq!(
            only_controller_unit("other@main.service loaded active running x\n"),
            None
        );
    }
}
