use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VirshError {
    #[error("failed to start virsh: {0}")]
    Start(#[from] io::Error),
    #[error("virsh timed out after {0:?}")]
    Timeout(Duration),
    #[error("virsh failed with status {status}: {stderr}")]
    Failed { status: String, stderr: String },
    #[error("virsh returned invalid UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    // Polkit authorizes root for every action and libvirt accepts root on the
    // local socket, so sudo is a valid manual test path; it is not a deployment.
    #[error("non-interactive libvirt management access is unavailable: {0}; configure authorization for the host service account before starting the controller, or run it with sudo for manual testing")]
    Authorization(String),
}

pub trait VirshCommand {
    fn run(&self, arguments: &[String]) -> Result<String, VirshError>;
}

#[derive(Debug, Clone)]
pub struct Virsh {
    binary: String,
    timeout: Duration,
    connection: String,
    /// Shared by clones: serves per-poll commands over one libvirt connection.
    #[cfg(unix)]
    session: Option<std::sync::Arc<crate::libvirt::LibvirtSession>>,
}

impl Virsh {
    pub fn new(binary: impl Into<String>, timeout: Duration) -> Self {
        Self {
            binary: binary.into(),
            timeout,
            // An absent or non-Unicode optional environment value selects the
            // documented local host connection, rather than driver autodetection.
            connection: std::env::var("LIBVIRT_DEFAULT_URI")
                .unwrap_or_else(|_| "qemu:///system".to_owned()),
            #[cfg(unix)]
            session: None,
        }
    }

    /// Like `new`, but `dumpxml` and `qemu-agent-command` share one
    /// long-lived libvirt connection instead of a `virsh` process (and
    /// polkit check) each. The daemon uses this; operator commands do not.
    #[cfg(unix)]
    pub fn persistent(binary: impl Into<String>, timeout: Duration) -> Self {
        let mut virsh = Self::new(binary, timeout);
        virsh.session = Some(std::sync::Arc::new(crate::libvirt::LibvirtSession::new(
            virsh.connection.clone(),
            timeout,
        )));
        virsh
    }

    pub fn with_connection(
        binary: impl Into<String>,
        timeout: Duration,
        connection: impl Into<String>,
    ) -> Self {
        Self {
            binary: binary.into(),
            timeout,
            connection: connection.into(),
            #[cfg(unix)]
            session: None,
        }
    }

    /// Share a connection for an observer's repeated XML and QGA reads too.
    /// Other commands retain their bounded process implementation.
    pub fn persistent_with_connection(
        binary: impl Into<String>,
        timeout: Duration,
        connection: impl Into<String>,
    ) -> Self {
        let virsh = Self::with_connection(binary, timeout, connection);
        #[cfg(unix)]
        {
            let mut virsh = virsh;
            virsh.session = Some(std::sync::Arc::new(crate::libvirt::LibvirtSession::new(
                virsh.connection.clone(),
                timeout,
            )));
            virsh
        }
        #[cfg(not(unix))]
        virsh
    }

    /// Check the local RHEL polkit policy once, at daemon startup, without
    /// permitting a password dialog, so a missing grant fails with a clear
    /// message. libvirt authorizes every connection itself; repeating this
    /// before each command only added polkit load. Remote transports retain
    /// their own authentication contract.
    pub fn require_management_access(&self) -> Result<(), VirshError> {
        if self.connection != "qemu:///system" {
            return Ok(());
        }
        let mut check = authorization_command().map_err(|error| {
            VirshError::Authorization(format!("identify controller process: {error}"))
        })?;
        let output = bounded_output(&mut check, self.timeout).map_err(|error| {
            VirshError::Authorization(format!("check polkit authorization: {error}"))
        })?;
        if !output.status.success() {
            return Err(VirshError::Authorization(format!(
                "pkcheck {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn event_command(&self) -> Command {
        self.command(&[
            // Every event, for memory-device-size-change and lifecycle.
            "event".to_owned(),
            "--all".to_owned(),
            "--loop".to_owned(),
        ])
    }

    fn command(&self, arguments: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(["-c", &self.connection]);
        if read_only(arguments) {
            command.arg("--readonly");
        }
        command.args(arguments);
        command
    }

    fn run_process(&self, arguments: &[String]) -> Result<String, VirshError> {
        let output =
            bounded_output(&mut self.command(arguments), self.timeout).map_err(|error| {
                if error.kind() == io::ErrorKind::TimedOut {
                    VirshError::Timeout(self.timeout)
                } else {
                    VirshError::Start(error)
                }
            })?;
        if !output.status.success() {
            return Err(VirshError::Failed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8(output.stdout)?)
    }
}

impl VirshCommand for Virsh {
    fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
        #[cfg(unix)]
        if let Some(result) = self
            .session
            .as_ref()
            .and_then(|session| session.run(arguments))
        {
            return result;
        }
        self.run_process(arguments)
    }
}

fn read_only(arguments: &[String]) -> bool {
    matches!(
        arguments.first().map(String::as_str),
        Some("dumpxml" | "domstate" | "domuuid" | "version")
    )
}

fn authorization_command() -> io::Result<Command> {
    let identity = process_identity()?;
    let mut command = Command::new("pkcheck");
    // Never add --allow-user-interaction or --enable-internal-agent: this check
    // must not ask the desktop user to authenticate each connection.
    command.args([
        "--action-id",
        "org.libvirt.unix.manage",
        "--process",
        &identity,
    ]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn process_identity() -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    let stat = std::fs::read_to_string("/proc/self/stat")?;
    // comm (field 2) may contain spaces and parentheses. starttime is field 22.
    let started = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .ok_or_else(|| io::Error::other("/proc/self/stat has no start time"))?
        .parse::<u64>()
        .map_err(io::Error::other)?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    Ok(format!("{},{started},{uid}", std::process::id()))
}

#[cfg(not(target_os = "linux"))]
fn process_identity() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local system libvirt authorization requires the Linux host",
    ))
}

pub fn bounded_output(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    capture_output(command, timeout, false)
}

/// Capture stdout while keeping terminal input and diagnostics available to
/// explicitly requested interactive commands such as operator elevation.
pub fn bounded_interactive_output(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    capture_output(command, timeout, true)
}

/// Capture at most 4 MiB per stream. The deadline includes descendants holding
/// inherited pipes, rather than just waiting for the direct child.
#[cfg(unix)]
fn capture_output(
    command: &mut Command,
    timeout: Duration,
    interactive: bool,
) -> io::Result<Output> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::time::Instant;

    // Elevated inventory can contain 32 escaped 256 KiB text files.
    let output_limit = if interactive {
        64 * 1024 * 1024
    } else {
        4 * 1024 * 1024
    };
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "command timeout is too large")
    })?;
    // An interactive child (sudo) must stay in the terminal's foreground
    // process group: in a new group its password prompt cannot read the tty
    // and PAM reports a failed conversation.
    if !interactive {
        command.process_group(0);
    }
    let mut child = command
        .stdin(if interactive {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(if interactive {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .spawn()?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        let mut stderr = child.stderr.take();
        for fd in std::iter::once(stdout.as_raw_fd()).chain(stderr.as_ref().map(AsRawFd::as_raw_fd))
        {
            // SAFETY: both descriptors are owned and open throughout this call.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_done = false;
        let mut err_done = stderr.is_none();
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
            }
            // One chunk per stream per turn: a continuously writing child
            // cannot starve the clock, other stream, or exit check.
            fn read_chunk(
                input: &mut impl Read,
                bytes: &mut Vec<u8>,
                limit: usize,
            ) -> io::Result<bool> {
                let mut chunk = [0; 16 * 1024];
                match input.read(&mut chunk) {
                    Ok(0) => Ok(true),
                    Ok(count) => {
                        if bytes.len() + count > limit {
                            return Err(io::Error::other(format!(
                                "command output exceeds {limit} byte stream limit"
                            )));
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                        Ok(false)
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        Ok(false)
                    }
                    Err(error) => Err(error),
                }
            }
            if !out_done {
                out_done = read_chunk(&mut stdout, &mut out, output_limit)?;
            }
            if !err_done {
                if let Some(stderr) = stderr.as_mut() {
                    err_done = read_chunk(stderr, &mut err, output_limit)?;
                }
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if out_done && err_done {
                if let Some(status) = status {
                    return Ok(Output {
                        status,
                        stdout: out,
                        stderr: err,
                    });
                }
            }
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    // Always clean up the owned process group, including descendants after the
    // direct child exits. Dropping the pipes cannot strand reader threads. An
    // interactive child shares our group, so only it is killed.
    let kill_error = if interactive {
        child.kill().err()
    } else {
        // SAFETY: spawn placed this child in a new process group with its own PID.
        let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        (killed < 0)
            .then(io::Error::last_os_error)
            .filter(|error| error.raw_os_error() != Some(libc::ESRCH))
    };
    let reaped = child.wait();
    match result {
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "{error}{}{}",
                kill_error
                    .map(|error| format!("; process-group cleanup failed: {error}"))
                    .unwrap_or_default(),
                reaped
                    .err()
                    .map(|error| format!("; reap failed: {error}"))
                    .unwrap_or_default()
            ),
        )),
        Ok(output) => {
            if let Some(error) = kill_error {
                return Err(error);
            }
            reaped?;
            Ok(output)
        }
    }
}

#[cfg(not(unix))]
fn capture_output(_: &mut Command, _: Duration, _: bool) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded host subprocesses require Unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "virsh::tests::process_fixture", "--nocapture"])
            .env("RAMPIRE_VIRSH_FIXTURE", mode);
        command
    }

    #[test]
    fn process_fixture() {
        use std::io::Write;

        let Ok(mode) = std::env::var("RAMPIRE_VIRSH_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "output" => {
                let bytes = vec![b'x'; 256 * 1024];
                io::stdout().write_all(&bytes).expect("fixture stdout");
                io::stderr().write_all(&bytes).expect("fixture stderr");
            }
            "excessive" => {
                io::stdout()
                    .write_all(&vec![b'x'; 5 * 1024 * 1024])
                    .expect("fixture output");
            }
            "descendant" => {
                #[expect(
                    clippy::zombie_processes,
                    reason = "the fixture exits first to orphan a pipe-holding descendant"
                )]
                let _descendant = Command::new("sh")
                    .args(["-c", "sleep 10"])
                    .spawn()
                    .expect("descendant");
            }
            "failure" => {
                eprintln!("fixture access denied");
                std::process::exit(7);
            }
            "timeout" => std::thread::sleep(Duration::from_secs(10)),
            other => panic!("unexpected fixture mode: {other}"),
        }
    }

    #[test]
    fn drains_large_stdout_and_stderr_before_waiting_for_exit() {
        let output = bounded_output(&mut fixture("output"), Duration::from_secs(5))
            .expect("large output does not block the child");
        assert!(output.status.success());
        assert!(output.stdout.len() >= 256 * 1024);
        assert_eq!(output.stderr.len(), 256 * 1024);
    }

    #[test]
    fn preserves_failed_command_status_and_stderr() {
        let output = bounded_output(&mut fixture("failure"), Duration::from_secs(5))
            .expect("capture failed child");
        assert_eq!(output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&output.stderr).contains("fixture access denied"));
    }

    #[test]
    fn terminates_a_command_that_exceeds_its_deadline() {
        let error = bounded_output(&mut fixture("timeout"), Duration::from_millis(100))
            .expect_err("sleeping child must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn descendants_and_excessive_output_are_bounded() {
        let started = std::time::Instant::now();
        let error =
            bounded_output(&mut fixture("descendant"), Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        let error = bounded_output(&mut fixture("excessive"), Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("output exceeds"));
    }

    #[test]
    fn read_commands_do_not_request_a_management_connection() {
        let virsh = Virsh::with_connection("virsh", Duration::from_secs(1), "qemu:///system");
        for operation in ["dumpxml", "domstate", "version"] {
            let command = virsh.command(&[operation.to_owned(), "guest".to_owned()]);
            let args = command.get_args().collect::<Vec<_>>();
            assert_eq!(
                args,
                ["-c", "qemu:///system", "--readonly", operation, "guest"]
            );
        }
        let command = virsh.command(&["update-memory-device".to_owned(), "guest".to_owned()]);
        assert!(!command.get_args().any(|arg| arg == "--readonly"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn authorization_check_is_non_interactive_and_identifies_the_exact_process() {
        use std::os::unix::fs::MetadataExt;

        let command = authorization_command().expect("construct local policy check");
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(command.get_program(), "pkcheck");
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "--action-id");
        assert_eq!(args[1], "org.libvirt.unix.manage");
        assert_eq!(args[2], "--process");
        let identity = args[3].to_str().expect("process identity");
        let fields = identity.split(',').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], std::process::id().to_string());
        assert!(fields[1].parse::<u64>().expect("process start ticks") > 0);
        assert_eq!(
            fields[2],
            std::fs::metadata("/proc/self")
                .expect("metadata")
                .uid()
                .to_string()
        );
    }

    struct RecordingVirsh {
        arguments: Vec<String>,
    }
    impl VirshCommand for RecordingVirsh {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            assert_eq!(arguments, self.arguments);
            Ok("ok".to_owned())
        }
    }
    #[test]
    fn trait_accepts_exact_argument_vectors() {
        assert_eq!(
            RecordingVirsh {
                arguments: vec![
                    "dumpxml".to_owned(),
                    "--live".to_owned(),
                    "guest".to_owned()
                ]
            }
            .run(&[
                "dumpxml".to_owned(),
                "--live".to_owned(),
                "guest".to_owned()
            ])
            .expect("recorded command"),
            "ok"
        );
    }
}
