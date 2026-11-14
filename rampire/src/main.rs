//! `rampire`: the controller daemon and every operator command, defined once
//! here with clap so `--help`, completions and man pages share one source
//! (decision D16).

mod daemon;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rampire_core::{ControlHealth, ControllerStatusSnapshot};
use rampire_host::cli::Target;
use rampire_host::config::HostSettings;
use rampire_ops::process;
use serde::Serialize;

/// The account the controller runs as. Recovery writes the controller's
/// state as this account, so the daemon can still write it afterwards.
const SERVICE_ACCOUNT: &str = "rampire";
/// The build this binary came from, stamped by the build (`RAMPIRE_BUILD_ID`).
const BUILD_ID: &str = match option_env!("RAMPIRE_BUILD_ID") {
    Some(id) => id,
    None => "unversioned",
};

/// Exit codes, the same for every command.
mod exit {
    /// Done, or nothing to report.
    pub const SUCCESS: u8 = 0;
    /// The command failed, or `check` found a problem.
    pub const FAILURE: u8 = 1;
    /// The command line was wrong (clap's own code).
    pub const USAGE: u8 = 2;
    /// `apply --detailed-exitcode`: the applied configuration changed.
    pub const CHANGED: u8 = 2;
}

rampire_core::schema_name! {
    /// `rampire status --json`.
    struct StatusReportSchema = "rampire.status-report";
}

rampire_core::schema_name! {
    /// `rampire check --json`.
    struct CheckSchema = "rampire.check";
}

rampire_core::schema_name! {
    /// `rampire doctor --json`.
    struct DoctorSchema = "rampire.doctor";
}

/// `rampire doctor --json`.
#[derive(Debug, Serialize)]
struct DoctorReport {
    schema: DoctorSchema,
    version: u16,
    passed: bool,
    findings: Vec<rampire_ops::doctor::Finding>,
}

/// `rampire status --json`: every VM's controller status.
#[derive(Debug, Serialize)]
struct StatusReport {
    schema: StatusReportSchema,
    version: u16,
    vms: Vec<ControllerStatusSnapshot>,
}

/// `rampire check --json`.
#[derive(Debug, Serialize)]
struct CheckReport {
    schema: CheckSchema,
    version: u16,
    passed: bool,
    vms: Vec<CheckedVm>,
}

#[derive(Debug, Serialize)]
struct CheckedVm {
    vm_name: String,
    problem: Option<String>,
}

/// The draft people and tools edit.
const DRAFT: &str = "/etc/rampire";

/// The configuration to change or check: `--config`, or the draft.
fn draft(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| PathBuf::from(DRAFT))
}

/// The configuration the controller runs: `--config`, or the applied copy,
/// or the draft before anything has been applied.
fn running(cli: &Cli) -> PathBuf {
    if let Some(config) = &cli.config {
        return config.clone();
    }
    let current = applied_root().join(rampire_ops::applied::CURRENT);
    if current.exists() {
        current
    } else {
        PathBuf::from(DRAFT)
    }
}

fn version() -> String {
    format!("{} (build {BUILD_ID})", env!("CARGO_PKG_VERSION"))
}

/// Share a pool of RAM between the VMs on a KVM host.
#[derive(Debug, Parser)]
#[command(name = "rampire", version = version(), propagate_version = true)]
struct Cli {
    /// The configuration directory. Default: /etc/rampire, the draft, for
    /// commands that change or check the configuration; the applied copy
    /// the controller runs for the views and recovery.
    #[arg(long, global = true, value_name = "DIR", env = "RAMPIRE_CONFIG")]
    config: Option<PathBuf>,
    /// The libvirt connection; default the configuration's `libvirt_uri`.
    #[arg(long, global = true, value_name = "URI")]
    connect: Option<String>,
    /// Answer yes to every question, as a script does.
    #[arg(long, global = true)]
    yes: bool,
    /// Write a versioned JSON document instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the controller. The systemd unit runs this.
    Daemon,
    /// Set up a new host: suggest global.toml from the host's memory and
    /// its VMs, create the configuration folders and apply.
    ///
    /// An existing global.toml is never changed; the suggestion is shown
    /// instead.
    Init,
    /// Show the controller's status for one VM, or for every VM.
    Status {
        /// The VM; default every VM.
        vm: Option<String>,
    },
    /// Watch the pool shared by every VM, live.
    Top {
        #[command(flatten)]
        view: View,
        /// Pool events shown.
        #[arg(long, value_name = "N", default_value_t = 8)]
        events: usize,
    },
    /// Watch one VM live.
    Watch {
        /// The VM; needed when there is more than one.
        vm: Option<String>,
        #[command(flatten)]
        view: View,
        /// Journal events shown.
        #[arg(long, value_name = "N", default_value_t = 6)]
        events: usize,
        /// Seconds between guest agent reads.
        #[arg(long, value_name = "SECONDS", default_value_t = 1)]
        guest_interval_seconds: u64,
        /// Skip guest agent reads.
        #[arg(long)]
        no_guest: bool,
    },
    /// Check the configuration and every VM's attestation against the live
    /// VMs; changes nothing.
    Check,
    /// Show what `apply` would change: the draft against the applied copy.
    Diff,
    /// Check the draft and, if it passes, make it what the controller runs.
    ///
    /// The draft (`--config`, normally /etc/rampire) is copied, the copy is
    /// checked like `check` while the controller keeps running, and only
    /// then does the controller restart on it. A draft that fails changes
    /// nothing.
    Apply {
        /// Exit 0 when nothing changed, 2 when something changed and 1 on
        /// failure, for configuration management.
        #[arg(long)]
        detailed_exitcode: bool,
    },
    /// Go back to the applied copy before the current one, through the same
    /// checks as `apply`.
    Rollback,
    /// Change settings in the draft, keeping its comments, then apply.
    ///
    /// TARGET is a VM's name, or `global` for global.toml. Each change is
    /// KEY=VALUE, for example `priority=150` or `max_ram=12GB`. Nothing is
    /// written unless the changed file is valid and the whole configuration
    /// still loads.
    Set {
        /// A VM's name, or `global`.
        target: String,
        /// KEY=VALUE, one or more.
        #[arg(required = true, value_name = "KEY=VALUE")]
        changes: Vec<String>,
        /// Change the draft only; apply later.
        #[arg(long)]
        no_apply: bool,
    },
    /// Check what the host needs for RAMpire, with the fix for each problem.
    Doctor,
    /// Show the controller's journal.
    Logs {
        /// Keep showing new entries.
        #[arg(short, long)]
        follow: bool,
        /// Start from this time, for example "1 hour ago".
        #[arg(long, value_name = "TIME")]
        since: Option<String>,
    },
    /// Clear a VM's latch once a person has checked it is safe to resume.
    ///
    /// Stops the controller, settles a stuck removal at what the guest
    /// kept, records the reason and starts the controller again.
    Recover {
        /// The latched VM.
        vm: String,
        /// Why it is safe to resume, recorded in the VM's state.
        reason: String,
    },
    /// Add, attest and remove the VMs RAMpire manages.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    /// Recovery's own step, run as the service account.
    #[command(hide = true)]
    ClearLatch {
        vm: String,
        reason: String,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
enum VmCommand {
    /// Attest a running VM again from its review profile, then apply.
    ///
    /// Needed after any change to the VM's definition: a VM whose live
    /// evidence has drifted from its attestation is never re-attested
    /// without a person (decision D11).
    Attest {
        /// The VM; or --drifted for every VM whose evidence has drifted.
        #[arg(required_unless_present = "drifted", conflicts_with = "drifted")]
        vm: Option<String>,
        /// Every running VM whose live evidence no longer matches.
        #[arg(long)]
        drifted: bool,
        /// Shut the VM down and start it first, so its saved definition
        /// takes effect.
        #[arg(long, conflicts_with = "drifted")]
        restart: bool,
        /// Write the attestation to the draft only; apply later.
        #[arg(long)]
        no_apply: bool,
    },
}

/// Options shared by the live views.
#[derive(Debug, clap::Args)]
struct View {
    /// Print one plain snapshot and exit.
    #[arg(long)]
    once: bool,
    /// Seconds between refreshes.
    #[arg(long, value_name = "SECONDS", default_value_t = 1)]
    interval_seconds: u64,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() {
                exit::USAGE
            } else {
                exit::SUCCESS
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    match &cli.command {
        // The unit runs the daemon as the service account, and recovery
        // runs its own step as that account too.
        Command::Daemon => return daemon::run(&running(&cli)),
        Command::ClearLatch { .. } => {}
        _ if process::effective_uid() != 0 => {
            eprintln!("rampire: run this as root, for example with sudo");
            return ExitCode::from(exit::FAILURE);
        }
        _ => {}
    }
    match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rampire: {error}");
            ExitCode::from(exit::FAILURE)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let cwd = Path::new("/");
    match &cli.command {
        Command::Daemon => unreachable!("handled in main"),
        Command::Status { vm } => status(cli, vm.as_deref()),
        Command::Init => init(cli),
        Command::Top { view, events } => {
            let mut args = view_args(cli, view)?;
            args.extend(["--events".to_owned(), events.to_string()]);
            rampire_ops::pool::run(&rampire_ops::pool::parse(&args)?, cwd)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch {
            vm,
            view,
            events,
            guest_interval_seconds,
            no_guest,
        } => {
            let mut args = view_args(cli, view)?;
            args.extend([
                "--events".to_owned(),
                events.to_string(),
                "--guest-interval-seconds".to_owned(),
                guest_interval_seconds.to_string(),
            ]);
            if let Some(vm) = vm {
                args.extend(["--vm".to_owned(), vm.clone()]);
            }
            if *no_guest {
                args.push("--no-guest".to_owned());
            }
            rampire_ops::watch::run(&rampire_ops::watch::parse(&args)?, cwd)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Check => check(cli),
        Command::Doctor => doctor(cli),
        Command::Diff => diff(cli),
        Command::Apply { detailed_exitcode } => apply(cli, *detailed_exitcode),
        Command::Rollback => rollback(cli),
        Command::Set {
            target,
            changes,
            no_apply,
        } => {
            let changes = changes
                .iter()
                .map(|change| rampire_ops::set::parse_assignment(change))
                .collect::<Result<Vec<_>, _>>()?;
            let path = rampire_ops::set::set(&draft(cli), target, &changes)?;
            println!("changed {}", path.display());
            if *no_apply {
                println!("not applied: run rampire apply when ready");
                return Ok(ExitCode::SUCCESS);
            }
            apply(cli, false)
        }
        Command::Logs { follow, since } => logs(*follow, since.as_deref()),
        Command::Recover { vm, reason } => recover(cli, vm, reason),
        Command::Vm { command } => match command {
            VmCommand::Attest {
                vm,
                drifted,
                restart,
                no_apply,
            } => vm_attest(cli, vm.as_deref(), *drifted, *restart, *no_apply),
        },
        Command::ClearLatch { vm, reason, apply } => {
            let target = target(cli, Some(vm))?;
            rampire_host::cli::clear_latch(reason, &target, *apply, &mut std::io::stdout())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The views' own options, from the shared ones.
fn view_args(cli: &Cli, view: &View) -> Result<Vec<String>, String> {
    let mut args = vec![
        "--config".to_owned(),
        path_text(&running(cli))?,
        "--connect".to_owned(),
        connection(cli)?,
        "--interval-seconds".to_owned(),
        view.interval_seconds.to_string(),
    ];
    if view.once {
        args.push("--once".to_owned());
    }
    Ok(args)
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))
}

/// `--connect`, or the configuration's libvirt connection.
fn connection(cli: &Cli) -> Result<String, String> {
    match &cli.connect {
        Some(uri) => Ok(uri.clone()),
        None => Ok(rampire_core::settings::load_globals(&running(cli))?.libvirt_uri),
    }
}

fn target(cli: &Cli, vm: Option<&str>) -> Result<Target, String> {
    Ok(Target {
        config: running(cli),
        vm: vm.map(str::to_owned),
        connection: connection(cli)?,
    })
}

fn status(cli: &Cli, vm: Option<&str>) -> Result<ExitCode, String> {
    let names = match vm {
        Some(vm) => vec![vm.to_owned()],
        None => HostSettings::load(&running(cli))
            .map_err(|error| error.to_string())?
            .vms
            .into_iter()
            .map(|config| config.vm_name)
            .collect(),
    };
    let mut vms = Vec::new();
    for name in &names {
        let mut bytes = Vec::new();
        rampire_host::cli::status(&target(cli, Some(name))?, &mut bytes)?;
        vms.push(
            rampire_core::parse_controller_status(&bytes)
                .map_err(|error| format!("status of {name}: {error}"))?,
        );
    }
    if cli.json {
        print_json(&StatusReport {
            schema: StatusReportSchema,
            version: 1,
            vms,
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    if vms.is_empty() {
        println!(
            "RAMpire manages no VMs yet. Add one to {}.",
            draft(cli)
                .join(rampire_core::settings::VMS_DIRECTORY)
                .display()
        );
    }
    for status in &vms {
        println!("{}", status_line(status));
    }
    Ok(ExitCode::SUCCESS)
}

/// One VM's status in a line a person can read.
fn status_line(status: &ControllerStatusSnapshot) -> String {
    let state = if status.actuation_latched {
        format!(
            "latched: {}",
            status
                .latch_reason
                .as_deref()
                .unwrap_or("no reason recorded")
        )
    } else {
        match status.control_health {
            ControlHealth::Converged => "steady".to_owned(),
            ControlHealth::Growing => "growing".to_owned(),
            ControlHealth::Shrinking => "shrinking".to_owned(),
            ControlHealth::RecoveryRequired => "needs recovery: see rampire recover".to_owned(),
            ControlHealth::Latched => "latched".to_owned(),
        }
    };
    let guest = match &status.guest_input.error {
        Some(error) => format!("; guest: {error}"),
        None => String::new(),
    };
    format!(
        "{}: {} of at most {}, {state}{}{guest}",
        status.vm_name,
        size(status.current_bytes),
        size(status.maximum_target_bytes),
        if status.observe_only {
            " (observe only)"
        } else {
            ""
        },
    )
}

fn size(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

fn print_json<T: Serialize>(document: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(document).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}

fn check(cli: &Cli) -> Result<ExitCode, String> {
    let report = rampire_ops::check::run(&draft(cli))?;
    let code = ExitCode::from(if report.passed() {
        exit::SUCCESS
    } else {
        exit::FAILURE
    });
    if cli.json {
        print_json(&CheckReport {
            schema: CheckSchema,
            version: 1,
            passed: report.passed(),
            vms: report
                .vms
                .into_iter()
                .map(|vm| CheckedVm {
                    vm_name: vm.vm_name,
                    problem: vm.problem,
                })
                .collect(),
        })?;
        return Ok(code);
    }
    if report.vms.is_empty() {
        println!("ok: the configuration is valid and manages no VMs");
    }
    for vm in &report.vms {
        match &vm.problem {
            None => println!("ok: {}", vm.vm_name),
            Some(problem) => println!("problem: {}: {problem}", vm.vm_name),
        }
    }
    Ok(code)
}

fn init(cli: &Cli) -> Result<ExitCode, String> {
    use rampire_ops::init;
    let host = rampire_ops::watch::read_meminfo()?.total;
    let connection = connection(cli)?;
    let timeout = std::time::Duration::from_secs(30);
    let virsh = |args: &[&str]| {
        let mut arguments = vec![process::os("-c"), process::os(&connection)];
        arguments.extend(args.iter().map(process::os));
        process::bounded_text("virsh", &arguments, Path::new("/"), timeout)
    };
    let mut domains = Vec::new();
    for name in virsh(&["list", "--all", "--name"])?.lines().map(str::trim) {
        if !name.is_empty() {
            domains.push(init::parse_domain(
                name,
                &virsh(&["dumpxml", "--inactive", name])?,
            )?);
        }
    }
    let suggestion = init::suggest(host, &domains);
    let text = init::global_toml(&suggestion, &domains);
    for folder in [
        rampire_core::settings::VMS_DIRECTORY,
        rampire_ops::applied::REVIEW_DIRECTORY,
        rampire_host::config::ATTESTATION_DIRECTORY,
    ] {
        let path = draft(cli).join(folder);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("create {}: {error}", path.display()))?;
    }
    let global = draft(cli).join(rampire_core::settings::GLOBAL_FILE);
    if global.exists() {
        println!(
            "{} already exists and is unchanged. Suggested from this host:

{text}",
            global.display()
        );
        println!("Change a value with: rampire set global KEY=VALUE");
    } else {
        std::fs::write(&global, &text)
            .map_err(|error| format!("write {}: {error}", global.display()))?;
        println!(
            "wrote {}:

{text}",
            global.display()
        );
    }
    if suggestion.pool_bytes.is_none() {
        println!("This host has no memory left for a pool once its VMs have theirs.");
    }
    apply(cli, false)
}

fn applied_root() -> &'static Path {
    Path::new(rampire_ops::applied::APPLIED_ROOT)
}

fn print_check(report: &rampire_ops::check::Report) {
    for vm in &report.vms {
        if let Some(problem) = &vm.problem {
            println!("problem: {}: {problem}", vm.vm_name);
            if problem.contains("drifted") {
                println!(
                    "  fix: if the change was intended, rampire vm attest {}",
                    vm.vm_name
                );
            }
        }
    }
}

fn libvirt(cli: &Cli) -> Result<rampire_ops::attest::Libvirt, String> {
    Ok(rampire_ops::attest::Libvirt {
        connection: connection(cli)?,
        timeout: std::time::Duration::from_secs(60),
    })
}

fn vm_attest(
    cli: &Cli,
    vm: Option<&str>,
    drifted: bool,
    restart: bool,
    no_apply: bool,
) -> Result<ExitCode, String> {
    use rampire_ops::attest;
    let directory = draft(cli);
    let libvirt = libvirt(cli)?;
    let vms = if drifted {
        let found = attest::drifted(&directory, &libvirt)?;
        if found.is_empty() {
            println!("no running VM has drifted from its attestation");
            return Ok(ExitCode::SUCCESS);
        }
        for (name, change) in &found {
            println!("{name}: {change}");
        }
        if !confirm(cli, "Attest these VMs again as they are now?")? {
            println!("nothing changed");
            return Ok(ExitCode::SUCCESS);
        }
        found.into_iter().map(|(name, _)| name).collect()
    } else {
        let vm = vm.ok_or("name a VM, or pass --drifted")?.to_owned();
        if restart {
            if !confirm(cli, &format!("Shut {vm} down and start it again?"))? {
                println!("nothing changed");
                return Ok(ExitCode::SUCCESS);
            }
            attest::cold_restart(&vm, &libvirt, std::time::Duration::from_secs(300))?;
            println!("{vm} restarted");
        }
        vec![vm]
    };
    for vm in &vms {
        let path = attest::attest(&directory, vm, &libvirt)?;
        println!("attested {vm}: {}", path.display());
    }
    if no_apply {
        println!("not applied: run rampire apply when ready");
        return Ok(ExitCode::SUCCESS);
    }
    apply(cli, false)
}

/// Show the difference between two configuration directories, `None` being
/// no configuration at all. Returns whether there is any.
fn show_difference(from: Option<&Path>, to: &Path) -> Result<bool, String> {
    let empty = std::env::temp_dir().join(format!("rampire-empty-{}", std::process::id()));
    let from = match from {
        Some(path) => path.to_path_buf(),
        None => {
            std::fs::create_dir_all(&empty).map_err(|error| error.to_string())?;
            empty.clone()
        }
    };
    let output = std::process::Command::new("diff")
        .arg("-ruN")
        .arg(&from)
        .arg(to)
        .output()
        .map_err(|error| format!("run diff: {error}"));
    let _ = std::fs::remove_dir(&empty);
    let output = output?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => {
            // Name the two sides, not the temporary paths behind them.
            let text = String::from_utf8_lossy(&output.stdout)
                .replace(&from.display().to_string(), "applied")
                .replace(&to.display().to_string(), "draft");
            print!("{text}");
            Ok(true)
        }
        _ => Err(format!(
            "diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

fn diff(cli: &Cli) -> Result<ExitCode, String> {
    let staged = std::env::temp_dir().join(format!("rampire-draft-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);
    let result = rampire_ops::applied::snapshot(&draft(cli), &staged).and_then(|()| {
        let current = rampire_ops::applied::current(applied_root())?;
        show_difference(current.as_deref(), &staged)
    });
    let _ = std::fs::remove_dir_all(&staged);
    if !result? {
        println!("no changes: the draft is what the controller runs");
    }
    Ok(ExitCode::SUCCESS)
}

fn apply(cli: &Cli, detailed: bool) -> Result<ExitCode, String> {
    let root = applied_root();
    let staged = rampire_ops::applied::staging(root)?;
    let result = stage_and_apply(cli, root, &staged);
    if staged.exists() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    let changed = result?;
    Ok(ExitCode::from(if changed && detailed {
        exit::CHANGED
    } else {
        exit::SUCCESS
    }))
}

/// Returns whether the applied configuration changed.
fn stage_and_apply(cli: &Cli, root: &Path, staged: &Path) -> Result<bool, String> {
    // A VM with no attestation yet is attested here when its file names a
    // review profile and it runs (decision D11); a drifted one is not.
    for vm in rampire_ops::attest::attest_missing(&draft(cli), &libvirt(cli)?)? {
        println!("attested {vm}, which had no attestation");
    }
    rampire_ops::applied::snapshot(&draft(cli), staged)?;
    // Check the copy, so what is checked is exactly what is applied.
    let report = rampire_ops::check::run(staged)?;
    if !report.passed() {
        print_check(&report);
        return Err("the draft did not pass its checks; nothing changed".to_owned());
    }
    let current = rampire_ops::applied::current(root)?;
    if let Some(current) = &current {
        if rampire_ops::applied::same(current, staged)? {
            println!("nothing to apply: the draft is what the controller runs");
            return Ok(false);
        }
    }
    show_difference(current.as_deref(), staged)?;
    let unit = controller_unit();
    if !confirm(cli, &format!("Apply this and restart {unit}?"))? {
        println!("nothing changed");
        return Ok(false);
    }
    secure(staged)?;
    let millis = process::unix_millis()?;
    let applied = rampire_ops::applied::install(root, staged, millis)?;
    println!("applied {}", applied.display());
    let restarted = restart_if_running(&unit)?;
    keep_evidence(Change {
        action: "apply",
        unix_millis: millis,
        draft: Some(&draft(cli)),
        from: current.as_deref(),
        to: &applied,
        report: &report,
        unit: &unit,
        restarted,
    })?;
    Ok(true)
}

fn rollback(cli: &Cli) -> Result<ExitCode, String> {
    let root = applied_root();
    let current = rampire_ops::applied::current(root)?.ok_or("nothing has been applied yet")?;
    let copies = rampire_ops::applied::copies(root)?;
    let index = copies
        .iter()
        .position(|copy| copy == &current)
        .ok_or("the current copy is missing from the applied copies")?;
    let previous = index
        .checked_sub(1)
        .map(|index| copies[index].clone())
        .ok_or("there is no earlier applied copy")?;
    let report = rampire_ops::check::run(&previous)?;
    if !report.passed() {
        print_check(&report);
        return Err(format!(
            "{} no longer passes its checks; nothing changed",
            previous.display()
        ));
    }
    show_difference(Some(&current), &previous)?;
    let unit = controller_unit();
    if !confirm(
        cli,
        &format!("Go back to {} and restart {unit}?", previous.display()),
    )? {
        println!("nothing changed");
        return Ok(ExitCode::SUCCESS);
    }
    rampire_ops::applied::make_current(root, &previous)?;
    println!("rolled back to {}", previous.display());
    let restarted = restart_if_running(&unit)?;
    keep_evidence(Change {
        action: "rollback",
        unix_millis: process::unix_millis()?,
        draft: None,
        from: Some(&current),
        to: &previous,
        report: &report,
        unit: &unit,
        restarted,
    })?;
    Ok(ExitCode::SUCCESS)
}

/// One change to the applied configuration, for its record.
struct Change<'a> {
    action: &'a str,
    unix_millis: u64,
    draft: Option<&'a Path>,
    from: Option<&'a Path>,
    to: &'a Path,
    report: &'a rampire_ops::check::Report,
    unit: &'a str,
    restarted: bool,
}

/// Record what the change did, then prune applied copies and records to
/// the `retention` of the copy now current.
fn keep_evidence(change: Change) -> Result<(), String> {
    use rampire_ops::evidence::{self, ApplyEvidence, ApplyEvidenceSchema};
    let evidence_root = Path::new(evidence::EVIDENCE_ROOT);
    let record = evidence::record(
        evidence_root,
        &ApplyEvidence {
            schema: ApplyEvidenceSchema,
            version: 1,
            action: change.action.to_owned(),
            unix_millis: change.unix_millis,
            build: version(),
            draft: change.draft.map(|path| path.display().to_string()),
            from: change.from.map(|path| path.display().to_string()),
            to: change.to.display().to_string(),
            files: evidence::digests(change.to)?,
            checked_vms: change
                .report
                .vms
                .iter()
                .map(|vm| vm.vm_name.clone())
                .collect(),
            unit: change.unit.to_owned(),
            restarted: change.restarted,
        },
    )?;
    println!("recorded {}", record.display());
    let keep = rampire_core::settings::load_globals(change.to)?.retention as usize;
    rampire_ops::applied::prune(applied_root(), keep)?;
    evidence::prune(evidence_root, keep)?;
    Ok(())
}

/// The controller reads the copy as the service account; nobody else does.
fn secure(copy: &Path) -> Result<(), String> {
    let cwd = Path::new("/");
    let timeout = std::time::Duration::from_secs(30);
    for (program, args) in [
        ("chgrp", ["-R", SERVICE_ACCOUNT]),
        ("chmod", ["-R", "u=rwX,g=rX,o="]),
    ] {
        let mut arguments: Vec<_> = args.iter().map(process::os).collect();
        arguments.push(copy.as_os_str().to_owned());
        process::bounded_text(program, &arguments, cwd, timeout)?;
    }
    Ok(())
}

/// Restart a running controller on the new copy. One that isn't running is
/// left alone: it starts on the new copy whenever it starts. Returns
/// whether it restarted.
fn restart_if_running(unit: &str) -> Result<bool, String> {
    let active = process::bounded_text(
        "systemctl",
        &[process::os("is-active"), process::os(unit)],
        Path::new("/"),
        std::time::Duration::from_secs(10),
    )
    .is_ok_and(|state| state.trim() == "active");
    if active {
        systemctl("restart", unit)?;
        println!("{unit} restarted");
    } else {
        println!("{unit} is not running; it will use the new copy when it starts");
    }
    Ok(active)
}

fn doctor(cli: &Cli) -> Result<ExitCode, String> {
    let findings = rampire_ops::doctor::run(&connection(cli)?, SERVICE_ACCOUNT, applied_root());
    let passed = findings.iter().all(|finding| finding.ok);
    if cli.json {
        print_json(&DoctorReport {
            schema: DoctorSchema,
            version: 1,
            passed,
            findings,
        })?;
    } else {
        for finding in &findings {
            match &finding.fix {
                None => println!("ok: {}: {}", finding.check, finding.detail),
                Some(fix) => {
                    println!("problem: {}: {}", finding.check, finding.detail);
                    println!("  fix: {fix}");
                }
            }
        }
    }
    Ok(ExitCode::from(if passed {
        exit::SUCCESS
    } else {
        exit::FAILURE
    }))
}

fn controller_unit() -> String {
    rampire_ops::units::CONTROLLER_UNIT.to_owned()
}

fn logs(follow: bool, since: Option<&str>) -> Result<ExitCode, String> {
    let mut command = std::process::Command::new("journalctl");
    command.args(["--unit", &controller_unit(), "--no-pager"]);
    if follow {
        command.arg("--follow");
    }
    if let Some(since) = since {
        command.args(["--since", since]);
    }
    let status = command
        .status()
        .map_err(|error| format!("run journalctl: {error}"))?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn recover(cli: &Cli, vm: &str, reason: &str) -> Result<ExitCode, String> {
    let unit = controller_unit();
    let config = path_text(&running(cli))?;
    let connection = connection(cli)?;
    let executable =
        std::env::current_exe().map_err(|error| format!("find the rampire executable: {error}"))?;
    let clear_latch = || {
        let mut command = std::process::Command::new("runuser");
        command
            .args(["-u", SERVICE_ACCOUNT, "--"])
            .arg(&executable)
            .args([
                "--config",
                &config,
                "--connect",
                &connection,
                "clear-latch",
                vm,
                reason,
            ]);
        command.arg("--apply");
        command
            .status()
            .map_err(|error| format!("run clear-latch as {SERVICE_ACCOUNT}: {error}"))
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("clear-latch failed with {status}"))
            })
    };
    // Show the VM as the controller sees it; clearing waits for the
    // controller to stop, since only one may hold the VM.
    let mut bytes = Vec::new();
    rampire_host::cli::status(&target(cli, Some(vm))?, &mut bytes)?;
    let status = rampire_core::parse_controller_status(&bytes)
        .map_err(|error| format!("status of {vm}: {error}"))?;
    println!("{}", status_line(&status));
    if !confirm(
        cli,
        &format!("Stop {unit}, clear {vm}'s latch and start it again?"),
    )? {
        println!("nothing changed");
        return Ok(ExitCode::SUCCESS);
    }
    systemctl("stop", &unit)?;
    let cleared = clear_latch();
    // Start it again however recovery ended: a still-latched controller
    // only observes.
    let started = systemctl("start", &unit);
    cleared?;
    started?;
    println!("{vm}: latch cleared; {unit} started");
    Ok(ExitCode::SUCCESS)
}

fn systemctl(action: &str, unit: &str) -> Result<(), String> {
    process::bounded_text(
        "systemctl",
        &[process::os(action), process::os(unit)],
        Path::new("/"),
        std::time::Duration::from_secs(120),
    )
    .map(|_| ())
}

/// Ask on a terminal; with `--yes` the answer is yes. Without a terminal and
/// without `--yes` nothing is changed.
fn confirm(cli: &Cli, question: &str) -> Result<bool, String> {
    if cli.yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err("no terminal to ask on: pass --yes to go ahead".to_owned());
    }
    print!("{question} [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn options_go_before_or_after_the_command() {
        let cli = Cli::try_parse_from(["rampire", "status", "--config", "/tmp/x", "web"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x")));
        assert_eq!(running(&cli), PathBuf::from("/tmp/x"));
        assert!(matches!(cli.command, Command::Status { vm: Some(ref vm) } if vm == "web"));
        let cli = Cli::try_parse_from(["rampire", "--yes", "recover", "web", "checked"]).unwrap();
        assert!(cli.yes);
        assert!(Cli::try_parse_from(["rampire", "recover", "web"]).is_err());
        assert!(Cli::try_parse_from(["rampire"]).is_err());
        let cli =
            Cli::try_parse_from(["rampire", "set", "web", "priority=150", "max_ram=12GB"]).unwrap();
        assert!(matches!(cli.command, Command::Set { ref changes, .. } if changes.len() == 2));
        assert!(Cli::try_parse_from(["rampire", "set", "web"]).is_err());
        assert!(Cli::try_parse_from(["rampire", "vm", "attest", "web", "--restart"]).is_ok());
        assert!(Cli::try_parse_from(["rampire", "vm", "attest", "--drifted"]).is_ok());
        assert!(Cli::try_parse_from(["rampire", "vm", "attest"]).is_err());
        assert!(Cli::try_parse_from(["rampire", "vm", "attest", "web", "--drifted"]).is_err());
    }

    #[test]
    fn json_documents_name_their_schema() {
        let report = CheckReport {
            schema: CheckSchema,
            version: 1,
            passed: true,
            vms: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.starts_with(r#"{"schema":"rampire.check","version":1,"#));
        let cli = Cli::try_parse_from(["rampire", "status", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn the_version_names_the_build() {
        assert!(version().starts_with(env!("CARGO_PKG_VERSION")));
        assert!(version().contains("(build "));
    }

    #[test]
    fn sizes_read_as_a_person_would_write_them() {
        assert_eq!(size(512 << 20), "512 MiB");
        assert_eq!(size(3 << 30), "3.0 GiB");
        assert_eq!(size((3 << 30) + (512 << 20)), "3.5 GiB");
    }

    #[test]
    fn the_default_configuration_is_etc_rampire() {
        let cli = Cli::try_parse_from(["rampire", "check"]).unwrap();
        if std::env::var_os("RAMPIRE_CONFIG").is_none() {
            assert_eq!(draft(&cli), PathBuf::from("/etc/rampire"));
        }
    }
}
