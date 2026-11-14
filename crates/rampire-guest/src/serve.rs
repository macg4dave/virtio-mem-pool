//! The service's life: wait until the host has published this VM's
//! settings, run a worker with them, and start again whenever the host
//! publishes different ones. The platform supplies its sources and transport.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rampire_core::GuestSettings;

use crate::requests::{published_settings, GuestIdentity, GuestRequests};
use crate::transport::{FileTransport, Transport};
use crate::worker::{process_session_id, Clock, GuestWorker, MemorySource, PressureSource, RunEnd};

/// How often a guest with no settings yet looks for them.
const SETTINGS_WAIT: Duration = Duration::from_millis(500);

/// How long a guest on the control channel waits for its settings there
/// before it also looks in the control files. A host built before the
/// channel answers only through the files, which it can write only where
/// the guest agent allows file commands. A host with the channel answers on
/// it within one poll (at most ten seconds).
const CHANNEL_SETTINGS_WAIT: Duration = Duration::from_secs(30);

/// The control files, when they hold a fresh status with this VM's settings.
/// Fresh means written within the last minute, the longest freshness bound
/// any poll interval allows, so files left by an earlier host are ignored.
fn answering_files(platform: &Platform<'_>) -> Result<Option<Box<dyn Transport>>, String> {
    let mut files = FileTransport::new(platform.control_path);
    let Some(status) = files.host_status()? else {
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_millis();
    let fresh = u128::from(status.observed_unix_millis).abs_diff(now) <= FILES_FRESH.as_millis();
    Ok(
        (fresh && published_settings(&status, platform.service_name).is_some())
            .then(|| Box::new(files) as Box<dyn Transport>),
    )
}

const FILES_FRESH: Duration = Duration::from_secs(60);

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
    let mut waiting_since: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        let Some((vm_name, settings)) = transport
            .host_status()?
            .and_then(|status| published_settings(&status, platform.service_name))
        else {
            let since = *waiting_since.get_or_insert_with(|| {
                eprintln!("waiting for the host to publish this VM's settings");
                Instant::now()
            });
            if transport.is_channel() && since.elapsed() >= CHANNEL_SETTINGS_WAIT {
                if let Some(files) = answering_files(platform)? {
                    eprintln!(
                        "the host answers only through {}; using them",
                        files.describe()
                    );
                    transport = files;
                    continue;
                }
            }
            std::thread::sleep(SETTINGS_WAIT);
            continue;
        };
        waiting_since = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rampire_core::settings::GlobalSettings;
    use rampire_core::{host_status_path, HostMemoryStatus, MEMORY_REQUEST_VERSION};

    const GIB: u64 = 1 << 30;

    fn now_millis() -> u64 {
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    }

    #[test]
    fn a_guest_on_the_channel_uses_files_only_with_fresh_settings_in_them() {
        let directory =
            std::env::temp_dir().join(format!("rampire-guest-serve-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let control = directory.join("control.json").display().to_string();
        let platform = Platform {
            service_name: "rampire-guest",
            control_path: &control,
            identity: GuestIdentity {
                build_id: "test".to_owned(),
                driver_version: Ok("test".to_owned()),
            },
            environment: BTreeMap::new,
        };
        let write = |observed: u64, with_settings: bool| {
            let status = HostMemoryStatus {
                version: MEMORY_REQUEST_VERSION,
                vm_name: "debian".to_owned(),
                service_name: "rampire-guest".to_owned(),
                device_alias: "ua-virtiomem0".to_owned(),
                observed_unix_millis: observed,
                device_size_bytes: 4 * GIB,
                block_size_bytes: 2 << 20,
                requested_bytes: 0,
                current_bytes: 0,
                result: None,
                guest_settings: with_settings.then(|| GuestSettings {
                    policy: GlobalSettings::default().guest_policy(2 * GIB, 3 * GIB),
                    min_ram: 2 * GIB,
                    max_ram: 5 * GIB,
                    poll_interval_millis: 1000,
                }),
            };
            let mut bytes = serde_json::to_vec(&status).unwrap();
            bytes.push(b'\n');
            std::fs::write(host_status_path(&control), bytes).unwrap();
        };

        assert!(answering_files(&platform).unwrap().is_none(), "no files");
        write(now_millis(), false);
        assert!(answering_files(&platform).unwrap().is_none(), "no settings");
        write(now_millis() - 120_000, true);
        assert!(
            answering_files(&platform).unwrap().is_none(),
            "left by an earlier host"
        );
        write(now_millis(), true);
        let files = answering_files(&platform)
            .unwrap()
            .expect("a host on the files");
        assert!(!files.is_channel());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
