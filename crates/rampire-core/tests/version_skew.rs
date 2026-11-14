//! A controller serves guests from its own minor release and the one before
//! it (roadmap, "Versions and releases"). Each release keeps the documents
//! its guest and its controller exchange in `fixtures/protocol/MAJOR.MINOR/`;
//! this checks that today's code reads those of its own minor release and
//! the one before.
//!
//! A new minor release adds its directory with
//! `cargo test -p rampire-core --test version_skew -- --ignored`.

use std::path::{Path, PathBuf};

use rampire_core::settings::GlobalSettings;
use rampire_core::{
    GuestSettings, HostMemoryStatus, HostStatusSchema, MemoryOperation, MemoryRequest,
    MemoryTelemetrySnapshot, RawTelemetryEnvelope, RequestIdentity, MEMORY_REQUEST_VERSION,
};

const GIB: u64 = 1 << 30;
const VM: &str = "guest";
const SERVICE: &str = "rampire-guest";

fn protocol_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/protocol")
}

/// This build's release as (major, minor).
fn current_release() -> (u32, u32) {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse::<u32>().expect("numeric version"));
    (parts.next().unwrap(), parts.next().unwrap())
}

/// The releases a controller of `current` serves: its own minor release
/// and, after the first, the one before.
fn served(current: (u32, u32)) -> Vec<(u32, u32)> {
    let mut releases = vec![current];
    if current.1 > 0 {
        releases.push((current.0, current.1 - 1));
    }
    releases
}

fn publication() -> RawTelemetryEnvelope {
    let mut envelope = RawTelemetryEnvelope::new(
        VM,
        SERVICE,
        "session-a",
        1_000_000,
        10,
        3,
        MemoryTelemetrySnapshot {
            physical_total_bytes: 4 * GIB,
            physical_available_bytes: GIB,
            memory_load_percent: 75,
            commit_total_bytes: 3 * GIB,
            commit_limit_bytes: 8 * GIB,
            commit_peak_bytes: 3 * GIB,
            system_cache_bytes: GIB / 2,
            kernel_paged_bytes: 0,
            kernel_nonpaged_bytes: 0,
        },
    );
    envelope.request = Some(MemoryRequest {
        version: MEMORY_REQUEST_VERSION,
        identity: RequestIdentity {
            vm_name: VM.to_owned(),
            service_name: SERVICE.to_owned(),
            session_id: "session-a".to_owned(),
            sequence: 3,
        },
        operation: MemoryOperation::Add,
        bytes: GIB,
        issued_unix_millis: 999_000,
        expires_unix_millis: 1_029_000,
    });
    envelope
}

fn host_status() -> HostMemoryStatus {
    let globals = GlobalSettings::default();
    HostMemoryStatus {
        schema: HostStatusSchema,
        version: MEMORY_REQUEST_VERSION,
        vm_name: VM.to_owned(),
        service_name: SERVICE.to_owned(),
        device_alias: "memory0".to_owned(),
        observed_unix_millis: 1_000_500,
        device_size_bytes: 8 * GIB,
        block_size_bytes: 2 << 20,
        requested_bytes: GIB,
        current_bytes: GIB,
        result: None,
        guest_settings: Some(GuestSettings {
            policy: globals.guest_policy(GIB / 2, 8 * GIB),
            min_ram: GIB,
            max_ram: 9 * GIB,
            poll_interval_millis: globals.poll_interval_seconds * 1000,
        }),
    }
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn reads_the_documents_of_every_release_it_serves() {
    let current = current_release();
    for (major, minor) in served(current) {
        let directory = protocol_fixtures().join(format!("{major}.{minor}"));
        if (major, minor) != current && !directory.exists() {
            // The release before this one came before fixtures were kept.
            continue;
        }
        assert!(
            directory.exists(),
            "{} is missing: run this test with --ignored to write it",
            directory.display()
        );
        let envelope: RawTelemetryEnvelope = read(&directory.join("publication.json"));
        envelope
            .validate_for(VM, SERVICE, envelope.observed_unix_millis, 60_000, 5_000)
            .unwrap_or_else(|error| panic!("{major}.{minor} publication: {error}"));
        let request = envelope
            .request
            .as_ref()
            .expect("the fixture holds a request");
        request
            .validate()
            .unwrap_or_else(|error| panic!("{major}.{minor} request: {error}"));
        let status: HostMemoryStatus = read(&directory.join("host-status.json"));
        status
            .validate_for(VM, SERVICE)
            .unwrap_or_else(|error| panic!("{major}.{minor} host status: {error}"));
    }
}

#[test]
fn a_controller_serves_its_own_minor_release_and_the_one_before() {
    assert_eq!(served((0, 1)), [(0, 1), (0, 0)]);
    assert_eq!(served((1, 0)), [(1, 0)]);
}

/// Write this release's fixtures. Run once when a minor release begins.
#[test]
#[ignore]
fn write_this_releases_fixtures() {
    let (major, minor) = current_release();
    let directory = protocol_fixtures().join(format!("{major}.{minor}"));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, json) in [
        (
            "publication.json",
            serde_json::to_string_pretty(&publication()),
        ),
        (
            "host-status.json",
            serde_json::to_string_pretty(&host_status()),
        ),
    ] {
        std::fs::write(directory.join(name), format!("{}\n", json.unwrap())).unwrap();
    }
}
