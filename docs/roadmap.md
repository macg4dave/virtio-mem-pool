# Roadmap

Written 2026-11-13. It turns the ideas in [prior art](prior-art.md) into
work, moves Windows guests from virtio-mem to Hyper-V Dynamic Memory, and
carries over what is still open from the previous roadmap (in git history
before commit `e665205`). Task IDs are new; old IDs are given where a task
carries over.

## Goal

One host controller on a KVM host manages a shared pool of RAM for many
VMs, Windows and Linux together. Each guest decides when it needs more or
less memory and asks; the host admits, grants within pool and host limits,
resizes, and reports the actual result. Linux guests are resized with
virtio-mem. Windows guests are resized with Hyper-V Dynamic Memory.

## Where we are

| Area | State |
|---|---|
| Core policy, request protocol, wire format | Working, platform-neutral, well tested |
| Host controller (RHEL 10.2, libvirt 11.10, QEMU 10.1) | Working; manages `debian13` |
| Linux guest service | Working under the real controller; PSI triggers, cache floor, readiness report, control channel |
| Windows guest service | Built on the shared guest crate; **not deployed**. Its virtio-mem path is paused (B1) |
| `viomem` workarounds and driver patch | **Paused** and moved to [archive/viomem](../archive/viomem/) on 2026-11-13 |
| Windows on Dynamic Memory | Not started. pc101's QEMU lacks the devices (B2) |
| License | GPL-3.0-or-later since 2026-11-13 |
| Development checkout on pc101 | `~/github/virtio-mem-pool` |

## Rules

These apply to every task and to review.

1. **No site values in code or tracked defaults.** VM names, SSH aliases,
   user paths, pool sizes and device aliases belong to a site's
   configuration. The repo carries neutral examples and fixtures only.
2. **A built-in value is a product default or a setting**, defined once,
   documented and backed by evidence. A number another project uses is a
   starting point, not evidence.
3. **Workarounds are fenced and dated.** Each names the defect, the guest
   it applies to and the condition for deleting it.
4. **Linux leads.** When the guests differ, Windows follows the Linux
   design unless the Windows answer is required by Windows.
5. **Tests use fixtures**, never a tracked site file or a live VM.
6. **Nothing reaches a guest by hand.** Tooling or a package installs it
   and records what it did.
7. **Third-party code** follows the import rule in
   [prior art](prior-art.md#importing-third-party-code).

## Blockers

| ID | Blocker | Blocks | Unblocked by |
|---|---|---|---|
| B1 | `viomem` removal bug ([#1706](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1706), [#1707](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1707)) | Windows on virtio-mem (paused, not planned) | A fixed virtio-win release; see [archive/viomem](../archive/viomem/README.md#resuming) |
| B2 | RHEL's `qemu-kvm` 10.1 on pc101 has no `hv-balloon` and no `vmbus-bridge` device (checked 2026-11-13) | All of milestone W | W0.1 |
| B3 | Libvirt has no model for `hv-balloon` | W2 in production form | Command-line passthrough for now (W0.3); upstream libvirt support later |

## Order of work

1. **P1, P2**: small, evidence-backed Linux fixes.
2. **W0** in parallel: find out whether Dynamic Memory works for our
   Windows VMs at all before building anything for it.
3. **P3, P4**, then **H**, **W1–W3** if W0 succeeds.
4. **N**, **L** and the carried-over milestones as capacity allows.

## Milestone P — Prior-art quick wins

Small changes, each from [prior art](prior-art.md#ideas-to-adopt).

- [ ] **P1** Settle timeout from the Linux retry schedule (idea 1).
  Implemented 2026-11-13: a Linux removal settles after 380 s without
  movement (the driver's third retry at +350 s plus a 30 s margin,
  `GuestOs::removal_settle_seconds`); Windows keeps
  `convergence_timeout_seconds`. Remaining: record a run on `debian13`
  where a removal completes on a late retry, and confirm or replace the
  provisional 30 s margin from it. Closes the old T2.8 item
  "stalled-removal delay".
- [ ] **P3** Derived VFIO budget and block-size guidance (idea 3). For a
  VM with an assigned device, read `dma_entry_limit` and refuse a device
  whose block count exceeds its share; document a block size that keeps a
  device near 32000 blocks. Keep `vfio_mapping_budget` only if the review
  still needs it.
- [ ] **P4** Backend option checks in attestation (idea 4): `reserve=off`,
  `dump=off` and no `dump-guest-core=on`, and `prealloc=on` on the device
  for huge-page backends. Update the administration guide's XML example.

Exit: each change is in the gate, documented, and P1 has a recorded run.

## Milestone W — Windows on Hyper-V Dynamic Memory

Windows has a built-in Dynamic Memory client. QEMU's `hv-balloon` device
(QEMU 8.2 and later) talks to it over VMBus: it hot-adds memory from a
memory backend and balloons memory back out, with no driver installed in
the guest. This replaces `viomem` for Windows. See
[prior art](prior-art.md#5-hyper-v-dynamic-memory-as-a-windows-alternative), idea 5.

The guest service and protocol stay as they are: the Windows guest still
decides and asks. Only the host's way of resizing a Windows VM changes.

### W0 — Feasibility (unblocks B2)

- [ ] **W0.1** A QEMU on pc101 with `hv-balloon` and `vmbus-bridge`: an
  upstream release built with both enabled, installed beside RHEL's
  `qemu-kvm` (not replacing it), usable by libvirt as the domain's
  `<emulator>` with the right SELinux labels. Install it with a script in
  `deploy/scripts/`, not by hand.
- [ ] **W0.2** A test Windows 11 VM on that QEMU with the Hyper-V
  enlightenments VMBus needs, `vmbus-bridge`, and `hv-balloon` with a
  `memdev`. Confirm Windows loads its Dynamic Memory driver.
- [ ] **W0.3** By hand over QMP (`balloon`, `query-balloon`, the status
  report event): grow by hot-add, shrink by balloon, grow again. Record
  sizes, timings, and what Windows and the host each report. Note the 50 s
  wait after the driver connects and the ~2.2 GiB per-removal limit.
- [ ] **W0.4** Check the things that could rule it out: Windows client
  editions, reboot and Fast Startup (whether the hang seen with `viomem`,
  RHEL-150604, has a Dynamic Memory counterpart), GPU passthrough
  alongside VMBus, live migration and snapshots.
- [ ] **W0.5** Decision recorded here: go, or stop and wait on B1.

Exit: a recorded grow, shrink and regrow of a Windows VM with
`hv-balloon`, and a go/no-go decision.

### W1 — Host backend

- [ ] **W1.1** A Dynamic Memory resize backend behind the existing
  `ResizeSink` and state-source boundaries: target through QMP `balloon`,
  state from `query-balloon` (and libvirt's QMP passthrough while B3
  stands). Respect the 128 MiB alignment, the per-removal limit and the
  post-boot wait.
- [ ] **W1.2** Per-VM setting choosing the backend
  (`virtio-mem` or `dynamic-memory`), validated against the domain.
- [ ] **W1.3** Attestation for a Dynamic Memory VM: QEMU version and
  devices, `memdev` size as the ceiling, no second balloon device
  (`<memballoon model='none'/>`).
- [ ] **W1.4** Use the guest's Dynamic Memory status report (committed
  and available pages) as a host-side cross-check of the Windows guest's
  own figures, in `watch` and `metrics`.

### W2 — Windows guest

- [ ] **W2.1** Diagnostics report the Dynamic Memory driver instead of
  `viomem.sys`; readiness reports whether it is loaded.
- [ ] **W2.2** Removal step for Windows from the Dynamic Memory limit
  instead of the `viomem` step (marked PAUSED in core settings).
- [ ] **W2.3** First Windows deployment under the controller; this
  finishes the old T6.2 (control channel, host-published settings) on a
  live VM.

### W3 — Qualification and mixed fleet

- [ ] **W3.1** `xtask qualification` for a Windows VM on Dynamic Memory.
- [ ] **W3.2** Mixed Windows and Linux soak on one controller and pool
  (old T6.3).
- [ ] **W3.3** Release notes and versioning (old T6.4).

Exit: one controller, one pool, Windows on Dynamic Memory and Linux on
virtio-mem, qualified together.

## Milestone H — Host behaviour under pressure

- [ ] **H1** Host PSI as an admission input (idea 11): hold growth while
  host memory stall exceeds a threshold, even with `MemAvailable` above
  the reserve. Measure the threshold on pc101.
- [ ] **H2** Give memory back when the host is short (idea 8): the host
  publishes a "host short" state with its status, and each guest returns
  toward its floor. This answers host shortage only; it does not move
  memory between VMs, which stays not planned.
- [ ] **H3** Review damping against MOM and concierge (idea 10): compare
  our step and settle rules, change them only with evidence.
- [ ] **H4** Free page reporting for Linux guests (idea 9): document and
  test a virtio-balloon with `freePageReporting='on'` alongside
  virtio-mem, and make the headroom check correct when it is present.

## Milestone N — NUMA

- [ ] **N1** One virtio-mem device per NUMA node (idea 7): parse every
  device of a VM, split a target into block-aligned shares, and move a
  share one device cannot remove to the others. Attestation and
  checkpoint learn about several devices.
- [ ] **N2** The movable-ratio check (done in P2, `readiness::movable_ratio_warning`) per node when `auto_movable_numa_aware` is on.

## Milestone L — Linux removal preparation

- [ ] **L1** Only if qualification shows removals stalling on unreclaimed
  cache: try mem-agent's PSI-paced cold-page reclaim before a shrink
  (idea 12). mem-agent is Apache-2.0, so its code may be used under the
  import rule.

## Carried over

Still open from the previous roadmap. Confirm each one's state before
starting it.

- [ ] **C1** (T0.9) Keep or delete the legacy configuration migration in
  `host-deploy`.
- [ ] **C2** (T2.8) Replace the remaining provisional Linux numbers:
  pressure level, failed-poll limit, the cache floor's refault threshold,
  quiet period and settling time. The stalled-removal delay is P1.
- [ ] **C3** (T2.11) Decide what else must survive a fill faster than
  growth, and record a run.
- [ ] **C4** (T3.2, T3.3) Recorded run on `debian13` with the control
  port, no guest-agent file access and host-published settings.
- [ ] **C5** (T3.4) Written host/guest version compatibility table and
  transport skew rule.
- [ ] **C6** (T4.1–T4.5) Packaging and administration: Debian guest
  package, host package with configurable paths, `xtask linux deploy`
  first use, systemd sandbox review, install/upgrade/recover guide.
- [ ] **C7** (T5.1–T5.4) Linux qualification: workload run, `xtask
  qualification` for Linux, failure injection, multi-VM soak.

## Decisions

Made by the owner on 2026-11-13:

- **License:** GPL-3.0-or-later, chosen to take in the widest range of
  third-party code (see [prior art](prior-art.md#our-license)).
- **Windows:** move to Hyper-V Dynamic Memory. All `viomem` work is paused
  and archived until its driver bug is repaired.
- **Development checkout:** a git checkout on pc101 at
  `~/github/virtio-mem-pool`, for building and deploying there.

Earlier decisions still in force: the dedicated virtio-serial control
channel is the product transport; the host publishes guest limits and
policy; per-VM `free_ram` replaces fixed sizes; Linux guests follow the
refault counter for cache; Debian first.

## Not planned

- Estimating guest demand on the host.
- Taking memory from one VM to give to another.
- Guests other than Windows and Linux.
- Hosts other than libvirt/KVM.
- Windows on virtio-mem while B1 stands.
