//! Framing for the dedicated control channel: the same JSON records the
//! control files hold, one per line, over a byte stream in each direction.
//!
//! Every record is a complete snapshot (the guest's publication, the host's
//! status), so only the newest matters. A reader keeps the latest complete
//! line and a writer never interleaves two records; a record lost to a
//! disconnect is replaced by the next one.

/// The virtio-serial port that carries guest publications to the host and
/// host status to the guest.
pub const MEMORY_CONTROL_CHANNEL_NAME: &str = "org.rampire.control.0";

/// Larger than any publication or status either side accepts.
pub const MAX_STREAM_RECORD_BYTES: usize = 64 * 1024;

/// Collects bytes from the stream and yields the newest complete record.
#[derive(Debug, Default)]
pub struct RecordReader {
    partial: Vec<u8>,
    /// The line being received is already too long; skip to its end.
    oversized: bool,
    latest: Option<Vec<u8>>,
}

impl RecordReader {
    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                if !self.oversized && !self.partial.is_empty() {
                    self.latest = Some(std::mem::take(&mut self.partial));
                }
                self.partial.clear();
                self.oversized = false;
            } else if !self.oversized {
                if self.partial.len() == MAX_STREAM_RECORD_BYTES {
                    self.oversized = true;
                    self.partial.clear();
                } else {
                    self.partial.push(byte);
                }
            }
        }
    }

    /// The newest complete record since the last call, without its newline.
    pub fn take_latest(&mut self) -> Option<Vec<u8>> {
        self.latest.take()
    }

    /// The peer went away: what was half received will never be completed.
    pub fn disconnected(&mut self) {
        self.partial.clear();
        self.oversized = false;
    }
}

/// Sends whole records. One in flight is finished before the next starts,
/// and of the records waiting only the newest is kept.
#[derive(Debug, Default)]
pub struct RecordWriter {
    in_flight: Vec<u8>,
    sent: usize,
    next: Option<Vec<u8>>,
}

impl RecordWriter {
    /// Queue `record`, with or without its trailing newline.
    pub fn queue(&mut self, record: &[u8]) {
        let mut line = record.strip_suffix(b"\n").unwrap_or(record).to_vec();
        line.push(b'\n');
        self.next = Some(line);
    }

    /// The bytes to write now; empty when nothing is waiting.
    pub fn pending(&mut self) -> &[u8] {
        if self.sent == self.in_flight.len() {
            if let Some(next) = self.next.take() {
                self.in_flight = next;
                self.sent = 0;
            }
        }
        &self.in_flight[self.sent..]
    }

    /// The stream accepted the first `count` bytes of `pending()`.
    pub fn consumed(&mut self, count: usize) {
        self.sent = (self.sent + count).min(self.in_flight.len());
    }

    /// The peer went away or has just arrived. A record cut short is sent
    /// again from its start, after a newline that ends whatever fragment the
    /// peer may still hold.
    pub fn reconnected(&mut self) {
        if self.sent < self.in_flight.len() && self.next.is_none() {
            self.next = Some(std::mem::take(&mut self.in_flight));
        }
        self.in_flight = vec![b'\n'];
        self.sent = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_yields_the_newest_complete_record_however_bytes_arrive() {
        let mut reader = RecordReader::default();
        assert_eq!(reader.take_latest(), None);
        reader.push(b"{\"a\":1}\n{\"a\":2}\n{\"a\":");
        assert_eq!(reader.take_latest().as_deref(), Some(&b"{\"a\":2}"[..]));
        assert_eq!(reader.take_latest(), None, "a record is yielded once");
        reader.push(b"3}");
        assert_eq!(reader.take_latest(), None, "incomplete until its newline");
        reader.push(b"\n\n\n");
        assert_eq!(reader.take_latest().as_deref(), Some(&b"{\"a\":3}"[..]));
    }

    #[test]
    fn oversized_and_abandoned_fragments_are_dropped_not_joined() {
        let mut reader = RecordReader::default();
        reader.push(&vec![b'x'; MAX_STREAM_RECORD_BYTES + 10]);
        reader.push(b"tail\nok\n");
        assert_eq!(reader.take_latest().as_deref(), Some(&b"ok"[..]));
        reader.push(&vec![b'y'; MAX_STREAM_RECORD_BYTES]);
        reader.push(b"\n");
        assert_eq!(
            reader.take_latest().map(|record| record.len()),
            Some(MAX_STREAM_RECORD_BYTES),
            "a record of exactly the limit is accepted"
        );
        reader.push(b"half a rec");
        reader.disconnected();
        reader.push(b"ord\nwhole\n");
        assert_eq!(reader.take_latest().as_deref(), Some(&b"whole"[..]));
    }

    /// Drains the writer `chunk` bytes at a time, as a slow stream would.
    fn drain(writer: &mut RecordWriter, chunk: usize, limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < limit {
            let pending = writer.pending();
            if pending.is_empty() {
                break;
            }
            let count = pending.len().min(chunk);
            out.extend_from_slice(&pending[..count]);
            writer.consumed(count);
        }
        out
    }

    #[test]
    fn the_writer_finishes_a_record_before_the_next_and_keeps_only_the_newest() {
        let mut writer = RecordWriter::default();
        assert!(writer.pending().is_empty());
        writer.queue(b"first\n");
        assert_eq!(drain(&mut writer, 3, 3), b"fir");
        writer.queue(b"second");
        writer.queue(b"third");
        assert_eq!(drain(&mut writer, 2, usize::MAX), b"st\nthird\n");
        assert!(writer.pending().is_empty());
    }

    #[test]
    fn a_record_cut_short_by_a_disconnect_is_sent_again_whole() {
        let mut writer = RecordWriter::default();
        writer.queue(b"status");
        assert_eq!(drain(&mut writer, 4, 4), b"stat");
        writer.reconnected();
        assert_eq!(drain(&mut writer, 64, usize::MAX), b"\nstatus\n");
        // A newer record waiting replaces the one that was cut short.
        writer.queue(b"old");
        assert_eq!(drain(&mut writer, 1, 1), b"o");
        writer.queue(b"new");
        writer.reconnected();
        assert_eq!(drain(&mut writer, 64, usize::MAX), b"\nnew\n");
        // Round trip through a reader that held a fragment.
        let mut reader = RecordReader::default();
        reader.push(b"frag");
        writer.queue(b"{}");
        writer.reconnected();
        reader.push(&drain(&mut writer, 64, usize::MAX));
        assert_eq!(reader.take_latest().as_deref(), Some(&b"{}"[..]));
    }
}
