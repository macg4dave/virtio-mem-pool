---
mode: agent
description: Build and test the Windows service over SSH, with libvirt and QEMU Guest Agent diagnostics
---

Goal: `${input:goal:Windows build or test behaviour to verify}`

Build and test the current Windows source. Carry the authorised work through to
verified artifacts and evidence; do not stop at a plan. Diagnose failures and fix
source/build problems within the requested scope. Ask only for information that
cannot be established from the repository or live read-only inspection. This
prompt does not itself authorise deployment, service restarts or RAM resizing;
use any explicit operational authorisation already given in the conversation.

## Establish the project and current state

1. Work from the repository containing this prompt, not the IDE terminal's
   assumed directory.
2. Read `AGENTS.md`, `.github/prompts/rust.md`, `docs/roadmap.md` and the
   Windows sections of `docs/development.md`. Inspect `Makefile`,
   `tools/xtask/src/windows.rs` and `cargo xtask help` for current syntax.
3. Record `git rev-parse HEAD` and `git status --short`. Preserve existing edits.
   Builds synchronise the working tree, including uncommitted source: a commit
   ID alone does not identify what was built. Never reset, clean or stash the
   user's work to obtain a clean build.
4. Treat recorded addresses, device geometry, tool versions and old test counts
   as historical observations. Re-establish what matters for this run. Never
   claim an old artifact was produced by the current source.

Endpoints and paths come from the site's Windows build file
(`site/windows-build.mk`, untracked; see
`deploy/examples/windows-build.mk.example`). Below, `$WINDOWS_SSH`,
`$WINDOWS_DIR`, `$WINDOWS_ARTIFACTS` and `$VM` stand for its SSH alias,
guest workspace, local output folder and the libvirt VM name. Verify them
before relying on them. Fixed product values:

| Item | Value |
| --- | --- |
| Libvirt connection | `qemu:///system` |
| Native target | `x86_64-pc-windows-msvc` |
| Service configuration | `C:\ProgramData\RAMpire\config.json` |
| Control file | `C:\ProgramData\RAMpire\telemetry.json` |

The SSH alias and libvirt VM name are different identifiers. Do not substitute
one for the other. Read the installed configuration to discover the actual
service identity and telemetry path when live tests require them.

## SSH and VM preflight

Use bounded, noninteractive SSH with host-key checking:

```bash
ssh -G "$WINDOWS_SSH"
ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 \
  "$WINDOWS_SSH" 'cmd /c ver'
timeout 15s virsh -c qemu:///system domstate "$VM"
timeout 15s virsh -c qemu:///system domiflist "$VM"
timeout 15s virsh -c qemu:///system domifaddr "$VM" --source agent
```

Do not print private keys, tokens or passwords. Do not bypass host-key checking,
automatically replace a changed key, or put passwords in commands/files. Verify
unexpected host keys through an independently trusted channel. Existing SSH
configuration/agent credentials are the normal authentication path.

If the endpoint is unreachable, compare DNS, `ssh -G`, VM state, NIC attachment
and guest-agent addresses before changing anything. Missing agent addresses do
not prove that networking is broken. Do not start/reboot a VM, change its NICs,
firewall, GPU/VFIO, disks or memory to make a build work without applicable
operational authorisation. Report a concrete access blocker if no usable
endpoint exists; a build-only task does not authorise network repair.

Remote commands run under Windows `cmd.exe` unless explicitly invoking
PowerShell. Prefer repository tooling for quoting, MSVC environment setup,
timeouts and SSH options. For nontrivial PowerShell, send a script or use
`-EncodedCommand` (UTF-16LE base64); avoid nested shell interpolation.

The native preflight changes directory to the configured workspace before
checking tool versions. A missing workspace previously caused “The system
cannot find the path specified.” After verifying the destination is the
intended build-only directory, create it if absent:

```bash
ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 \
  "$WINDOWS_SSH" "if not exist \"$WINDOWS_DIR\" mkdir \"$WINDOWS_DIR\""
```

Before sync, inspect an existing remote checkout for independent edits. Do not
overwrite another person's source; preserve it or select an isolated build
workspace using the environment override. Source sync overlays tracked and
nonignored working-tree files; it is not a remote clean. If a stale remote
source/configuration file affects the build, identify it and use an isolated
workspace rather than indiscriminately deleting directories.

## Native build and tests

From the repository root, prefer the existing presets:

```bash
mkdir -p "$WINDOWS_ARTIFACTS"
set -o pipefail
make windows-native-gate 2>&1 | tee "$WINDOWS_ARTIFACTS/native-gate.log"
```

Stop and inspect the actual failing command if the gate fails. `pipefail` is
required so `tee` cannot hide failure. Read the log, not just the exit code.
`make windows-native-gate` checks the toolchain, synchronises current source, builds
release binaries, runs native Windows tests, checks formatting and Clippy with
warnings denied, then fetches and SHA-256 verifies the service and workload
executables and records the build in `build.json` (see Build identity in
Development). Run `make windows-status` to compare the installed service with
that record before and after any deployment.
`make windows-build` runs check/sync/build/fetch only; use it when tests are
explicitly out of scope, not as a substitute for a failed gate.

The presets provide the required environment to their child commands. For
individual `cargo xtask windows ...` commands, export it explicitly:

```bash
export RAMPIRE_WINDOWS_SSH="$WINDOWS_SSH"
export RAMPIRE_WINDOWS_DIR="$WINDOWS_DIR"
export RAMPIRE_WINDOWS_ARTIFACTS="$WINDOWS_ARTIFACTS"
export RAMPIRE_WINDOWS_CONNECT_TIMEOUT_SECONDS=10
export RAMPIRE_WINDOWS_OPERATION_TIMEOUT_SECONDS=1200
```

Available operations: `check`, `sync`, `build`, `test`, `lint`, `fetch`, `all`;
`build-status` and `uninstall` take `SERVICE --output PATH`.
Use the Make/environment overrides when a different verified endpoint or
workspace is required. `RAMPIRE_WINDOWS_HOST_NAME` and
`RAMPIRE_WINDOWS_HOST_KEY_ALIAS` must be supplied together; verify trust
before overriding an endpoint. The Windows check expects Rust/rustfmt/Clippy,
Visual Studio C++ tools discovered through `vswhere`, Windows tar and certutil.
Inspect the detected installation rather than hardcoding a compiler version.

For shared-policy or host/tooling changes, also run:

```bash
cargo xtask gate local
```

Linux cross-target Clippy is useful diagnostics, but it does not replace a
native Windows link and execution of Windows tests. Do not install a MinGW
linker just to work around a working MSVC guest build. Core policy tests run in
the local gate; Windows worker and Win32 lifecycle tests run in the native gate.
Some timeout tests intentionally terminate a child process and print an error;
judge them by the named test result and overall gate, not that line alone.

The gate fetches the service and workload executables from the same release
build and verifies each SHA-256 against Windows. Identify any candidate or
installed binary by its build ID and hash from `build.json` and `windows-status`,
not by file timestamps.

Compare the actual hashes; printing two hashes is not verification. Apply an
outer operation timeout to direct SSH/SCP commands when transferring/reading
artifacts, as the xtask does. Do not run the workload merely to check its build:
execution allocates guest RAM and belongs to a bounded authorised trial.

## Other VM tools: diagnostics versus operational tests

Use read-only observations to diagnose the failing layer:

- `virsh ... dumpxml "$VM"`: live NICs, guest-agent channel, virtio-mem
  device alias, size, block size, requested/current allocation. Save XML under
  `.artifacts/` if needed; do not redefine the domain or edit its disks.
- `virsh ... qemu-agent-command "$VM" '{"execute":"guest-info"}'`
  and `'{"execute":"guest-network-get-interfaces"}'`: agent reachability,
  supported commands and guest addresses. Use timeouts. Inspect support before
  assuming `guest-exec`, guest-file access or a memory-stat command exists.
- `cargo xtask windows diagnose-service SERVICE --output PATH`: service
  diagnostics through SSH; provide the exported Windows environment above.
  Verify the installed executable/configuration and its version separately
  from the freshly built candidate.

SSH failure does not imply QGA failure or vice versa. QGA can provide read-only
observations while SSH is unavailable, but do not use guest-exec or guest-file
writes to bypass a missing authorisation or install an ad hoc second harness.
Do not treat balloon statistics, total configured domain memory or requested
virtio-mem bytes as actual returned memory.

Build/test success is not service deployment or live memory qualification.
When the conversation explicitly authorises those effects, use the existing
reviewable workflows and satisfy their prerequisites:

1. Validate a Windows deployment manifest with `windows deploy MANIFEST
   --output PATH`; add `--apply` only for authorised installation. Preserve the
   actual working executable/configuration and documented rollback inputs.
2. Use `windows service-cycle SERVICE --output PATH [--apply]` and the bounded
   `preflight` workflow for authorised lifecycle/session/durable-request checks.
   Do not assume a restart discards a pending request.
3. Establish the observe-only baseline with a reviewed host config and
   `host-run --config FILE --max-run-seconds N
   --min-host-available-bytes BYTES`. Confirm the systemd controller is inactive
   and use exclusive device ownership; never run two controllers on one VM.
4. For authorised growth/reclaim trials, use `qualification start/run/status/
   review` as supported by the current parser/help, with explicit allocation
   caps, time limits, host reserve, paths, device alias and expected outcomes.
   Exercise resident and committed modes, actual growth, actual returned bytes,
   and renewed pressure after reclaim. A trial that never resizes does not
   qualify resizing. Observe requested/current and final durable state.
5. Do not issue raw `virsh setmem`/`update-memory-device`, clear latches, delete
   checkpoints/request-state, change VFIO or restart the VM as a test shortcut.
   Use the attestation-aware recovery tools for explicitly authorised recovery.
   A stopped runner does not automatically restore memory or settle a shrink.

Do not invent options or run literal placeholders. Inspect current parser/help
and prepare concrete inputs before execution. If operational approval is
missing, finish authorised builds, tests, diagnostics and reviewable inputs,
then identify the exact remaining action and why it needs approval. Honour
existing approval; do not ask twice. Interactive sudo belongs in the user's
terminal if required by the host's rules; never assume passwordless sudo.

## Required pressure-policy coverage

When changing the Windows policy/runtime, cover the applicable behaviours:

- Native Low reaches the durable Add request even with ample numeric reserves.
- Physical-reserve and commit-reserve deficits independently permit bounded
  growth; an unavailable native signal must not disable emergency growth.
- Reclaim requires continuous native High plus numeric surplus, history,
  hysteresis, block alignment, device limits and physical/commit safety floors.
- Low, Neutral, failed/unknown signals, missing/stale/mixed host geometry and
  failed numeric samples reset reclaim evidence. Standby is already available;
  cache must not be subtracted twice or used to justify unsafe release.
- Monotonic history survives wall-clock adjustments and frequent samples while
  retaining peak demand and gap checks. Partial removals back off.
- Kernel notification waits cancel promptly; sustained Low cannot busy-loop;
  failed collection/notification creation can recover without restarting.
- One unresolved request survives restart and is retransmitted unchanged.
  Renewed pressure cannot silently replace an owned removal. Verify actual
  convergence/settlement in live trials before claiming that case qualified.

Use existing fixtures and gates. Add focused behavioural tests for changed
contracts; do not lower assertions to make failures disappear. Native unit
tests do not establish real-world reclaim latency, ACL correctness after an
upgrade, QGA file sharing or rendered Event Log messages; gather that evidence
in authorised operational tests.

## Finish with evidence

Save logs, source revision plus dirty-tree/source hashes, toolchain/target,
commands, native test totals, exit statuses, artifact hashes and final observed
VM/service/device state when inspected. Keep evidence in `.artifacts/` and do
not publish secrets. Update `docs/roadmap.md` with meaningful qualification
results and `docs/roadmap.md` only when acceptance criteria are actually met.

Report what built, which tests passed, artifact paths, what failed or remains
unverified, and whether any live service/device effect occurred. Do not claim
success from configuration, ping, a successful copy or a stale executable.
