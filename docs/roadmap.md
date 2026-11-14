# Roadmap

Written 2026-11-14. It replaces the previous roadmap, which is in git
history up to commit `08e54a6`. This roadmap covers one thing: making the
project, renamed **RAMpire**, simple to install and run. That means simple
for a hobbyist with one host, and simple to script and automate for a
large fleet. Work the previous roadmap had open is parked in
[After this roadmap](#after-this-roadmap).

Nothing here starts until this roadmap has been reviewed.

## Goal

A person installs RAMpire with their package manager and manages it with
one command. A fleet drives the same command from configuration
management. Neither needs a source checkout, a Rust toolchain, an instance
name or a hand-written JSON file.

The safety model stays exactly as it is: attestation, fail-stop, latch,
durable state and evidence. This roadmap automates it; it doesn't loosen it.

## Where we are

| Area | State |
|---|---|
| Controller, Linux guest service, protocol | Working and well tested; see [administration](administration.md) |
| Installs | Development and test only. None needs migrating (decision D9) |
| Host install | From a checkout: `deploy/scripts/install-host.sh` builds with cargo and runs `cargo xtask host-deploy` |
| Guest install | Linux: a `.deb` built by `cargo xtask linux package`, or `cargo xtask linux deploy` over SSH. Windows: `make windows-deploy` (Windows guests are paused) |
| Operator tools | Split across `deploy/scripts/`, `cargo xtask` and `virtio-mem-host status` / `clear-latch` |
| Packages and releases | Guest `.deb` only, built locally. No host package, no CI workflow, no releases |
| Tested host | RHEL 10 family, one host |

## Problems found

From a review of [usage](usage.md), [administration](administration.md),
`deploy/scripts/`, `tools/xtask/` and the roadmap itself.

| # | Problem | Who it hurts | Where |
|---|---|---|---|
| X1 | Installing or changing the host needs a checkout and Rust. Every settings change rebuilds from source | Everyone | `install-host.sh` |
| X2 | The controller is stopped before the configuration is checked, and a mistake leaves it stopped | Everyone | `install-host.sh`; `host-deploy` requires the unit inactive |
| X3 | Operator work is split across three tools, and `xtask` runs only inside a checkout | Everyone | `tools/xtask/src/main.rs` (`repo_root`) |
| X4 | Users must find and type an instance name, though the rule is one controller per host | Hobbyist | `virtio-mem-host@.service` |
| X5 | Adding a VM takes six manual steps, including hand-edited domain XML and a JSON review of eight confirmations most users can't judge | Hobbyist | [Adding a VM](administration.md#adding-a-vm) |
| X6 | Operator commands require development flags (`--command-timeout-seconds`, `--output`) and write evidence into the checkout | Fleet | `host-deploy`, `attestation` |
| X7 | There's no host package, no automatic service account and no release build | Everyone | — |
| X8 | The unit's `SupplementaryGroups=qemu` exists only on RHEL-family hosts. Debian and Ubuntu use `libvirt-qemu` | Debian/Ubuntu hosts | `host/systemd/virtio-mem-host@.service` |
| X9 | Each VM is spread over `host_list.toml` and its own settings file, with `vm_name` in both, so adding a VM means editing a shared file | Fleet | configuration layout |
| X10 | Nothing gives automation machine-readable status, metrics or a documented exit-code contract | Fleet | — |
| X11 | The name `virtio-mem` describes one resize mechanism. Windows guests are moving to Hyper-V Dynamic Memory, so the name no longer fits the product | Everyone | project, binaries, units, paths, packages, wire names |
| X12 | With packages, settings would be edited in place in `/etc`. A restart or a reboot would then run edits nobody checked | Everyone | the daemon reads its configuration directly |
| X13 | The controller refuses a configuration with no VMs, so a freshly installed host can't start | Everyone | `crates/virtio-mem-core/src/settings.rs` |
| X14 | Nothing says which hosts and guests are supported, how releases are numbered, or which controller serves which guests | Everyone | — |
| X15 | Nobody has defined what happens when a VM leaves management, when a managed VM disappears from libvirt, or when the host package is removed | Everyone | — |
| X16 | The unit is ordered after `libvirtd.service`, but RHEL 10 and recent Debian run libvirt's split daemons (`virtqemud`), so the controller can start before libvirt | Everyone | the unit's `After=` |
| X17 | Setting names such as `pool_bytes = "16GB"` end in `_bytes` but take values with a unit | Everyone | `global-settings.toml` |
| X18 | Evidence and deployment backups are never pruned | Long-running hosts | `/var/lib/virtio-mem-host/` |

## Principles

1. **One command for operators.** Everything an operator does goes
   through the installed `rampire` command. `xtask` is for development
   only: gate, qualification and Windows builds.
2. **Check before changing.** No command stops the controller or
   changes what it runs until the new configuration has passed every
   check, including live attestation. The controller only ever runs a
   configuration that `rampire apply` has checked.
3. **The same command works for a person and for a script.** On a
   terminal it explains, shows diffs and asks. With `--yes`, or without a
   terminal, it never prompts. `--json` gives a stable, versioned schema.
   Exit codes are documented.
4. **Defaults for everything that has a sensible one.** Evidence is
   still written, to a fixed place on the host, without anyone asking.
5. **Install with the package manager.** A checkout stays the
   development route, not the user's route.
6. **Clean break.** Only test installs exist, so nothing is migrated.
   Old installs are removed and installed again under the new names, and
   code kept only for old names, layouts or versions is deleted.

These rules from the previous roadmap still apply:

- **No site values in code or tracked defaults.** VM names, SSH aliases,
  user paths, pool sizes and device aliases belong to a site's
  configuration.
- **A built-in value is a product default or a setting,** defined once,
  documented and backed by evidence.
- **Tests use fixtures,** never a tracked site file or a live VM.
- **Nothing reaches a guest by hand.** Tooling or a package installs it
  and records what it did.
- **Third-party code** is copied only under the
  [import rule](#importing-third-party-code) below.

## Target shape

What a user sees once this roadmap is done. The names are fixed in R1.

```text
dnf install rampire            # or: apt install rampire
sudo rampire init              # detect host memory and VMs, write global settings
sudo rampire vm add webserver  # check and fix the domain, review, attest, add
sudo rampire status            # controller and every VM
sudo rampire top               # the live pool view
sudo rampire set webserver priority=150 max_ram=12GB
sudo rampire diff              # what an apply would change
sudo rampire apply             # check, then switch; a bad config changes nothing
```

```text
/etc/rampire/                  # what people and tools edit
  global.toml
  vms.d/webserver.toml         # one VM, one file
  reviews/linux-standard.toml  # a reusable review profile
/var/lib/rampire/
  applied/                     # what the controller runs: written only by apply
  attestations/                # managed by rampire
  evidence/                    # what each apply did
  state/                       # durable request state
```

Every command takes `--json` and `--yes`. `apply --detailed-exitcode`
exits 0 when nothing changed, 2 when something changed and 1 on failure.
Every command runs as root (decision D14).

## Supported platforms

For the first release (decision D12):

| Side | Supported | Why the line is there |
|---|---|---|
| Host | RHEL, AlmaLinux and Rocky Linux 10; current Fedora; Debian 13; Ubuntu 24.04 and later | The device needs QEMU 8.2 or later (`dynamic-memslots`) |
| Linux guest | Any distribution with systemd 253 or later: Debian 13, Ubuntu 24.04 and later, RHEL 10, current Fedora | The unit opens the control channel with `OpenFile=` |
| Windows guest | Not supported while Windows work is parked | — |

## Versions and releases

Decision D15:

- Releases follow semantic versioning, starting at **0.1.0** at the end
  of O2.
- **Upgrade the controller first, then the guests.** A controller serves
  guests from its own minor release and the one before it. A test built
  from fixtures proves it for each release.
- Packages are signed. Each release has an entry in `CHANGELOG.md`.
- Each wire and file schema carries a version, starting at 1 (R3).

## Milestones

### R — Clean break: RAMpire

Everything that changes a name, a file format or a wire identifier
happens here, once, before the command is built on top of it.

- [x] **R0** Check the name is free before anything is renamed, and
  record the result here:
  - crates.io;
  - Debian and Fedora package names;
  - the GitHub repository name;
  - a trademark search.

  Checked 2026-11-14:

  | Where | Result |
  |---|---|
  | crates.io | Free: no crate `rampire` |
  | Debian and Ubuntu | Free: no source or binary package |
  | Fedora and COPR | Free: no package or COPR project |
  | PyPI, npm | Free |
  | GitHub `rampire` account | Taken by an unrelated personal account (since 2013), so `github.com/rampire` isn't available |
  | GitHub repositories | `jufasa23/RAMpire` is a Windows RAM optimiser (2025, small, inactive). `hrcoolis/rampire` is a Docker wrapper for a DDoS tool (2023, inactive) |
  | Web | No product called RAMpire found |
  | Trademark | No mark found by web search. Not yet checked in the USPTO, EUIPO or UKIPO registers directly |

  The owner accepted the name on 2026-11-14 on these results.
- [x] **R1** A names table, reviewed before any code changes, covering:
  - the project and repository;
  - the crate layout, including the new operations library from O1.1, so
    crates are renamed and restructured once;
  - the one binary (D7);
  - the host and guest units, packages, paths and service account;
  - the polkit rule, the Windows service and environment variables;
  - the control channel's port (for example `org.rampire.control.0`)
    and every schema name.

  Where `virtio-mem` names the kernel and QEMU device itself (the device
  model, its alias, the driver), it stays: that is the device's real
  name, not ours.

  Done: [names](names.md), reviewed 2026-11-14.
- [x] **R2** Rename and restructure the workspace following R1: crates,
  binary, units, paths and environment variables. The host unit is
  `rampire@.service` until O1.8 removes the instance.
- [x] **R3** Rename the wire identifiers (decision D8): the control
  channel port in the guest unit, the controller and the documented
  domain XML, and the schema names in publications, host status and
  attestations. Each schema version restarts at 1. Done as listed in
  [names](names.md#wire-identifiers-r3-decision-d8). The publication
  readers for older versions went with it.
- [x] **R4** The configuration layout (decision D5):
  - `global.toml`, and `vms.d/NAME.toml` holding one VM's alias,
    priority, limits and review profile;
  - the file name is the VM's name, so the name isn't repeated inside;
  - setting names lose their `_bytes` suffixes: `pool_bytes` becomes
    `pool`, and `host_min_headroom_bytes` becomes `host_reserve`
    (decision D17, X17);
  - an empty `vms.d/` is valid: the controller runs, reports that it
    manages no VMs, and `status` says how to add one (decision D13,
    X13).

  `host_list.toml` and `NAME_settings.toml` are no longer read.
  `--config` names the configuration directory. Only `vms.d/*.toml` files
  are VM settings, so editor backups and `.rpmnew` files are ignored.
  Attestations stay in `/etc/rampire/attestations/` until the applied
  copy in O1.4 gives them their place under `/var/lib/rampire`.
- [x] **R5** The review profile format: `reviews/NAME.toml`, replacing the
  JSON review. Each answer records its source, detected from the domain
  or confirmed by a person, so O3.2 can fill it in. Done: each answer is
  `{ value = ..., source = "detected" | "confirmed" }`, and
  `workload_reviewed` must be confirmed. `guest_driver_version` is still
  per guest, so until O3.2 detects it a Linux profile serves VMs on one
  kernel release.
- [x] **R6** Delete the compatibility code the clean break leaves unused:
  - ~~`host-deploy`'s move of attestations from the old layout~~ (done in R4);
  - ~~readers for publication versions older than current~~ (done in R3);
  - ~~the `windows_driver_version` review alias~~ (done in R5);
  - ~~the compatibility tables in [administration](administration.md)~~
    (done in R7), and an old serde alias in qualification records.
- [x] **R7** The README, documentation and repository name. Remove the
  links to the deleted `prior-art.md`. Renaming the GitHub repository is
  done by the owner, and is still to do. O5 rewrites the guides around
  `rampire`; R7 only brings them up to date.

Exit: the workspace builds and passes the gate under the new names with
the new layout and formats, and no code reads an old name, layout or
version.

### O1 — The `rampire` command

- [x] **O1.1** Move the operator views (`pool`, `watch`, controller
  status) out of `xtask` into the operations library fixed in R1, with no
  dependence on a checkout. `xtask` keeps thin wrappers for development.
  Done: `pool` and `watch` are in `rampire-ops`, and the host package is
  split into the `rampire-host` library and the `rampire` binary. The
  controller status view is `rampire status` itself; `xtask
  controller-status`, which records evidence from a checkout build, stays
  a development tool.
- [x] **O1.2** One binary (decision D7), with its command line defined in
  clap, so `--help`, shell completion and man pages come from one
  definition (decision D16). The unit runs `rampire daemon`. People run
  `status`, `top`, `check`, `diff`, `apply`, `logs`, `doctor` and
  `recover`. `check` validates the settings and every attestation
  against the live VMs, and changes nothing.

  Done for `daemon`, `status`, `top`, `watch`, `check`, `logs` and
  `recover` (which replaces `recover-latch.sh`). `diff` and `apply` come
  with O1.4, `doctor` with O1.9. Completions and man pages are generated
  from the same definition when the packages are built (O2).
- [x] **O1.3** The contract shared by every command (X6):
  - `--json`, `--yes`, and no prompts without a terminal;
  - documented exit codes, including `apply --detailed-exitcode`;
  - a default for every timeout;
  - `--version`, matching the build identity.

  Every command runs as root (decision D14).

  Done for the commands O1.2 built; `apply --detailed-exitcode` comes
  with `apply` in O1.4. The contract is in
  [administration](administration.md#the-rampire-command).
- [x] **O1.4** The applied configuration (decision D10, X12):
  - `/etc/rampire` is the draft that people and tools edit;
  - `apply` checks the draft while the controller keeps running (X2),
    then writes it to `/var/lib/rampire/applied/` and restarts the
    controller;
  - the daemon reads only the applied copy;
  - `diff` shows the draft against the applied copy;
  - `rollback` returns to the previous applied copy, through the same
    checks.

  Done. Each applied copy holds `global.toml`, the VM files, their
  attestations and the review profiles they name; attestations still come
  from the draft's `attestations/` until O3 makes them.
- [x] **O1.5** Evidence: each apply records what it did under
  `/var/lib/rampire/evidence/`. A retention count, a setting with a
  documented default, prunes old evidence and old applied copies (X18). Done:
  `retention` (default 10, at least 2) never removes the current copy or
  the one before it. `host-deploy`'s deployment backups go when it does.
- [x] **O1.6** `set VM KEY=VALUE ...` and `set global KEY=VALUE ...`
  change a setting in the draft with the file's comments kept, then
  `apply`. Done; `--no-apply` edits only. `global` means
  `global.toml`, so `global` is a reserved VM name (judgment call,
  2026-11-14).
- [x] **O1.7** `init`: on a first run, read the host's memory and every
  libvirt domain, and write `global.toml`. It suggests `host_reserve`,
  and suggests `pool` as the host's memory minus:
  - the reserve;
  - each managed VM's boot RAM;
  - all the memory of every VM RAMpire won't manage.

  Done. Judgment calls (2026-11-14): until `vm add` (O3) exists, a domain
  with a virtio-mem device counts as managed and sets aside its boot RAM
  (its NUMA cells); every other domain, running or not, sets aside all its
  `<memory>`. `host_reserve` is an eighth of the host, 2 to 8 GiB. An
  existing `global.toml` is never changed: the suggestion is shown. `init`
  then applies, so a new host's controller can start with no VMs.
- [x] **O1.8** One unit, `rampire.service`, with no `@INSTANCE`
  template (decision D2, X4). Because every command runs as root, the
  runtime directory holding guest publications becomes private to the
  service account and root. Done.
- [x] **O1.9** `doctor` checks:
  - `virsh` and the polkit rule;
  - the service account and its libvirt socket group;
  - QEMU 8.2 or later, with the device options attestation needs;
  - the libvirt version.

  It gives the fix for each problem it finds. Done; it also checks that a
  configuration has been applied. The minimums are libvirt 10.1 (the first
  with `dynamicMemslots`) and QEMU 8.2.
- [x] **O1.10** Delete `install-host.sh`, `recover-latch.sh`,
  `restart-vm-attest.sh` and `add-linux-vm.sh`. `clone-vm.sh` stays a
  development tool. Done; a development host installs with
  `xtask host-deploy` and `rampire apply`
  ([development](development.md#installing-a-development-build)). Adding a
  VM and re-attesting after a cold restart are by hand until `vm add` and
  `vm attest --restart` (O3).
- [x] **O1.11** Remove the old installs from the development host and
  the test VMs, and install them again with `rampire`. The VMs get the
  new control channel port, are attested again and are restarted cold.
  Done on pc101 on 2026-11-14: `rampire.service` manages `safe_vm` (new
  guest service, new port, cold restart, attested); `doctor` passes all
  seven checks. The VMs that were shut off (`debian13`, `safe_vm_1` to
  `safe_vm_4`, `minecraft101`) have the new port in their saved
  definitions and converted settings in `/etc/rampire/parked/`, to be
  attested when next started (judgment call: they were off, on a host
  short of memory). The old install is backed up in
  `~/scratch/old-install-20261114` on pc101.

Exit: on the development host, changing a priority, viewing the pool and
recovering a latch each take one command, with no build and no instance
name. A typo in a settings file leaves the controller running on its
last applied configuration, through a restart and through a reboot.

Met on 2026-11-14 (`rampire set safe_vm priority=120`, `rampire top`,
`rampire recover`; a typo was refused by `apply`, and the controller ran
its last applied copy through a restart). A reboot follows from the same
unit; it wasn't tried on the shared host.

### O2 — Packages and releases

- [x] **O2.1** A CI workflow that runs the Linux gate on every push and,
  on a tag, builds signed release artefacts with checksums.
- [x] **O2.2** A host RPM containing:
  - the binary, in `/usr/bin`;
  - the unit and the polkit rule;
  - `sysusers.d` for the service account;
  - example configuration marked `noreplace`.

  The unit's libvirt socket group is the RHEL family's `qemu`.
- [x] **O2.3** A host `.deb` with the same contents, with the
  `libvirt-qemu` group (decision D3, X8).
- [x] **O2.4** The unit is ordered after libvirt whichever daemon runs
  it: `libvirtd.service`, or `virtqemud.service` and its socket (X16).
- [x] **O2.5** Removing the host package (X15):
  - it stops the controller, and each VM keeps the memory it has;
  - it keeps `/etc/rampire` and `/var/lib/rampire`;
  - purging removes both.
- [x] **O2.6** A guest RPM beside the guest `.deb`.
- [ ] **O2.7** Signed package repositories (COPR, and an apt
  repository), so installs and upgrades go through `dnf` and `apt`.
- [x] **O2.8** A static (musl) tarball for hosts without a package.
- [x] **O2.9** `CHANGELOG.md`, and the version-skew test from
  [Versions and releases](#versions-and-releases).

Exit: release 0.1.0. A clean host of each supported family goes from
nothing to a running controller, managing no VMs yet, with the package
manager and `rampire init`. A guest installs with one package command.

Status, 2026-11-14: everything but O2.7 is built. Packages are made with
nfpm from `packaging/` by `packaging/build.sh`; CI builds and lists them
on every push and, on a `v*` tag, signs `SHA256SUMS` with cosign (keyless)
and publishes a GitHub release. The binary is `/usr/bin/rampire` for
development installs too. Left for the owner: O2.7 (a COPR project, an
apt repository and their signing keys), signing the packages themselves
with that key, tagging `v0.1.0`, and installing on a clean host of each
family.

### O3 — Adding and removing VMs

- [ ] **O3.1** `vm add NAME` reads the domain and reports a missing or
  wrongly configured virtio-mem device or control channel. With
  `--fix-xml` it shows the exact change, applies it only after
  confirmation (or `--yes`), keeps the old definition, and says when a
  cold restart is needed (decision D4).
- [ ] **O3.2** The review is filled in from the domain XML wherever the
  XML can show it: `hostdev`, vDPA and vhost-user interfaces,
  `launchSecurity`, `memoryBacking/locked`, `memballoon`. The user is
  asked only what the host can't see.
- [ ] **O3.3** `vm add NAME --review PROFILE` reuses a reviewed profile
  for every VM of a kind.
- [ ] **O3.4** Default settings from the domain:
  - `min_ram` from boot RAM;
  - `max_ram` from the lower of the device ceiling and, for Linux, about
    four times boot RAM;
  - `free_ram` 512 MB.

  Each can be overridden on the command line.
- [x] **O3.5** `apply` attests a VM that has no attestation yet, when
  its file names a review profile and the VM is running. A VM whose live
  evidence has drifted still needs a person: `vm attest NAME`
  (decision D11).
- [x] **O3.6** `vm attest NAME [--restart]`, and `vm attest --drifted`
  for every VM whose live evidence no longer matches.
- [ ] **O3.7** `vm remove NAME` stops managing the VM at once. The VM
  keeps the memory it has until its next cold start, which begins on boot
  RAM alone (decision D18).
- [x] **O3.8** A managed VM that libvirt no longer knows fails `check`,
  which names `vm remove NAME` as the fix (X15).

Exit: a VM with no virtio-mem device goes from `vm add` to managed in one
guided command, and a fleet adds a VM by writing one file and running
`apply`.

### O4 — Fleets

- [ ] **O4.1** Document every `--json` schema with a version, and test
  each one against fixtures.
- [ ] **O4.2** Metrics as a Prometheus textfile
  (`/run/rampire/metrics.prom`), so there's no listening socket and the
  sandbox doesn't change.
- [ ] **O4.3** Stable, named journal fields for every decision.
- [ ] **O4.4** An example Ansible role (`rampire check`, then
  `rampire apply --yes --detailed-exitcode`) and a cloud-init snippet for
  guests.
- [ ] **O4.5** `systemctl reload rampire`: priority, limit and pool
  changes take effect without a restart. Adding or removing a VM may
  still restart.

Exit: a host's configuration kept in git and applied by the example role
reports "changed" only when something changed.

### O5 — Documentation

- [ ] **O5.1** Rewrite the guides around `rampire`:
  - Quick start: about ten minutes from install to a managed VM;
  - Everyday use: today's [usage](usage.md);
  - Reference: every setting and command;
  - Running a fleet.

### Later, optional

- A Cockpit plugin built on `rampire --json`, as a graphical front end
  for small sites.

## Order of work

1. **R0, R1**: the name check, then the names table, reviewed.
2. **R2–R7**: the clean break.
3. **O1**: the command, ending with the development host and test VMs
   installed again (O1.11).
4. **O2**: packages, and release 0.1.0.
5. **O3**: adding and removing VMs.
6. **O4**: fleet features.
7. **O5**: documentation, finished after each milestone above changes
   what users do.

## Decisions

Made by the owner on 2026-11-14:

| ID | Decision |
|---|---|
| D1 | The project is renamed **RAMpire**. The operator command is `rampire` |
| D2 | One unit per host, replacing the `@INSTANCE` template |
| D3 | Host packages for the RHEL family and Fedora, and for Debian and Ubuntu |
| D4 | `vm add --fix-xml` may edit a domain's definition, only after showing the change and confirming |
| D5 | Configuration moves to `vms.d/`, one file per VM |
| D6 | Proxmox, which doesn't use libvirt, is out of scope |
| D7 | One binary: the daemon is `rampire daemon` |
| D8 | Rename everything, wire identifiers included: the control channel port and the schema names |
| D9 | No migration. Existing installs are test installs and are installed again |
| D10 | The controller runs an applied copy that only `apply` writes. `/etc/rampire` is the draft |
| D11 | `apply` attests new VMs that name a review profile. Drifted VMs need a person |
| D12 | Current releases only (see [Supported platforms](#supported-platforms)) |
| D13 | A controller with no VMs is valid, and idles |
| D14 | Every `rampire` command runs as root |
| D15 | Semantic versioning, controller first, one minor release of skew (see [Versions and releases](#versions-and-releases)) |
| D16 | The command line is defined with clap |
| D17 | Setting names drop their `_bytes` suffixes in the clean break |
| D18 | `vm remove` leaves the VM at its current size |
| D19 | The milestone order stays R, O1, O2, O3, O4, O5. CI arrives in O2, and the test fleet is installed again at the end of O1 |
| D20 | Every JSON document names its schema (`"schema": "rampire.NAME"`) beside its version |
| D21 | The Windows guest service is `RampireGuest` |
| D22 | The operator views get their own crate, `rampire-ops` |

## After this roadmap

Parked, not dropped. Each item is described in the previous roadmap
(`git show 08e54a6:docs/roadmap.md`) and in the previous prior-art notes
(`git show 08e54a6:docs/prior-art.md`).

| Item | What it is |
|---|---|
| P1 | Record a Linux removal that completes on a late driver retry, and confirm the 30 s settle margin |
| P3 | A VFIO mapping budget derived from `dma_entry_limit`, and block-size guidance |
| P4 | Backend option checks in attestation (`reserve=off`, `dump=off`, `prealloc=on` for huge pages) |
| W | Windows guests on Hyper-V Dynamic Memory (`hv-balloon`), blocked on a QEMU with the devices |
| H1–H4 | Host pressure as an admission input, returning memory when the host is short, damping review, free page reporting |
| N1–N2 | One virtio-mem device per NUMA node |
| L1 | Cold-page reclaim before a Linux removal, only if removals stall on cache |
| C2, C3 | The remaining provisional Linux numbers, and surviving a fill faster than growth |
| C4 | Review of the six-hour `safe_vm` capture on the control channel |
| C7 | Linux qualification: workload run, failure injection, multi-VM soak |

The previous carried-over items C1 (legacy migration code), C5 (version
compatibility) and C6 (packaging) are now R6,
[Versions and releases](#versions-and-releases) and O2.

## Importing third-party code

Moved here from the deleted prior-art notes. Before copying any code,
file or substantial snippet:

1. **Check the license** at the version you copy, in the file's own
   header first. Copy only code under MIT, BSD-2/3-Clause, ISC, zlib,
   Apache-2.0, LGPL-2.1-or-later, GPL-2.0-or-later, GPL-3.0 or
   AGPL-3.0. Never copy GPL-2.0-only code or code with no stated
   license; implement from the specification or documentation instead.
2. **Keep the original notices.** Leave the source file's copyright and
   license header in place. Add our own copyright line under it, and an
   `SPDX-License-Identifier` line naming the original license.
3. **Add the license text** as `LICENSES/<SPDX-id>.txt` if it isn't
   already there.
4. **Record it** in `THIRD_PARTY_NOTICES.md` at the root: project, URL,
   commit, license, copyright holders, and which of our files contain it.
5. **Apache-2.0:** copy the project's `NOTICE` text (if any) into
   `THIRD_PARTY_NOTICES.md`, and mark each modified file with a line
   saying it was changed.
6. **AGPL-3.0:** keep the copied code in its own files; those files stay
   AGPL-3.0. The host daemon isn't offered to users over a network, so
   AGPL's extra condition doesn't arise in normal use, but check before
   copying.
7. **GPL-2.0-or-later code** becomes GPL-3.0-or-later in our combined
   work; keep its own header unchanged.
8. **Rust dependencies** need no copying; `cargo` records them. Check
   each new crate's license against step 1.

## Not planned

- Hosts other than libvirt/KVM, Proxmox included.
- Migrating installs made under the old names.
- Weakening or skipping attestation to make setup shorter.
- A network API or listening service on the host.
- Running `rampire` commands without root.
