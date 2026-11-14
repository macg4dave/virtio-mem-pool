//! A stand-in for the systemd unit while the daemon is under test: it runs the
//! host binary in the foreground with the unit's arguments, logs its output,
//! and stops it at a deadline or when host memory runs low. Nothing is
//! installed, enabled, or left running after this command returns.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rampire_host::host_memory::{HostMemorySource, ProcMeminfoSource};
use signal_hook::consts::signal::{SIGINT, SIGTERM};

use rampire_ops::process;

/// How long the daemon may take to stop after SIGTERM before this runner
/// gives up waiting and reports it.
const STOP_GRACE: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    config: PathBuf,
    max_run: Duration,
    min_host_available_bytes: Option<u64>,
    log: Option<PathBuf>,
    sudo: bool,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut config = None;
    let mut max_run = None;
    let mut min_host_available_bytes = None;
    let mut log = None;
    let mut sudo = false;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--sudo" && !sudo {
            sudo = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{option} requires a value"))?;
        let positive = || {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("{option} requires a positive integer"))
        };
        match option {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--max-run-seconds" if max_run.is_none() => {
                max_run = Some(Duration::from_secs(positive()?));
            }
            "--min-host-available-bytes" if min_host_available_bytes.is_none() => {
                min_host_available_bytes = Some(positive()?);
            }
            "--log" if log.is_none() => log = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown or duplicate host-run option: {option}")),
        }
        index += 2;
    }
    Ok(Command {
        config: config.ok_or_else(|| "host-run requires --config".to_owned())?,
        max_run: max_run.ok_or_else(|| "host-run requires --max-run-seconds".to_owned())?,
        min_host_available_bytes,
        log,
        sudo,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    process::run(
        "cargo",
        &["build", "--release", "--locked", "-p", "rampire"],
        repo,
    )?;
    let binary = repo.join("target/release/rampire");
    let config = resolve(repo, &command.config);
    let config = config
        .canonicalize()
        .map_err(|error| format!("host-run config {}: {error}", config.display()))?;
    let log_path = match &command.log {
        Some(path) => resolve(repo, path),
        None => repo.join(format!(".artifacts/host-run/{}.log", now_millis())),
    };
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create host-run log directory: {error}"))?;
    }
    let log = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| format!("open host-run log {}: {error}", log_path.display()))?,
    ));

    // Ctrl-C reaches the daemon directly through the terminal's process group;
    // this runner only notes it and waits for the daemon to finish.
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))
            .map_err(|error| format!("host-run signal setup: {error}"))?;
    }

    // The same arguments and libvirt URI as the systemd unit. sudo replaces
    // the environment, so the URI is passed through `env`.
    let mut process = if command.sudo {
        let mut process = ProcessCommand::new("sudo");
        process.args(["--", "env", "LIBVIRT_DEFAULT_URI=qemu:///system"]);
        process.arg(&binary);
        process
    } else {
        let mut process = ProcessCommand::new(&binary);
        process.env("LIBVIRT_DEFAULT_URI", "qemu:///system");
        process
    };
    process
        .arg("--config")
        .arg(&config)
        .current_dir(repo)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    record(
        &log,
        &format!(
            "host-run: starting {} --config {} (max run {:?}, host floor {:?})",
            binary.display(),
            config.display(),
            command.max_run,
            command.min_host_available_bytes
        ),
    );
    let mut child = process
        .spawn()
        .map_err(|error| format!("start host daemon: {error}"))?;
    let readers = [
        child
            .stdout
            .take()
            .map(|out| forward(out, Arc::clone(&log))),
        child
            .stderr
            .take()
            .map(|err| forward(err, Arc::clone(&log))),
    ];

    let outcome = supervise(&mut child, command, &interrupted, &log);
    for reader in readers.into_iter().flatten() {
        let _ = reader.join();
    }
    record(&log, &format!("host-run: {outcome}"));
    println!("host-run log: {}", log_path.display());
    outcome.result()
}

enum Outcome {
    Exited(String),
    Stopped { reason: String, status: String },
    StillRunning { reason: String, pid: u32 },
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited(status) => write!(formatter, "daemon exited by itself: {status}"),
            Self::Stopped { reason, status } => {
                write!(formatter, "stopped the daemon ({reason}): {status}")
            }
            Self::StillRunning { reason, pid } => write!(
                formatter,
                "the daemon did not stop within {STOP_GRACE:?} after SIGTERM ({reason}); it is still running under pid {pid}"
            ),
        }
    }
}

impl Outcome {
    fn result(&self) -> Result<(), String> {
        match self {
            Self::Stopped { .. } => Ok(()),
            Self::Exited(status) if status == "exit status: 0" => Ok(()),
            other => Err(other.to_string()),
        }
    }
}

fn supervise(
    child: &mut Child,
    command: &Command,
    interrupted: &AtomicBool,
    log: &Mutex<File>,
) -> Outcome {
    let started = Instant::now();
    let host = ProcMeminfoSource::new();
    let reason = loop {
        match child.try_wait() {
            Ok(Some(status)) => return Outcome::Exited(status.to_string()),
            Ok(None) => {}
            Err(error) => break format!("lost track of the daemon: {error}"),
        }
        if interrupted.load(Ordering::Acquire) {
            break "interrupted".to_owned();
        }
        if started.elapsed() >= command.max_run {
            break format!("reached --max-run-seconds {}", command.max_run.as_secs());
        }
        if let Some(floor) = command.min_host_available_bytes {
            match host.available_bytes() {
                Ok(available) if available < floor => {
                    break format!("host MemAvailable {available} fell below {floor}");
                }
                Ok(_) => {}
                Err(error) => break format!("cannot read host memory: {error}"),
            }
        }
        thread::sleep(CHECK_INTERVAL);
    };
    record(log, &format!("host-run: stopping the daemon: {reason}"));
    // sudo relays SIGTERM to the daemon; a SIGKILL would orphan it instead.
    let _ = ProcessCommand::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Outcome::Stopped {
                reason,
                status: status.to_string(),
            };
        }
        thread::sleep(CHECK_INTERVAL);
    }
    Outcome::StillRunning {
        reason,
        pid: child.id(),
    }
}

fn forward(stream: impl Read + Send + 'static, log: Arc<Mutex<File>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            record(&log, &line);
        }
    })
}

/// Prints a line and appends it to the log with a Unix-millisecond stamp.
fn record(log: &Mutex<File>, line: &str) {
    eprintln!("{line}");
    if let Ok(mut file) = log.lock() {
        let _ = writeln!(file, "{} {line}", now_millis());
    }
}

fn now_millis() -> u64 {
    rampire_ops::process::unix_millis().unwrap_or_default()
}

fn resolve(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn requires_a_config_and_a_run_limit() {
        let command = parse(&args(&[
            "--config",
            ".artifacts/run/guest.toml",
            "--max-run-seconds",
            "600",
            "--min-host-available-bytes",
            "2147483648",
            "--sudo",
        ]))
        .expect("valid host-run");
        assert_eq!(command.max_run, Duration::from_secs(600));
        assert_eq!(
            command.min_host_available_bytes,
            Some(2 * 1024 * 1024 * 1024)
        );
        assert!(command.sudo);
        assert!(parse(&args(&["--config", "a.toml"])).is_err());
        assert!(parse(&args(&["--max-run-seconds", "60"])).is_err());
        assert!(parse(&args(&["--config", "a.toml", "--max-run-seconds", "0"])).is_err());
        assert!(parse(&args(&[
            "--config",
            "a",
            "--config",
            "b",
            "--max-run-seconds",
            "1"
        ]))
        .is_err());
    }

    fn sleeping_child() -> Child {
        ProcessCommand::new("sleep")
            .arg("60")
            .spawn()
            .expect("start sleep")
    }

    fn scratch_log(name: &str) -> Mutex<File> {
        let path =
            std::env::temp_dir().join(format!("rampire-run-{name}-{}.log", std::process::id()));
        Mutex::new(File::create(path).expect("scratch log"))
    }

    fn limits(max_run: Duration, floor: Option<u64>) -> Command {
        Command {
            config: PathBuf::from("unused.toml"),
            max_run,
            min_host_available_bytes: floor,
            log: None,
            sudo: false,
        }
    }

    #[test]
    fn stops_the_daemon_at_the_run_limit() {
        let mut child = sleeping_child();
        let outcome = supervise(
            &mut child,
            &limits(Duration::from_millis(200), None),
            &AtomicBool::new(false),
            &scratch_log("deadline"),
        );
        assert!(
            matches!(&outcome, Outcome::Stopped { reason, .. } if reason.contains("--max-run-seconds")),
            "{outcome}"
        );
        assert!(outcome.result().is_ok());
    }

    #[test]
    fn stops_the_daemon_when_host_memory_falls_below_the_floor() {
        let mut child = sleeping_child();
        let outcome = supervise(
            &mut child,
            &limits(Duration::from_secs(60), Some(u64::MAX)),
            &AtomicBool::new(false),
            &scratch_log("floor"),
        );
        assert!(
            matches!(&outcome, Outcome::Stopped { reason, .. } if reason.contains("MemAvailable")),
            "{outcome}"
        );
    }
}
