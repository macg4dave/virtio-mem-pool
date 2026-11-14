use std::path::Path;
use std::process::ExitCode;
use std::sync::{atomic::AtomicBool, Arc};

use rampire::attestation::{read_attestation, AttestedCompatibilitySource};
use rampire::cli::CliCommand;
use rampire::config::{HostConfig, HostSettings};
use rampire::control_channel::ControlChannel;
use rampire::guest_channel::{publication_copy_path, GuestChannel};
use rampire::host_memory::ProcMeminfoSource;
use rampire::ownership::DeviceOwner;
use rampire::qga::VirshGuestFiles;
use rampire::resize_sink::VirshResizeSink;
use rampire::runtime::{HostRuntime, VmController};
use rampire::virsh::Virsh;
use rampire::xml_source::VirshXmlSource;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match rampire::cli::parse_args(&args) {
        Ok(CliCommand::Run { config }) => config,
        Ok(command) => {
            return match rampire::cli::run(command) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("rampire CLI failed: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let settings = match HostSettings::load(&config_path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("rampire configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    // virsh binary and timeouts are global settings, shared by every VM.
    // Per-poll commands share one libvirt connection: one polkit check
    // instead of several a second.
    let first = &settings.vms[0];
    let virsh = Virsh::persistent_with_connection(
        first.virsh_binary.clone(),
        first.command_timeout,
        first.libvirt_uri.clone(),
    );
    if let Err(error) = virsh.require_management_access() {
        eprintln!("rampire authorization error: {error}");
        return ExitCode::FAILURE;
    }
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(error) = flag::register(SIGTERM, Arc::clone(&stop))
        .and_then(|_| flag::register(SIGINT, Arc::clone(&stop)))
    {
        eprintln!("rampire signal setup error: {error}");
        return ExitCode::FAILURE;
    }
    // Device owners are held until the runtime ends.
    let mut owners = Vec::with_capacity(settings.vms.len());
    let mut vms = Vec::with_capacity(settings.vms.len());
    let mut controls = Vec::with_capacity(settings.vms.len());
    for config in settings.vms {
        let control = ControlChannel::new(virsh.clone(), config.vm_name.clone());
        match vm_controller(&virsh, config, Arc::clone(&control)) {
            Ok((owner, vm)) => {
                owners.push(owner);
                vms.push(vm);
                controls.push(control);
            }
            Err(error) => {
                eprintln!("rampire controller setup error: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    let mut runtime = match HostRuntime::new(vms, ProcMeminfoSource::new(), settings.pool_bytes) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rampire configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let events = match rampire::wake::MemoryEvents::start(&virsh) {
        Ok(events) => Some(events),
        Err(error) => {
            eprintln!("libvirt events unavailable; polling continues: {error}");
            None
        }
    };
    runtime = runtime.with_wake(controls, events);
    let result = runtime.run(&stop);
    drop(owners);
    match result {
        Ok(()) => {
            eprintln!("rampire stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rampire failed: {error}");
            ExitCode::FAILURE
        }
    }
}

type Controller = VmController<
    GuestChannel<VirshGuestFiles<Virsh>>,
    VirshXmlSource<Virsh>,
    VirshResizeSink<Virsh, AttestedCompatibilitySource<Virsh>>,
>;

fn vm_controller(
    virsh: &Virsh,
    config: HostConfig,
    control: Arc<ControlChannel>,
) -> Result<(DeviceOwner, Controller), String> {
    let owner = DeviceOwner::for_domain(virsh, &config.vm_name)?;
    let fingerprint =
        read_attestation(Path::new(&config.compatibility_attestation_path))?.fingerprint_sha256;
    let controller = VmController::new(
        GuestChannel::new(
            VirshGuestFiles::new(virsh.clone(), config.vm_name.clone()),
            &config.raw_telemetry_path,
            &config.vm_name,
            &config.raw_telemetry_service_name,
            config.raw_telemetry_max_age,
            config.raw_telemetry_future_tolerance,
        )
        .with_control(control)
        .with_publication_copy(publication_copy_path(&config.vm_name)),
        VirshXmlSource::new(virsh.clone(), config.vm_name.clone(), config.alias.clone()),
        VirshResizeSink::new(
            virsh.clone(),
            config.vm_name.clone(),
            config.alias.clone(),
            // Evidence is hashed byte-for-byte, so collect it exactly as the
            // attestation was made: through `virsh`, not the shared session.
            AttestedCompatibilitySource::new(
                Virsh::with_connection(
                    config.virsh_binary.clone(),
                    config.command_timeout,
                    config.libvirt_uri.clone(),
                ),
                config.vm_name.clone(),
                config.alias.clone(),
                config.guest_os,
                config.compatibility_attestation_path.clone(),
            ),
        ),
        config,
        fingerprint,
    )?;
    Ok((owner, controller))
}
