//! One long-lived libvirt connection for the controller's per-poll commands.
//!
//! Every `virsh` process opens its own libvirt connection, and libvirt asks
//! polkit to authorize each one. At a 1 s poll that was several polkit checks
//! a second; polkitd exhausted its file descriptors and then refused every
//! check, which stopped the controller (27 October). This session connects
//! once and serves `dumpxml`, `qemu-agent-command`, `qemu-monitor-command`
//! and live `update-memory-device` over that connection: with several VMs
//! growing at once, a `virsh` process (and polkit check) per resize cost
//! about 230 ms each, one after another. Rare commands (suspend/resume,
//! version, `domxml-to-native`) stay on `virsh`.
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
/// `VIR_DOMAIN_AFFECT_LIVE`, as `virsh update-memory-device --live`.
const AFFECT_LIVE: c_uint = 1;

type Ptr = *mut c_void;

type ConnectOpen = unsafe extern "C" fn(*const c_char) -> Ptr;
type HandleCall = unsafe extern "C" fn(Ptr) -> c_int;
type LookupByName = unsafe extern "C" fn(Ptr, *const c_char) -> Ptr;
type GetXmlDesc = unsafe extern "C" fn(Ptr, c_uint) -> *mut c_char;
type AgentCommand = unsafe extern "C" fn(Ptr, *const c_char, c_int, c_uint) -> *mut c_char;
type MonitorCommand = unsafe extern "C" fn(Ptr, *const c_char, *mut *mut c_char, c_uint) -> c_int;
type UpdateDeviceFlags = unsafe extern "C" fn(Ptr, *const c_char, c_uint) -> c_int;
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
    monitor_command: MonitorCommand,
    update_device_flags: UpdateDeviceFlags,
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
                monitor_command: transmute::<Ptr, MonitorCommand>(symbol(
                    qemu,
                    c"virDomainQemuMonitorCommand",
                )?),
                update_device_flags: transmute::<Ptr, UpdateDeviceFlags>(symbol(
                    libvirt,
                    c"virDomainUpdateDeviceFlags",
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
    DumpXml {
        vm: String,
    },
    Agent {
        vm: String,
        command: String,
    },
    Monitor {
        vm: String,
        command: String,
    },
    /// A live device update; `xml` is the device's complete element.
    UpdateDevice {
        vm: String,
        xml: String,
    },
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
            eprintln!("rampire: event=libvirt_connected");
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
            Request::DumpXml { vm }
            | Request::Agent { vm, .. }
            | Request::Monitor { vm, .. }
            | Request::UpdateDevice { vm, .. } => vm,
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
        let result = match request {
            // SAFETY: domain is live; flags 0 matches `virsh dumpxml`.
            Request::DumpXml { .. } => self.owned(unsafe { (self.api.get_xml_desc)(domain, 0) }),
            Request::Agent { command, .. } => match CString::new(command.as_str()) {
                // SAFETY: domain is live and command is NUL-terminated.
                Ok(command) => self.owned(unsafe {
                    (self.api.agent_command)(domain, command.as_ptr(), AGENT_TIMEOUT_DEFAULT, 0)
                }),
                Err(_) => Err("agent command contains NUL".to_owned()),
            },
            Request::Monitor { command, .. } => match CString::new(command.as_str()) {
                Ok(command) => {
                    let mut reply: *mut c_char = std::ptr::null_mut();
                    // SAFETY: domain is live, command is NUL-terminated and
                    // reply receives a malloc'd string on success; flags 0 is
                    // VIR_DOMAIN_QEMU_MONITOR_COMMAND_DEFAULT, as virsh uses.
                    let status = unsafe {
                        (self.api.monitor_command)(domain, command.as_ptr(), &mut reply, 0)
                    };
                    if status == 0 {
                        self.owned(reply)
                    } else {
                        Err(self.api.last_error())
                    }
                }
                Err(_) => Err("monitor command contains NUL".to_owned()),
            },
            Request::UpdateDevice { xml, .. } => match CString::new(xml.as_str()) {
                // SAFETY: domain is live and xml is NUL-terminated.
                Ok(xml) => match unsafe {
                    (self.api.update_device_flags)(domain, xml.as_ptr(), AFFECT_LIVE)
                } {
                    0 => Ok(String::new()),
                    _ => Err(self.api.last_error()),
                },
                Err(_) => Err("device XML contains NUL".to_owned()),
            },
        };
        // SAFETY: domain was returned by lookup and is freed exactly once.
        unsafe { (self.api.domain_free)(domain) };
        result
    }

    /// A libvirt-allocated result, or this thread's error when it is null.
    fn owned(&self, output: *mut c_char) -> Result<String, String> {
        if output.is_null() {
            Err(self.api.last_error())
        } else {
            Ok(take_string(output))
        }
    }
}

/// The `<memory model='virtio-mem'>` element with `alias` from live domain
/// XML, with its `<requested>` set to `requested_kib`: the device XML
/// `virsh update-memory-device` builds. `None` unless exactly one such
/// device and one KiB `<requested>` in it are found.
fn virtio_mem_update_xml(domain_xml: &str, alias: &str, requested_kib: u64) -> Option<String> {
    const START: &str = "<memory model='virtio-mem'";
    const END: &str = "</memory>";
    const OPEN: &str = "<requested unit='KiB'>";
    const CLOSE: &str = "</requested>";
    let alias_tag = format!("<alias name='{alias}'/>");
    let mut found = domain_xml.match_indices(START).filter_map(|(start, _)| {
        let end = start + domain_xml[start..].find(END)? + END.len();
        let element = &domain_xml[start..end];
        element.contains(&alias_tag).then_some(element)
    });
    let element = found.next()?;
    if found.next().is_some() || element.matches(OPEN).count() != 1 {
        return None;
    }
    let value_start = element.find(OPEN)? + OPEN.len();
    let value_end = value_start + element[value_start..].find(CLOSE)?;
    element[value_start..value_end].parse::<u64>().ok()?;
    Some(format!(
        "{}{requested_kib}{}",
        &element[..value_start],
        &element[value_end..]
    ))
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
            [operation, vm, command] if operation == "qemu-monitor-command" => Request::Monitor {
                vm: vm.clone(),
                command: command.clone(),
            },
            [operation, vm, alias_flag, alias, size_flag, kib, live]
                if operation == "update-memory-device"
                    && alias_flag == "--alias"
                    && size_flag == "--requested-size"
                    && live == "--live" =>
            {
                // Build the device XML from live state as virsh does. Anything
                // short of a complete element leaves the command to virsh, so
                // nothing is attempted here that virsh would not attempt.
                let requested_kib = kib.parse().ok()?;
                let domain_xml = self.call(Request::DumpXml { vm: vm.clone() }).ok()?;
                let xml = virtio_mem_update_xml(&domain_xml, alias, requested_kib)?;
                Request::UpdateDevice {
                    vm: vm.clone(),
                    xml,
                }
            }
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
            arguments(&[
                "update-memory-device",
                "guest",
                "--alias",
                "a",
                "--requested-size",
                "1024",
                "--config",
            ]),
            arguments(&["suspend", "guest"]),
            arguments(&["resume", "guest"]),
            arguments(&["version"]),
            arguments(&["domuuid", "guest"]),
            arguments(&["domxml-to-native", "qemu-argv", "--domain", "guest"]),
            arguments(&["dumpxml", "guest", "--inactive"]),
        ] {
            assert!(session.run(&other).is_none(), "{other:?}");
        }
    }

    const DEVICE: &str = "    <memory model='virtio-mem'>
      <target>
        <size unit='KiB'>15728640</size>
        <node>0</node>
        <block unit='KiB'>2048</block>
        <requested unit='KiB'>284672</requested>
        <current unit='KiB'>284672</current>
        <address base='0x100000000'/>
      </target>
      <alias name='ua-virtiomem0'/>
      <address type='pci' domain='0x0000' bus='0x00' slot='0x07' function='0x0'/>
    </memory>
";

    fn live_xml(devices: &str) -> String {
        format!(
            "<domain type='kvm' id='7'>
  <devices>
    <memory model='dimm'>
      <target><size unit='KiB'>1048576</size></target>
      <alias name='dimm0'/>
    </memory>
{devices}  </devices>
</domain>"
        )
    }

    #[test]
    fn a_resize_sends_the_devices_live_element_with_only_requested_changed() {
        let xml =
            virtio_mem_update_xml(&live_xml(DEVICE), "ua-virtiomem0", 1_048_576).expect("element");
        assert!(xml.starts_with("<memory model='virtio-mem'>"), "{xml}");
        assert!(xml.ends_with("</memory>"), "{xml}");
        assert!(
            xml.contains("<requested unit='KiB'>1048576</requested>"),
            "{xml}"
        );
        assert!(
            xml.contains("<current unit='KiB'>284672</current>"),
            "{xml}"
        );
        assert!(!xml.contains("dimm"), "{xml}");
        // No such device, two of them, or no KiB requested: left to virsh.
        assert_eq!(virtio_mem_update_xml(&live_xml(DEVICE), "other", 1), None);
        let twice = live_xml(&format!("{DEVICE}{DEVICE}"));
        assert_eq!(virtio_mem_update_xml(&twice, "ua-virtiomem0", 1), None);
        let bytes = DEVICE.replace(
            "<requested unit='KiB'>284672",
            "<requested unit='bytes'>291504128",
        );
        assert_eq!(
            virtio_mem_update_xml(&live_xml(&bytes), "ua-virtiomem0", 1),
            None
        );
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
    /// `RAMPIRE_LIVE_VM=<domain> cargo test -p rampire live_ -- --ignored`
    #[test]
    #[ignore]
    fn live_session_matches_virsh_on_a_running_vm() {
        let vm = std::env::var("RAMPIRE_LIVE_VM").expect("RAMPIRE_LIVE_VM");
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
