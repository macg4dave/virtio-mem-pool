//! The controller daemon: one round of observe, allocate and resize per
//! poll, for every configured VM, until it is told to stop.

use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rampire_host::attestation::{read_attestation, AttestedCompatibilitySource};
use rampire_host::config::{HostConfig, HostSettings};
use rampire_host::control_channel::ControlChannel;
use rampire_host::guest_channel::{publication_copy_path, GuestChannel};
use rampire_host::host_memory::ProcMeminfoSource;
use rampire_host::ownership::DeviceOwner;
use rampire_host::qga::VirshGuestFiles;
use rampire_host::resize_sink::VirshResizeSink;
use rampire_host::runtime::{HostRuntime, VmController};
use rampire_host::virsh::Virsh;
use rampire_host::xml_source::VirshXmlSource;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;

/// `rampire daemon`: run the controller on the configuration in
/// `config_path` until SIGTERM or SIGINT.
pub fn run(config_path: &Path) -> ExitCode {
    let settings = match HostSettings::load(config_path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("rampire configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(error) = flag::register(SIGTERM, Arc::clone(&stop))
        .and_then(|_| flag::register(SIGINT, Arc::clone(&stop)))
    {
        eprintln!("rampire signal setup error: {error}");
        return ExitCode::FAILURE;
    }
    if settings.vms.is_empty() {
        // A configuration with no VMs is valid (decision D13): a freshly
        // installed host runs, idle, until a VM is added.
        eprintln!(
            "event=no_vms rampire manages no VMs; add one to {} and restart",
            config_path
                .join(rampire_core::settings::VMS_DIRECTORY)
                .display()
        );
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("rampire stopped");
        return ExitCode::SUCCESS;
    }
    // Timeouts and the connection are global settings, shared by every VM.
    // Per-poll commands share one libvirt connection: one polkit check
    // instead of several a second.
    let virsh = Virsh::persistent_with_connection(
        settings.virsh_binary.clone(),
        settings.command_timeout,
        settings.libvirt_uri.clone(),
    );
    if let Err(error) = virsh.require_management_access() {
        eprintln!("rampire authorization error: {error}");
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
    let events = match rampire_host::wake::MemoryEvents::start(&virsh) {
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
