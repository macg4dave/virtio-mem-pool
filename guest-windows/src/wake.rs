//! Optional virtio-serial doorbell. No protocol data or resize authority goes
//! over this channel: every wake re-reads the existing durable files.
use crate::service_host::StopSignal;

#[derive(Debug, Default)]
pub(crate) struct WakeChannel {
    enabled: bool,
    #[cfg(windows)]
    native: Option<native::Channel>,
    #[cfg(windows)]
    reported: bool,
    #[cfg(windows)]
    event_source: String,
}

impl WakeChannel {
    pub fn enabled(service_name: &str) -> Self {
        #[cfg(not(windows))]
        let _ = service_name;
        Self {
            enabled: true,
            #[cfg(windows)]
            event_source: service_name.to_owned(),
            ..Self::default()
        }
    }

    pub fn refresh(&mut self) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        #[cfg(windows)]
        {
            if self.native.is_none() {
                match native::Channel::open() {
                    Ok(channel) => {
                        self.report(true, "virtio-serial wake channel opened");
                        self.native = Some(channel);
                        self.reported = false;
                    }
                    Err(error) => {
                        if !self.reported {
                            self.report(
                                false,
                                &format!(
                                    "virtio-serial wake open failed; polling continues: {error}"
                                ),
                            );
                            self.reported = true;
                        }
                    }
                }
            }
            if let Some(channel) = &mut self.native {
                match channel.arm() {
                    Ok(event) => return Some(event),
                    Err(error) => {
                        self.report(
                            false,
                            &format!("virtio-serial wake read failed; polling continues: {error}"),
                        );
                        self.native = None;
                    }
                }
            }
        }
        None
    }

    #[cfg(windows)]
    fn report(&self, connected: bool, message: &str) {
        use crate::event_log::{
            ServiceEvent, ServiceEventId, ServiceEventLevel, ServiceEventSink, WindowsEventLog,
        };
        eprintln!("{message}");
        let (id, level) = if connected {
            (
                ServiceEventId::WakeConnected,
                ServiceEventLevel::Information,
            )
        } else {
            (ServiceEventId::WakeUnavailable, ServiceEventLevel::Warning)
        };
        if let Err(error) = WindowsEventLog::open(&self.event_source)
            .and_then(|log| log.emit(&ServiceEvent::new(id, level, message)))
        {
            eprintln!("publish wake channel event failed: {error}");
        }
    }

    pub fn event(&self) -> Option<usize> {
        #[cfg(windows)]
        {
            self.native.as_ref().map(native::Channel::event)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    pub fn notify(&mut self, stop: &StopSignal) {
        #[cfg(windows)]
        if let Some(channel) = &mut self.native {
            if let Err(error) = channel.notify(stop) {
                self.report(
                    false,
                    &format!("virtio-serial wake write failed; polling continues: {error}"),
                );
                self.native = None;
            }
        }
        #[cfg(not(windows))]
        {
            let _ = stop;
        }
    }
}

#[cfg(windows)]
mod native {
    use crate::service_host::StopSignal;
    use std::io;
    use std::ptr;
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::ntdef::HANDLE;
    use winapi::shared::winerror::{ERROR_IO_INCOMPLETE, ERROR_IO_PENDING};
    use winapi::um::fileapi::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::ioapiset::{CancelIoEx, GetOverlappedResult};
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::synchapi::{CreateEventW, ResetEvent, WaitForMultipleObjects};
    use winapi::um::winbase::FILE_FLAG_OVERLAPPED;
    use winapi::um::winnt::{GENERIC_READ, GENERIC_WRITE};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: each successful handle is uniquely owned and closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct ReadOperation {
        overlapped: OVERLAPPED,
        byte: [u8; 1],
        pending: bool,
    }

    pub struct Channel {
        port: Handle,
        event: Handle,
        read: Box<ReadOperation>,
    }
    impl std::fmt::Debug for Channel {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Channel").finish_non_exhaustive()
        }
    }
    // SAFETY: handles and boxed I/O storage are uniquely owned. Moving the
    // channel to its service worker never moves the pending read's storage;
    // no other thread accesses it. Drop cancels and completes outstanding I/O.
    unsafe impl Send for Channel {}

    impl Channel {
        pub fn open() -> io::Result<Self> {
            let path: Vec<u16> = format!(
                "\\\\.\\Global\\{}\0",
                virtio_mem_core::MEMORY_WAKE_CHANNEL_NAME
            )
            .encode_utf16()
            .collect();
            // SAFETY: NUL-terminated path and null optional security/template
            // pointers satisfy CreateFileW. The returned handle is owned here.
            let port = unsafe {
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
            if port == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let port = Handle(port);
            // SAFETY: null security/name and documented event flags.
            let event = unsafe { CreateEventW(ptr::null_mut(), 1, 0, ptr::null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = Handle(event);
            // SAFETY: zero is the documented initial OVERLAPPED state.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event.0;
            Ok(Self {
                port,
                event,
                read: Box::new(ReadOperation {
                    overlapped,
                    byte: [0],
                    pending: false,
                }),
            })
        }

        pub fn event(&self) -> usize {
            self.event.0 as usize
        }

        pub fn arm(&mut self) -> io::Result<usize> {
            if self.read.pending {
                let mut count = 0;
                // SAFETY: boxed read storage and its handle remain alive until
                // completion; a nonblocking check does not release storage.
                if unsafe {
                    GetOverlappedResult(self.port.0, &mut self.read.overlapped, &mut count, 0)
                } == 0
                {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
                        return Ok(self.event.0 as usize);
                    }
                    return Err(io::Error::new(
                        error.kind(),
                        format!("complete pending read: {error}"),
                    ));
                }
                self.read.pending = false;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "wake channel closed",
                    ));
                }
            }
            // SAFETY: no read remains pending; reset before issuing a new read
            // into stable boxed storage, retained through cancellation/completion.
            unsafe {
                ResetEvent(self.event.0);
            }
            let result = unsafe {
                ReadFile(
                    self.port.0,
                    self.read.byte.as_mut_ptr().cast(),
                    1,
                    ptr::null_mut(),
                    &mut self.read.overlapped,
                )
            };
            if result == 0
                && io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING as i32)
            {
                let error = io::Error::last_os_error();
                return Err(io::Error::new(
                    error.kind(),
                    format!("arm new read: {error}"),
                ));
            }
            self.read.pending = true;
            Ok(self.event.0 as usize)
        }

        pub fn notify(&mut self, stop: &StopSignal) -> io::Result<()> {
            if stop.is_cancelled() {
                return Ok(());
            }
            // SAFETY: null security/name and documented event flags.
            let raw = unsafe { CreateEventW(ptr::null_mut(), 1, 0, ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = Handle(raw);
            // SAFETY: zero is the documented initial OVERLAPPED state.
            let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
            operation.hEvent = event.0;
            let byte = [1u8];
            // SAFETY: byte, OVERLAPPED and event remain live until the write
            // completes or is cancelled and completion is joined below.
            if unsafe {
                WriteFile(
                    self.port.0,
                    byte.as_ptr().cast(),
                    1,
                    ptr::null_mut(),
                    &mut operation,
                )
            } != 0
            {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
            let mut handles = vec![event.0];
            if let Some(stop) = stop.native_event() {
                handles.push(stop as HANDLE);
            }
            // SAFETY: owned event handles remain live for this bounded wait.
            let waited =
                unsafe { WaitForMultipleObjects(handles.len() as DWORD, handles.as_ptr(), 0, 100) };
            if waited != 0 {
                // SAFETY: operation belongs to this port and has stable storage.
                unsafe {
                    CancelIoEx(self.port.0, &mut operation);
                }
            }
            let mut count = 0;
            // SAFETY: join completion before the stack buffer and event drop,
            // even when cancellation raced with a successful write.
            if unsafe { GetOverlappedResult(self.port.0, &mut operation, &mut count, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
    impl Drop for Channel {
        fn drop(&mut self) {
            if self.read.pending {
                let mut count = 0;
                // SAFETY: cancel our read and join it before releasing its box
                // or handles. Cancellation errors may mean it already completed.
                unsafe {
                    CancelIoEx(self.port.0, &mut self.read.overlapped);
                    GetOverlappedResult(self.port.0, &mut self.read.overlapped, &mut count, 1);
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod port_tests {
    use super::*;
    #[test]
    #[ignore = "requires the dedicated VM serial port and running host controller"]
    fn pending_read_survives_writes_and_periodic_sampling() {
        let mut port = native::Channel::open().expect("open dedicated wake port");
        port.arm().expect("arm first read");
        port.notify(&StopSignal::new())
            .expect("write hint with pending read");
        std::thread::sleep(std::time::Duration::from_millis(250));
        port.arm().expect("read remains valid after write");
        port.notify(&StopSignal::new()).expect("second write");
        port.arm().expect("read remains valid after second write");
    }
}
