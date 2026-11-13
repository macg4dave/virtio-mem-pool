# Prior art

Written 2026-11-13 from a survey of public projects that resize guest
memory with virtio-mem, a balloon or Hyper-V Dynamic Memory. It records
what each project does, its license and whether its code may be copied
here, and the ideas worth taking. It also sets the rule for importing
third-party code.

No surveyed project does what this one does: one host controller sharing
a pool between Windows and Linux guests, with each guest asking for
memory. No third-party code has been copied into this repository yet.

## Our license

This project is under the GNU General Public License, version 3 or (at
your option) any later version (`GPL-3.0-or-later`, see
[LICENSE](../LICENSE)). It was chosen because it can take in code under
more licenses than any other: MIT, BSD, ISC, zlib, Apache-2.0,
LGPL-2.1-or-later, GPL-2.0-or-later, GPL-3.0 and, combined under its
section 13, AGPL-3.0.

The one family it cannot take is **GPL-2.0-only** (QEMU's `virtio-mem.c`,
Linux `hv_balloon.c`, oVirt MOM, oomd). No license can take both that and
Apache-2.0. Apache-2.0 matters more here: most reusable Rust code (Kata,
Cloud Hypervisor, mem-agent, many crates) uses it. GPL-2.0-only projects
remain useful for their ideas.

All current Rust dependencies are under MIT, Apache-2.0, BSD-style, zlib,
Unicode or Unlicense terms, all compatible.

## Sources

"Copy code" says whether this project may include the source's code,
following [Importing third-party code](#importing-third-party-code).
"Ideas only" means the code must not be copied or translated. Re-check a
license before copying: it applies to the version copied, not to this
survey, and some projects license file by file.

| Project | What it is | License | Copy code |
|---|---|---|---|
| [Linux `virtio_mem.c`](https://github.com/torvalds/linux/blob/master/drivers/virtio/virtio_mem.c) | The Linux guest driver | GPL-2.0-or-later | Yes |
| [QEMU `hv-balloon.c`](https://gitlab.com/qemu-project/qemu/-/blob/master/hw/hyperv/hv-balloon.c) | Hyper-V Dynamic Memory device for Windows guests | GPL-2.0-or-later | Yes |
| [QEMU `virtio-mem.c`](https://gitlab.com/qemu-project/qemu/-/blob/master/hw/virtio/virtio-mem.c) | The virtio-mem device | GPL-2.0-only | Ideas only |
| [Linux `hv_balloon.c`](https://github.com/torvalds/linux/blob/master/drivers/hv/hv_balloon.c) | Linux guest side of Hyper-V Dynamic Memory | GPL-2.0-only | Ideas only |
| [virtio-win/kvm-guest-drivers-windows](https://github.com/virtio-win/kvm-guest-drivers-windows) (`viomem`) | The Windows virtio-mem driver | BSD-3-Clause | Yes. [archive/viomem](../archive/viomem/) modifies it and keeps its license |
| [libvirt](https://gitlab.com/libvirt/libvirt) | VM management; virtio-mem `<memory>` model and size-change event | LGPL-2.1-or-later | Yes |
| [systemd-oomd](https://github.com/systemd/systemd/tree/main/src/oom) | PSI-driven OOM handling | LGPL-2.1-or-later | Yes |
| [Proxmox `qemu-server`](https://git.proxmox.com/?p=qemu-server.git) | Proxmox VE; one virtio-mem device per NUMA node | AGPL-3.0 | Yes; the copied part stays AGPL-3.0 |
| [ChromiumOS concierge `balloon_policy.cc`](https://chromium.googlesource.com/chromiumos/platform2/+/HEAD/vm_tools/concierge/balloon_policy.cc) | Host balloon policy balancing guest and host headroom | BSD-3-Clause | Yes |
| [mem-agent](https://gitee.com/teawater/mem-agent) (used by Kata Containers) | Rust guest agent: PSI-paced cold-page reclaim and compaction | Apache-2.0 | Yes |
| [Kata Containers](https://github.com/kata-containers/kata-containers) | Container VM runtime; resizes virtio-mem with `qom-set` | Apache-2.0 | Yes |
| [KubeVirt](https://github.com/kubevirt/kubevirt) | Kubernetes VMs; memory hotplug for Linux and Windows | Apache-2.0 | Yes |
| [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) | VMM with virtio-mem zones and a resize API | Apache-2.0 / BSD-3-Clause (per file) | Yes |
| [oVirt MOM](https://github.com/oVirt/mom) | Host balloon and KSM policy engine | GPL-2.0-only | Ideas only |
| [oomd](https://github.com/facebookincubator/oomd), senpai | PSI-driven OOM handling and memory sizing | GPL-2.0-only (oomd); GPL-2.0 (senpai, version not checked) | Ideas only |
| [vmrd RFC](https://lore.kernel.org/linux-mm/cover.1690836010.git.quic_sudaraja@quicinc.com/T/) (Qualcomm, 2023) | Guest daemon that plugs memory on PSI events (Gunyah) | Not stated | Ideas only |
| [memlinkd](https://gitcode.com/openeuler/memlinkd) (openEuler) | Host balloon auto-reclaim daemon | Not stated | Ideas only |
| [instavm/tarit PR #92](https://github.com/instavm/tarit/pull/92) | VMM virtio-mem resize with live test results | Not confirmed | Ideas only |
| [Hyper-V Dynamic Memory](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/dynamic-memory) | Microsoft's design: buffer and weight | Documentation | Ideas only |
| [virtio-mem user guide](https://virtio-mem.gitlab.io/user-guide/), [kernel memory-hotplug guide](https://docs.kernel.org/admin-guide/mm/memory-hotplug.html) | Operational guidance | Documentation | Paraphrase only |

## Already in this project

The survey confirms these choices; nothing to change.

- **PSI triggers for growth.** vmrd and mem-agent also watch PSI. Ours
  registers a kernel trigger rather than polling
  ([guest-linux/src/pressure.rs](../guest-linux/src/pressure.rs)).
- **Online policy check.** The tarit live tests show a shrink to zero left
  256 MiB plugged with `memhp_default_state=online`, and completed in a
  second with movable onlining. The Linux readiness report already warns
  about this ([guest-linux/src/readiness.rs](../guest-linux/src/readiness.rs)).
- **Size-change events.** The host follows libvirt's
  `memory-device-size-change` rather than polling alone.
- **Device attestation.** `dynamic-memslots` and `unplugged-inaccessible`
  are checked from QEMU, as the user guide requires.
- **Priority between VMs.** Like Hyper-V's memory weight, `priority`
  decides who is served first from free pool memory.

## Ideas to adopt

Ordered by value. Each is scheduled in [the roadmap](roadmap.md)
(milestones P, W, H, N and L). Numbers quoted from other projects are theirs; ours need our
own evidence.

### 1. Match the settle timeout to the Linux retry schedule

Source: Linux `virtio_mem.c`. When a removal finds memory busy, the
driver retries after 50 s, then doubles the wait each time up to 300 s
(`VIRTIO_MEM_RETRY_TIMER_MIN_MS`, `_MAX_MS`); any progress resets it to
50 s. After a stall the retries come at +50 s, +150 s, +350 s, then every
300 s.

`convergence_timeout_seconds` defaults to 300 s without movement, so a
Linux removal is settled after only two retries, just before the third.
This is the evidence the roadmap's "settle delay" item asks for: a Linux
value should cover the retries the controller is willing to wait for
(for example just over 350 s for three), and be a Linux default, not the
Windows one.

### 2. Check the movable ratio on Linux guests

Source: kernel memory-hotplug guide. With `online_policy=auto-movable`
the kernel puts hot-plugged memory in `ZONE_MOVABLE`, where it can be
removed reliably, only up to `auto_movable_ratio` (default 301 %) of the
memory in kernel zones (also per NUMA node unless
`auto_movable_numa_aware=N`). Above that, plugged memory lands in
`ZONE_NORMAL` and may never come back.

The readiness report checks the policy but not the ratio. It can compare
the device's maximum with ratio × boot memory and warn, and the
administration guide can say how large a device a given boot size
supports (about three times boot RAM at the default).

### 3. Derive the VFIO mapping budget and block size

Sources: virtio-mem user guide, Proxmox. Each plugged block uses one VFIO
DMA mapping. The kernel's limit (`dma_entry_limit` in
`/sys/module/vfio_iommu_type1/parameters/`, about 64k by default) is
shared with other devices; the guide budgets about half for virtio-mem.
At 2 MiB blocks, 256 GiB needs a limit of at least 163840. Proxmox sizes
blocks to keep a device at about 32000 blocks: the device size divided by
32000, at least 2 MiB, rounded up to a power of two.

Today `vfio_mapping_budget` is a value an administrator enters in the
compatibility review. For a VM with an assigned device, the host can read
the limit and refuse a device with too many blocks, and the guide can
recommend a block size from the device size, instead of a plug failing
part way through.

### 4. Check the memory backend options

Sources: virtio-mem user guide, Proxmox (which does the first and third).
Attestation could also require:

- `reserve=off` on the backend, so huge-page reservation cannot fail;
- `dump=off` on the backend and no `dump-guest-core=on`, so a core dump
  does not touch the whole region;
- `prealloc=on` on the device (not the backend) for huge-page backends, so
  a plug the host cannot back is refused cleanly and retried, rather than
  the VM crashing.

### 5. Hyper-V Dynamic Memory as a Windows alternative

Source: QEMU `hv-balloon.c`. Windows has a built-in Hyper-V Dynamic
Memory client: with QEMU's `hv-balloon` device it can hot-add memory and
balloon it back out, with no virtio driver installed. The Windows guest
also sends a status report (committed and available pages), which QEMU
can emit as a QMP event. That is a Windows-native demand signal.

Our Windows guest is paused on the `viomem` removal defect, so this is
worth an experiment. Points from the QEMU source:

- QEMU waits 50 s after the guest's driver connects before sending
  requests, because some Windows versions crash on a hot-add request
  straight after boot. Hyper-V does the same.
- Windows returns at most 585728 pages (about 2.2 GiB) per hot-remove
  response and ignores larger requests. Compare our 4 GiB removal cap in
  [archive/viomem](../archive/viomem/).
- Hot-add is done in 2 GiB chunks with 128 MiB memslot alignment.

Libvirt does not model `hv-balloon`; it would need QEMU command-line
passthrough, and our host's resize path would need a second backend.

### 6. Windows Fast Startup during removal

Source: Red Hat issue
[RHEL-150604](https://issues.redhat.com/browse/RHEL-150604). A `viomem`
hot-remove can hang a Windows 11 24H2 hybrid shutdown when Fast Startup
is on. The details could not be read during the survey; confirm them
before acting. Candidate responses: report `HiberbootEnabled` in the
Windows readiness facts, document turning Fast Startup off, and have the
service withdraw a pending shrink on pre-shutdown.

### 7. One device per NUMA node

Source: Proxmox. Proxmox gives a VM one virtio-mem device per NUMA node,
as the user guide recommends, and splits a target across them in
block-aligned shares, the last device taking the remainder. A later patch
moves a share that one device cannot remove to the others. Our
controller manages exactly one device per VM, which suits VMs without
NUMA but not large multi-socket guests.

### 8. Give memory back when the host is short

Sources: ChromiumOS concierge, oVirt MOM, Hyper-V. Today the host only
refuses or reduces growth when `MemAvailable` would cross
`host_min_headroom_bytes`. It never asks a guest to return memory. All
three do:

- concierge moves each VM toward the point where guest headroom and host
  headroom are equal (half the difference per step);
- MOM shrinks every guest under host pressure, keeps each guest a free
  allowance that shrinks as host pressure becomes severe, and deflates
  slowly once pressure passes;
- Hyper-V drops each VM's buffer when the host cannot honour all of them,
  and weighs a VM's pressure against its weight, so a VM in more need can
  be served before one with higher weight.

Here the guest owns its request, so the host would publish a "host short"
state with its status and each guest would give back toward its floor.
How to share the shortfall between guests is a policy decision for core.

### 9. Free page reporting on Linux guests

Source: virtio-mem user guide. A balloon cannot take back virtio-mem
memory, but free page reporting works alongside it: a Linux guest with a
virtio-balloon device and `freePageReporting='on'` hands free pages
inside its plugged memory back to the host between resizes. That gives
the host memory back sooner than a shrink and with no policy. Host
`MemAvailable` then rises by what guests are not using, which the
headroom check would need to account for.

### 10. Damp small and large changes

Sources: oVirt MOM, ChromiumOS concierge. MOM skips changes below 0.25 %
of guest memory and caps one change at 5 %. Concierge ignores changes
under 1 % of the remaining headroom. Compare with our step and settle
rules before adding anything.

### 11. Host PSI as an admission input

Sources: oomd, senpai, systemd-oomd. Host `MemAvailable` can look fine
while the host is already stalling (for example on swap or a cgroup
limit). Holding growth while host `/proc/pressure/memory` shows stall
would catch that.

### 12. Prepare a Linux guest for removal

Source: mem-agent. It reclaims cold pages with MGLRU and runs compaction,
pausing whenever PSI shows stall. Linux virtio-mem already migrates pages
out of a block it removes, so this is worth trying only if qualification
shows removals stalling on unreclaimed cache.

## Importing third-party code

Before copying any code, file or substantial snippet:

1. **Check the license** at the version you copy, in the file's own header
   first. Copy only code under MIT, BSD-2/3-Clause, ISC, zlib,
   Apache-2.0, LGPL-2.1-or-later, GPL-2.0-or-later, GPL-3.0 or AGPL-3.0.
   Never copy GPL-2.0-only code or code with no stated license; implement
   from the specification or documentation instead.
2. **Keep the original notices.** Leave the source file's copyright and
   license header in place. Add our own copyright line under it, and an
   `SPDX-License-Identifier` line naming the original license.
3. **Add the license text** as `LICENSES/<SPDX-id>.txt` if not already
   there.
4. **Record it** in `THIRD_PARTY_NOTICES.md` at the root: project, URL,
   commit, license, copyright holders, and which of our files contain it.
5. **Apache-2.0:** copy the project's `NOTICE` text (if any) into
   `THIRD_PARTY_NOTICES.md`, and mark each modified file with a line saying
   it was changed.
6. **AGPL-3.0:** keep the copied code in its own files; those files stay
   AGPL-3.0. The host daemon is not offered to users over a network, so
   AGPL's extra condition does not arise in normal use, but check before
   copying.
7. **GPL-2.0-or-later code** becomes GPL-3.0-or-later in our combined work;
   keep its own header unchanged.
8. **Rust dependencies** need no copying; `cargo` records them. Check each
   new crate's license against step 1.
