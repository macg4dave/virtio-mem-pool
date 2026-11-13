//! The service's life: wait until the host has published this VM's
//! settings, run a worker with them, and start again whenever the host
//! publishes different ones. The platform supplies its sources and transport.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use virtio_mem_core::GuestSettings;

use crate::requests::{published_settings, GuestIdentity, GuestRequests};
use crate::transport::Transport;
use crate::worker::{process_session_id, Clock, GuestWorker, MemorySource, PressureSource, RunEnd};

/// How often a guest with no settings yet looks for them.
const SETTINGS_WAIT: Duration = Duration::from_millis(500);

/// What one run of the worker is built from, for the settings it was given.
pub struct Sources<M, P, C> {
    pub memory: M,
    pub pressure: P,
    pub clock: C,
}

/// The platform's side of the service.
pub struct Platform<'a> {
    /// The guest service's identity on the wire.
    pub service_name: &'a str,
    /// The control publication's path: the request state lives beside it,
    /// and the control files there when there is no control channel.
    pub control_path: &'a str,
    pub identity: GuestIdentity,
    /// Facts published with every sample's diagnostics.
    pub environment: fn() -> BTreeMap<String, String>,
}

/// Serve until `stop` is set. `sources` builds a worker's sources for the
/// settings the host published.
pub fn serve<M, P, C>(
    platform: &Platform<'_>,
    mut transport: Box<dyn Transport>,
    stop: &AtomicBool,
    mut sources: impl FnMut(&GuestSettings) -> Result<Sources<M, P, C>, String>,
) -> Result<(), String>
where
    M: MemorySource,
    P: PressureSource,
    C: Clock,
{
    eprintln!("host via {}", transport.describe());
    let mut waiting_reported = false;
    while !stop.load(Ordering::Acquire) {
        let Some((vm_name, settings)) = transport
            .host_status()?
            .and_then(|status| published_settings(&status, platform.service_name))
        else {
            if !waiting_reported {
                eprintln!("waiting for the host to publish this VM's settings");
                waiting_reported = true;
            }
            std::thread::sleep(SETTINGS_WAIT);
            continue;
        };
        waiting_reported = false;
        eprintln!(
            "settings from host: vm={vm_name} min_ram={} max_ram={} free_ram={} poll_interval_millis={}",
            settings.min_ram,
            settings.max_ram,
            settings.policy.target.physical_reserve_bytes,
            settings.poll_interval_millis
        );
        let requests = GuestRequests::open(
            settings,
            &vm_name,
            platform.service_name,
            &process_session_id()?,
            platform.control_path,
            platform.identity.clone(),
        )?;
        let Sources {
            memory,
            pressure,
            clock,
        } = sources(&settings)?;
        let mut worker = GuestWorker::new(
            memory,
            pressure,
            clock,
            requests,
            transport,
            Duration::from_millis(settings.poll_interval_millis),
        )?
        .with_environment(platform.environment);
        let end = worker.run(stop)?;
        transport = worker.into_transport();
        if end == RunEnd::SettingsChanged {
            eprintln!("the host published different settings; restarting with them");
        }
    }
    Ok(())
}
