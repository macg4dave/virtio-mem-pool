//! The Linux guest service. It samples `/proc` memory telemetry, applies the
//! shared guest policy from `virtio-mem-core` and exchanges requests and
//! results with the host controller through the same control files and wire
//! format as the Windows service.

pub mod config;
pub mod files;
pub mod meminfo;
pub mod pressure;
pub mod readiness;
pub mod requests;
pub mod worker;

/// Build identity stamped at build time from the synchronised source
/// (`VIRTIO_MEM_BUILD_ID`); builds without the stamp report `unversioned`.
pub const BUILD_ID: &str = match option_env!("VIRTIO_MEM_BUILD_ID") {
    Some(id) => id,
    None => "unversioned",
};
