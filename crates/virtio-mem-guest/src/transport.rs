//! How publications reach the host and host status reaches the guest.
//!
//! - The control channel: a dedicated virtio-serial port carrying one JSON
//!   record per line each way. It needs no guest agent, gives the host no
//!   file access to the guest, and wakes each side when the other writes.
//! - Control files: the publication and the host status as files the host
//!   reads and writes through the QEMU guest agent. Used when the VM has no
//!   control channel.
//!
//! Both carry the same records. The durable request state is a local file
//! either way.

use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;

use virtio_mem_core::{
    host_status_path, HostMemoryStatus, RecordReader, RecordWriter, MAX_HOST_STATUS_BYTES,
    MEMORY_CONTROL_CHANNEL_NAME,
};

use crate::files::{read_bounded, replace_file, transient_sharing_error};

pub trait Transport: std::fmt::Debug {
    /// Send one publication, a JSON record ending in a newline. A record the
    /// host is not there to receive is dropped: the next one replaces it.
    fn publish(&mut self, record: &[u8]) -> Result<(), String>;

    /// The latest complete host status, or `None` while there is none.
    fn host_status(&mut self) -> Result<Option<HostMemoryStatus>, String>;

    /// True when a host status has arrived that `host_status` has not yet
    /// returned, so the caller need not wait out its interval.
    fn status_arrived(&mut self) -> bool {
        false
    }

    fn describe(&self) -> String;
}

/// The control files beside `control_path`.
#[derive(Debug)]
pub struct FileTransport {
    control_path: PathBuf,
    status_path: PathBuf,
}

impl FileTransport {
    pub fn new(control_path: &str) -> Self {
        Self {
            control_path: PathBuf::from(control_path),
            status_path: PathBuf::from(host_status_path(control_path)),
        }
    }
}

impl Transport for FileTransport {
    fn publish(&mut self, record: &[u8]) -> Result<(), String> {
        // Rewritten every poll, so it needs no durability; the request it
        // carries is already durable in the request state.
        replace_file(&self.control_path, record, false)
    }

    /// `None` covers a status not yet written and one seen mid-write: the
    /// guest agent cannot replace the file atomically, so truncated or
    /// undecodable contents are expected and retried on the next poll.
    fn host_status(&mut self) -> Result<Option<HostMemoryStatus>, String> {
        let bytes = match read_bounded(&self.status_path, MAX_HOST_STATUS_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            // Held open by the guest agent writing it.
            Err(error) if transient_sharing_error(&error) => return Ok(None),
            Err(error) => return Err(format!("read host status: {error}")),
        };
        if !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        Ok(serde_json::from_slice(&bytes).ok())
    }

    fn describe(&self) -> String {
        format!("control files at {}", self.control_path.display())
    }
}

/// The control channel. `S` never blocks: reads and writes that cannot
/// proceed report `WouldBlock`.
#[derive(Debug)]
pub struct StreamTransport<S> {
    stream: S,
    reader: RecordReader,
    writer: RecordWriter,
    latest: Option<HostMemoryStatus>,
    arrived: bool,
}

impl<S: Read + Write> StreamTransport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            reader: RecordReader::default(),
            writer: RecordWriter::default(),
            latest: None,
            arrived: false,
        }
    }

    fn receive(&mut self) {
        let mut bytes = [0; 4096];
        loop {
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    // The host is not connected to its end.
                    self.reader.disconnected();
                    self.writer.reconnected();
                    break;
                }
                Ok(count) => self.reader.push(&bytes[..count]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if let Some(status) = self
            .reader
            .take_latest()
            .and_then(|record| serde_json::from_slice(&record).ok())
        {
            self.latest = Some(status);
            self.arrived = true;
        }
    }

    fn send(&mut self) {
        loop {
            let pending = self.writer.pending();
            if pending.is_empty() {
                break;
            }
            match self.stream.write(pending) {
                Ok(0) => break,
                Ok(count) => self.writer.consumed(count),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                // Not connected or full: the record waits, or is replaced.
                Err(_) => break,
            }
        }
    }
}

impl<S: Read + Write + std::fmt::Debug> Transport for StreamTransport<S> {
    fn publish(&mut self, record: &[u8]) -> Result<(), String> {
        self.writer.queue(record);
        self.send();
        Ok(())
    }

    /// The last status received. Its age is the policy's to judge.
    fn host_status(&mut self) -> Result<Option<HostMemoryStatus>, String> {
        self.receive();
        self.arrived = false;
        Ok(self.latest.clone())
    }

    fn status_arrived(&mut self) -> bool {
        self.receive();
        self.arrived
    }

    fn describe(&self) -> String {
        format!("control channel {MEMORY_CONTROL_CHANNEL_NAME}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use virtio_mem_core::MEMORY_REQUEST_VERSION;

    /// A stream whose host end the test plays: bytes queued in `incoming`
    /// are read once, `written` collects what the guest sends, and a
    /// disconnected host reads as end of file and refuses writes.
    #[derive(Debug, Default)]
    struct Port {
        incoming: VecDeque<u8>,
        written: Vec<u8>,
        connected: bool,
        accept: usize,
    }

    impl Read for &mut Port {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.connected {
                return Ok(0);
            }
            if self.incoming.is_empty() {
                return Err(ErrorKind::WouldBlock.into());
            }
            let count = buffer.len().min(self.incoming.len());
            for slot in &mut buffer[..count] {
                *slot = self.incoming.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for &mut Port {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.connected || self.accept == 0 {
                return Err(ErrorKind::WouldBlock.into());
            }
            let count = bytes.len().min(self.accept);
            self.accept -= count;
            self.written.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn status(current: u64) -> HostMemoryStatus {
        HostMemoryStatus {
            version: MEMORY_REQUEST_VERSION,
            vm_name: "debian".to_owned(),
            service_name: "virtio-mem-guest".to_owned(),
            device_alias: "ua-virtiomem0".to_owned(),
            observed_unix_millis: 1_000_000,
            device_size_bytes: 4 << 30,
            block_size_bytes: 2 << 20,
            requested_bytes: current,
            current_bytes: current,
            result: None,
            guest_settings: None,
        }
    }

    fn line(status: &HostMemoryStatus) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(status).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn the_channel_delivers_the_newest_status_and_says_when_one_arrives() {
        let mut port = Port {
            connected: true,
            accept: usize::MAX,
            ..Port::default()
        };
        let mut transport = StreamTransport::new(&mut port);
        assert_eq!(transport.host_status(), Ok(None));
        assert!(!transport.status_arrived());
        transport.stream.incoming.extend(line(&status(0)));
        transport.stream.incoming.extend(line(&status(2 << 20)));
        // Half of a third record must not hide the complete ones.
        transport
            .stream
            .incoming
            .extend(&line(&status(4 << 20))[..20]);
        assert!(transport.status_arrived());
        assert_eq!(transport.host_status(), Ok(Some(status(2 << 20))));
        assert!(!transport.status_arrived(), "reported once");
        // Nothing new: the last status stands, for the policy to age.
        assert_eq!(transport.host_status(), Ok(Some(status(2 << 20))));
        // Garbage is not a status and replaces nothing.
        transport.stream.incoming.extend(b"\nnot json\n");
        assert!(!transport.status_arrived());
        assert_eq!(transport.host_status(), Ok(Some(status(2 << 20))));
    }

    #[test]
    fn publications_survive_a_slow_and_an_absent_host_as_whole_records() {
        let mut port = Port::default();
        let mut transport = StreamTransport::new(&mut port);
        // No host: nothing is written and nothing fails.
        transport.publish(b"{\"sequence\":0}\n").unwrap();
        assert_eq!(transport.host_status(), Ok(None));
        assert!(transport.stream.written.is_empty());
        // The host connects but takes only a few bytes at a time.
        transport.stream.connected = true;
        transport.stream.accept = 5;
        transport.publish(b"{\"sequence\":1}\n").unwrap();
        transport.stream.accept = 3;
        transport.publish(b"{\"sequence\":2}\n").unwrap();
        transport.stream.accept = usize::MAX;
        transport.publish(b"{\"sequence\":3}\n").unwrap();
        let mut reader = RecordReader::default();
        reader.push(&transport.stream.written);
        assert_eq!(
            reader.take_latest().as_deref(),
            Some(&b"{\"sequence\":3}"[..])
        );
        let text = String::from_utf8(transport.stream.written.clone()).unwrap();
        for record in text.lines().filter(|record| !record.is_empty()) {
            assert!(
                record.starts_with("{\"sequence\":") && record.ends_with('}'),
                "no record is interleaved or cut: {text:?}"
            );
        }
    }

    #[test]
    fn control_files_carry_the_same_records() {
        let directory =
            std::env::temp_dir().join(format!("virtio-mem-guest-transport-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let control = directory.join("control.json").display().to_string();
        let mut transport = FileTransport::new(&control);
        assert_eq!(transport.host_status(), Ok(None));
        transport.publish(b"{}\n").unwrap();
        assert_eq!(std::fs::read(&control).unwrap(), b"{}\n");
        let status_path = host_status_path(&control);
        // Mid-write views are not a status.
        std::fs::write(&status_path, &line(&status(0))[..30]).unwrap();
        assert_eq!(transport.host_status(), Ok(None));
        std::fs::write(&status_path, line(&status(0))).unwrap();
        assert_eq!(transport.host_status(), Ok(Some(status(0))));
        assert!(!transport.status_arrived());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
