use serde::Deserialize;
use serde_json::{json, Value};

use crate::virsh::VirshCommand;

const MAX_GUEST_FILE_READS: usize = 16;

pub trait GuestFileReader {
    fn read_file(&self, path: &str, maximum_bytes: usize) -> Result<Vec<u8>, String>;
}

/// Replaces a guest file's contents. QGA cannot rename, so readers must
/// reject the truncated or partial contents they may observe mid-write.
pub trait GuestFileWriter {
    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String>;
}

pub struct VirshGuestFiles<C> {
    command: C,
    vm_name: String,
}

impl<C> VirshGuestFiles<C> {
    pub fn new(command: C, vm_name: impl Into<String>) -> Self {
        Self {
            command,
            vm_name: vm_name.into(),
        }
    }
}

#[derive(Deserialize)]
struct OpenResponse {
    #[serde(rename = "return")]
    handle: u64,
}

#[derive(Deserialize)]
struct ReadResponse {
    #[serde(rename = "return")]
    result: ReadResult,
}

#[derive(Deserialize)]
struct WriteResponse {
    #[serde(rename = "return")]
    result: WriteResult,
}

#[derive(Deserialize)]
struct WriteResult {
    count: usize,
}

#[derive(Deserialize)]
struct ReadResult {
    count: usize,
    #[serde(rename = "buf-b64", default)]
    buffer_base64: String,
    eof: bool,
}

impl<C: VirshCommand> VirshGuestFiles<C> {
    fn execute(&self, request: Value) -> Result<String, String> {
        self.command
            .run(&[
                "qemu-agent-command".to_owned(),
                self.vm_name.clone(),
                request.to_string(),
            ])
            .map_err(|error| error.to_string())
    }

    fn close(&self, handle: u64) -> Result<(), String> {
        let response = self.execute(json!({
            "execute": "guest-file-close",
            "arguments": {"handle": handle}
        }))?;
        let response: Value = serde_json::from_str(&response)
            .map_err(|error| format!("parse guest-file-close response: {error}"))?;
        if response.get("return").and_then(Value::as_object).is_none() {
            return Err("guest-file-close response has no return object".to_owned());
        }
        Ok(())
    }

    fn open(&self, path: &str, mode: &str) -> Result<u64, String> {
        if path.trim().is_empty() {
            return Err("QGA guest file path must not be empty".to_owned());
        }
        let response = self.execute(json!({
            "execute": "guest-file-open",
            "arguments": {"path": path, "mode": mode}
        }))?;
        serde_json::from_str::<OpenResponse>(&response)
            .map(|response| response.handle)
            .map_err(|error| format!("parse guest-file-open response: {error}"))
    }

    /// Runs `operation` on an open handle and always closes it, keeping the
    /// operation's error as the primary failure.
    fn with_handle<T>(
        &self,
        handle: u64,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let result = operation();
        let close = self.close(handle);
        match (result, close) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(close_error)) => Err(close_error),
            (Err(error), Err(close_error)) => Err(format!(
                "{error}; additionally failed to close QGA handle: {close_error}"
            )),
        }
    }
}

impl<C: VirshCommand> GuestFileWriter for VirshGuestFiles<C> {
    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        let handle = self.open(path, "w")?;
        self.with_handle(handle, || {
            let response = self.execute(json!({
                "execute": "guest-file-write",
                "arguments": {"handle": handle, "buf-b64": encode_base64(contents)}
            }))?;
            let response: WriteResponse = serde_json::from_str(&response)
                .map_err(|error| format!("parse guest-file-write response: {error}"))?;
            if response.result.count != contents.len() {
                return Err(format!(
                    "guest-file-write wrote {} of {} bytes",
                    response.result.count,
                    contents.len()
                ));
            }
            Ok(())
        })
    }
}

impl<C: VirshCommand> GuestFileReader for VirshGuestFiles<C> {
    fn read_file(&self, path: &str, maximum_bytes: usize) -> Result<Vec<u8>, String> {
        let handle = match self.open(path, "r") {
            Err(error)
                if error.contains("being used by another process")
                    || error.contains("sharing violation") =>
            {
                // The publisher atomically replaces this file. Retry only a
                // known transient collision, never an ambiguous read/close.
                std::thread::sleep(std::time::Duration::from_millis(25));
                self.open(path, "r")?
            }
            result => result?,
        };
        self.with_handle(handle, || {
            let mut contents = Vec::new();
            for _ in 0..MAX_GUEST_FILE_READS {
                let requested = maximum_bytes
                    .checked_sub(contents.len())
                    .and_then(|remaining| remaining.checked_add(1))
                    .ok_or_else(|| "QGA read bound overflowed".to_owned())?;
                let read = self.execute(json!({
                    "execute": "guest-file-read",
                    "arguments": {"handle": handle, "count": requested}
                }))?;
                let read: ReadResponse = serde_json::from_str(&read)
                    .map_err(|error| format!("parse guest-file-read response: {error}"))?;
                let bytes = decode_base64(&read.result.buffer_base64)?;
                if read.result.count != bytes.len() {
                    return Err(format!(
                        "guest-file-read count {} does not match decoded length {}",
                        read.result.count,
                        bytes.len()
                    ));
                }
                if bytes.is_empty() && !read.result.eof {
                    return Err("QGA guest-file-read made no progress before EOF".to_owned());
                }
                contents.extend_from_slice(&bytes);
                if contents.len() > maximum_bytes {
                    return Err(format!("QGA guest file exceeds {maximum_bytes} byte limit"));
                }
                if read.result.eof {
                    return Ok(contents);
                }
            }
            Err(format!(
                "QGA guest file did not reach EOF within {MAX_GUEST_FILE_READS} reads"
            ))
        })
    }
}

/// Standard padded base64, as QGA and PowerShell payloads require.
pub fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let value = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (value >> (18 - 6 * index)) & 0x3f;
                output.push(char::from(ALPHABET[sextet as usize]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    if !input.len().is_multiple_of(4) {
        return Err("QGA base64 payload length is invalid".to_owned());
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    for (index, chunk) in input.as_bytes().as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == input.len() / 4;
        let padding = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            (_, _) => 0,
        };
        if padding > 0 && !last {
            return Err("QGA base64 padding appears before the final block".to_owned());
        }
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3])?
        };
        if chunk[0] == b'=' || chunk[1] == b'=' || (chunk[2] == b'=' && chunk[3] != b'=') {
            return Err("QGA base64 padding is invalid".to_owned());
        }
        output.push((a << 2) | (b >> 4));
        if padding < 2 {
            output.push((b << 4) | (c >> 2));
        }
        if padding == 0 {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("QGA base64 payload contains an invalid character".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virsh::VirshError;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    struct FileFake {
        responses: Mutex<VecDeque<String>>,
        requests: Mutex<Vec<Value>>,
    }

    impl FileFake {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: Mutex::new(responses.iter().map(|value| (*value).to_owned()).collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl VirshCommand for FileFake {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            assert_eq!(&arguments[..2], &["qemu-agent-command", "guest"]);
            self.requests
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&arguments[2]).expect("request JSON"));
            Ok(self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("response"))
        }
    }

    #[test]
    fn retries_a_sharing_collision_before_opening_a_read_handle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct SharingFake(AtomicUsize);
        impl VirshCommand for SharingFake {
            fn run(&self, _: &[String]) -> Result<String, VirshError> {
                match self.0.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(VirshError::Failed {
                        status: "1".to_owned(),
                        stderr: "being used by another process".to_owned(),
                    }),
                    1 => Ok(r#"{"return":7}"#.to_owned()),
                    2 => Ok(r#"{"return":{"count":3,"buf-b64":"YWJj","eof":true}}"#.to_owned()),
                    3 => Ok(r#"{"return":{}}"#.to_owned()),
                    _ => panic!("retry must be bounded"),
                }
            }
        }
        let files = VirshGuestFiles::new(SharingFake(AtomicUsize::new(0)), "guest");
        assert_eq!(files.read_file("publication", 3).unwrap(), b"abc");
        assert_eq!(files.command.0.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn reads_and_closes_one_bounded_guest_file() {
        let fake = FileFake::new(&[
            r#"{"return":7}"#,
            r#"{"return":{"count":3,"buf-b64":"YWJj","eof":true}}"#,
            r#"{"return":{}}"#,
        ]);
        assert_eq!(
            VirshGuestFiles::new(fake, "guest").read_file(r"C:\telemetry.json", 3),
            Ok(b"abc".to_vec())
        );
    }

    #[test]
    fn joins_bounded_partial_qga_reads_before_closing() {
        let fake = FileFake::new(&[
            r#"{"return":7}"#,
            r#"{"return":{"count":2,"buf-b64":"YWI=","eof":false}}"#,
            r#"{"return":{"count":1,"buf-b64":"Yw==","eof":true}}"#,
            r#"{"return":{}}"#,
        ]);
        assert_eq!(
            VirshGuestFiles::new(fake, "guest").read_file(r"C:\telemetry.json", 3),
            Ok(b"abc".to_vec())
        );
    }

    #[test]
    fn rejects_oversized_or_malformed_base64_and_still_closes() {
        for read_response in [
            r#"{"return":{"count":4,"buf-b64":"YWJjZA==","eof":true}}"#,
            r#"{"return":{"count":1,"buf-b64":"!===","eof":true}}"#,
        ] {
            let fake = FileFake::new(&[r#"{"return":9}"#, read_response, r#"{"return":{}}"#]);
            let reader = VirshGuestFiles::new(fake, "guest");
            assert!(reader.read_file(r"C:\telemetry.json", 3).is_err());
            assert_eq!(reader.command.requests.lock().expect("requests").len(), 3);
        }
    }

    #[test]
    fn writes_the_complete_contents_and_closes() {
        let fake = FileFake::new(&[
            r#"{"return":7}"#,
            r#"{"return":{"count":3,"eof":false}}"#,
            r#"{"return":{}}"#,
        ]);
        let files = VirshGuestFiles::new(fake, "guest");
        files.write_file(r"C:\status.json", b"abc").unwrap();
        let requests = files.command.requests.lock().expect("requests");
        assert_eq!(requests[0]["arguments"]["mode"], "w");
        assert_eq!(requests[1]["arguments"]["buf-b64"], "YWJj");
        assert_eq!(requests[2]["execute"], "guest-file-close");

        let short = FileFake::new(&[
            r#"{"return":7}"#,
            r#"{"return":{"count":2,"eof":false}}"#,
            r#"{"return":{}}"#,
        ]);
        assert!(VirshGuestFiles::new(short, "guest")
            .write_file(r"C:\status.json", b"abc")
            .is_err());
    }

    #[test]
    fn base64_round_trips_every_padding_length() {
        for input in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            &[0xff, 0x00, 0x7f, 0x80],
        ] {
            assert_eq!(decode_base64(&encode_base64(input)), Ok(input.to_vec()));
        }
    }

    #[test]
    fn decodes_standard_base64_strictly() {
        assert_eq!(decode_base64(""), Ok(Vec::new()));
        assert_eq!(decode_base64("YQ=="), Ok(b"a".to_vec()));
        assert_eq!(decode_base64("YWI="), Ok(b"ab".to_vec()));
        assert_eq!(decode_base64("YWJj"), Ok(b"abc".to_vec()));
        assert!(decode_base64("YQ=").is_err());
    }
}
