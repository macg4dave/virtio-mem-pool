//! The control channel on Windows: the virtio-serial port
//! `org.virtio-mem.control.0`, opened for overlapped I/O and presented as a
//! non-blocking stream for the shared transport. A read that cannot finish
//! now reports `WouldBlock` and stays queued; a write waits at most a moment
//! for the host and otherwise reports `WouldBlock`, so the newest record is
//! sent later instead.

#[cfg(windows)]
pub use native::ControlPort;

#[cfg(windows)]
mod native {
    use std::io;
    use std::ptr;

    use winapi::shared::minwindef::DWORD;
    use winapi::shared::ntdef::HANDLE;
    use winapi::shared::winerror::{ERROR_IO_INCOMPLETE, ERROR_IO_PENDING};
    use winapi::um::fileapi::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::ioapiset::{CancelIoEx, GetOverlappedResult};
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::synchapi::{CreateEventW, ResetEvent, WaitForSingleObject};
    use winapi::um::winbase::{FILE_FLAG_OVERLAPPED, WAIT_OBJECT_0};
    use winapi::um::winnt::{GENERIC_READ, GENERIC_WRITE};

    /// How long a write waits for the host to take it.
    const WRITE_WAIT_MILLIS: DWORD = 100;
    const READ_BYTES: usize = 4096;

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: each handle is uniquely owned and closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn event() -> io::Result<Handle> {
        // SAFETY: null security and name with documented flags (manual reset).
        let raw = unsafe { CreateEventW(ptr::null_mut(), 1, 0, ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle(raw))
    }

    /// A read in flight or finished. Boxed so the kernel's pointers into it
    /// stay valid however the port moves.
    struct Read {
        overlapped: OVERLAPPED,
        event: Handle,
        buffer: [u8; READ_BYTES],
        pending: bool,
        /// Received bytes not yet handed out: `buffer[start..end]`.
        start: usize,
        end: usize,
    }

    pub struct ControlPort {
        port: Handle,
        read: Box<Read>,
    }

    impl std::fmt::Debug for ControlPort {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("ControlPort")
                .field("pending_read", &self.read.pending)
                .finish_non_exhaustive()
        }
    }

    // SAFETY: the handles and the boxed read are uniquely owned; the worker
    // that owns the port is the only user, and Drop joins outstanding I/O.
    unsafe impl Send for ControlPort {}

    impl ControlPort {
        pub fn open() -> io::Result<Self> {
            let path: Vec<u16> = format!(
                "\\\\.\\Global\\{}\0",
                virtio_mem_core::MEMORY_CONTROL_CHANNEL_NAME
            )
            .encode_utf16()
            .collect();
            // SAFETY: NUL-terminated path; null security and template.
            let raw = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let port = Handle(raw);
            let event = event()?;
            // SAFETY: zero is the documented initial OVERLAPPED state.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event.0;
            Ok(Self {
                port,
                read: Box::new(Read {
                    overlapped,
                    event,
                    buffer: [0; READ_BYTES],
                    pending: false,
                    start: 0,
                    end: 0,
                }),
            })
        }

        /// Hand out buffered bytes.
        fn take(&mut self, into: &mut [u8]) -> usize {
            let count = into.len().min(self.read.end - self.read.start);
            into[..count]
                .copy_from_slice(&self.read.buffer[self.read.start..self.read.start + count]);
            self.read.start += count;
            count
        }
    }

    impl io::Read for ControlPort {
        fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
            if self.read.start < self.read.end {
                return Ok(self.take(into));
            }
            if !self.read.pending {
                let read = &mut *self.read;
                // SAFETY: no read is pending, so the event and buffer are
                // free; the boxed storage outlives the operation (see Drop).
                unsafe {
                    ResetEvent(read.event.0);
                }
                let issued = unsafe {
                    ReadFile(
                        self.port.0,
                        read.buffer.as_mut_ptr().cast(),
                        READ_BYTES as DWORD,
                        ptr::null_mut(),
                        &mut read.overlapped,
                    )
                };
                if issued == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                        return Err(error);
                    }
                }
                read.pending = true;
            }
            let mut count: DWORD = 0;
            // SAFETY: the pending read's storage is alive; this check does
            // not wait.
            if unsafe { GetOverlappedResult(self.port.0, &mut self.read.overlapped, &mut count, 0) }
                == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.read.pending = false;
                return Err(error);
            }
            self.read.pending = false;
            if count == 0 {
                // The host end is not connected.
                return Ok(0);
            }
            self.read.start = 0;
            self.read.end = count as usize;
            Ok(self.take(into))
        }
    }

    impl io::Write for ControlPort {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let event = event()?;
            // SAFETY: zero is the documented initial OVERLAPPED state.
            let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
            operation.hEvent = event.0;
            let length = DWORD::try_from(bytes.len()).unwrap_or(DWORD::MAX);
            // SAFETY: `bytes`, `operation` and `event` stay alive until the
            // write is joined below.
            let issued = unsafe {
                WriteFile(
                    self.port.0,
                    bytes.as_ptr().cast(),
                    length,
                    ptr::null_mut(),
                    &mut operation,
                )
            };
            if issued == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(error);
                }
                // SAFETY: the event belongs to this operation.
                if unsafe { WaitForSingleObject(event.0, WRITE_WAIT_MILLIS) } != WAIT_OBJECT_0 {
                    // SAFETY: cancels only this operation on this port.
                    unsafe {
                        CancelIoEx(self.port.0, &mut operation);
                    }
                }
            }
            let mut count: DWORD = 0;
            // SAFETY: join the write before its storage goes away, whether
            // it finished or was cancelled.
            let joined = unsafe { GetOverlappedResult(self.port.0, &mut operation, &mut count, 1) };
            match (joined, count) {
                (_, count) if count > 0 => Ok(count as usize),
                // Cancelled before the host took anything.
                _ => Err(io::ErrorKind::WouldBlock.into()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ControlPort {
        fn drop(&mut self) {
            if self.read.pending {
                let mut count: DWORD = 0;
                // SAFETY: cancel our read and join it before its box and
                // handles are released; it may already have completed.
                unsafe {
                    CancelIoEx(self.port.0, &mut self.read.overlapped);
                    GetOverlappedResult(self.port.0, &mut self.read.overlapped, &mut count, 1);
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::{Read, Write};

    #[test]
    #[ignore = "requires the VM's control port and a running host controller"]
    fn the_port_reads_without_blocking_and_writes_whole_records() {
        let mut port = super::ControlPort::open().expect("open the control port");
        let mut buffer = [0; 64];
        match port.read(&mut buffer) {
            Ok(_) => {}
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
        }
        port.write_all(b"{}\n").expect("write a record");
    }
}
