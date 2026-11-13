use std::process;

use virtio_mem_guest_windows::{
    install_service, remove_service, run_as_service, start_service, stop_service, ProtocolWorker,
    RuntimeWiringError, ServiceConfig, ServiceHost, BUILD_ID,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceCommand {
    Install,
    Run,
    Stop,
    Start,
    Remove,
    Help,
    Version,
}

fn parse_command(args: &[String]) -> Option<ServiceCommand> {
    if args.len() > 1 {
        return None;
    }
    let command = args.first().map(String::as_str).unwrap_or("run");
    match command {
        "install" => Some(ServiceCommand::Install),
        "stop" => Some(ServiceCommand::Stop),
        "start" => Some(ServiceCommand::Start),
        "remove" => Some(ServiceCommand::Remove),
        "help" | "--help" | "-h" => Some(ServiceCommand::Help),
        "version" | "--version" => Some(ServiceCommand::Version),
        "run" | "" => Some(ServiceCommand::Run),
        _ => None,
    }
}

/// A console run; `load_default` has already validated `config`.
fn run_service(config: ServiceConfig) -> Result<(), RuntimeWiringError> {
    let mut host = ServiceHost::with_shutdown_timeout(ProtocolWorker, config.shutdown_timeout());
    host.run()
        .map_err(|error| RuntimeWiringError::HostExecution(error.to_string()))?;

    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = parse_command(&args) else {
        eprintln!("unknown command; use 'help' for usage");
        process::exit(2);
    };
    match command {
        ServiceCommand::Help => {
            println!("Usage: virtio-mem-service [install|start|run|stop|remove|version|help]\nRecovery with missing/damaged configuration: sc.exe stop SERVICE_NAME; sc.exe delete SERVICE_NAME (administrator terminal). Use the installed SCM identity.");
            return;
        }
        ServiceCommand::Version => {
            println!("{BUILD_ID}");
            return;
        }
        // Under the SCM the dispatcher runs the service and loads its own
        // configuration; only a console run continues here.
        ServiceCommand::Run => match run_as_service() {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                eprintln!("service dispatcher failed: {error}");
                process::exit(1);
            }
        },
        ServiceCommand::Install
        | ServiceCommand::Start
        | ServiceCommand::Stop
        | ServiceCommand::Remove => {}
    }
    let config = match ServiceConfig::load_default() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("service configuration failed: {error}");
            process::exit(1);
        }
    };
    match command {
        ServiceCommand::Install => {
            if let Err(error) = install_service(&config) {
                eprintln!("service installation failed: {error}");
                process::exit(1);
            }
            println!("service installed successfully");
        }
        ServiceCommand::Stop => {
            if let Err(error) = stop_service(&config.service_name) {
                eprintln!("service stop failed: {error}");
                process::exit(1);
            }
            println!("service stop requested");
        }
        ServiceCommand::Start => {
            if let Err(error) = start_service(&config.service_name) {
                eprintln!("service start failed: {error}");
                process::exit(1);
            }
            println!("service start requested");
        }
        ServiceCommand::Remove => {
            if let Err(error) = remove_service(&config.service_name) {
                eprintln!("service removal failed: {error}");
                process::exit(1);
            }
            println!("service removed successfully");
        }
        ServiceCommand::Run => {
            if let Err(error) = run_service(config) {
                eprintln!("virtio-mem service runtime failed: {error}");
                process::exit(1);
            }
        }
        ServiceCommand::Help | ServiceCommand::Version => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_service_commands_for_install_and_stop() {
        assert_eq!(
            parse_command(&[String::from("install")]),
            Some(ServiceCommand::Install)
        );
        assert_eq!(
            parse_command(&[String::from("stop")]),
            Some(ServiceCommand::Stop)
        );
        assert_eq!(
            parse_command(&[String::from("run")]),
            Some(ServiceCommand::Run)
        );
        assert_eq!(parse_command(&[]), Some(ServiceCommand::Run));
        assert_eq!(
            parse_command(&[String::from("start")]),
            Some(ServiceCommand::Start)
        );
        assert_eq!(
            parse_command(&[String::from("remove")]),
            Some(ServiceCommand::Remove)
        );
        assert_eq!(
            parse_command(&[String::from("help")]),
            Some(ServiceCommand::Help)
        );
        assert_eq!(
            parse_command(&[String::from("version")]),
            Some(ServiceCommand::Version)
        );
        assert_eq!(parse_command(&[String::from("unknown")]), None);
        assert_eq!(
            parse_command(&[String::from("stop"), String::from("ignored")]),
            None
        );
    }
}
