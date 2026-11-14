# Next roadmap

Written 2026-11-14, when [the roadmap](roadmap.md) closed. That roadmap
built the `rampire` command, the packages and the fleet features. This one
holds what it left to show working on real hosts and VMs, and what the
owner still has to set up for the first release.

Every VM these tasks need is made with `cargo xtask testvm`
([tools](tools.md#test-vms)), never by hand ([AGENTS.md](../AGENTS.md)).
The packages come from a CI run started by hand (`gh workflow run CI`),
since CI no longer runs on every commit.

## Where things stand

| Area | State |
|---|---|
| Test VM tooling | `cargo xtask testvm` and its scenarios are built and unit-tested. No VM has been made with it yet: the first run was stopped while downloading its image |
| Packages | Built by CI from commit `a70a0a6` (a run on 2026-11-14), copied to pc101 in `~/scratch/dist` with their checksums |
| pc101 | Runs `rampire` build `6c2d09d`, managing `safe_vm`. About 3.6 GiB of RAM available: room for one 2 GiB test VM at a time |
| Image downloads | About 0.5 MB/s from pc101. Each image is downloaded once into `testvm-cache/` |

## Tasks

### S1: Clean hosts, starting with AlmaLinux 10

The previous roadmap's O2 exit: a clean host of each supported family goes
from nothing to a running controller, managing no VMs, with the package
manager and `rampire init`.

- [ ] **S1.1** AlmaLinux 10, the first `testvm` run:

  ```bash
  set -a; . site/testvm.env; set +a
  sudo -E target/debug/xtask testvm up tools/testvm/clean-host-almalinux10.toml \
    --name clean-alma10 --file ~/scratch/dist/rampire-VERSION-1.x86_64.rpm --apply
  ```

  It also shows `testvm` itself working: making the VM, reporting each
  step over the serial port, recording evidence and removing the VM. Fix
  whatever the first run shows in the tool, not by hand.
- [ ] **S1.2** Rocky Linux 10 (`clean-host-rocky10`).
- [ ] **S1.3** Fedora 45 (`clean-host-fedora45`).
- [ ] **S1.4** Debian 13 (`clean-host-debian13`, with the host `.deb`).
- [ ] **S1.5** Ubuntu 24.04 (`clean-host-ubuntu2404`). Ubuntu 24.04 ships
  libvirt 10.0, but `rampire doctor` requires 10.1, the first with
  `dynamicMemslots`. Expect `doctor` to fail there. The owner then decides
  whether to raise the supported Ubuntu release or to lower the minimum.
- [ ] **S1.6** Record each run's evidence path here, and the result in the
  previous roadmap's O2 status.

### S2: Adding a VM end to end

The previous roadmap's O3 exit: a VM with no virtio-mem device goes from
`vm add` to managed in one guided command.

- [ ] **S2.1** Make the guest with `linux-guest-debian13 --keep`. Then run
  `rampire vm add t-guest --fix-xml --review linux-standard --yes` on
  pc101. Check that the controller manages it (`rampire status t-guest`),
  then remove it with `testvm down t-guest --apply`.

### S3: The Ansible role on a host

The previous roadmap's O4 exit: a host's configuration applied by the
example role reports "changed" only when something changed.

- [ ] **S3.1** The `ansible-almalinux10` scenario: the role run three
  times on a clean host, against itself: first "changed", then no
  change, then "changed" after a settings change.

### S4: The parked VMs on pc101

`debian13`, `safe_vm_1` to `safe_vm_4` and `minecraft101` were parked in
O1.11. `rampire vm add VM` now takes a VM's file back from `parked/`, starts
the VM if asked, and applies, which attests it. What remains:

- [ ] **S4.1** These VMs still run the old `virtio-mem-guest` service.
  `cargo xtask linux deploy` installs the new one, but no tool removes
  the old one: the clean break deleted that code. Decide one of:
  - rebuild the VMs from their cloud image with the new guest package;
  - accept the old service, which can't reach the new port and only
    idles;
  - a one-time removal step in `xtask linux deploy`.
- [ ] **S4.2** pc101 has no SSH access to `debian13`, which `xtask linux
  deploy` needs from there.
- [ ] **S4.3** The host is short of memory: five more VMs, each wanting
  their ClamAV working set, don't fit beside the test VMs. Bring them
  back one at a time, after S1 to S3.
- [ ] **S4.4** `minecraft101` is the owner's real server. Its Java
  pre-touches a 6 GB heap, more than pc101 has free. The owner decides
  when it comes back.
- [ ] **S4.5** Replace the hand procedure in `/etc/rampire/parked/README.md`
  on pc101 with `rampire vm add VM`.

### S5: The first release (owner)

From [releasing](releasing.md#one-time-setup):

- [ ] **S5.1** The signing key, as the secret `RAMPIRE_SIGNING_KEY`.
- [ ] **S5.2** GitHub Pages with the source "GitHub Actions". The
  permission check blocked turning it on from a session.
- [ ] **S5.3** The COPR project `rampire`: chroots `epel-10-x86_64` and
  the current Fedora, internet access during builds on. Its API
  configuration goes in the secret `COPR_CONFIG`.
- [ ] **S5.4** A CI run started by hand that passes, then tag `v0.1.0`.
  Check the release, the apt repository and the COPR build.
- [ ] **S5.5** Install the host from `dnf copr enable` and the apt
  repository on a clean host (the S1 scenarios, with the repository
  instead of `--file`).

### S6: Tooling found under the new rule

Under the [AGENTS.md](../AGENTS.md) rule, tasks that are done the same way
twice get combined:

- [ ] **S6.1** `deploy/scripts/clone-vm.sh` and `xtask testvm` both make
  VMs from a cloud image and a cloud-init seed. Fold the clone into
  `testvm`, or say why the two stay apart.
- [ ] **S6.2** `cargo xtask linux package` builds its own guest `.deb`, and
  `packaging/build.sh` builds another with nfpm. Keep one.
- [ ] **S6.3** Downloading packages from a CI run to pc101 is done by
  hand (`gh run download`, then `scp`). Give it a command if it keeps
  happening.

## After this roadmap

The previous roadmap's parked items (P1, P3, P4, W, H1–H4, N1–N2, L1,
C2–C7) are still parked: see
[After this roadmap](roadmap.md#after-this-roadmap) there.
