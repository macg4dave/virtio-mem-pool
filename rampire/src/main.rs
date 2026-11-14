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
/// The controller unit when systemd lists none, or several.
const DEFAULT_UNIT: &str = "rampire.service";
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
}

rampire_core::schema_name! {
    /// `rampire status --json`.
    struct StatusReportSchema = "rampire.status-report";
}

rampire_core::schema_name! {
    /// `rampire check --json`.
    struct CheckSchema = "rampire.check";
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

fn version() -> String {
    format!("{} (build {BUILD_ID})", env!("CARGO_PKG_VERSION"))
}

/// Share a pool of RAM between the VMs on a KVM host.
#[derive(Debug, Parser)]
#[command(name = "rampire", version = version(), propagate_version = true)]
struct Cli {
    /// The configuration directory.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        env = "RAMPIRE_CONFIG",
        default_value = "/etc/rampire"
    )]
    config: PathBuf,
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
    /// Recovery's own step, run as the service account.
    #[command(hide = true)]
    ClearLatch {
        vm: String,
        reason: String,
        #[arg(long)]
        apply: bool,
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
    /// The controller unit's instance; default the installed controller.
    #[arg(long, value_name = "NAME")]
    instance: Option<String>,
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
        Command::Daemon => return daemon::run(&cli.config),
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
        Command::Logs { follow, since } => logs(*follow, since.as_deref()),
        Command::Recover { vm, reason } => recover(cli, vm, reason),
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
        path_text(&cli.config)?,
        "--connect".to_owned(),
        connection(cli)?,
        "--interval-seconds".to_owned(),
        view.interval_seconds.to_string(),
    ];
    if let Some(instance) = &view.instance {
        args.extend(["--instance".to_owned(), instance.clone()]);
    }
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
        None => Ok(rampire_core::settings::load_globals(&cli.config)?.libvirt_uri),
    }
}

fn target(cli: &Cli, vm: Option<&str>) -> Result<Target, String> {
    Ok(Target {
        config: cli.config.clone(),
        vm: vm.map(str::to_owned),
        connection: connection(cli)?,
    })
}

fn status(cli: &Cli, vm: Option<&str>) -> Result<ExitCode, String> {
    let names = match vm {
        Some(vm) => vec![vm.to_owned()],
        None => HostSettings::load(&cli.config)
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
            cli.config
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
    let report = rampire_ops::check::run(&cli.config)?;
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

fn controller_unit() -> String {
    rampire_ops::units::installed_controller_unit(DEFAULT_UNIT, Path::new("/"))
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
    let config = path_text(&cli.config)?;
    let connection = connection(cli)?;
    let executable =
        std::env::current_exe().map_err(|error| format!("find the rampire executable: {error}"))?;
    let clear_latch = |apply: bool| {
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
        if apply {
            command.arg("--apply");
        }
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
    // Show what recovery would do while the controller still runs.
    clear_latch(false)?;
    if !confirm(
        cli,
        &format!("Stop {unit}, clear {vm}'s latch and start it again?"),
    )? {
        println!("nothing changed");
        return Ok(ExitCode::SUCCESS);
    }
    systemctl("stop", &unit)?;
    let cleared = clear_latch(true);
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
        assert_eq!(cli.config, PathBuf::from("/tmp/x"));
        assert!(matches!(cli.command, Command::Status { vm: Some(ref vm) } if vm == "web"));
        let cli = Cli::try_parse_from(["rampire", "--yes", "recover", "web", "checked"]).unwrap();
        assert!(cli.yes);
        assert!(Cli::try_parse_from(["rampire", "recover", "web"]).is_err());
        assert!(Cli::try_parse_from(["rampire"]).is_err());
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
            assert_eq!(cli.config, PathBuf::from("/etc/rampire"));
        }
    }
}
