//! Virtio-serial wake hints. The existing validated file protocol remains the
//! authority; disconnected or missing channels retain bounded polling.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use quick_xml::{events::Event, Reader};
use virtio_mem_core::MEMORY_WAKE_CHANNEL_NAME;

use crate::virsh::{Virsh, VirshCommand};

/// Read-only libvirt events accelerate observation of resize convergence.
/// The runtime owns and reaps the subscription process.
pub struct MemoryEvents {
    child: Child,
    output: ChildStdout,
    closed: bool,
}
impl MemoryEvents {
    pub fn start(command: &Virsh) -> io::Result<Self> {
        let mut child = command
            .event_command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let Some(output) = child.stdout.take() else {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::other("event subscription has no stdout"));
        };
        // SAFETY: output owns this descriptor. Preserve its existing flags
        // while enabling bounded, nonblocking reads.
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            let error = io::Error::last_os_error();
            if let Err(cleanup) = child.kill().and_then(|_| child.wait().map(|_| ())) {
                eprintln!("event child cleanup failed: {cleanup}");
            }
            return Err(error);
        }
        Ok(Self {
            child,
            output,
            closed: false,
        })
    }
    fn drain(&mut self) {
        let mut bytes = [0; 4096];
        match self.output.read(&mut bytes) {
            Ok(0) => {
                self.closed = true;
                eprintln!("libvirt event subscription ended; polling continues");
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                self.closed = true;
                eprintln!("libvirt event read failed; polling continues: {error}");
            }
        }
    }
}
impl Drop for MemoryEvents {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    eprintln!("stop libvirt events: {error}");
                }
                if let Err(error) = self.child.wait() {
                    eprintln!("reap libvirt events: {error}");
                }
            }
            Err(error) => {
                eprintln!("check libvirt events child: {error}");
            }
        }
    }
}

#[derive(Debug)]
pub struct WakePeer {
    command: Virsh,
    vm: String,
    stream: Mutex<Option<UnixStream>>,
    reported: AtomicBool,
    /// No reconnect attempt (a domain XML read) before this instant: an
    /// absent channel would otherwise be looked up every poll.
    retry_after: Mutex<Option<Instant>>,
}

/// How long a missing or failed wake channel waits before the next lookup.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

impl WakePeer {
    pub fn new(command: Virsh, vm: String) -> Arc<Self> {
        Arc::new(Self {
            command,
            vm,
            stream: Mutex::new(None),
            reported: AtomicBool::new(false),
            retry_after: Mutex::new(None),
        })
    }

    fn stream(&self) -> MutexGuard<'_, Option<UnixStream>> {
        self.stream.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn connect(&self) {
        if self.stream().is_some() {
            return;
        }
        let mut retry_after = self
            .retry_after
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if retry_after.is_some_and(|instant| Instant::now() < instant) {
            return;
        }
        *retry_after = Instant::now().checked_add(RECONNECT_INTERVAL);
        drop(retry_after);
        let attempt = || -> Result<UnixStream, String> {
            let xml = self
                .command
                .run(&["dumpxml".to_owned(), self.vm.clone()])
                .map_err(|e| e.to_string())?;
            let path = channel_path(&xml, MEMORY_WAKE_CHANNEL_NAME)?
                .ok_or("virtio-serial wake channel is absent")?;
            let stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
            stream.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(stream)
        };
        match attempt() {
            Ok(stream) => {
                eprintln!("virtio-mem-host: vm={} event=wake_connected", self.vm);
                *self.stream() = Some(stream);
                self.reported.store(false, Ordering::Relaxed);
                // A later disconnect may reconnect straight away.
                *self
                    .retry_after
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
            }
            Err(error) => {
                if !self.reported.swap(true, Ordering::Relaxed) {
                    eprintln!("virtio-mem-host: vm={} event=wake_unavailable fallback=poll reason={error}", self.vm);
                }
            }
        }
    }

    pub fn notify(&self) {
        let mut slot = self.stream();
        if let Some(stream) = slot.as_mut() {
            match stream.write(b"\x01") {
                Ok(1) => {}
                // A queued hint already wakes the peer; no authority is lost.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                _ => {
                    *slot = None;
                }
            }
        }
    }

    fn drain(&self) {
        let mut slot = self.stream();
        if let Some(stream) = slot.as_mut() {
            let mut hints = [0; 4096];
            match stream.read(&mut hints) {
                Ok(0) => {
                    *slot = None;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    *slot = None;
                }
            }
        }
    }
}

/// Wait for a hint or a control record from any VM, or the periodic
/// fallback, with bounded stop latency. Either only accelerates a complete,
/// guarded controller round.
pub fn wait(
    peers: &[Arc<WakePeer>],
    controls: &[Arc<crate::control_channel::ControlChannel>],
    mut events: Option<&mut MemoryEvents>,
    stop: &AtomicBool,
    timeout: Duration,
) {
    for peer in peers {
        peer.connect();
    }
    for control in controls {
        control.connect();
    }
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) && started.elapsed() < timeout {
        let mut descriptors: Vec<_> = peers
            .iter()
            .filter_map(|peer| {
                peer.stream().as_ref().map(|s| libc::pollfd {
                    fd: s.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
            })
            .collect();
        descriptors.extend(controls.iter().filter_map(|control| {
            control.raw_fd().map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
        }));
        if let Some(events) = events.as_ref().filter(|e| !e.closed) {
            descriptors.push(libc::pollfd {
                fd: events.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let bound = timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_millis(100));
        // SAFETY: descriptors is a live, uniquely borrowed array of pollfd;
        // all file descriptors remain owned by peers during this call.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                bound.as_millis().max(1) as i32,
            )
        };
        if result > 0 {
            if let Some(events) = events.as_mut().filter(|e| !e.closed) {
                events.drain();
            }
            for peer in peers {
                peer.drain();
            }
            for control in controls {
                control.pump();
            }
            return;
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            eprintln!(
                "virtio-mem-host: wake wait failed: {}",
                io::Error::last_os_error()
            );
            std::thread::sleep(bound);
        }
    }
}

/// The host socket of the domain's virtio-serial port `name`, if it has one.
pub(crate) fn channel_path(xml: &str, name: &str) -> Result<Option<String>, String> {
    let mut reader = Reader::from_str(xml);
    let mut in_channel = false;
    let mut source = None;
    let mut matched = false;
    let mut unix = false;
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(tag) if tag.name().as_ref() == b"channel" => {
                in_channel = true;
                source = None;
                matched = false;
                unix = attribute(&tag, b"type")?.as_deref() == Some("unix");
            }
            Event::Empty(tag) if in_channel => {
                if tag.name().as_ref() == b"source" {
                    source = attribute(&tag, b"path")?;
                }
                if tag.name().as_ref() == b"target" {
                    matched = attribute(&tag, b"type")?.as_deref() == Some("virtio")
                        && attribute(&tag, b"name")?.as_deref() == Some(name);
                }
            }
            Event::End(tag) if tag.name().as_ref() == b"channel" => {
                if matched && unix {
                    return Ok(source);
                }
                in_channel = false;
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

fn attribute(
    tag: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
) -> Result<Option<String>, String> {
    tag.try_get_attribute(name)
        .map_err(|e| e.to_string())?
        .map(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|s| s.into_owned())
                .map_err(|e| e.to_string())
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_only_the_dedicated_channel() {
        let xml = "<domain><devices><channel type='unix'><source path='/qga'/><target type='virtio' name='org.qemu.guest_agent.0'/></channel><channel type='unix'><source path='/wake'/><target type='virtio' name='org.virtio-mem.wake.0'/></channel></devices></domain>";
        assert_eq!(
            channel_path(xml, MEMORY_WAKE_CHANNEL_NAME),
            Ok(Some("/wake".to_owned()))
        );
        assert_eq!(channel_path(xml, "org.virtio-mem.control.0"), Ok(None));
        assert_eq!(
            channel_path("<domain/>", MEMORY_WAKE_CHANNEL_NAME),
            Ok(None)
        );
    }

    #[test]
    fn socket_hint_wakes_without_waiting_for_poll_and_handles_disconnect() {
        let (ours, mut guest) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let peer = Arc::new(WakePeer {
            command: Virsh::new("unused", Duration::from_secs(1)),
            vm: "guest".to_owned(),
            stream: Mutex::new(Some(ours)),
            reported: AtomicBool::new(false),
            retry_after: Mutex::new(None),
        });
        guest.write_all(b"hint").unwrap();
        let started = Instant::now();
        wait(
            &[Arc::clone(&peer)],
            &[],
            None,
            &AtomicBool::new(false),
            Duration::from_secs(5),
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        peer.notify();
        let mut byte = [0];
        guest.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        drop(guest);
        peer.drain();
        assert!(peer.stream().is_none());
    }
}
