//! What wakes the controller before its poll interval: a record on a
//! guest's control channel, or a libvirt domain event. Polling remains the
//! fallback; neither carries any authority of its own.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quick_xml::{events::Event, Reader};

use crate::virsh::Virsh;

/// Libvirt domain events: a device size change speeds up observing a
/// resize, and a domain start lets its control channel connect at once
/// instead of after the reconnect interval. The runtime owns and reaps the
/// subscription process.
pub struct MemoryEvents {
    child: Child,
    output: ChildStdout,
    closed: bool,
    /// Output after the last complete line.
    partial: Vec<u8>,
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
            partial: Vec::new(),
        })
    }

    /// Reads what the subscription has written and returns the domains it
    /// reported started.
    fn drain(&mut self) -> Vec<String> {
        let mut bytes = [0; 4096];
        loop {
            match self.output.read(&mut bytes) {
                Ok(0) => {
                    self.closed = true;
                    eprintln!("libvirt event subscription ended; polling continues");
                    break;
                }
                Ok(count) => self.partial.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.closed = true;
                    eprintln!("libvirt event read failed; polling continues: {error}");
                    break;
                }
            }
        }
        started_domains(&mut self.partial)
    }
}

/// Takes the complete lines from `output` and returns the domains whose
/// lifecycle event is `Started`. Libvirt prints
/// `event 'lifecycle' for domain 'NAME': Started Booted`. An overlong
/// unterminated line is dropped, so the buffer stays bounded.
fn started_domains(output: &mut Vec<u8>) -> Vec<String> {
    const PREFIX: &str = "event 'lifecycle' for domain '";
    let mut started = Vec::new();
    let complete = output
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    for line in String::from_utf8_lossy(&output[..complete]).lines() {
        if let Some((name, state)) = line
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.rsplit_once("': "))
        {
            if state.starts_with("Started") {
                started.push(name.to_owned());
            }
        }
    }
    output.drain(..complete);
    if output.len() > 64 * 1024 {
        output.clear();
    }
    started
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

/// Wait for a control record from any VM, a libvirt event, or the periodic
/// fallback, with bounded stop latency. Each only accelerates a complete,
/// guarded controller round.
pub fn wait(
    controls: &[Arc<crate::control_channel::ControlChannel>],
    mut events: Option<&mut MemoryEvents>,
    stop: &AtomicBool,
    timeout: Duration,
) {
    for control in controls {
        control.connect();
    }
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) && started.elapsed() < timeout {
        let mut descriptors: Vec<_> = controls
            .iter()
            .filter_map(|control| {
                control.raw_fd().map(|fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
            })
            .collect();
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
        // all file descriptors remain owned by their channels during this call.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                bound.as_millis().max(1) as i32,
            )
        };
        if result > 0 {
            if let Some(events) = events.as_mut().filter(|e| !e.closed) {
                for domain in events.drain() {
                    for control in controls.iter().filter(|c| c.vm() == domain) {
                        control.domain_started();
                    }
                }
            }
            for control in controls {
                control.pump();
            }
            return;
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            eprintln!("rampire: wake wait failed: {}", io::Error::last_os_error());
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
        let xml = "<domain><devices><channel type='unix'><source path='/qga'/><target type='virtio' name='org.qemu.guest_agent.0'/></channel><channel type='unix'><source path='/control'/><target type='virtio' name='org.virtio-mem.control.0'/></channel></devices></domain>";
        assert_eq!(
            channel_path(xml, "org.virtio-mem.control.0"),
            Ok(Some("/control".to_owned()))
        );
        assert_eq!(channel_path(xml, "org.virtio-mem.other.0"), Ok(None));
        assert_eq!(
            channel_path("<domain/>", "org.virtio-mem.control.0"),
            Ok(None)
        );
    }

    #[test]
    fn reports_started_domains_from_complete_event_lines() {
        let mut output = b"event 'memory-device-size-change' for domain 'a':\nalias: ua-virtiomem0\nsize: 524288\nevent 'lifecycle' for domain 'safe_vm_2': Started Booted\nevent 'lifecycle' for domain 'b': Stopped Shutdown\nevent 'lifecycle' for domain 'd': Started Restored\nevent 'lifecycle' for domain 'c': Sta".to_vec();
        assert_eq!(started_domains(&mut output), ["safe_vm_2", "d"]);
        // The unfinished line waits for the rest.
        output.extend_from_slice(b"rted Booted\n");
        assert_eq!(started_domains(&mut output), ["c"]);
        assert!(output.is_empty());
        let mut endless = vec![b'x'; 70 * 1024];
        assert!(started_domains(&mut endless).is_empty());
        assert!(endless.is_empty());
    }
}
