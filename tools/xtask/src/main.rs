mod attestation;
mod build_identity;
mod controller_status;
mod dashboard;
mod debian;
mod host_deploy;
mod host_run;
mod linux;
mod local;
mod metrics;
mod pool;
mod preflight;
mod process;
mod qualification;
mod watch;
mod windows;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = r#"Repository build and test tooling

Usage:
  cargo xtask gate <format|build|test|lint|checks|local|all>
  cargo xtask doctor host
  cargo xtask controller-status INSTANCE [OPTIONS]
  cargo xtask attestation VM_NAME ALIAS REVIEW --output PATH [OPTIONS]
  cargo xtask host-deploy INSTANCE --config PATH --attestation VM=PATH... [OPTIONS]
  cargo xtask host-run --config PATH --max-run-seconds N [OPTIONS]
  cargo xtask preflight VM_NAME ALIAS [OPTIONS]
  cargo xtask linux build
  cargo xtask linux package --output DIRECTORY
  cargo xtask linux deploy SSH_DESTINATION --output PATH [--apply]
  cargo xtask linux status SSH_DESTINATION
  cargo xtask linux uninstall SSH_DESTINATION --output PATH [--apply] [--discard-state]
  cargo xtask windows <check|sync|build|test|lint|fetch|all>
  cargo xtask windows deploy MANIFEST --output PATH [--apply]
  cargo xtask windows service-cycle SERVICE --output PATH [--apply]
  cargo xtask windows diagnose-service SERVICE --output PATH
  cargo xtask windows build-status SERVICE --output PATH
  cargo xtask windows uninstall SERVICE --output PATH [--apply]
  cargo xtask windows verify EXPECTED_ED25519_FINGERPRINT --runs N
  cargo xtask qualification <start|run|status|review> [OPTIONS]
  cargo xtask watch [OPTIONS]
  cargo xtask metrics [OPTIONS]
  cargo xtask pool [OPTIONS]

Attestation options:
  --output PATH                 Required generated attestation path.
  --command-timeout-seconds N   Required external-command bound.
  --connect URI                 Libvirt URI; default qemu:///system.

Host deployment options (INSTANCE names the unit; settings install under /etc/virtio-mem-host/):
  --config PATH                 Reviewed host_list.toml with sibling global/VM settings.
  --attestation VM=PATH         Required per VM; installed as
                                /etc/virtio-mem-host/<vm_name>.attestation.json.
  --output PATH                 Required structured JSON evidence path.
  --command-timeout-seconds N   Required external-command bound.
  --apply                       Install; otherwise validate only.
  --elevate                     Invoke this prebuilt xtask once through sudo.

Host run options (the daemon in the foreground instead of systemd):
  --config PATH                 Required host config file.
  --max-run-seconds N           Required; the daemon is stopped (SIGTERM) after N seconds.
  --min-host-available-bytes N  Stop the daemon if host MemAvailable falls below N.
  --log PATH                    Default .artifacts/host-run/<unix-millis>.log.
  --sudo                        Run the daemon through sudo (root-only attestation).

Controller status options:
  --vm NAME                     Select the VM; required when the instance has several.
  --host-cli PATH               Required absolute candidate host executable.
  --output PATH                 Required structured JSON evidence path.
  --command-timeout-seconds N   Required external-command bound.
  --connect URI                 Libvirt URI; default qemu:///system.
  --elevate                     Invoke this prebuilt xtask once through sudo.

No-actuation preflight options:
  --unit UNIT                   Required disabled/inactive virtio-mem-host@INSTANCE unit.
  --telemetry-path PATH         Required protected Windows telemetry path.
  --service SERVICE             Required Windows telemetry service identity.
  --attestation PATH            Required reviewed attestation.
  --host-deployment-evidence PATH  Required applied host deployment evidence.
  --windows-evidence PATH       Required Windows restart evidence path.
  --output PATH                 Required structured result path.
  --host-headroom-bytes N       Required minimum MemAvailable.
  --sample-interval-seconds N   Required poll interval.
  --telemetry-max-age-seconds N Required telemetry freshness bound.
  --future-tolerance-seconds N  Required future-clock tolerance.
  --preflight-timeout-seconds N Required overall new-session bound.
  --command-timeout-seconds N   Required external-command bound.
  --connect URI                 Libvirt URI; default qemu:///system.
  --apply-service-restart       Restart only the named Windows service.

Qualification commands:
  start VM ALIAS --ssh-target TARGET --mode resident|committed|file-cache [--apply] [OPTIONS]
  status RUN_ID [--output-root PATH]
  review RUN_ID [--output-root PATH]

Qualification options:
  --apply                    Start the workload; otherwise validate only.
  --elevate                  Use one sudo boundary for controller and libvirt observation.
  --mode MODE                Required resident or committed workload behavior;
                             file-cache (Linux) keeps files' page cache in use.
  --guest-os windows|linux   The guest's operating system; default windows.
  --cache-dir PATH           Guest directory for the file-cache workload.
  --peak-bytes N             Required peak allocation.
  --retained-bytes N         Required allocation retained between peaks.
  --max-allocation-bytes N   Required independent workload safety cap.
  --peak-hold-seconds N      Required first-peak hold.
  --settled-hold-seconds N   Required retained-allocation hold.
  --renewed-hold-seconds N   Required renewed-peak hold.
  --resident-refresh-seconds N  Required page refresh interval.
  --post-hold-seconds N      Required final observation window; may be zero.
  --interval-seconds N       Required host sampling interval.
  --command-timeout-seconds N  Required bound for external commands.
  --controller-timeout-seconds N Required hard bound for controller ownership.
  --expect-growth-bytes N    Required minimum observed growth.
  --expect-reclaim-bytes N   Required minimum observed reclaim.
  --expect-renewed-growth-bytes N Required minimum growth after settled reclaim.
  --expect-no-resize          Require observe-only mode and unchanged requested/current allocation.
  --expect-no-reclaim         Require growth without any lower requested target; replaces --expect-reclaim-bytes.
  --require-renewed-during-pending-shrink Require an observed pending shrink before renewed pressure.
  --remote-workload PATH     Required workload executable path in the guest.
  --controller-unit UNIT     Required virtio-mem-host@INSTANCE unit whose config holds VM.
  --guest-service NAME       Required guest service identity on the wire.
  --apply-service-restart    Restart the named Windows service after the controller is active.
  --guest-service-cycle-timeout-seconds N Required bound for that explicit restart.
  --telemetry-path PATH      Required guest control publication path (read from the
                             controller's copy, else through QGA).
  --telemetry-max-age-seconds N Required telemetry freshness bound.
  --telemetry-future-tolerance-seconds N Required future-clock tolerance.
  --connect URI              Libvirt URI; default qemu:///system.
  --output-root PATH         Default .artifacts/qualification.

Windows environment:
  VIRTIO_MEM_WINDOWS_SSH              Required SSH config alias or USER@HOST.
  VIRTIO_MEM_WINDOWS_DIR              Remote workspace directory.
  VIRTIO_MEM_WINDOWS_ARTIFACTS        Local artifact directory.
  VIRTIO_MEM_WINDOWS_CONNECT_TIMEOUT_SECONDS Required SSH connection bound.
  VIRTIO_MEM_WINDOWS_OPERATION_TIMEOUT_SECONDS Required operation bound.
  VIRTIO_MEM_WINDOWS_KNOWN_HOSTS_FILE Optional pinned known-hosts file.
  VIRTIO_MEM_WINDOWS_IDENTITY_FILE    Optional private-key path.
  VIRTIO_MEM_WINDOWS_HOST_NAME        Optional current endpoint override.
  VIRTIO_MEM_WINDOWS_HOST_KEY_ALIAS   Required with host-name override.
"#;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match execute(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("virtio-mem-xtask failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(arguments: &[String]) -> Result<(), String> {
    let repo = repo_root()?;
    match arguments.first().map(String::as_str) {
        Some("help" | "--help" | "-h") | None => {
            print!("{HELP}\n{}\n{}\n{}", watch::HELP, pool::HELP, metrics::HELP);
            Ok(())
        }
        Some("gate") => gate(&arguments[1..], &repo),
        Some("doctor")
            if arguments.get(1).map(String::as_str) == Some("host") && arguments.len() == 2 =>
        {
            local::doctor_host()
        }
        Some("doctor") => Err("doctor requires exactly: host".to_owned()),
        Some("controller-status") => {
            controller_status::run(&controller_status::parse(&arguments[1..])?, &repo)
        }
        Some("attestation") => attestation::run(&attestation::parse(&arguments[1..])?, &repo),
        Some("host-deploy") => host_deploy::run(&host_deploy::parse(&arguments[1..])?, &repo),
        Some("host-run") => host_run::run(&host_run::parse(&arguments[1..])?, &repo),
        Some("preflight") => preflight::run(&preflight::parse(&arguments[1..])?, &repo),
        Some("linux") => linux::run(&arguments[1..], &repo),
        Some("windows") => windows::run(windows::parse(&arguments[1..])?, &repo),
        Some("qualification") => qualification::execute(&arguments[1..], &repo),
        Some("watch") => watch::run(&watch::parse(&arguments[1..])?, &repo),
        Some("pool") => pool::run(&pool::parse(&arguments[1..])?, &repo),
        Some("metrics") => metrics::execute(&arguments[1..], &repo),
        Some(command) => Err(format!("unknown command: {command}\n\n{HELP}")),
    }
}

fn gate(arguments: &[String], repo: &Path) -> Result<(), String> {
    if arguments.len() != 1 {
        return Err("gate requires format|build|test|lint|checks|local|all".to_owned());
    }
    let step = match arguments[0].as_str() {
        "format" => local::Step::Format,
        "build" => local::Step::Build,
        "test" => local::Step::Test,
        "lint" => local::Step::Lint,
        "checks" => local::Step::Checks,
        "local" => local::Step::Local,
        "all" => {
            local::run(local::Step::Local, repo)?;
            return windows::run_all(repo);
        }
        value => return Err(format!("unknown gate: {value}")),
    };
    local::run(step, repo)
}

fn repo_root() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "xtask manifest is not nested under the repository root".to_owned())?;
    Ok(root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_root_contains_workspace_manifest() {
        assert!(repo_root()
            .expect("repository root")
            .join("Cargo.toml")
            .is_file());
    }
}
