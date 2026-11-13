# Roadmap

Written 2026-11-12. This replaces ad-hoc planning from the Windows test phase.
It states where the project is, what each part is responsible for, how the
Linux guest should be designed on its own terms, what blocks progress, and the
milestones and tasks that follow.

## Goal

One host controller on a KVM host manages a shared pool of virtio-mem RAM for
many VMs, Windows and Linux together. Each guest decides when it needs more or
less memory and asks; the host admits, grants within pool and host limits,
resizes, and reports the actual result. The finished product is installed from
packages and configured by an administrator, with nothing in it that only
makes sense on the machines it was developed on.

## Where we are

| Area | State |
|---|---|
| Core policy, request protocol, wire format | Working, platform-neutral, well tested |
| Host controller (RHEL, libvirt) | Working for one Windows VM (`win11_gpu`); multi-VM pool implemented |
| Windows guest service | Working, **paused** until the `viomem` driver removal bug is repaired |
| Linux guest service | Runs on `debian13` under the real controller: growth and removal both completed on 2026-11-13 |
| Tooling (`xtask`: deploy, watch, metrics, qualification) | Built for the Windows VM; Linux has config generation only |
| Linux work from 2026-11-12 | Committed on `main` (`e9b8683`) |
| Controller on `pc101` | Running as `virtio-mem-host@main`, managing `debian13` only, from a site configuration outside the repo |

## Responsibilities

Each responsibility has one owner. New work goes to the owner, not to a copy.

| Part | Owns | Must not own |
|---|---|---|
| `crates/virtio-mem-core` | Policy, request/result protocol, wire and settings formats, unit rules | Any OS call, file path convention or operational wording |
| `host/` | Admission, pool and host-reserve accounting, device resize, durable checkpoint, guest transport (host side) | Estimating guest demand; guest-OS policy |
| `guest-windows/`, `guest-linux/` | Sampling the OS, running the core policy, durable request state, guest transport (guest side), service lifecycle | Resizing anything; policy rules that belong in core |
| `tools/xtask` | Build, deploy, observe, qualify | Runtime behaviour the daemon or guests depend on |
| Packaging and site configuration | Install paths, units, administrator settings for one site | Anything checked in as a default for everyone |

## Leaving early development

These rules apply to every task below and to review.

1. **No site values in code or tracked defaults.** VM names, SSH aliases, user
   paths, pool sizes and device aliases belong to a site's configuration.
   The repo carries neutral examples and test fixtures only.
2. **A built-in value is a product default or it is a setting.** A default is
   defined once, documented, sensible for any supported VM and overridable
   where an administrator could reasonably need to. A number chosen to make
   one test VM behave is neither; it is replaced by a derived value or a
   setting, with evidence for the default.
3. **Workarounds are fenced and dated.** Each names the defect, the guest it
   applies to and the condition for deleting it. It never applies to a guest
   that does not have the defect.
4. **Linux is designed for Linux.** A Windows choice carries over only when it
   is also the right answer for Linux. The section below records each one.
5. **Tests use fixtures.** No unit test reads a tracked site file or depends
   on a live VM.
6. **Nothing reaches a guest by hand.** If a step is needed to install or
   upgrade, tooling or a package does it and records what it did.

## Linux guest design

What the first Linux version inherited from Windows, and what is right for
Linux. "Keep" means the Windows answer is also correct here.

| Topic | Inherited from Windows | Right for Linux | Milestone |
|---|---|---|---|
| Request protocol, durable single request, host-owned resize | Shared core | Keep. Nothing here is OS-specific | — |
| Pressure signal | A categorical Low/Neutral/High, polled | Pressure stall information. Use PSI **triggers** (kernel wakes the service when stall time crosses a threshold) for growth, not the ten-second average read once a second: the OOM killer can act faster than a poll. Keep the average only as the "calm" test for reclaim | M2 |
| Commit accounting | Commit total and limit | Linux has no enforced commit under default overcommit. Current mapping (RAM plus swap, less available plus free swap) is a stand-in. Decide whether the commit reserve should exist at all for Linux, and honour strict overcommit where it is enabled | M2 |
| Page cache | Counted as available | Linux fills free RAM with cache by design. Treating all cache as surplus shrinks a file-heavy guest until it thrashes its cache. Reclaim needs a cache-aware floor (working-set refault activity, or a configurable cache allowance) | M2 |
| Reserve sizes | Fixed 2 GiB reserve, 1 GiB floor, 3 GiB shrink step | Wrong for a 2–5 GiB Linux VM: the reserve is as large as the guest. Size reserves relative to the VM, with per-VM overrides | M2 |
| Fixed (boot) RAM | Large; never a concern | Linux boots in little RAM, but boot RAM must hold the kernel plus page metadata for every byte that can be plugged (about 1.6 %), and kernel memory cannot live in movable zones. Validate boot RAM against the device size before managing a VM | M2 |
| Removal reliability | Driver workaround on the host | Linux removes memory reliably only from the movable zone. How hot-plugged memory is onlined (`online_movable`, `auto-movable` policy and ratio) decides whether reclaim works. The guest must check and report this; the product must document or set it | M2 |
| Stalled removal | Host lowers the target step by step | Not applicable. The Linux driver retries by itself. The host holds, then settles partial. The settle delay needs its own evidence-based value | M2 |
| Transport | Host reads and writes guest files through the QEMU guest agent | Works on Debian. Not a product answer: RHEL-family guests block guest-agent file commands by default, the agent gives root file access far beyond this need, and writes are not atomic. A dedicated virtio-serial port carrying the same records suits both guest OSes (the wake channel already uses one). Decision required | M3 |
| Wake-up | Dedicated virtio-serial doorbell | Keep the idea. Folds into the transport decision | M3 |
| Guest configuration | Generated file copied into the guest, duplicating host-side limits | Limits and policy already live on the host. If the host publishes them with its status, a Linux guest needs no generated file and cannot drift from the host. Decision required | M3 |
| Service model | SCM service under LocalService | systemd unit, unprivileged dynamic user, state in `/var/lib`. Keep; review the sandbox once the transport is final | M4 |
| Installation | SSH sync, build in guest, PowerShell deploy | A Debian package with unit and defaults; `xtask linux deploy` for development. Building inside the guest is a development convenience, not the install path | M4 |
| Driver identity | On-disk `viomem.sys` version | Kernel release, plus the driver facts that matter (module present, device block size, onlining policy) | M2 |
| Qualification workload | Windows workload executable | A Linux workload with resident and file-cache modes, since cache behaviour is the Linux-specific risk | M5 |

## Blockers

| ID | Blocker | Blocks | Unblocked by |
|---|---|---|---|
| B1 | `viomem` Windows driver removal bug | All Windows milestones; mixed-fleet qualification (M6) | Patched driver deployed and verified; then delete the fenced workaround |
| B2 | ~~Go-ahead to install the controller on `pc101`~~ | — | Cleared 2026-11-13: the owner allows the controller to be updated and installed |
| B3 | ~~Attestation requires `windows_driver_version`~~ | — | Cleared 2026-11-13: the review field is now `guest_driver_version` and older reviews are still read. `trusted_development_guest` remains (T0.6) |
| B4 | Policy defaults sized for one large Windows VM | Meaningful Linux behaviour: on `debian13` the reserve alone is 2 GiB of a 5 GiB guest (M1 results, M2) | Relative reserves and per-VM overrides (T2.3) |
| B5 | Removal reliability on Debian's default onlining is unmeasured | Signing off reclaim for Linux (M2) | Measurement on `debian13` under each onlining policy (T2.5) |
| B6 | Transport depends on guest-agent file commands | RHEL-family guests; packaging (M4); the final sandbox | Transport decision and implementation (T3.1, T3.2) |
| B7 | Linux gate runs only by hand on `pc101`; host and `xtask` do not build on the Windows workstation | Trustworthy review of every change | Scripted Linux gate (T0.4) |
| B8 | `docs/development.md` is referenced by the Rust standards and does not exist | Agents and contributors following the standards | Write it (T0.5) |

## Milestones

A milestone is done when its exit criteria hold, not when its tasks are ticked.

### M0 — Leave early development

Remove what only the test phase needed, so later work starts from product
shape. No behaviour change for `win11_gpu`.

- [x] **T0.1** Commit the 2026-11-12 Linux work (`e9b8683`).
- [ ] **T0.2** Move site configuration out of tracked defaults: replace
  `deploy/config_files/` with neutrally named examples and keep each site's
  real files outside the repo. Covers the artefact table rows marked T0.2.
- [ ] **T0.3** Parameterise scripts, Makefile targets and editor tasks that
  name `win11_gpu` or a user path.
- [ ] **T0.4** One command that runs the full Linux gate from any workstation
  (remote build host by configuration), plus the whitespace check.
- [ ] **T0.5** Write `docs/development.md`: how to build, gate and test each
  crate, and which checks need which platform.
- [ ] **T0.6** Make the compatibility review guest-neutral. Done: driver
  identity is `guest_driver_version`. Left: a guest-OS field, and
  `trusted_development_guest` replaced by the conditions it stood in for.
- [ ] **T0.7** Give unit tests their own fixtures instead of tracked site
  files (`win11_gpu` and `debian13` settings).
- [x] **T0.8** Naming (done 2026-11-12): repository `virtio-mem-pool`;
  guest crates `guest-windows/` (`virtio-mem-guest-windows`) and
  `guest-linux/` (`virtio-mem-guest-linux`). The installed Windows names
  (`virtio-mem-service.exe`, `VirtioMemService`) stay until T6.2, because
  changing them needs a Windows deployment that B1 blocks.
- [ ] **T0.9** Decide the fate of pre-release migration code in `host-deploy`
  (legacy config handling): keep with a removal date, or delete.

Exit: `grep` for site names finds only examples and fixtures; the gate runs
from one command; the standards' references resolve; `win11_gpu` behaves as
before.

### M1 — First Linux VM under the real controller

Prove the existing design end to end before changing it. Needs B2 and B3.

- [x] **T1.1** Attest `debian13` with the guest-neutral review.
- [x] **T1.2** Run the controller against `debian13` in observe-only mode
  first, then actuating.
- [ ] **T1.3** Observe one growth and one removal end to end (done, from
  the controller journal); record them with `xtask metrics` (left, needs T1.4).
- [ ] **T1.4** Make `watch` and `metrics` label and read a Linux guest
  correctly (guest-neutral panels and column names; no Windows-only fields
  required).
- [ ] **T1.5** Stamp the Linux binary with a build identity, as the Windows
  build is, so a capture names what ran.
- [ ] **T1.6** Write down every surprise as a task in M2 or M3 rather than
  patching around it.

Exit: a recorded capture showing a completed Add and a completed or partial
Remove on `debian13`, with no latch, and the Windows VM unaffected.

First run, 2026-11-13 (`debian13`, 1 GiB fixed RAM, 4 GiB device, 2 MiB
blocks, default onlining, no swap):

- Removal of 2.5 GiB completed in about five seconds; a later 460 MiB
  removal also completed. No stall, no latch.
- Growth of 382 MiB completed about two seconds after memory was used.
- The guest settles at about 2.3 GiB total with under 300 MiB in use: the
  fixed 2 GiB reserve dominates a small VM (B4).
- Growth asks for exactly the shortfall, so noise at the reserve boundary
  produces a run of 2 MiB and 14 MiB requests, each a full host resize.
- With no wake channel each step waits for a poll on both sides.

### M2 — Linux-native policy and signals

Replace the Windows carry-overs with Linux answers. Needs M1 evidence.

- [ ] **T2.1** PSI triggers for growth wake-ups; verify the privileges the
  unit needs on the target kernels.
- [ ] **T2.2** Decide the Linux commit model: drop the commit reserve for
  default overcommit, honour `CommitLimit` under strict overcommit.
- [ ] **T2.3** Relative reserve, floor and step sizes with per-VM overrides in
  settings; defaults backed by M1 and M5 captures. Include a minimum growth
  step or upward hysteresis so boundary noise does not cause block-sized
  resizes. Removes B4.
- [ ] **T2.4** Cache-aware reclaim floor, measured with a file-heavy workload.
- [ ] **T2.5** Measure removal under each onlining policy; choose the
  recommended one; have the guest report zone layout and onlining policy.
  Removes B5.
- [ ] **T2.6** Guest readiness check at start and in diagnostics: driver
  present, PSI available, onlining policy, swap, boot RAM against device size.
- [ ] **T2.7** Host-side validation that a VM's fixed RAM can carry its
  device, refusing to manage one that cannot.
- [ ] **T2.8** Replace the provisional Linux thresholds (pressure level,
  failed-poll limit, stalled-removal delay) with measured defaults or settings.
- [ ] **T2.9** Move guest-side code that is identical on both guests behind
  one owner, once the Linux shape is stable.

Exit: a small (2–4 GiB) and a large Linux VM both grow before the OOM killer
acts and reclaim without cache thrash, in recorded runs, using defaults.

### M3 — Product transport and configuration

- [x] **T3.1** Transport decided (2026-11-12): a dedicated virtio-serial
  stream for both guests. Whether the guest-agent file path survives as a
  compatibility option is settled during T3.2.
- [ ] **T3.2** Implement it behind the host's existing guest-link boundary,
  for Linux first; Windows follows when B1 clears.
- [ ] **T3.3** Implement host-published limits and policy, so the Linux
  guest needs no generated configuration (decided 2026-11-12).
- [ ] **T3.4** Version-skew rules: which host and guest versions interoperate,
  and the upgrade order, enforced by the protocol rather than by a README.

Exit: a Linux guest with no guest-agent file access and no generated config
file is managed correctly; removes B6.

### M4 — Packaging and deployment

- [ ] **T4.1** Debian package for the guest: binary, unit, defaults, state
  directory, clean removal that preserves or explicitly discards request state.
- [ ] **T4.2** Host package or install tooling with configurable paths and
  libvirt URI; remove built-in `/usr/bin/virsh` and `/usr/local/libexec`
  assumptions where a site may differ.
- [ ] **T4.3** `xtask linux deploy|status|uninstall` for development VMs,
  recording evidence as the Windows commands do.
- [ ] **T4.4** Final systemd sandbox review for the guest unit.
- [ ] **T4.5** Administrator documentation: install, add a VM, recover,
  upgrade.

Exit: a fresh Debian VM goes from nothing to managed using packages and
documented steps only.

### M5 — Linux qualification

- [ ] **T5.1** Linux workload with resident, committed and file-cache modes.
- [ ] **T5.2** `xtask qualification` for Linux guests.
- [ ] **T5.3** Failure injection: guest service restart mid-request, guest
  reboot, controller restart, guest-agent or channel loss, clock step.
- [ ] **T5.4** Soak: several Linux VMs contending for one pool at different
  priorities, with host reserve pressure.

Exit: qualification passes on Debian with defaults; every latch seen is
explained.

### M6 — Mixed fleet

Needs B1.

- [ ] **T6.1** Resume Windows on the patched driver; delete the fenced
  workaround and its tests.
- [ ] **T6.2** Bring Windows to the M3 transport and configuration model.
- [ ] **T6.3** Mixed Windows and Linux soak on one controller and pool.
- [ ] **T6.4** Release documentation and versioning.

Exit: one controller, one pool, both guest types, qualified together.

## Test-phase artefacts to replace

| Artefact | Where | Replace with | Task |
|---|---|---|---|
| Tracked site settings, described as "kept here for testing" | `deploy/config_files/*.toml`, its README | Neutral examples; site files outside the repo | T0.2 |
| Workstation path and SSH alias | `deploy/config_files/windows-build.mk` | Environment or untracked site file | T0.2 |
| `vm=win11_gpu` | `deploy/config_files/recover-latch.sh` | Instance and VM arguments | T0.3 |
| `win11-*` targets and tasks | `Makefile`, `.vscode/tasks.json` | Generic targets driven by configuration | T0.3 |
| Tests that load tracked site files | `host/src/config.rs`, `tools/xtask/src/linux.rs`, `tools/xtask/src/metrics.rs`, `tools/xtask/src/watch.rs` | Test fixtures | T0.7 |
| `windows_driver_version`, `trusted_development_guest` | `host/src/attestation.rs` | Guest-neutral review | T0.6 |
| Fixed reserves, floors, steps and one-second reclaim history | `GlobalSettings::guest_policy` in core settings | Relative defaults with overrides | T2.3 |
| Control file named `telemetry.json`; field `demand_report_path` | Core settings, Windows config | Accurate names at the next Windows schema change | T6.2 |
| Built-in `/usr/bin/virsh`, `/etc/virtio-mem-host`, `/var/lib/virtio-mem-host`, `/usr/local/libexec` | Host config, units, `xtask` | Packaging defaults, overridable | T4.2 |
| `viomem` shrink extension and usable-region widening | `host/src/runtime.rs` (fenced) | Deleted when B1 clears; already limited to Windows guests | T6.1 |
| Windows wording in observation tools | `xtask` metrics, watch | Guest-neutral labels | T1.4 |
| Legacy configuration migration | `tools/xtask/src/host_deploy.rs` | Decision in T0.9 | T0.9 |
| Provisional Linux numbers: 10 % pressure, five failed polls, convergence timeout reused for stalled removal | `guest-linux/src/pressure.rs`, `guest-linux/src/worker.rs`, `host/src/runtime.rs` | Measured defaults or settings | T2.8 |
| Hand-installed, unversioned guest binary on `debian13` | The dev VM | Stamped build, then package | T1.5, T4.1 |
| Installed Windows names `virtio-mem-service.exe` and `VirtioMemService` | `guest-windows/`, core settings, `xtask` | Guest-neutral scheme with the next Windows deployment | T6.2 |
| Old checkout paths ending `virtio_mem_windows_service` | `deploy/config_files/windows-build.mk`, Windows build prompt | Site configuration | T0.2 |

## Decisions

Made by the owner on 2026-11-12:

- **Names:** rename to logical, distinct names. Done in T0.8.
- **Transport:** dedicated virtio-serial stream is the product path (T3.1).
- **Guest configuration:** the host publishes limits and policy; Linux guests
  get no generated file (T3.3).
- **Controller:** it may be updated and installed on `pc101` without asking
  each time (2026-11-13).
- **Scope:** Debian first. Other Linux distributions later; keep the guest
  free of Debian-specific code so they remain possible.

Still open:

- None.

## Not planned

- Estimating guest demand on the host.
- Taking memory from one VM to give to another.
- Guests other than Windows and Linux.
- Hosts other than libvirt/KVM.
