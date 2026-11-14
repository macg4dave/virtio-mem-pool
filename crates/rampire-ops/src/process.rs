use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

pub fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub fn run(program: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|error| format!("failed to start {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with status {status}"))
    }
}

pub fn output(program: &str, args: &[OsString], cwd: &Path) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to start {program}: {error}"))
}

pub fn checked_output(program: &str, args: &[OsString], cwd: &Path) -> Result<Vec<u8>, String> {
    let output = output(program, args, cwd)?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

pub fn checked_text(program: &str, args: &[OsString], cwd: &Path) -> Result<String, String> {
    String::from_utf8(checked_output(program, args, cwd)?)
        .map_err(|error| format!("{program} returned invalid UTF-8: {error}"))
}

pub fn bounded_text(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let output = bounded_output(program, args, cwd, timeout)?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} returned invalid UTF-8: {error}"))
}

pub fn elevated_text(args: &[OsString], cwd: &Path, timeout: Duration) -> Result<String, String> {
    let mut command = Command::new("sudo");
    command.args(args).current_dir(cwd);
    let output = rampire_host::virsh::bounded_interactive_output(&mut command, timeout)
        .map_err(|error| format!("elevated command: {error}"))?;
    if !output.status.success() {
        return Err(format!("elevated command failed with {}", output.status));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("elevated command returned invalid UTF-8: {error}"))
}

pub fn bounded_output(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    bounded(command, program, timeout)
}

/// Runs `program` with only the supplied environment and a fixed system PATH.
pub fn bounded_output_with_env<I, K, V>(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    timeout: Duration,
    environment: I,
) -> Result<Output, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .envs(environment);
    bounded(command, &program.display().to_string(), timeout)
}

/// Use the same output and descendant lifetime bounds as host commands.
fn bounded(mut command: Command, name: &str, timeout: Duration) -> Result<Output, String> {
    rampire_host::virsh::bounded_output(&mut command, timeout)
        .map_err(|error| format!("{name}: {error}"))
}

pub fn run_with_input(
    program: &str,
    args: &[&str],
    cwd: &Path,
    input: &[u8],
) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("failed to open {program} stdin"))?
        .write_all(input)
        .map_err(|error| format!("failed to write {program} input: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("failed to wait for {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with status {status}"))
    }
}

pub fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_os_string()
}

pub fn read_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut value = String::new();
    file.read_to_string(&mut value)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(value)
}

/// Wall-clock Unix time in milliseconds, for evidence and log names.
pub fn unix_millis() -> Result<u64, String> {
    use rampire_host::guest_channel::{SystemUnixClock, UnixClock};
    SystemUnixClock.now_unix_millis()
}

/// The caller's effective user ID; `u32::MAX` where there is none.
#[cfg(unix)]
pub fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` has no arguments and returns the caller's effective UID.
    unsafe { geteuid() }
}

#[cfg(not(unix))]
pub fn effective_uid() -> u32 {
    u32::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_preserves_plain_arguments() {
        assert_eq!(os("value"), OsString::from("value"));
    }
}
