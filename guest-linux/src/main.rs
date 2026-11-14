use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rampire_core::settings::{LINUX_CONTROL_PATH, LINUX_SERVICE_NAME};
use rampire_guest::requests::GuestIdentity;
use rampire_guest::serve::{serve, Platform, Sources};
use rampire_guest_linux::clock::SystemClock;
use rampire_guest_linux::meminfo::ProcMeminfo;
use rampire_guest_linux::pressure::ProcPressure;
use rampire_guest_linux::{channel, kernel_release, readiness, BUILD_ID};
use signal_hook::consts::signal::{SIGINT, SIGTERM};

const HELP: &str = "Usage: rampire-guest [run [--control PATH] [--channel PATH]|version|help]\n\
Runs in the foreground until SIGTERM or SIGINT; systemd supervises it as\n\
rampire-guest.service. It has no configuration file: the host publishes\n\
this VM's name, limits and policy with its status.\n\
\n\
The host is reached over the control channel when there is one: the port\n\
named by --channel, or the one systemd opened for the unit. Otherwise the\n\
control files are used, which the host reaches through the guest agent.\n\
--control names the control publication and the directory of the durable\n\
request state; the default is /var/lib/rampire-guest/control.json.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run {
        control: String,
        channel: Option<String>,
    },
    Version,
    Help,
}

fn absolute_file(path: &str) -> bool {
    path.starts_with('/') && !path.ends_with('/')
}

fn parse_command(arguments: &[String]) -> Result<Command, String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        ["version" | "--version"] => return Ok(Command::Version),
        ["help" | "--help" | "-h"] => return Ok(Command::Help),
        [] => {}
        ["run", ..] => {}
        _ => return Err("unknown command; use 'help' for usage".to_owned()),
    }
    let mut control = None;
    let mut channel = None;
    let mut options = arguments.iter().skip(1);
    while let Some(option) = options.next() {
        let slot = match *option {
            "--control" if control.is_none() => &mut control,
            "--channel" if channel.is_none() => &mut channel,
            _ => return Err("unknown command; use 'help' for usage".to_owned()),
        };
        match options.next() {
            Some(path) if absolute_file(path) => *slot = Some((*path).to_owned()),
            _ => return Err(format!("{option} needs an absolute file path")),
        }
    }
    Ok(Command::Run {
        control: control.unwrap_or_else(|| LINUX_CONTROL_PATH.to_owned()),
        channel,
    })
}

fn run(control_path: &str, channel_path: Option<&str>) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("signal setup failed: {error}"))?;
    }
    let transport = channel::select(control_path, channel_path)?;
    eprintln!("rampire-guest {BUILD_ID} started; request state beside {control_path}");
    let ready = readiness::assess(&readiness::Probe::read());
    let facts: Vec<String> = ready
        .environment
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    eprintln!("environment: {}", facts.join(" "));
    for warning in &ready.warnings {
        eprintln!("warning: {warning}");
    }
    let platform = Platform {
        service_name: LINUX_SERVICE_NAME,
        control_path,
        identity: GuestIdentity {
            build_id: BUILD_ID.to_owned(),
            driver_version: kernel_release(),
        },
        environment: readiness::environment,
    };
    serve(&platform, transport, &stop, |settings| {
        // max_ram comes from the host, so this check waits for its settings.
        if let Some(warning) =
            readiness::movable_ratio_warning(&readiness::Probe::read(), settings.max_ram)
        {
            eprintln!("warning: {warning}");
        }
        Ok(Sources {
            memory: ProcMeminfo::with_cache_floor(settings.policy.grow_step_bytes),
            pressure: ProcPressure::open(),
            clock: SystemClock::default(),
        })
    })
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match parse_command(&arguments) {
        Ok(Command::Help) => println!("{HELP}"),
        Ok(Command::Version) => println!("{BUILD_ID}"),
        Ok(Command::Run { control, channel }) => {
            if let Err(error) = run(&control, channel.as_deref()) {
                eprintln!("rampire-guest failed: {error}");
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
        let run = |control: &str, channel: Option<&str>| Command::Run {
            control: control.to_owned(),
            channel: channel.map(str::to_owned),
        };
        assert_eq!(parse(&[]), Ok(run(LINUX_CONTROL_PATH, None)));
        assert_eq!(parse(&["run"]), parse(&[]));
        assert_eq!(
            parse(&["run", "--control", "/tmp/control.json"]),
            Ok(run("/tmp/control.json", None))
        );
        assert_eq!(
            parse(&[
                "run",
                "--channel",
                "/dev/virtio-ports/org.virtio-mem.control.0",
                "--control",
                "/tmp/c.json"
            ]),
            Ok(run(
                "/tmp/c.json",
                Some("/dev/virtio-ports/org.virtio-mem.control.0")
            ))
        );
        assert_eq!(parse(&["version"]), Ok(Command::Version));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        for bad in [
            &["install"][..],
            &["run", "--control"],
            &["run", "--control", ""],
            &["run", "--control", "relative.json"],
            &["run", "--channel"],
            &["run", "--channel", "/a", "--channel", "/b"],
            &["run", "--config", "/etc/rampire-guest/config.json"],
            &["run", "extra"],
            &["version", "extra"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
