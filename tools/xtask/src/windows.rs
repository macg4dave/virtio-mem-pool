use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use virtio_mem_core::settings::{self, VmSettings, CONTROL_PATH, SERVICE_NAME};
use virtio_mem_host::qga::encode_base64;

use crate::build_identity::{
    BuildIdentity, BuildRecord, InstalledBuild, Verdict, RECORD_FILE, STAMP_FILE,
};
use crate::process;

const SERVICE_EXE: &str = "virtio-mem-service.exe";
const WORKLOAD_EXE: &str = "virtio-mem-workload.exe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Check,
    Sync,
    Build,
    Test,
    Lint,
    Fetch,
    All,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Operation),
    Verify {
        expected_fingerprint: String,
        runs: u32,
    },
    Deploy {
        manifest: PathBuf,
        output: PathBuf,
        apply: bool,
    },
    ServiceCycle {
        service: String,
        output: PathBuf,
        apply: bool,
    },
    DiagnoseService {
        service: String,
        output: PathBuf,
    },
    BuildStatus {
        service: String,
        output: PathBuf,
    },
    Uninstall {
        service: String,
        output: PathBuf,
        apply: bool,
    },
}

#[derive(Debug, Serialize)]
struct DeploymentEvidence {
    schema_version: u32,
    applied: bool,
    source_revision: String,
    build_id: String,
    candidate_sha256: String,
    manifest: VmSettings,
    service_config: virtio_mem_core::WindowsServiceConfigFile,
    #[serde(skip_serializing_if = "Option::is_none")]
    windows: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct Config {
    ssh_target: String,
    remote_dir: String,
    artifact_dir: PathBuf,
    known_hosts_file: Option<PathBuf>,
    identity_file: Option<PathBuf>,
    host_name: Option<String>,
    host_key_alias: Option<String>,
    connect_timeout_seconds: u64,
    operation_timeout_seconds: u64,
}

#[derive(Debug)]
struct Toolchain {
    cargo: String,
    rustc: String,
    rustdoc: String,
    cargo_fmt: String,
    cargo_clippy: String,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let operation = args.first().ok_or_else(|| {
        "windows requires check|sync|build|test|lint|fetch|all|verify|deploy|service-cycle|diagnose-service|build-status|uninstall".to_owned()
    })?;
    if operation == "deploy" {
        if args.len() != 4 && args.len() != 5 {
            return Err("windows deploy requires MANIFEST --output PATH [--apply]".to_owned());
        }
        if args[2] != "--output" || args[3].trim().is_empty() {
            return Err("windows deploy requires MANIFEST --output PATH [--apply]".to_owned());
        }
        let apply = args.get(4).map(String::as_str) == Some("--apply");
        if args.len() == 5 && !apply {
            return Err(format!("unknown windows deploy option: {}", args[4]));
        }
        return Ok(Command::Deploy {
            manifest: PathBuf::from(&args[1]),
            output: PathBuf::from(&args[3]),
            apply,
        });
    }
    match operation.as_str() {
        "service-cycle" => {
            let (service, output, apply) = parse_service_command(args, true)?;
            return Ok(Command::ServiceCycle {
                service,
                output,
                apply,
            });
        }
        "diagnose-service" => {
            let (service, output, _) = parse_service_command(args, false)?;
            return Ok(Command::DiagnoseService { service, output });
        }
        "build-status" => {
            let (service, output, _) = parse_service_command(args, false)?;
            return Ok(Command::BuildStatus { service, output });
        }
        "uninstall" => {
            let (service, output, apply) = parse_service_command(args, true)?;
            return Ok(Command::Uninstall {
                service,
                output,
                apply,
            });
        }
        _ => {}
    }
    if operation == "verify" {
        if args.len() != 4 || args[2] != "--runs" {
            return Err("windows verify requires EXPECTED_ED25519_FINGERPRINT --runs N".to_owned());
        }
        let expected_fingerprint = args[1].clone();
        if !valid_fingerprint(&expected_fingerprint) {
            return Err("expected an OpenSSH SHA256 fingerprint".to_owned());
        }
        let runs = args[3]
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "--runs requires a positive integer".to_owned())?;
        return Ok(Command::Verify {
            expected_fingerprint,
            runs,
        });
    }
    if args.len() != 1 {
        return Err(format!(
            "windows {operation} does not accept positional arguments"
        ));
    }
    let operation = match operation.as_str() {
        "check" => Operation::Check,
        "sync" => Operation::Sync,
        "build" => Operation::Build,
        "test" => Operation::Test,
        "lint" => Operation::Lint,
        "fetch" => Operation::Fetch,
        "all" => Operation::All,
        _ => return Err(format!("unknown windows operation: {operation}")),
    };
    Ok(Command::Run(operation))
}

/// Parses `OPERATION SERVICE --output PATH`, plus `[--apply]` when allowed.
fn parse_service_command(
    args: &[String],
    allow_apply: bool,
) -> Result<(String, PathBuf, bool), String> {
    let operation = &args[0];
    let usage = if allow_apply {
        format!("windows {operation} requires SERVICE --output PATH [--apply]")
    } else {
        format!("windows {operation} requires SERVICE --output PATH")
    };
    let apply = match args.get(4).map(String::as_str) {
        None => false,
        Some("--apply") if allow_apply => true,
        Some(option) => return Err(format!("unknown windows {operation} option: {option}")),
    };
    if args.len() > 5
        || args.len() < 4
        || args[2] != "--output"
        || args[1].trim().is_empty()
        || args[3].trim().is_empty()
    {
        return Err(usage);
    }
    validate_ssh_target(&args[1])?;
    Ok((args[1].clone(), PathBuf::from(&args[3]), apply))
}

pub fn run(command: Command, repo: &Path) -> Result<(), String> {
    match command {
        Command::Run(operation) => run_operation(operation, &Config::from_env(repo)?, repo),
        Command::Verify {
            expected_fingerprint,
            runs,
        } => verify(repo, &Config::from_env(repo)?, &expected_fingerprint, runs),
        Command::Deploy {
            manifest,
            output,
            apply,
        } => deploy(repo, &Config::from_env(repo)?, &manifest, &output, apply),
        Command::ServiceCycle {
            service,
            output,
            apply,
        } => service_cycle(repo, &Config::from_env(repo)?, &service, &output, apply),
        Command::DiagnoseService { service, output } => {
            diagnose_service(repo, &Config::from_env(repo)?, &service, &output)
        }
        Command::BuildStatus { service, output } => {
            build_status(repo, &Config::from_env(repo)?, &service, &output)
        }
        Command::Uninstall {
            service,
            output,
            apply,
        } => uninstall(repo, &Config::from_env(repo)?, &service, &output, apply),
    }
}

pub fn run_all(repo: &Path) -> Result<(), String> {
    run_operation(Operation::All, &Config::from_env(repo)?, repo)
}

impl Config {
    fn from_env(repo: &Path) -> Result<Self, String> {
        let ssh_target = std::env::var("VIRTIO_MEM_WINDOWS_SSH").map_err(|_| {
            "VIRTIO_MEM_WINDOWS_SSH is required; refusing to guess a guest".to_owned()
        })?;
        let remote_dir = required_env("VIRTIO_MEM_WINDOWS_DIR")?;
        let artifact_dir = PathBuf::from(required_env("VIRTIO_MEM_WINDOWS_ARTIFACTS")?);
        let artifact_dir = if artifact_dir.is_absolute() {
            artifact_dir
        } else {
            repo.join(artifact_dir)
        };
        let known_hosts_file = optional_path("VIRTIO_MEM_WINDOWS_KNOWN_HOSTS_FILE")?;
        let identity_file = optional_path("VIRTIO_MEM_WINDOWS_IDENTITY_FILE")?;
        let host_name = optional_env("VIRTIO_MEM_WINDOWS_HOST_NAME");
        let host_key_alias = optional_env("VIRTIO_MEM_WINDOWS_HOST_KEY_ALIAS");
        let connect_timeout_seconds = positive_env("VIRTIO_MEM_WINDOWS_CONNECT_TIMEOUT_SECONDS")?;
        let operation_timeout_seconds =
            positive_env("VIRTIO_MEM_WINDOWS_OPERATION_TIMEOUT_SECONDS")?;
        let config = Self {
            ssh_target,
            remote_dir,
            artifact_dir,
            known_hosts_file,
            identity_file,
            host_name,
            host_key_alias,
            connect_timeout_seconds,
            operation_timeout_seconds,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        validate_ssh_target(&self.ssh_target)?;
        if self.host_name.is_some() != self.host_key_alias.is_some() {
            return Err("VIRTIO_MEM_WINDOWS_HOST_NAME and VIRTIO_MEM_WINDOWS_HOST_KEY_ALIAS must be supplied together".to_owned());
        }
        if let Some(value) = &self.host_name {
            validate_ssh_target(value)?;
        }
        if let Some(value) = &self.host_key_alias {
            validate_ssh_target(value)?;
        }
        if self.remote_dir.is_empty()
            || self.remote_dir.chars().any(char::is_control)
            || self
                .remote_dir
                .chars()
                .any(|character| "&|<>^%!\"".contains(character))
        {
            return Err("VIRTIO_MEM_WINDOWS_DIR contains characters unsafe for cmd.exe".to_owned());
        }
        Ok(())
    }

    fn for_qualification(
        repo: &Path,
        ssh_target: &str,
        connect_timeout_seconds: u64,
        operation_timeout_seconds: u64,
    ) -> Result<Self, String> {
        let config = Self {
            ssh_target: ssh_target.to_owned(),
            remote_dir: r"C:\".to_owned(),
            artifact_dir: repo.to_path_buf(),
            known_hosts_file: optional_path("VIRTIO_MEM_WINDOWS_KNOWN_HOSTS_FILE")?,
            identity_file: optional_path("VIRTIO_MEM_WINDOWS_IDENTITY_FILE")?,
            host_name: optional_env("VIRTIO_MEM_WINDOWS_HOST_NAME"),
            host_key_alias: optional_env("VIRTIO_MEM_WINDOWS_HOST_KEY_ALIAS"),
            connect_timeout_seconds,
            operation_timeout_seconds,
        };
        if connect_timeout_seconds == 0 || operation_timeout_seconds == 0 {
            return Err("qualification service-cycle timeouts must be positive".to_owned());
        }
        config.validate()?;
        Ok(config)
    }

    fn ssh_options(&self) -> Vec<OsString> {
        let mut options = vec![
            process::os("-o"),
            process::os("BatchMode=yes"),
            process::os("-o"),
            process::os(format!("ConnectTimeout={}", self.connect_timeout_seconds)),
        ];
        if let Some(path) = &self.known_hosts_file {
            options.extend([
                process::os("-o"),
                process::os(format!("UserKnownHostsFile={}", path.display())),
                process::os("-o"),
                process::os("StrictHostKeyChecking=yes"),
            ]);
        }
        if let Some(path) = &self.identity_file {
            options.extend([
                process::os("-o"),
                process::os("IdentitiesOnly=yes"),
                process::os("-i"),
                path.as_os_str().to_owned(),
            ]);
        }
        if let (Some(host_name), Some(host_key_alias)) = (&self.host_name, &self.host_key_alias) {
            options.extend([
                process::os("-o"),
                process::os(format!("HostName={host_name}")),
                process::os("-o"),
                process::os(format!("HostKeyAlias={host_key_alias}")),
            ]);
        }
        options
    }
}

fn run_operation(operation: Operation, config: &Config, repo: &Path) -> Result<(), String> {
    match operation {
        Operation::Check => check(config, repo),
        Operation::Sync => sync_source(config, repo),
        Operation::Build => build(config, repo),
        Operation::Test => test(config, repo),
        Operation::Lint => lint(config, repo),
        Operation::Fetch => fetch(config, repo),
        Operation::All => {
            check(config, repo)?;
            sync_source(config, repo)?;
            build(config, repo)?;
            test(config, repo)?;
            lint(config, repo)?;
            fetch(config, repo)
        }
    }
}

fn check(config: &Config, repo: &Path) -> Result<(), String> {
    require_commands(&["ssh", "scp", "tar", "sha256sum"])?;
    print_remote(
        config,
        repo,
        r#"where rustup && rustup --version && rustup target list --installed && rustup component list --installed && where tar && where certutil && if not exist "%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" exit /b 1"#,
    )?;
    let toolchain = resolve_toolchain(config, repo)?;
    print_remote_msvc(
        config,
        repo,
        &format!(
            "\"{}\" --version ^&^& \"{}\" --version ^&^& \"{}\" clippy --version ^&^& \"{}\" fmt --version ^&^& where link",
            toolchain.rustc, toolchain.cargo, toolchain.cargo_clippy, toolchain.cargo_fmt
        ),
    )?;
    Ok(())
}

fn sync_source(config: &Config, repo: &Path) -> Result<(), String> {
    require_commands(&["git", "tar", "scp", "ssh"])?;
    let archive = std::env::temp_dir().join(format!(
        "virtio-mem-windows-source-{}.tar",
        std::process::id()
    ));
    let archive_text = archive
        .to_str()
        .ok_or_else(|| "temporary archive path is not valid UTF-8".to_owned())?;
    let result = (|| {
        let listed = process::checked_output(
            "git",
            &[
                process::os("ls-files"),
                process::os("--cached"),
                process::os("--others"),
                process::os("--exclude-standard"),
                process::os("-z"),
            ],
            repo,
        )?;
        let filtered = existing_paths(repo, &listed)?;
        let identity = source_identity(repo, &filtered)?;
        process::run_with_input(
            "tar",
            &["--null", "--files-from=-", "-cf", archive_text],
            repo,
            &filtered,
        )?;
        append_stamp(repo, archive_text, &identity)?;
        let remote_archive = format!("virtio-mem-windows-source-{}.tar", std::process::id());
        print_remote(
            config,
            repo,
            &format!(
                "if not exist {} mkdir {}",
                quoted_remote_dir(config),
                quoted_remote_dir(config)
            ),
        )?;
        let mut scp_args = config.ssh_options();
        scp_args.extend([
            process::os("--"),
            archive.as_os_str().to_owned(),
            process::os(format!("{}:{remote_archive}", config.ssh_target)),
        ]);
        let output = process::bounded_output(
            "scp",
            &scp_args,
            repo,
            Duration::from_secs(config.operation_timeout_seconds),
        )?;
        print_checked(output, "scp source archive")?;
        print_remote(
            config,
            repo,
            &format!(
                "tar -xf {remote_archive} -C {} && del /q {remote_archive}",
                quoted_remote_dir(config)
            ),
        )?;
        println!("Synchronised source as build {}", identity.build_id);
        Ok(())
    })();
    let _ = std::fs::remove_file(&archive);
    result
}

/// Identifies the exact files being synchronised: the commit, whether the
/// working tree differs from it, and a digest of every synchronised file.
fn source_identity(repo: &Path, nul_separated: &[u8]) -> Result<BuildIdentity, String> {
    let commit = process::checked_text(
        "git",
        &[process::os("rev-parse"), process::os("HEAD")],
        repo,
    )?;
    let status = process::checked_output(
        "git",
        &[
            process::os("status"),
            process::os("--porcelain=v1"),
            process::os("--untracked-files=normal"),
        ],
        repo,
    )?;
    let paths = std::str::from_utf8(nul_separated)
        .map_err(|error| format!("git returned a non-UTF-8 path: {error}"))?;
    let mut args = vec![process::os("--")];
    args.extend(
        paths
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(process::os),
    );
    let manifest = process::checked_output("sha256sum", &args, repo)?;
    let manifest_path = std::env::temp_dir().join(format!(
        "virtio-mem-windows-source-{}.sha256",
        std::process::id()
    ));
    std::fs::write(&manifest_path, manifest)
        .map_err(|error| format!("write source manifest: {error}"))?;
    let digest = sha256(repo, &manifest_path);
    // Best effort: the manifest is a derived temporary and the digest is
    // already computed (or its error is returned below).
    let _ = std::fs::remove_file(&manifest_path);
    BuildIdentity::new(commit.trim(), !status.is_empty(), &digest?)
}

/// Adds the build stamp to the source archive so it lands in the remote
/// workspace atomically with the files it describes.
fn append_stamp(repo: &Path, archive: &str, identity: &BuildIdentity) -> Result<(), String> {
    let directory =
        std::env::temp_dir().join(format!("virtio-mem-windows-stamp-{}", std::process::id()));
    let result = (|| {
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("create build stamp directory: {error}"))?;
        let stamp = serde_json::to_string(identity)
            .map_err(|error| format!("encode build stamp: {error}"))?;
        std::fs::write(directory.join(STAMP_FILE), stamp)
            .map_err(|error| format!("write build stamp: {error}"))?;
        let directory_text = directory
            .to_str()
            .ok_or_else(|| "temporary stamp path is not valid UTF-8".to_owned())?;
        process::run(
            "tar",
            &["-rf", archive, "-C", directory_text, STAMP_FILE],
            repo,
        )
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

/// Reads the identity of the source last synchronised to the build guest.
fn remote_identity(config: &Config, repo: &Path) -> Result<BuildIdentity, String> {
    let stamp = remote_stdout(
        config,
        repo,
        &format!("type \"{}\"", remote_path(config, STAMP_FILE)),
    )
    .map_err(|error| format!("{error}; run windows sync before building"))?;
    BuildIdentity::parse_stamp(&stamp)
}

fn remote_path(config: &Config, relative: &str) -> String {
    format!(
        r"{}\{relative}",
        config.remote_dir.trim_end_matches(['\\', '/'])
    )
}

fn release_path(config: &Config, executable: &str) -> String {
    remote_path(config, &format!(r"target\release\{executable}"))
}

fn build(config: &Config, repo: &Path) -> Result<(), String> {
    let identity = remote_identity(config, repo)?;
    let tools = resolve_toolchain(config, repo)?;
    println!("Building Windows service {}", identity.build_id);
    // The build ID is validated by `parse_stamp` and contains only
    // [0-9a-z.-], so it is safe inside the cmd.exe `set` below.
    print_remote_msvc(
        config,
        repo,
        &format!(
            "set \"VIRTIO_MEM_BUILD_ID={}\" ^&^& set \"RUSTC={}\" ^&^& set \"RUSTDOC={}\" ^&^& \"{}\" build -p virtio-mem-service --release --locked",
            identity.build_id, tools.rustc, tools.rustdoc, tools.cargo
        ),
    )?;
    Ok(())
}

fn test(config: &Config, repo: &Path) -> Result<(), String> {
    let tools = resolve_toolchain(config, repo)?;
    print_remote_msvc(
        config,
        repo,
        &format!(
            "set \"RUSTC={}\" ^&^& set \"RUSTDOC={}\" ^&^& \"{}\" test -p virtio-mem-service --all-features --locked",
            tools.rustc, tools.rustdoc, tools.cargo
        ),
    )?;
    Ok(())
}

fn lint(config: &Config, repo: &Path) -> Result<(), String> {
    let tools = resolve_toolchain(config, repo)?;
    print_remote_msvc(
        config,
        repo,
        &format!(
            "\"{}\" fmt --all -- --check ^&^& set \"RUSTC={}\" ^&^& set \"RUSTDOC={}\" ^&^& \"{}\" clippy -p virtio-mem-service --all-targets --all-features --locked -- -D warnings",
            tools.cargo_fmt, tools.rustc, tools.rustdoc, tools.cargo_clippy
        ),
    )?;
    Ok(())
}

/// Fetches the release executables, checks that the candidate was built from
/// the synchronised source and records the verified build in `build.json`.
fn fetch(config: &Config, repo: &Path) -> Result<(), String> {
    require_commands(&["scp", "ssh", "sha256sum"])?;
    std::fs::create_dir_all(&config.artifact_dir).map_err(|error| {
        format!(
            "failed to create artifact directory {}: {error}",
            config.artifact_dir.display()
        )
    })?;
    let identity = remote_identity(config, repo)?;
    let reported = remote_stdout(
        config,
        repo,
        &format!("\"{}\" version", release_path(config, SERVICE_EXE)),
    )?;
    if reported.trim() != identity.build_id {
        return Err(format!(
            "the release candidate reports build {:?} but the synchronised source is {}; run windows build",
            reported.trim(),
            identity.build_id
        ));
    }
    let service_sha256 = fetch_verified(config, repo, SERVICE_EXE)?;
    let workload_sha256 = fetch_verified(config, repo, WORKLOAD_EXE)?;
    let build_id = identity.build_id.clone();
    let record = BuildRecord::new(identity, service_sha256, workload_sha256, unix_millis()?);
    persist_json(&config.artifact_dir.join(RECORD_FILE), &record)?;
    println!(
        "Windows build {build_id} recorded in {}",
        config.artifact_dir.join(RECORD_FILE).display()
    );
    Ok(())
}

/// Copies one release executable and returns its SHA-256 after comparing it
/// with the hash computed on Windows.
fn fetch_verified(config: &Config, repo: &Path, executable: &str) -> Result<String, String> {
    let destination = config.artifact_dir.join(executable);
    let remote_artifact = release_path(config, executable).replace('\\', "/");
    let mut scp_args = config.ssh_options();
    scp_args.extend([
        process::os("--"),
        process::os(format!("{}:{remote_artifact}", config.ssh_target)),
        destination.as_os_str().to_owned(),
    ]);
    print_checked(
        process::bounded_output(
            "scp",
            &scp_args,
            repo,
            Duration::from_secs(config.operation_timeout_seconds),
        )?,
        "scp Windows artifact",
    )?;
    let remote_hash_output = remote_stdout(
        config,
        repo,
        &format!(
            "certutil -hashfile \"{}\" SHA256",
            release_path(config, executable)
        ),
    )?;
    let remote_hash = parse_sha256(&remote_hash_output)
        .ok_or_else(|| "Windows certutil output did not contain a SHA-256 value".to_owned())?;
    let local_hash = sha256(repo, &destination)?;
    if remote_hash != local_hash {
        return Err(format!(
            "{executable} checksum mismatch (Windows={remote_hash}, RHEL={local_hash})"
        ));
    }
    println!(
        "Windows artifact verified: {} ({local_hash})",
        destination.display()
    );
    Ok(local_hash)
}

fn read_record(config: &Config) -> Result<Option<BuildRecord>, String> {
    let path = config.artifact_dir.join(RECORD_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => BuildRecord::parse(&text).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn required_record(config: &Config) -> Result<BuildRecord, String> {
    read_record(config)?.ok_or_else(|| {
        format!(
            "no fetched build record in {}; run windows build and fetch first",
            config.artifact_dir.display()
        )
    })
}

fn resolve_toolchain(config: &Config, repo: &Path) -> Result<Toolchain, String> {
    let cargo = remote_stdout(config, repo, "rustup which cargo")?
        .replace('\r', "")
        .trim()
        .to_owned();
    let suffix = r"\bin\cargo.exe";
    let toolchain_bin = cargo
        .strip_suffix(suffix)
        .ok_or_else(|| format!("unable to resolve the active Windows Cargo toolchain: {cargo}"))?;
    Ok(Toolchain {
        cargo: cargo.clone(),
        rustc: format!(r"{toolchain_bin}\bin\rustc.exe"),
        rustdoc: format!(r"{toolchain_bin}\bin\rustdoc.exe"),
        cargo_fmt: format!(r"{toolchain_bin}\bin\cargo-fmt.exe"),
        cargo_clippy: format!(r"{toolchain_bin}\bin\cargo-clippy.exe"),
    })
}

fn print_remote_msvc(config: &Config, repo: &Path, command: &str) -> Result<String, String> {
    print_remote(
        config,
        repo,
        &format!(
            "for /f \"delims=\" %i in ('\"%ProgramFiles(x86)%\\Microsoft Visual Studio\\Installer\\vswhere.exe\" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find VC\\Auxiliary\\Build\\vcvars64.bat') do @call \"%i\" ^&^& cd /d {} ^&^& {command}",
            quoted_remote_dir(config)
        ),
    )
}

fn print_remote(config: &Config, repo: &Path, command: &str) -> Result<String, String> {
    let output = remote_output(config, repo, command)?;
    let stdout = String::from_utf8(output.stdout.clone())
        .map_err(|error| format!("ssh returned invalid UTF-8 on stdout: {error}"))?;
    print_checked(output, "ssh remote command")?;
    Ok(stdout)
}

fn remote_stdout(config: &Config, repo: &Path, command: &str) -> Result<String, String> {
    let output = remote_output(config, repo, command)?;
    if !output.status.success() {
        return Err(format!(
            "ssh remote command failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("ssh returned invalid UTF-8 on stdout: {error}"))
}

fn remote_output(
    config: &Config,
    repo: &Path,
    command: &str,
) -> Result<std::process::Output, String> {
    let mut args = config.ssh_options();
    args.extend([
        process::os("--"),
        process::os(&config.ssh_target),
        process::os(format!("cmd.exe /d /c {command}")),
    ]);
    process::bounded_output(
        "ssh",
        &args,
        repo,
        Duration::from_secs(config.operation_timeout_seconds),
    )
}

fn quoted_remote_dir(config: &Config) -> String {
    format!("\"{}\"", config.remote_dir)
}

fn verify(
    repo: &Path,
    config: &Config,
    expected_fingerprint: &str,
    runs: u32,
) -> Result<(), String> {
    require_commands(&["ssh", "ssh-keygen", "ssh-keyscan", "sha256sum"])?;
    let timeout = Duration::from_secs(config.operation_timeout_seconds);
    let ssh_config = process::bounded_text(
        "ssh",
        &[
            process::os("-G"),
            process::os("--"),
            process::os(&config.ssh_target),
        ],
        repo,
        timeout,
    )?;
    let host_name = ssh_config_value(&ssh_config, "hostname")
        .ok_or_else(|| "could not resolve SSH hostname".to_owned())?;
    let host_port = ssh_config_value(&ssh_config, "port")
        .ok_or_else(|| "could not resolve SSH port".to_owned())?;
    let known_hosts =
        std::env::temp_dir().join(format!("virtio-mem-known-hosts-{}", std::process::id()));
    let result = (|| {
        let scan = process::bounded_text(
            "ssh-keyscan",
            &[
                process::os("-T"),
                process::os(config.connect_timeout_seconds.to_string()),
                process::os("-p"),
                process::os(&host_port),
                process::os("-t"),
                process::os("ed25519"),
                process::os("--"),
                process::os(&host_name),
            ],
            repo,
            timeout,
        )?;
        std::fs::write(&known_hosts, scan)
            .map_err(|error| format!("failed to write pinned known-hosts file: {error}"))?;
        let fingerprint_output = process::bounded_text(
            "ssh-keygen",
            &[
                process::os("-E"),
                process::os("sha256"),
                process::os("-lf"),
                known_hosts.as_os_str().to_owned(),
            ],
            repo,
            timeout,
        )?;
        let observed = fingerprint_output
            .split_whitespace()
            .find(|field| field.starts_with("SHA256:"))
            .ok_or_else(|| "ssh-keygen did not return a SHA-256 fingerprint".to_owned())?;
        if observed != expected_fingerprint {
            return Err(format!(
                "Windows SSH host fingerprint mismatch (expected={expected_fingerprint}, observed={observed})"
            ));
        }

        let timestamp = unix_millis()?;
        let evidence_dir = config
            .artifact_dir
            .join(format!("verification-{timestamp}"));
        std::fs::create_dir_all(&evidence_dir)
            .map_err(|error| format!("failed to create {}: {error}", evidence_dir.display()))?;
        let summary = evidence_dir.join("summary.txt");
        append_summary(
            &summary,
            &format!(
                "Unix run timestamp (milliseconds): {timestamp}\nWindows SSH target: {}\nVerified Windows SSH host fingerprint: {observed}\nConfigured aggregate runs: {runs}\nEvidence directory: {}\n",
                config.ssh_target,
                evidence_dir.display()
            ),
        )?;
        println!(
            "Verified endpoint; evidence directory: {}",
            evidence_dir.display()
        );

        let mut verified_config = config.clone();
        verified_config.known_hosts_file = Some(known_hosts.clone());
        for run_number in 1..=runs {
            println!("Starting aggregate gate {run_number} of {runs}.");
            crate::local::run(crate::local::Step::Local, repo)?;
            run_operation(Operation::All, &verified_config, repo)?;
            let artifact = config.artifact_dir.join(SERVICE_EXE);
            let hash = sha256(repo, &artifact)?;
            std::fs::write(
                evidence_dir.join(format!("artifact-run-{run_number}.sha256")),
                format!("{hash}  {}\n", artifact.display()),
            )
            .map_err(|error| format!("failed to write artifact hash: {error}"))?;
        }
        append_summary(
            &summary,
            &format!("{runs} configured aggregate gate run(s) passed.\n"),
        )?;
        println!("{runs} configured aggregate gate run(s) passed.");
        Ok(())
    })();
    let _ = std::fs::remove_file(&known_hosts);
    result
}

fn append_summary(path: &Path, value: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    file.write_all(value.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn deploy(
    repo: &Path,
    config: &Config,
    manifest_path: &Path,
    output_path: &Path,
    apply: bool,
) -> Result<(), String> {
    let manifest_path = resolve_local(repo, manifest_path);
    let output_path = resolve_local(repo, output_path);
    let manifest = settings::load_vm(&manifest_path)?;
    let globals = settings::load_globals(manifest_path.parent().unwrap_or(Path::new(".")))?;
    let service_config = manifest.service_config(&globals)?;
    service_config.validate()?;
    service_config.validate_paths(r"C:\ProgramData\VirtioMemService\config.json")?;
    let record = required_record(config)?;

    let candidate_path = release_path(config, SERVICE_EXE);
    let candidate_hash_output = remote_stdout(
        config,
        repo,
        &format!("certutil -hashfile \"{candidate_path}\" SHA256"),
    )?;
    let candidate_sha256 = parse_sha256(&candidate_hash_output)
        .ok_or_else(|| "Windows certutil output did not contain a candidate SHA-256".to_owned())?;
    if candidate_sha256 != record.service_sha256 {
        return Err(format!(
            "the build guest's candidate ({candidate_sha256}) is not the fetched build {} ({}); run windows build and fetch",
            record.identity.build_id, record.service_sha256
        ));
    }
    let source_revision = process::bounded_text(
        "git",
        &[process::os("rev-parse"), process::os("HEAD")],
        repo,
        Duration::from_secs(config.operation_timeout_seconds),
    )?
    .trim()
    .to_owned();

    let windows = if apply {
        let config_base64 = encode_base64(
            serde_json::to_string_pretty(&service_config)
                .map_err(|error| format!("encode Windows service configuration: {error}"))?
                .as_bytes(),
        );
        let script = deployment_powershell(
            &manifest,
            &candidate_path,
            &config_base64,
            service_config.poll_interval_millis,
        );
        let output = remote_powershell(config, repo, &script)?;
        Some(parse_last_json_line(&output)?)
    } else {
        None
    };

    let installed_build_id = windows
        .as_ref()
        .map(|value| value["installed_build_id"].as_str().map(str::to_owned));
    let evidence = DeploymentEvidence {
        schema_version: 2,
        applied: apply,
        source_revision,
        build_id: record.identity.build_id.clone(),
        candidate_sha256,
        manifest,
        service_config,
        windows,
    };
    persist_json(&output_path, &evidence)?;
    println!(
        "Windows deployment {} of build {} evidence written to {}",
        if apply { "apply" } else { "dry-run" },
        record.identity.build_id,
        output_path.display()
    );
    match installed_build_id {
        Some(reported) if reported.as_deref() != Some(record.identity.build_id.as_str()) => {
            Err(format!(
                "installed service reports build {reported:?}, expected {}",
                record.identity.build_id
            ))
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Serialize)]
struct BuildStatusEvidence {
    schema_version: u32,
    captured_unix_millis: u64,
    service_name: String,
    verdict: Verdict,
    service_state: Option<String>,
    installed: Option<InstalledBuild>,
    candidate: Option<BuildRecord>,
}

/// Compares the installed service binary with the last fetched candidate.
fn build_status(repo: &Path, config: &Config, service: &str, output: &Path) -> Result<(), String> {
    let candidate = read_record(config)?;
    let script = format!(
        r#"{PS_BUILD_FUNCTIONS}
$ErrorActionPreference='Stop'
$service={service}
$svc=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
if($null -eq $svc){{[ordered]@{{state=$null;installed=$null}} | ConvertTo-Json -Compress; exit 0}}
$exe=Get-ServiceExe $svc
if(-not (Test-Path -LiteralPath $exe)){{throw ('registered service executable is missing: '+$exe)}}
[ordered]@{{state=[string]$svc.State;installed=[ordered]@{{path=$exe;sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $exe).Hash.ToLowerInvariant();build_id=(Get-BuildId $exe)}}}} | ConvertTo-Json -Compress
"#,
        service = ps_literal(service),
    );
    let value = parse_last_json_line(&remote_powershell(config, repo, &script)?)?;
    let installed: Option<InstalledBuild> = serde_json::from_value(value["installed"].clone())
        .map_err(|error| format!("parse installed build: {error}"))?;
    let verdict = Verdict::of(installed.as_ref(), candidate.as_ref());
    let evidence = BuildStatusEvidence {
        schema_version: 1,
        captured_unix_millis: unix_millis()?,
        service_name: service.to_owned(),
        verdict,
        service_state: value["state"].as_str().map(str::to_owned),
        installed,
        candidate,
    };
    let output = resolve_local(repo, output);
    persist_json(&output, &evidence)?;
    println!("{}", describe_build_status(&evidence));
    println!("Build status written to {}", output.display());
    Ok(())
}

fn describe_build_status(status: &BuildStatusEvidence) -> String {
    let installed = match &status.installed {
        None => "installed: none".to_owned(),
        Some(build) => format!(
            "installed: {} ({}, {}) at {}",
            build
                .build_id
                .as_deref()
                .unwrap_or("unknown build, predates build tracking"),
            build.sha256,
            status.service_state.as_deref().unwrap_or("unknown state"),
            build.path
        ),
    };
    let candidate = match &status.candidate {
        None => "candidate: none fetched".to_owned(),
        Some(record) => format!(
            "candidate: {} ({})",
            record.identity.build_id, record.service_sha256
        ),
    };
    let verdict = match status.verdict {
        Verdict::NotInstalled => "not installed; deploy the candidate with windows deploy",
        Verdict::Current => "current; the installed service is the fetched candidate",
        Verdict::UpdateAvailable => {
            "update available; the candidate differs from the installed service (windows deploy)"
        }
        Verdict::NoCandidate => "no candidate; run windows build and fetch",
    };
    format!("{installed}\n{candidate}\nverdict: {verdict}")
}

/// Removes the named service registration, its executable and configuration
/// after backing them up. Publication and request-state files are retained
/// and listed; they belong to the guest's durable request history.
fn uninstall(
    repo: &Path,
    config: &Config,
    service: &str,
    output: &Path,
    apply: bool,
) -> Result<(), String> {
    let script = format!(
        r#"{PS_BUILD_FUNCTIONS}
$ErrorActionPreference='Stop'
$service={service}
$apply={apply}
$root='C:\ProgramData\VirtioMemService'
$config=Join-Path $root 'config.json'
$svc=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
if($null -eq $svc){{throw 'named Windows service was not found'}}
$exe=Get-ServiceExe $svc
$exeExists=Test-Path -LiteralPath $exe
$before=[ordered]@{{state=[string]$svc.State;path=$exe;sha256=$(if($exeExists){{(Get-FileHash -Algorithm SHA256 -LiteralPath $exe).Hash.ToLowerInvariant()}}else{{$null}});build_id=$(if($exeExists){{Get-BuildId $exe}}else{{$null}})}}
$backup=$null
$removed=@()
if($apply){{
  $backup=Join-Path $root ('deployment-backups\uninstall-'+[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
  New-Item -ItemType Directory -Force -Path $backup | Out-Null
  if($exeExists){{Copy-Item -LiteralPath $exe -Destination (Join-Path $backup 'service.exe') -Force}}
  if(Test-Path -LiteralPath $config){{Copy-Item -LiteralPath $config -Destination (Join-Path $backup 'config.json') -Force}}
  & reg.exe export ('HKLM\SYSTEM\CurrentControlSet\Services\'+$service) (Join-Path $backup 'service.reg') /y | Out-Null
  if($LASTEXITCODE -ne 0){{throw "service registry backup failed with exit code $LASTEXITCODE"}}
  if($svc.State -ne 'Stopped'){{Stop-Service -Name $service -Force; (Get-Service $service).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30))}}
  & sc.exe delete $service | Out-Null
  if($LASTEXITCODE -ne 0){{throw "sc.exe delete failed with exit code $LASTEXITCODE"}}
  $deadline=(Get-Date).AddSeconds(30)
  while($null -ne (Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'"))){{if((Get-Date) -gt $deadline){{throw 'service registration is still present after deletion; close open service handles'}}; Start-Sleep -Milliseconds 250}}
  if($exeExists){{Remove-Item -LiteralPath $exe -Force; $removed+=$exe; $dir=Split-Path -Parent $exe; if(@(Get-ChildItem -LiteralPath $dir -Force).Count -eq 0){{Remove-Item -LiteralPath $dir -Force; $removed+=$dir}}}}
  if(Test-Path -LiteralPath $config){{Remove-Item -LiteralPath $config -Force; $removed+=$config}}
}}
$retained=@(if(Test-Path -LiteralPath $root){{Get-ChildItem -LiteralPath $root -File | ForEach-Object {{$_.FullName}}}})
[ordered]@{{captured_unix_millis=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();applied=$apply;service_name=$service;before=$before;backup_path=$backup;removed=$removed;retained_files=$retained}} | ConvertTo-Json -Depth 4 -Compress
"#,
        service = ps_literal(service),
        apply = if apply { "$true" } else { "$false" },
    );
    let value = parse_last_json_line(&remote_powershell(config, repo, &script)?)?;
    let output = resolve_local(repo, output);
    persist_json(&output, &value)?;
    println!(
        "Windows uninstall {} of {} (build {}) evidence written to {}",
        if apply { "apply" } else { "dry-run" },
        service,
        value["before"]["build_id"]
            .as_str()
            .unwrap_or("unknown, predates build tracking"),
        output.display()
    );
    Ok(())
}

/// PowerShell helpers shared by deployment, build status and uninstall.
/// `Get-BuildId` runs `EXE version` with a bound: binaries that predate the
/// command reject it, and any other answer is reported as unknown (`$null`).
const PS_BUILD_FUNCTIONS: &str = r#"function Get-ServiceExe($svc){
  $raw=([string]$svc.PathName).Trim()
  if($raw.StartsWith('"')){return $raw.Substring(1,$raw.IndexOf('"',1)-1)}
  $end=$raw.ToLowerInvariant().IndexOf('.exe')
  if($end -lt 0){throw 'service path does not name an .exe'}
  return $raw.Substring(0,$end+4)
}
function Get-BuildIdOnce($exe){
  $out=[IO.Path]::GetTempFileName()
  try{
    $p=Start-Process -FilePath $exe -ArgumentList 'version' -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError ($out+'.err')
    $null=$p.Handle
    if(-not $p.WaitForExit(20000)){$p.Kill(); return $null}
    if($p.ExitCode -ne 0){return $null}
    $id=[string](Get-Content -LiteralPath $out -Raw)
    $id=$id.Trim()
    if($id -cmatch '^[0-9a-z.\-]{1,64}$'){return $id}
    return $null
  } finally {Remove-Item -LiteralPath $out,($out+'.err') -Force -ErrorAction SilentlyContinue}
}
function Get-BuildId($exe){
  # The first run of a freshly copied binary can be slow (e.g. scanned), so
  # retry before reporting an unknown build.
  for($attempt=0; $attempt -lt 3; $attempt++){
    $id=Get-BuildIdOnce $exe
    if($null -ne $id){return $id}
    Start-Sleep -Seconds 2
  }
  return $null
}
"#;

fn service_cycle(
    repo: &Path,
    config: &Config,
    service: &str,
    output: &Path,
    apply: bool,
) -> Result<(), String> {
    let value = service_cycle_value(repo, config, service, apply)?;
    let output = resolve_local(repo, output);
    persist_json(&output, &value)?;
    println!(
        "Windows service-cycle {} evidence written to {}",
        if apply { "apply" } else { "dry-run" },
        output.display()
    );
    Ok(())
}

pub(crate) fn apply_service_cycle_for_qualification(
    repo: &Path,
    ssh_target: &str,
    service: &str,
    connect_timeout_seconds: u64,
    operation_timeout_seconds: u64,
) -> Result<serde_json::Value, String> {
    let config = Config::for_qualification(
        repo,
        ssh_target,
        connect_timeout_seconds,
        operation_timeout_seconds,
    )?;
    service_cycle_value(repo, &config, service, true)
}

fn service_cycle_value(
    repo: &Path,
    config: &Config,
    service: &str,
    apply: bool,
) -> Result<serde_json::Value, String> {
    let service = ps_literal(service);
    let action = if apply {
        "if($before.State -ne 'Stopped'){Stop-Service -Name $service -Force; (Get-Service $service).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30))}; Start-Service -Name $service; (Get-Service $service).WaitForStatus('Running',[TimeSpan]::FromSeconds(30))"
    } else {
        ""
    };
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$service={service}
$before=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
if($null -eq $before){{throw 'named Windows service was not found'}}
$beforeState=[string]$before.State
try {{{action}}} catch {{if($beforeState -eq 'Running'){{Start-Service -Name $service -ErrorAction SilentlyContinue}}; throw}}
$after=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
[ordered]@{{captured_unix_millis=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();applied={apply};service_name=$after.Name;display_name=$after.DisplayName;before_state=$beforeState;after_state=$after.State;start_mode=$after.StartMode;account=$after.StartName;path=$after.PathName}} | ConvertTo-Json -Compress
"#,
        apply = if apply { "$true" } else { "$false" },
    );
    parse_last_json_line(&remote_powershell(config, repo, &script)?)
}

fn diagnose_service(
    repo: &Path,
    config: &Config,
    service: &str,
    output: &Path,
) -> Result<(), String> {
    let service = ps_literal(service);
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$service={service}
$svc=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
if($null -eq $svc){{throw 'named Windows service was not found'}}
$since=(Get-Date).AddHours(-24)
$application=@(Get-WinEvent -FilterHashtable @{{LogName='Application';StartTime=$since}} -ErrorAction SilentlyContinue | Where-Object {{$_.ProviderName -eq $service}} | Select-Object -First 50 | ForEach-Object {{[ordered]@{{time_created=$_.TimeCreated;id=$_.Id;level=$_.LevelDisplayName;provider=$_.ProviderName;message=$_.Message;data=@($_.Properties | ForEach-Object {{$_.Value}})}}}})
$system=@(Get-WinEvent -FilterHashtable @{{LogName='System';ProviderName='Service Control Manager';StartTime=$since}} -ErrorAction SilentlyContinue | Where-Object {{$_.Message -like ('*'+$service+'*')}} | Select-Object -First 50 | ForEach-Object {{[ordered]@{{time_created=$_.TimeCreated;id=$_.Id;level=$_.LevelDisplayName;provider=$_.ProviderName;message=$_.Message;data=@($_.Properties | ForEach-Object {{$_.Value}})}}}})
$failureActions=(& sc.exe qfailure $service 2>&1 | Out-String)
[ordered]@{{captured_unix_millis=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();service_name=$svc.Name;state=$svc.State;status=$svc.Status;exit_code=$svc.ExitCode;service_specific_exit_code=$svc.ServiceSpecificExitCode;process_id=$svc.ProcessId;start_mode=$svc.StartMode;account=$svc.StartName;path=$svc.PathName;failure_actions=$failureActions;application_events=$application;system_events=$system}} | ConvertTo-Json -Depth 6 -Compress
"#,
    );
    let value = parse_last_json_line(&remote_powershell(config, repo, &script)?)?;
    let output = resolve_local(repo, output);
    persist_json(&output, &value)?;
    println!(
        "Windows service diagnostics written to {}",
        output.display()
    );
    Ok(())
}

fn parse_last_json_line(output: &str) -> Result<serde_json::Value, String> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| {
            let bounded = output.chars().take(1_024).collect::<String>();
            format!("Windows deployment did not return JSON evidence: {bounded:?}")
        })
}

fn deployment_powershell(
    manifest: &VmSettings,
    candidate_path: &str,
    config_base64: &str,
    poll_interval_millis: u64,
) -> String {
    let service = ps_literal(SERVICE_NAME);
    let candidate = ps_literal(candidate_path);
    let install = ps_literal(manifest.windows_install().map_or("", |(path, _)| path));
    let telemetry = ps_literal(CONTROL_PATH);
    let wait_millis = poll_interval_millis.saturating_mul(3).max(1000);
    format!(
        r#"{PS_BUILD_FUNCTIONS}
$ErrorActionPreference='Stop'
$service={service}
$candidate={candidate}
$install={install}
$telemetry={telemetry}
$config='C:\ProgramData\VirtioMemService\config.json'
$root='C:\ProgramData\VirtioMemService'
$backup=Join-Path $root ('deployment-backups\'+[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
$prior=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'") -ErrorAction SilentlyContinue
$priorRunning=($null -ne $prior -and $prior.State -eq 'Running')
New-Item -ItemType Directory -Force -Path $backup | Out-Null
if(Test-Path $install){{Copy-Item -LiteralPath $install -Destination (Join-Path $backup 'service.exe') -Force}}
if(Test-Path $config){{Copy-Item -LiteralPath $config -Destination (Join-Path $backup 'config.json') -Force}}
if($null -ne $prior){{& reg.exe export ('HKLM\SYSTEM\CurrentControlSet\Services\'+$service) (Join-Path $backup 'service.reg') /y | Out-Null}}
# Refuse unsupported automatic migrations before stopping or replacing a service.
# Original config, binary and registry have already been retained above.
function Assert-LegacySetting($old,$new,[string]$name) {{
  if($null -eq $old -or $null -eq $new) {{throw ("Cannot migrate missing setting "+$name+"; retain and review the old configuration")}}
  if($new -is [System.Management.Automation.PSCustomObject]) {{
    foreach($property in $new.PSObject.Properties) {{Assert-LegacySetting $old.($property.Name) $property.Value ($name+'.'+$property.Name)}}
  }} elseif([string]$old -cne [string]$new) {{throw ("Legacy setting "+$name+" differs; migrate the custom setting/state explicitly before deployment")}}
}}
if(Test-Path $config) {{
  $oldConfig=Get-Content -LiteralPath $config -Raw | ConvertFrom-Json
  $newConfig=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{config_base64}')) | ConvertFrom-Json
  if($oldConfig.schema_version -ne $newConfig.schema_version) {{
    foreach($field in @('vm_name','service_name','service_account','demand_report_path','poll_interval_millis','shutdown_timeout_millis','policy')) {{
      Assert-LegacySetting $oldConfig.$field $newConfig.$field $field
    }}
  }}
}}
try {{
  if($null -ne $prior){{if($prior.State -ne 'Stopped'){{Stop-Service -Name $service -Force; (Get-Service $service).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30))}}; & sc.exe delete $service | Out-Null; Start-Sleep -Milliseconds 500}}
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $install),$root | Out-Null
  Copy-Item -LiteralPath $candidate -Destination $install -Force
  [IO.File]::WriteAllBytes($config,[Convert]::FromBase64String('{config_base64}'))
  & $install install
  if($LASTEXITCODE -ne 0){{throw "candidate install failed with exit code $LASTEXITCODE"}}
  {wake_access}
  $wakeAccess=Set-VirtioMemWakeChannelAccess -BackupDirectory $backup
  & $install start
  if($LASTEXITCODE -ne 0){{throw "candidate start failed with exit code $LASTEXITCODE"}}
  (Get-Service $service).WaitForStatus('Running',[TimeSpan]::FromSeconds(30))
  Start-Sleep -Milliseconds {wait_millis}
  $first=Get-Content -LiteralPath $telemetry -Raw | ConvertFrom-Json
  Start-Sleep -Milliseconds {wait_millis}
  $second=Get-Content -LiteralPath $telemetry -Raw | ConvertFrom-Json
  if($second.sequence -le $first.sequence){{throw 'telemetry sequence did not advance'}}
  if($second.vm_name -ne {vm} -or $second.service_name -ne $service){{throw 'telemetry identity mismatch'}}
  $installed=Get-CimInstance Win32_Service -Filter ("Name='"+$service.Replace("'","''")+"'")
  [ordered]@{{
    captured_unix_millis=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    backup_path=$backup
    binary_sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $install).Hash.ToLowerInvariant()
    installed_build_id=(Get-BuildId $install)
    config_sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $config).Hash.ToLowerInvariant()
    binary_acl=(Get-Acl -LiteralPath $install).Sddl
    config_acl=(Get-Acl -LiteralPath $config).Sddl
    telemetry_acl=(Get-Acl -LiteralPath $telemetry).Sddl
    wake_access=$wakeAccess
    service_name=$installed.Name
    service_state=$installed.State
    service_start_mode=$installed.StartMode
    service_account=$installed.StartName
    service_path=$installed.PathName
    service_error_control=$installed.ErrorControl
    first_session_id=$first.session_id
    first_sequence=$first.sequence
    second_session_id=$second.session_id
    second_sequence=$second.sequence
  }} | ConvertTo-Json -Compress
}} catch {{
  try {{Stop-Service -Name $service -Force -ErrorAction SilentlyContinue; & sc.exe delete $service | Out-Null}} catch {{}}
  if(Test-Path (Join-Path $backup 'service.exe')){{Copy-Item -LiteralPath (Join-Path $backup 'service.exe') -Destination $install -Force}}
  if(Test-Path (Join-Path $backup 'config.json')){{Copy-Item -LiteralPath (Join-Path $backup 'config.json') -Destination $config -Force}} else {{Remove-Item -LiteralPath $config -Force -ErrorAction SilentlyContinue}}
  if(Test-Path (Join-Path $backup 'service.reg')){{& reg.exe import (Join-Path $backup 'service.reg') | Out-Null; if($priorRunning){{Start-Service -Name $service}}}}
  throw
}}"#,
        vm = ps_literal(&manifest.vm_name),
        wake_access = include_str!("../../../tools/windows/Set-WakeChannelAccess.ps1"),
    )
}

fn remote_powershell(config: &Config, repo: &Path, script: &str) -> Result<String, String> {
    let nonce = format!("{}-{}", std::process::id(), unix_millis()?);
    let local = std::env::temp_dir().join(format!("virtio-mem-deploy-{nonce}.ps1"));
    let remote = format!("virtio-mem-deploy-{nonce}.ps1");
    std::fs::write(&local, script)
        .map_err(|error| format!("write temporary PowerShell payload: {error}"))?;
    let result = (|| {
        let mut copy_args = config.ssh_options();
        copy_args.extend([
            process::os("--"),
            local.as_os_str().to_owned(),
            process::os(format!("{}:{remote}", config.ssh_target)),
        ]);
        let copy = process::bounded_output(
            "scp",
            &copy_args,
            repo,
            Duration::from_secs(config.operation_timeout_seconds),
        )?;
        if !copy.status.success() {
            return Err(format!(
                "copy temporary PowerShell payload failed with status {}: {}",
                copy.status,
                String::from_utf8_lossy(&copy.stderr).trim()
            ));
        }
        remote_stdout(
            config,
            repo,
            &format!("powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {remote}"),
        )
    })();
    let _ = remote_output(config, repo, &format!("del /q {remote}"));
    let _ = std::fs::remove_file(local);
    result
}

use crate::process::unix_millis;

fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn resolve_local(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

fn persist_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("deployment evidence path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create deployment evidence directory: {error}"))?;
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize deployment evidence: {error}"))?;
    bytes.push(b'\n');
    let temporary = parent.join(format!(".windows-deployment-{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("write deployment evidence: {error}"))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("commit deployment evidence: {error}"))
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn positive_env(name: &str) -> Result<u64, String> {
    let value = required_env(name)?;
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn optional_path(name: &str) -> Result<Option<PathBuf>, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if !path.is_file() {
        return Err(format!("{name} does not exist: {}", path.display()));
    }
    Ok(Some(path))
}

fn require_commands(commands: &[&str]) -> Result<(), String> {
    let missing: Vec<_> = commands
        .iter()
        .copied()
        .filter(|command| !process::command_exists(command))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing required command(s): {}",
            missing.join(", ")
        ))
    }
}

fn validate_ssh_target(value: &str) -> Result<(), String> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_control) {
        Err("SSH target must be an alias or USER@HOST, not an option or command".to_owned())
    } else {
        Ok(())
    }
}

fn valid_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("SHA256:")
        .filter(|digest| !digest.is_empty())
        .is_some_and(|digest| {
            digest
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+/=".contains(character))
        })
}

fn ssh_config_value(input: &str, key: &str) -> Option<String> {
    input.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(key))
            .then(|| fields.next().map(str::to_owned))
            .flatten()
    })
}

fn parse_sha256(input: &str) -> Option<String> {
    input
        .lines()
        .map(str::trim)
        .find(|line| {
            line.len() == 64 && line.chars().all(|character| character.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
}

fn existing_paths(repo: &Path, nul_separated: &[u8]) -> Result<Vec<u8>, String> {
    let input = std::str::from_utf8(nul_separated)
        .map_err(|error| format!("git returned a non-UTF-8 path: {error}"))?;
    let mut output = Vec::with_capacity(nul_separated.len());
    for relative in input.split('\0').filter(|path| !path.is_empty()) {
        if std::fs::symlink_metadata(repo.join(relative)).is_ok() {
            output.extend_from_slice(relative.as_bytes());
            output.push(0);
        }
    }
    if output.is_empty() {
        return Err("Git did not report any existing source files to synchronize".to_owned());
    }
    Ok(output)
}

fn sha256(repo: &Path, path: &Path) -> Result<String, String> {
    let output = process::checked_text("sha256sum", &[path.as_os_str().to_owned()], repo)?;
    output
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64)
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum returned malformed output".to_owned())
}

fn print_checked(output: std::process::Output, label: &str) -> Result<(), String> {
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{label} failed with status {}", output.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_operations_and_explicit_verification_count() {
        assert_eq!(parse(&strings(&["all"])), Ok(Command::Run(Operation::All)));
        assert!(matches!(
            parse(&strings(&["verify", "SHA256:abc+", "--runs", "2"])),
            Ok(Command::Verify { runs: 2, .. })
        ));
        assert!(matches!(
            parse(&strings(&[
                "deploy",
                "deployment.json",
                "--output",
                "evidence.json",
                "--apply"
            ])),
            Ok(Command::Deploy { apply: true, .. })
        ));
    }

    #[test]
    fn parses_build_status_and_dry_run_first_uninstall() {
        assert_eq!(
            parse(&strings(&[
                "build-status",
                "VirtioMemService",
                "--output",
                "status.json"
            ])),
            Ok(Command::BuildStatus {
                service: "VirtioMemService".to_owned(),
                output: PathBuf::from("status.json"),
            })
        );
        assert!(matches!(
            parse(&strings(&[
                "uninstall",
                "VirtioMemService",
                "--output",
                "u.json"
            ])),
            Ok(Command::Uninstall { apply: false, .. })
        ));
        assert!(matches!(
            parse(&strings(&[
                "uninstall",
                "VirtioMemService",
                "--output",
                "u.json",
                "--apply"
            ])),
            Ok(Command::Uninstall { apply: true, .. })
        ));
        assert!(parse(&strings(&[
            "build-status",
            "VirtioMemService",
            "--output",
            "s.json",
            "--apply"
        ]))
        .is_err());
        assert!(parse(&strings(&[
            "uninstall",
            "-oProxyCommand",
            "--output",
            "u.json"
        ]))
        .is_err());
        assert!(parse(&strings(&["uninstall", "VirtioMemService"])).is_err());
    }

    #[test]
    fn rejects_unsafe_remote_inputs() {
        assert!(parse(&strings(&["unknown"])).is_err());
        assert!(parse(&strings(&["verify", "md5:abc", "--runs", "1"])).is_err());
        assert!(parse(&strings(&["verify", "SHA256:abc", "--runs", "0"])).is_err());
    }

    #[test]
    fn parses_tool_outputs_without_accepting_noise() {
        assert_eq!(
            parse_sha256("SHA256 hash of file:\r\nABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789\r\n"),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_owned())
        );
        assert_eq!(
            ssh_config_value("hostname host.example\nport 22\n", "port"),
            Some("22".to_owned())
        );
    }

    #[test]
    fn filters_working_tree_deletions_from_git_inventory() {
        let root =
            std::env::temp_dir().join(format!("virtio-mem-xtask-sync-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        std::fs::write(root.join("present"), "fixture").expect("write fixture");
        let filtered = existing_paths(&root, b"present\0deleted\0").expect("filter paths");
        std::fs::remove_file(root.join("present")).expect("remove fixture file");
        std::fs::remove_dir(root).expect("remove fixture directory");
        assert_eq!(filtered, b"present\0");
    }

    #[test]
    fn tracked_vm_deploys_without_optional_telemetry() {
        let path = crate::repo_root()
            .expect("repo")
            .join("deploy/config_files/win11_gpu_settings.toml");
        let vm = settings::load_vm(&path).expect("VM settings");
        let globals = settings::load_globals(path.parent().unwrap()).expect("globals");
        let wire = vm.service_config(&globals).unwrap();
        wire.validate().expect("valid service config");
        wire.validate_paths(r"C:\ProgramData\VirtioMemService\config.json")
            .expect("distinct paths");
        assert!(wire.telemetry_path.is_none());
        assert_eq!(wire.demand_report_path, CONTROL_PATH);
        let script = deployment_powershell(&vm, "candidate.exe", "e30=", wire.poll_interval_millis);
        assert!(script.contains(CONTROL_PATH));
        assert!(script.contains("deployment-backups"));
        assert!(script.contains("Assert-LegacySetting"));
    }

    #[test]
    fn powershell_payload_encoding_is_standard_base64() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(ps_literal("a'b"), "'a''b'");
        assert_eq!(
            parse_last_json_line("service installed\n{\"ok\":true}\n").expect("last JSON line")
                ["ok"],
            true
        );
    }
}
