//! Bounded reads and whole-file replacement for the control files.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The file's contents, `None` when it does not exist. More than
/// `maximum_bytes` is an error rather than a truncated read.
pub fn read_bounded(path: &Path, maximum_bytes: usize) -> std::io::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(maximum_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {maximum_bytes} bytes"),
        ));
    }
    Ok(Some(bytes))
}

/// How long a replacement waits out a reader holding the destination open.
/// On Windows the guest agent opens a file it reads without sharing delete,
/// and a rename over it fails until the read ends.
#[cfg(windows)]
const SHARING_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// A file error that clears when another process stops using the file:
/// a sharing or lock violation, or on Windows an access denial from a
/// rename over a file someone has open.
pub fn transient_sharing_error(error: &std::io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// Replace `path` with `bytes` through a temporary file and a rename, so
/// readers see the old or new contents whole. `durable` also flushes the data
/// and the rename, for state that must survive a crash.
pub fn replace_file(path: &Path, bytes: &[u8], durable: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("open {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        if durable {
            file.sync_all()
                .map_err(|error| format!("flush {}: {error}", temporary.display()))?;
        }
        drop(file);
        rename(&temporary, path, durable)
            .map_err(|error| format!("replace {}: {error}", path.display()))?;
        #[cfg(unix)]
        if durable {
            // The rename itself is durable only once its directory is.
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("flush {}: {error}", parent.display()))?;
        }
        #[cfg(not(unix))]
        let _ = parent;
        Ok(())
    })();
    if result.is_err() {
        // Best effort: the replacement error is what is reported.
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn rename(source: &Path, destination: &Path, _durable: bool) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// `MoveFileExW`, written through for durable state, retried while a reader
/// holds the destination open.
#[cfg(windows)]
fn rename(source: &Path, destination: &Path, durable: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    let wide =
        |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (source, destination) = (wide(source), wide(destination));
    let flags = if durable {
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
    } else {
        MOVEFILE_REPLACE_EXISTING
    };
    let deadline = std::time::Instant::now() + SHARING_RETRY;
    loop {
        // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that
        // live for the whole call, and MoveFileExW does not retain them.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !transient_sharing_error(&error) || std::time::Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_whole_and_reads_are_bounded() {
        let directory =
            std::env::temp_dir().join(format!("virtio-mem-guest-files-{}", std::process::id()));
        let path = directory.join("nested").join("state.json");
        assert_eq!(read_bounded(&path, 16).unwrap(), None);
        replace_file(&path, b"first", false).unwrap();
        replace_file(&path, b"second", true).unwrap();
        assert_eq!(read_bounded(&path, 16).unwrap().unwrap(), b"second");
        assert!(read_bounded(&path, 5).is_err());
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file remains");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
