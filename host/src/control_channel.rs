//! The host end of a guest's control channel: the virtio-serial port
//! `org.rampire.control.0`, which libvirt exposes on the host as a Unix
//! socket. The guest sends its publications over it and the host sends its
//! status back, one JSON record per line, with no guest agent involved.
//!
//! A VM without the port, or whose guest service does not use it, is still
//! served through the guest-agent control files (see `guest_channel`).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rampire_core::{RecordReader, RecordWriter, MEMORY_CONTROL_CHANNEL_NAME};

use crate::virsh::{Virsh, VirshCommand};

/// How long a missing or failed channel waits before the next lookup, which
/// costs a domain XML read.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct State {
    stream: Option<UnixStream>,
    reader: RecordReader,
    writer: RecordWriter,
    /// The newest complete record from the guest, until a newer one.
    latest: Option<Vec<u8>>,
    retry_after: Option<Instant>,
    reported: bool,
}

#[derive(Debug)]
pub struct ControlChannel {
    command: Option<Virsh>,
    vm: String,
    state: Mutex<State>,
}

impl ControlChannel {
    pub fn new(command: Virsh, vm: String) -> Arc<Self> {
        Arc::new(Self {
            command: Some(command),
            vm,
            state: Mutex::new(State::default()),
        })
    }

    /// A channel over an already connected stream, never looked up again.
    pub fn over(stream: UnixStream, vm: String) -> io::Result<Arc<Self>> {
        stream.set_nonblocking(true)?;
        Ok(Arc::new(Self {
            command: None,
            vm,
            state: Mutex::new(State {
                stream: Some(stream),
                ..State::default()
            }),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect if the domain has the port and no connection is open.
    pub fn connect(&self) {
        let Some(command) = &self.command else {
            return;
        };
        {
            let mut state = self.state();
            if state.stream.is_some()
                || state
                    .retry_after
                    .is_some_and(|instant| Instant::now() < instant)
            {
                return;
            }
            state.retry_after = Instant::now().checked_add(RECONNECT_INTERVAL);
        }
        let attempt = || -> Result<UnixStream, String> {
            let xml = command
                .run(&["dumpxml".to_owned(), self.vm.clone()])
                .map_err(|error| error.to_string())?;
            let path = crate::wake::channel_path(&xml, MEMORY_CONTROL_CHANNEL_NAME)?
                .ok_or("the control channel is absent; using the guest-agent control files")?;
            let stream = UnixStream::connect(path).map_err(|error| error.to_string())?;
            stream
                .set_nonblocking(true)
                .map_err(|error| error.to_string())?;
            Ok(stream)
        };
        let result = attempt();
        let mut state = self.state();
        match result {
            Ok(stream) => {
                eprintln!("rampire: vm={} event=control_channel_connected", self.vm);
                state.stream = Some(stream);
                state.reader.disconnected();
                state.writer.reconnected();
                state.reported = false;
                state.retry_after = None;
            }
            Err(error) => {
                if !std::mem::replace(&mut state.reported, true) {
                    eprintln!(
                        "rampire: vm={} event=control_channel_unavailable reason={error}",
                        self.vm
                    );
                }
            }
        }
    }

    pub fn vm(&self) -> &str {
        &self.vm
    }

    /// The domain has just started, so its channel socket now exists: look
    /// it up again at once rather than after the reconnect interval.
    pub fn domain_started(&self) {
        let reconnect = {
            let mut state = self.state();
            state.retry_after = None;
            state.stream.is_none()
        };
        if reconnect {
            self.connect();
        }
    }

    pub fn connected(&self) -> bool {
        self.state().stream.is_some()
    }

    /// Move whatever bytes can move now, in both directions.
    pub fn pump(&self) {
        let mut state = self.state();
        let State {
            stream,
            reader,
            writer,
            latest,
            ..
        } = &mut *state;
        let Some(open) = stream.as_mut() else {
            return;
        };
        let mut lost = false;
        let mut bytes = [0; 4096];
        loop {
            match open.read(&mut bytes) {
                Ok(0) => {
                    lost = true;
                    break;
                }
                Ok(count) => reader.push(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    lost = true;
                    break;
                }
            }
        }
        if let Some(record) = reader.take_latest() {
            *latest = Some(record);
        }
        while !lost {
            let pending = writer.pending();
            if pending.is_empty() {
                break;
            }
            match open.write(pending) {
                Ok(0) => break,
                Ok(count) => writer.consumed(count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => lost = true,
            }
        }
        if lost {
            *stream = None;
            reader.disconnected();
            writer.reconnected();
            // Reconnect promptly: the socket stays while the domain runs.
            state.retry_after = None;
            eprintln!("rampire: vm={} event=control_channel_lost", self.vm);
        }
    }

    /// The newest record received from the guest, if any yet.
    pub fn latest(&self) -> Option<Vec<u8>> {
        self.pump();
        self.state().latest.clone()
    }

    /// Send `record` to the guest, replacing any not yet sent.
    pub fn send(&self, record: &[u8]) {
        self.state().writer.queue(record);
        self.pump();
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        self.state().stream.as_ref().map(AsRawFd::as_raw_fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_flow_both_ways_and_a_lost_guest_is_noticed() {
        let (ours, guest) = UnixStream::pair().unwrap();
        let channel = ControlChannel::over(ours, "guest".to_owned()).unwrap();
        let mut guest = guest;
        assert_eq!(channel.latest(), None);
        guest.write_all(b"{\"n\":1}\n{\"n\":2}\n{\"n\"").unwrap();
        assert_eq!(channel.latest().as_deref(), Some(&b"{\"n\":2}"[..]));
        // Kept until a newer one, so a slow controller round still sees it.
        assert_eq!(channel.latest().as_deref(), Some(&b"{\"n\":2}"[..]));
        channel.send(b"{\"status\":1}\n");
        let mut received = [0; 13];
        guest.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"{\"status\":1}\n");
        assert!(channel.raw_fd().is_some());
        drop(guest);
        channel.pump();
        assert!(!channel.connected());
        assert_eq!(channel.raw_fd(), None);
    }
}
