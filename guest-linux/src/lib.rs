//! The Linux guest service: Linux's memory counters (`/proc/meminfo`), its
//! pressure signal (pressure stall information), the page-cache floor, the
//! readiness report and how the control channel is opened. The request
//! protocol, worker and service loop are the shared guest crate's.

pub mod cache;
pub mod channel;
pub mod clock;
pub mod meminfo;
pub mod pressure;
pub mod readiness;

/// Build identity stamped at build time from the synchronised source
/// (`VIRTIO_MEM_BUILD_ID`); builds without the stamp report `unversioned`.
pub const BUILD_ID: &str = match option_env!("VIRTIO_MEM_BUILD_ID") {
    Some(id) => id,
    None => "unversioned",
};

/// The in-tree virtio-mem driver is versioned with its kernel.
pub fn kernel_release() -> Result<String, String> {
    let path = std::path::Path::new("/proc/sys/kernel/osrelease");
    let bytes = virtio_mem_guest::files::read_bounded(path, 256)
        .map_err(|error| format!("read kernel release: {error}"))?
        .ok_or("kernel release is unavailable on this platform")?;
    let release = String::from_utf8(bytes).map_err(|_| "kernel release is not UTF-8")?;
    Ok(format!("linux {}", release.trim()))
}
