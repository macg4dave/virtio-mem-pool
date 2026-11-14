//! Opening the control channel on Linux: the port systemd opened for the
//! unit (`OpenFile=`), or one named on the command line. Everything carried
//! over it is the shared transport's.

use rampire_guest::transport::{FileTransport, StreamTransport, Transport};

/// The name systemd passes the opened control channel under (`OpenFile=`).
pub const CONTROL_FD_NAME: &str = "control";

/// The descriptor systemd passed under `name`, from the `LISTEN_*`
/// environment as given: `LISTEN_PID` must be this process, descriptors
/// start at 3 and `LISTEN_FDNAMES` names them in order.
pub fn named_descriptor(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    listen_fdnames: Option<&str>,
    process_id: u32,
    name: &str,
) -> Option<i32> {
    if listen_pid?.trim().parse::<u32>().ok()? != process_id {
        return None;
    }
    let count = listen_fds?.trim().parse::<usize>().ok()?;
    let index = listen_fdnames?
        .split(':')
        .take(count)
        .position(|entry| entry == name)?;
    i32::try_from(index).ok()?.checked_add(3)
}

/// The transport for this run: the control channel at `channel_path` when
/// one is named, else the one systemd opened for the unit, else the control
/// files.
pub fn select(
    control_path: &str,
    channel_path: Option<&str>,
) -> Result<Box<dyn Transport>, String> {
    if let Some(stream) = port::open(channel_path)? {
        return Ok(Box::new(StreamTransport::new(stream)));
    }
    Ok(Box::new(FileTransport::new(control_path)))
}

#[cfg(unix)]
mod port {
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, FromRawFd};

    pub fn open(path: Option<&str>) -> Result<Option<File>, String> {
        let file = match path {
            Some(path) => OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|error| format!("open control channel {path}: {error}"))?,
            None => {
                let variable = |name: &str| std::env::var(name).ok();
                let Some(descriptor) = super::named_descriptor(
                    variable("LISTEN_PID").as_deref(),
                    variable("LISTEN_FDS").as_deref(),
                    variable("LISTEN_FDNAMES").as_deref(),
                    std::process::id(),
                    super::CONTROL_FD_NAME,
                ) else {
                    return Ok(None);
                };
                // SAFETY: systemd passed this descriptor to this process for
                // it to own, and nothing else in the process uses it.
                unsafe { File::from_raw_fd(descriptor) }
            }
        };
        // SAFETY: `file` owns a valid descriptor for both calls.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(format!(
                "control channel cannot be made non-blocking: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Some(file))
    }
}

#[cfg(not(unix))]
mod port {
    pub fn open(path: Option<&str>) -> Result<Option<std::fs::File>, String> {
        match path {
            Some(_) => Err("the control channel is not available on this platform".to_owned()),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_names_the_descriptor_it_opened() {
        let find = |pid, fds, names| named_descriptor(pid, fds, names, 4242, CONTROL_FD_NAME);
        assert_eq!(find(Some("4242"), Some("1"), Some("control")), Some(3));
        assert_eq!(
            find(Some("4242"), Some("2"), Some("other:control")),
            Some(4)
        );
        // Meant for another process, absent, or beyond the passed count.
        assert_eq!(find(Some("1"), Some("1"), Some("control")), None);
        assert_eq!(find(None, Some("1"), Some("control")), None);
        assert_eq!(find(Some("4242"), Some("0"), Some("control")), None);
        assert_eq!(find(Some("4242"), Some("1"), Some("other:control")), None);
        assert_eq!(find(Some("4242"), Some("1"), Some("stored")), None);
        assert_eq!(find(Some("4242"), Some("many"), Some("control")), None);
    }
}
