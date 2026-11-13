//! One long-lived libvirt connection for the controller's per-poll commands.
//!
//! Every `virsh` process opens its own libvirt connection, and libvirt asks
//! polkit to authorize each one. At a 1 s poll that was several polkit checks
//! a second; polkitd exhausted its file descriptors and then refused every
//! check, which stopped the controller (27 October). This session connects
//! once and serves `dumpxml` and `qemu-agent-command` over that connection.
//! Rare commands (resize, suspend/resume, version, QMP) stay on `virsh`.
//!
//! libvirt is loaded at runtime with `dlopen`: the host has the runtime
//! library but not its development headers or unversioned link symlink.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crate::virsh::VirshError;

/// `VIR_DOMAIN_QEMU_AGENT_COMMAND_DEFAULT`, which virsh also uses.
const AGENT_TIMEOUT_DEFAULT: c_int = -1;

type Ptr = *mut c_void;

type ConnectOpen = unsafe extern "C" fn(*const c_char) -> Ptr;
type HandleCall = unsafe extern "C" fn(Ptr) -> c_int;
type LookupByName = unsafe extern "C" fn(Ptr, *const c_char) -> Ptr;
type GetXmlDesc = unsafe extern "C" fn(Ptr, c_uint) -> *mut c_char;
type AgentCommand = unsafe extern "C" fn(Ptr, *const c_char, c_int, c_uint) -> *mut c_char;
type LastErrorMessage = unsafe extern "C" fn() -> *const c_char;
type ResetLastError = unsafe extern "C" fn();

struct Api {
    connect_open: ConnectOpen,
    connect_close: HandleCall,
    connect_is_alive: HandleCall,
    lookup_by_name: LookupByName,
    domain_free: HandleCall,
    get_xml_desc: GetXmlDesc,
    agent_command: AgentCommand,
    last_error_message: LastErrorMessage,
    reset_last_error: ResetLastError,
}

impl Api {
    fn load() -> Result<Self, String> {
        use std::mem::transmute;
        let libvirt = open_library(c"libvirt.so.0")?;
        let qemu = open_library(c"libvirt-qemu.so.0")?;
        // SAFETY: each symbol is resolved from the library that exports it and
        // cast to the signature documented in libvirt's public headers. The
        // libraries are never closed, so the pointers stay valid.
        unsafe {
            Ok(Self {
                connect_open: transmute::<Ptr, ConnectOpen>(symbol(libvirt, c"virConnectOpen")?),
                connect_close: transmute::<Ptr, HandleCall>(symbol(libvirt, c"virConnectClose")?),
                connect_is_alive: transmute::<Ptr, HandleCall>(symbol(
                    libvirt,
                    c"virConnectIsAlive",
                )?),
                lookup_by_name: transmute::<Ptr, LookupByName>(symbol(
                    libvirt,
                    c"virDomainLookupByName",
                )?),
                domain_free: transmute::<Ptr, HandleCall>(symbol(libvirt, c"virDomainFree")?),
                get_xml_desc: transmute::<Ptr, GetXmlDesc>(symbol(
                    libvirt,
                    c"virDomainGetXMLDesc",
                )?),
                agent_command: transmute::<Ptr, AgentCommand>(symbol(
                    qemu,
                    c"virDomainQemuAgentCommand",
                )?),
                last_error_message: transmute::<Ptr, LastErrorMessage>(symbol(
                    libvirt,
                    c"virGetLastErrorMessage",
                )?),
                reset_last_error: transmute::<Ptr, ResetLastError>(symbol(
                    libvirt,
                    c"virResetLastError",
                )?),
            })
        }
    }

    /// The calling thread's last libvirt error. Must run on the thread that
    /// made the failing call.
    fn last_error(&self) -> String {
        // SAFETY: libvirt returns a thread-local, NUL-terminated string that
        // stays valid until the next libvirt call on this thread.
        let message = unsafe { (self.last_error_message)() };
        if message.is_null() {
            return "unknown libvirt error".to_owned();
        }
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

fn open_library(name: &CStr) -> Result<Ptr, String> {
    // SAFETY: dlopen takes a NUL-terminated name; the handle is never closed.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("load {}: {}", name.to_string_lossy(), dl_error()));
    }
    Ok(handle)
}

fn symbol(library: Ptr, name: &CStr) -> Result<Ptr, String> {
    // SAFETY: library is a live dlopen handle and name is NUL-terminated.
    let address = unsafe { libc::dlsym(library, name.as_ptr()) };
    if address.is_null() {
        return Err(format!(
            "resolve {}: {}",
            name.to_string_lossy(),
            dl_error()
        ));
    }
    Ok(address)
}

fn dl_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_owned()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Takes ownership of a libvirt-allocated string.
fn take_string(value: *mut c_char) -> String {
    // SAFETY: libvirt returns a malloc'd NUL-terminated string the caller frees.
    let text = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(value.cast()) };
    text
}

enum Request {
    DumpXml { vm: String },
    Agent { vm: String, command: String },
}

type Job = (Request, Sender<Result<String, String>>);

/// The worker thread owns the connection, so libvirt's thread-local error
/// messages are always read on the thread that failed.
struct Worker {
    api: Api,
    uri: CString,
    connection: Ptr,
}

impl Worker {
    fn serve(mut self, jobs: Receiver<Job>) {
        for (request, reply) in jobs {
            let result = self.execute(&request);
            if result.is_err() && !self.connection.is_null() {
                // SAFETY: connection is a live handle owned by this worker.
                if unsafe { (self.api.connect_is_alive)(self.connection) } != 1 {
                    self.disconnect();
                }
            }
            if reply.send(result).is_err() {
                // The caller timed out and replaced this worker.
                break;
            }
        }
        self.disconnect();
    }

    fn connect(&mut self) -> Result<Ptr, String> {
        if self.connection.is_null() {
            // SAFETY: uri is NUL-terminated; a null result reports an error.
            let connection = unsafe { (self.api.connect_open)(self.uri.as_ptr()) };
            if connection.is_null() {
                return Err(format!("connect to libvirt: {}", self.api.last_error()));
            }
            eprintln!("virtio-mem-host: event=libvirt_connected");
            self.connection = connection;
        }
        Ok(self.connection)
    }

    fn disconnect(&mut self) {
        if !self.connection.is_null() {
            // SAFETY: connection is a live handle owned by this worker.
            unsafe { (self.api.connect_close)(self.connection) };
            self.connection = std::ptr::null_mut();
        }
    }

    fn execute(&mut self, request: &Request) -> Result<String, String> {
        // SAFETY: resets this thread's error so a stale one is never reported.
        unsafe { (self.api.reset_last_error)() };
        let connection = self.connect()?;
        let vm = match request {
            Request::DumpXml { vm } | Request::Agent { vm, .. } => vm,
        };
        let name = CString::new(vm.as_str()).map_err(|_| "VM name contains NUL".to_owned())?;
        // SAFETY: connection is live and name is NUL-terminated.
        let domain = unsafe { (self.api.lookup_by_name)(connection, name.as_ptr()) };
        if domain.is_null() {
            return Err(format!(
                "failed to get domain '{vm}': {}",
                self.api.last_error()
            ));
        }
        let output = match request {
            // SAFETY: domain is live; flags 0 matches `virsh dumpxml`.
            Request::DumpXml { .. } => unsafe { (self.api.get_xml_desc)(domain, 0) },
            Request::Agent { command, .. } => match CString::new(command.as_str()) {
                // SAFETY: domain is live and command is NUL-terminated.
                Ok(command) => unsafe {
                    (self.api.agent_command)(domain, command.as_ptr(), AGENT_TIMEOUT_DEFAULT, 0)
                },
                Err(_) => std::ptr::null_mut(),
            },
        };
        let result = if output.is_null() {
            Err(self.api.last_error())
        } else {
            Ok(take_string(output))
        };
        // SAFETY: domain was returned by lookup and is freed exactly once.
        unsafe { (self.api.domain_free)(domain) };
        result
    }
}

/// A shared, lazily connected libvirt session. A call that exceeds the
/// timeout abandons its worker (and connection) and the next call starts a
/// fresh one, as killing a timed-out `virsh` did.
pub struct LibvirtSession {
    uri: String,
    timeout: Duration,
    jobs: Mutex<Option<Sender<Job>>>,
}

impl std::fmt::Debug for LibvirtSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibvirtSession")
            .field("uri", &self.uri)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl LibvirtSession {
    pub fn new(uri: impl Into<String>, timeout: Duration) -> Self {
        Self {
            uri: uri.into(),
            timeout,
            jobs: Mutex::new(None),
        }
    }

    /// Serves the `virsh` commands this session supports, or `None` to let
    /// the caller run `virsh`.
    pub fn run(&self, arguments: &[String]) -> Option<Result<String, VirshError>> {
        let request = match arguments {
            [operation, vm] if operation == "dumpxml" => Request::DumpXml { vm: vm.clone() },
            [operation, vm, command] if operation == "qemu-agent-command" => Request::Agent {
                vm: vm.clone(),
                command: command.clone(),
            },
            _ => return None,
        };
        Some(self.call(request))
    }

    fn call(&self, request: Request) -> Result<String, VirshError> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if jobs.is_none() {
            *jobs = Some(self.spawn().map_err(|error| VirshError::Failed {
                status: "libvirt session unavailable".to_owned(),
                stderr: error,
            })?);
        }
        let (reply, response) = mpsc::channel();
        let sent = jobs
            .as_ref()
            .is_some_and(|sender| sender.send((request, reply)).is_ok());
        let result = if sent {
            response.recv_timeout(self.timeout)
        } else {
            Err(RecvTimeoutError::Disconnected)
        };
        match result {
            Ok(result) => result.map_err(|stderr| VirshError::Failed {
                status: "libvirt error".to_owned(),
                stderr: format!("error: {stderr}"),
            }),
            Err(error) => {
                // Abandon the worker; a stuck call cannot block later ones.
                *jobs = None;
                Err(match error {
                    RecvTimeoutError::Timeout => VirshError::Timeout(self.timeout),
                    RecvTimeoutError::Disconnected => VirshError::Failed {
                        status: "libvirt session ended".to_owned(),
                        stderr: String::new(),
                    },
                })
            }
        }
    }

    fn spawn(&self) -> Result<Sender<Job>, String> {
        let api = Api::load()?;
        let uri = CString::new(self.uri.as_str()).map_err(|_| "URI contains NUL".to_owned())?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("libvirt-session".to_owned())
            .spawn(move || {
                Worker {
                    api,
                    uri,
                    connection: std::ptr::null_mut(),
                }
                .serve(receiver)
            })
            .map_err(|error| format!("start libvirt session thread: {error}"))?;
        Ok(sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn only_per_poll_commands_use_the_session() {
        let session = LibvirtSession::new("test:///default", Duration::from_secs(5));
        for other in [
            arguments(&["update-memory-device", "guest", "--alias", "a"]),
            arguments(&["suspend", "guest"]),
            arguments(&["resume", "guest"]),
            arguments(&["version"]),
            arguments(&["domuuid", "guest"]),
            arguments(&["qemu-monitor-command", "guest", "{}"]),
            arguments(&["dumpxml", "guest", "--inactive"]),
        ] {
            assert!(session.run(&other).is_none(), "{other:?}");
        }
    }

    /// libvirt's built-in test driver: no daemon, polkit or VM required.
    #[test]
    fn serves_dumpxml_and_reports_errors_from_one_connection() {
        if Api::load().is_err() {
            eprintln!("libvirt runtime library unavailable; skipped");
            return;
        }
        let session = LibvirtSession::new("test:///default", Duration::from_secs(5));
        for _ in 0..3 {
            let xml = session
                .run(&arguments(&["dumpxml", "test"]))
                .expect("served")
                .expect("test domain XML");
            assert!(xml.contains("<name>test</name>"), "{xml}");
        }
        let error = session
            .run(&arguments(&["dumpxml", "missing"]))
            .expect("served")
            .expect_err("unknown domain");
        assert!(error.to_string().contains("missing"), "{error}");
    }

    /// Live check against a running VM with a guest agent:
    /// `VIRTIO_MEM_LIVE_VM=<domain> cargo test -p virtio-mem-host live_ -- --ignored`
    #[test]
    #[ignore]
    fn live_session_matches_virsh_on_a_running_vm() {
        let vm = std::env::var("VIRTIO_MEM_LIVE_VM").expect("VIRTIO_MEM_LIVE_VM");
        let session = LibvirtSession::new("qemu:///system", Duration::from_secs(10));
        let xml = session
            .run(&arguments(&["dumpxml", &vm]))
            .expect("served")
            .expect("domain XML");
        let virsh = std::process::Command::new("virsh")
            .args(["-c", "qemu:///system", "dumpxml", &vm])
            .output()
            .expect("virsh");
        assert_eq!(
            xml.trim_end(),
            String::from_utf8_lossy(&virsh.stdout).trim_end()
        );
        for _ in 0..3 {
            let pong = session
                .run(&arguments(&[
                    "qemu-agent-command",
                    &vm,
                    r#"{"execute":"guest-ping"}"#,
                ]))
                .expect("served")
                .expect("guest-ping");
            assert!(pong.contains("\"return\""), "{pong}");
        }
    }
}
