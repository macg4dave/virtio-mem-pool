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
| Linux guest service | Runs on `debian13` under the real controller: growth and removal both completed on 2026-11-13. M2 started: PSI triggers, commit model, growth margin and readiness report are in |
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
| Page cache | Counted as available | Done: cache the guest is re-reading from disk (the kernel's refault counter) is counted as in use, grown until refaults stop and given back in trial steps | — |
| Free memory target | Fixed 2 GiB reserve, 1 GiB floor, 3 GiB shrink step from the Windows test | Done: one required per-VM setting, `free_ram`; every policy size derives from it and nothing is built in. Fixed (boot) RAM needs no check of its own: the administrator sets `min_ram` | — |
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
| B4 | ~~Policy sizes fixed for one Windows VM~~ | — | Cleared 2026-11-13: per-VM `free_ram` replaces every built-in size |
| B5 | ~~Removal reliability on Debian's default onlining is unmeasured~~ | — | Cleared 2026-11-13: measured under three onlining policies (T2.5). Long-uptime fragmentation is left to the soak (T5.4) |
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
- [ ] **T0.10** Make the metrics recorder say when it cannot read the
  controller journal, instead of recording zero controller events.
- [ ] **T0.9** Decide the fate of pre-release migration code in `host-deploy`
  (legacy config handling): keep with a removal date, or delete.

Exit: `grep` for site names finds only examples and fixtures; the gate runs
from one command; the standards' references resolve; `win11_gpu` behaves as
before.

### M1 — First Linux VM under the real controller (closed 2026-11-13)

Prove the existing design end to end before changing it. Needs B2 and B3.

- [x] **T1.1** Attest `debian13` with the guest-neutral review.
- [x] **T1.2** Run the controller against `debian13` in observe-only mode
  first, then actuating.
- [x] **T1.3** Observe growth and removal end to end and record them with
  `xtask metrics`.
- [x] **T1.4** `watch` and `metrics` label a Linux guest correctly, and a
  guest service incident points at the guest OS's own logs.
- [x] **T1.5** `cargo xtask linux build` stamps the Linux binary with the
  checkout's build identity; the stamped build is installed on `debian13`.
- [x] **T1.6** Surprises are written below and assigned to tasks.

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

Recorded capture, 2026-11-13 (`.artifacts/metrics/m1-debian13/`, guest build
`6134275b136c-dirty.4efd11acb388`): 1.5 GiB used in five steps, held, then
freed.

- 8 Adds (1,518 MiB) and 2 Removes (1,520 MiB), all Completed. No refusal,
  latch or incident. The device moved between 1,468 and 2,988 MiB.
- Host grant to completion averaged 0.9 s for Add and 1.3 s for Remove.
  Guest issue to completion averaged 3.1 s and 3.5 s: most of the delay is
  polling, not resizing (T2.1, T3.2).
- Guest available memory dipped to 1,735 MiB against the 2 GiB reserve while
  growth caught up.
- Run without journal access, the recorder reports zero controller events
  and "0 completed requests" with no warning (T0.10).

### M2 — Linux-native policy and signals

Replace the Windows carry-overs with Linux answers. Needs M1 evidence.

- [x] **T2.1** PSI triggers for growth wake-ups (2026-11-13). The service
  registers a 100 ms in 1 s trigger, which kernel 6.12 grants only with
  `CAP_SYS_RESOURCE` (now the unit's one capability), and falls back to
  200 ms in 2 s without it. With a trigger, the ten-second average no longer
  means Low. Seen firing in the T2.5 runs: a request issued between polls
  with pressure Low.
- [x] **T2.2** Linux commit model (2026-11-13): the kernel's `CommitLimit`
  pair is used only under strict overcommit with a RAM-scaled limit.
  Otherwise the RAM-plus-swap pair stays, whose headroom is never below
  available RAM, so the commit reserve cannot ask for anything.
- [x] **T2.3** Per-VM `free_ram` in the settings file replaces the built-in
  reserve, floor and step sizes (owner decision, 2026-11-13).
- [x] **T2.10** Growth and reclaim both aim a sixteenth of `free_ram` above
  the threshold, so noise at the boundary causes no resize (2026-11-13).
  This is in the shared policy and so applies to Windows too.
- [x] **T2.4** Cache floor that follows the kernel's refault counter
  (2026-11-13, owner decision). File cache the guest is re-reading from disk
  is counted as in use: the guest grows until refaults stop, then gives the
  cache back a step at a time and restores a step that turns out to be
  needed. Measured below. Its numbers are provisional (T2.8).
- [x] **T2.5** Removal measured under each onlining policy (2026-11-13,
  below). Recommendation: Debian's default needs no change; the guest
  reports its onlining policy and names `memory_hotplug.online_policy=auto-movable`
  as the remedy if removals come back partial. Removes B5.
- [x] **T2.6** Guest readiness check at start and in diagnostics: driver
  bound, PSI and trigger, onlining policy, movable zone, swap, commit model
  (2026-11-13). `watch` and `metrics` do not show it yet.
- [ ] **T2.11** Survive a fill faster than growth. Done: the guest unit sets
  `OOMScoreAdjust=-900`. Left: decide what else must survive (the guest
  agent until T3.2 replaces it) and record a run.
- [ ] **T2.8** Replace the provisional Linux thresholds (pressure level,
  failed-poll limit, stalled-removal delay, and the cache floor's refault
  threshold, quiet period and settling time) with measured defaults or
  settings.
- [ ] **T2.9** Move guest-side code that is identical on both guests behind
  one owner, once the Linux shape is stable.

Exit: a small (2–4 GiB) and a large Linux VM both grow before the OOM killer
acts and reclaim without cache thrash, in recorded runs, using defaults.

First runs, 2026-11-13 (`debian13`, guest build
`6134275b136c-dirty.43344f8f4a8f`, read from the journals, not a recorded
capture):

- `debian13` onlines hot-plugged memory as `online` with `contig-zones` and
  its Movable zone is empty. Removals of 2.9 GiB and 374 MiB from the Normal
  zone still completed.
- 1.6 GiB filled in four steps grew the guest by 374 MiB in total; the last
  request was 36 MiB (4 MiB short plus the 32 MiB margin). Available memory
  then stayed put for 20 quiet seconds and the growth was returned in one
  removal once the fill was freed. The M1 run of 2 MiB and 14 MiB requests did not recur.
- 2.6 GiB written to tmpfs in about a second ran the guest out of memory
  before any request was granted. The OOM killer killed the guest agent and
  the guest service, so the guest could not ask again and had to be reset
  (T2.11, and more evidence for B6).

Onlining measurement, 2026-11-13 (`.artifacts/metrics/m2-t25-debian13/`, one
run per policy from a fresh boot): the guest was grown to about 4.5 GiB with
3.2 GiB of tmpfs data, 200,000 files were created and a 1.5 GiB file written
and read, then the tmpfs data was freed.

| Onlining | Hot-plugged memory lands in | Removal | Result |
|---|---|---|---|
| Debian default (`online`, `contig-zones`) | Normal | 2,394 MiB | Completed in 3 s |
| `memhp_default_state=online_movable` | Movable | 2,378 MiB | Completed in 2 s |
| `memory_hotplug.online_policy=auto-movable` | Movable, with 136 MiB left in Normal | 2,386 MiB | Completed in 3 s |

- All three returned everything asked for. These are short runs on a fresh
  boot; fragmentation after long uptime is not measured (T5.4).
- With `online_movable` the kernel's own memory is confined to the 1 GiB of
  boot RAM however large the guest grows, which is why `auto-movable` is
  the remedy named and not `online_movable`.
- Reclaim started 48 to 54 seconds after the data was freed in the two runs
  where the load had stalled on memory just before; the third stalled less
  and started after 37 seconds. Reclaim waits for the ten-second average
  to return to exactly zero. Whether that is the right calm test is T2.8.
- Writing the large file caused memory stalls while available memory was at
  or above `free_ram`. In two runs the trigger fired and the guest asked for
  256 MiB it did not otherwise need: page-cache reclaim reads as pressure.
  This belongs with the cache-aware work in T2.4.
- Growth kept up with 400 MiB every five seconds, but available memory fell
  to 125–140 MiB against the 512 MiB `free_ram` between grants: each
  request took two to three seconds to complete (T3.2).

File-cache baseline, 2026-11-13 (`.artifacts/metrics/m2-t24-debian13/`):
2.5 GiB of files re-read in a loop for four minutes in the 2 GiB guest, then
90 idle seconds.

- The guest service issued no request. The guest stayed at 2 GiB and the
  kernel re-read about 1.7 GiB from disk on every pass (about 440,000 refaulted
  pages per pass, 54 million in the run).
- Nothing the policy reads saw it. `MemAvailable` stayed near 1.76 GiB
  because page cache counts as available. Memory stall time was about 2.4 %,
  under the 10 % trigger and above the zero that reclaim needs, so the
  guest neither grew nor shrank.
- `Active(file)` stayed at 35 MiB throughout. With the multi-generation LRU
  that Debian 13 enables, repeated sequential reads are not promoted, so
  `Active(file)` is not a usable measure of the file working set here.
- The refault counter (`workingset_refault_file` in `/proc/vmstat`) is the
  signal that did show it: it rose by about 225,000 pages a second while
  the loop ran and stopped the moment the loop stopped.
- Each pass still took 0.93 s, because the host caches the guest's disk.
  On storage without that, the cost would be disk reads.

The same workload with the cache floor (`floor.log` in the same directory,
guest build `6134275b136c-dirty.cdf3554aac2a`, loop of 150 seconds):

- Refaults were seen on the second pass. Three requests grew the guest from
  2.0 to 3.4 GiB in seven seconds, refaults stopped, and a pass fell from
  0.93 s to 0.20 s.
- After a quiet minute the floor was lowered by 256 MiB and the guest
  returned 238 MiB with no refaults: the working set still fitted.
- The next step a minute later removed 260 MiB and cut into the working
  set. Refaults came back within two seconds, the floor returned to its
  previous level, the guest asked for 236 MiB back, and the wait before the
  next trial doubled to two minutes. The guest held at 3.1 GiB.
- Earlier runs of the same day showed two faults since corrected: a restore
  that overshot by about 500 MiB because the removal under way kept evicting
  cache, and a 544 MiB grow-then-return caused by one second of refaults
  while the test files were written.
- Not yet measured: how fast a guest gives cache back after the workload
  stops for good, and any workload other than sequential re-reads.

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
| ~~Fixed reserves, floors and steps~~ | Core settings | Replaced by per-VM `free_ram` | T2.3 |
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
- **Policy sizes:** the fixed reserves were a Windows test artefact; a
  required per-VM `free_ram` replaces them. No boot-RAM check: `min_ram` is
  the administrator's control (2026-11-13).
- **Scope:** Debian first. Other Linux distributions later; keep the guest
  free of Debian-specific code so they remain possible.

- **Cache policy (2026-11-13):** a Linux guest follows the refault counter.
  It grows while file cache is being re-read from disk, holds what stopped
  that, and gives it back slowly. Cache activity can therefore take pool
  memory up to `max_ram` without a per-VM setting (T2.4).

Still open:

- None.

## Not planned

- Estimating guest demand on the host.
- Taking memory from one VM to give to another.
- Guests other than Windows and Linux.
- Hosts other than libvirt/KVM.
