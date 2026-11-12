use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use signal_hook::consts::signal::{SIGINT, SIGTERM};
use virtio_mem_linux_service::config::{self, DEFAULT_CONFIG_PATH};
use virtio_mem_linux_service::meminfo::ProcMeminfo;
use virtio_mem_linux_service::pressure::ProcPressure;
use virtio_mem_linux_service::requests::GuestRequests;
use virtio_mem_linux_service::worker::{process_session_id, GuestWorker, SystemClock};
use virtio_mem_linux_service::BUILD_ID;

const HELP: &str = "Usage: virtio-mem-guest [run [--config PATH]|version|help]\n\
Runs in the foreground until SIGTERM or SIGINT; systemd supervises it as\n\
virtio-mem-guest.service. The default configuration is\n\
/etc/virtio-mem-guest/config.json.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(PathBuf),
    Version,
    Help,
}

fn parse_command(arguments: &[String]) -> Result<Command, String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        [] | ["run"] => Ok(Command::Run(PathBuf::from(DEFAULT_CONFIG_PATH))),
        ["run", "--config", path] if !path.is_empty() => Ok(Command::Run(PathBuf::from(path))),
        ["version" | "--version"] => Ok(Command::Version),
        ["help" | "--help" | "-h"] => Ok(Command::Help),
        _ => Err("unknown command; use 'help' for usage".to_owned()),
    }
}

fn run(config_path: &Path) -> Result<(), String> {
    let config = config::load(config_path)?;
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("signal setup failed: {error}"))?;
    }
    let mut requests = GuestRequests::open(
        config.policy,
        &config.vm_name,
        &config.service_name,
        &process_session_id()?,
        &config.control_path,
    )?;
    requests.set_ram_limits(config.min_ram, config.max_ram)?;
    let mut worker = GuestWorker::new(
        ProcMeminfo::default(),
        ProcPressure,
        SystemClock::default(),
        requests,
        &config.control_path,
        Duration::from_millis(config.poll_interval_millis),
    )?;
    eprintln!(
        "virtio-mem-guest {BUILD_ID} started: vm={} control={}",
        config.vm_name, config.control_path
    );
    worker.run(&stop)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match parse_command(&arguments) {
        Ok(Command::Help) => println!("{HELP}"),
        Ok(Command::Version) => println!("{BUILD_ID}"),
        Ok(Command::Run(config_path)) => {
            if let Err(error) = run(&config_path) {
                eprintln!("virtio-mem-guest failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_rejects_anything_else() {
        let parse = |arguments: &[&str]| {
            parse_command(
                &arguments
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        let default = Command::Run(PathBuf::from(DEFAULT_CONFIG_PATH));
        assert_eq!(parse(&[]), Ok(default));
        assert_eq!(
            parse(&["run", "--config", "/tmp/config.json"]),
            Ok(Command::Run(PathBuf::from("/tmp/config.json")))
        );
        assert_eq!(parse(&["version"]), Ok(Command::Version));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        for bad in [
            &["install"][..],
            &["run", "--config"],
            &["run", "--config", ""],
            &["run", "extra"],
            &["version", "extra"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
