# Administration

How to install the RAMpire controller on a KVM host, add VMs to it,
install the guest services, recover from faults and upgrade. Development
builds and checks are in [development](development.md); the observation
tools are in [tools](tools.md).

## How it fits together

One controller (`rampire`) runs on the KVM host as the systemd unit
`rampire.service`. It manages every VM in its configuration
from one pool of virtio-mem device memory. In each VM a guest service
decides when it needs more or less memory and asks; the controller admits
the request within the pool, the VM's limits and the host's own reserve,
resizes the VM's virtio-mem device and reports the result.

Guest and host talk over a dedicated virtio-serial port,
`org.rampire.control.0` (the control channel). A VM without the port is
served through control files that the controller reads and writes with the
QEMU guest agent.

Run one controller per host: separate controllers cannot share a pool.

## Host layout

| Item | Location |
|---|---|
| Controller binary | `/usr/bin/rampire` |
| Unit | `/etc/systemd/system/rampire.service` |
| libvirt access rule | `/etc/polkit-1/rules.d/50-rampire.rules` |
| Configuration (the draft) | `/etc/rampire/` (`global.toml`, `vms.d/<vm_name>.toml`, `reviews/`) |
| Attestations | `/etc/rampire/attestations/<vm_name>.attestation.json`, apart from the settings |
| Applied copies | `/var/lib/rampire/applied/<id>/`, with `current` naming the one the controller runs |
| What each apply did | `/var/lib/rampire/evidence/<id>-apply.json` (and `-rollback.json`): the build, the copies switched between, every file's SHA-256, the VMs checked, whether the controller restarted |
| Durable request state | `/var/lib/rampire/state/<vm_name>-requests.json` (the unit's `StateDirectory`) |
| Guest publications for tools | `/run/rampire/<vm_name>.publication.json` |
| Deployment backups | `/var/lib/rampire/deployment-backups/` |

The unit runs `rampire daemon --config /var/lib/rampire/applied/current`.
The controller takes its attestations from the `attestations/` folder of
the copy it runs and its state from the unit's `StateDirectory`.

## Installing the controller

`sudo rampire doctor` checks these once the controller is installed:
libvirt 10.1 or later and QEMU 8.2 or later, QEMU's `virtio-mem-pci`
with `dynamic-memslots` and `unplugged-inaccessible`, the service
account, the unit's socket group, the account's libvirt access (the
polkit rule) and an applied configuration. It prints the fix for each
problem.

Requirements: a RHEL-family (or other systemd) KVM host with libvirt and
QEMU, `virsh` on the path, polkit, and Rust to build the controller.

1. Create the service account once:

   ```bash
   sudo useradd --system --no-create-home --shell /sbin/nologin rampire
   ```

   The unit adds the `qemu` group, which owns libvirt's channel sockets.

2. Write the site configuration in a directory of your own, starting from
   [deploy/examples](../deploy/examples). See [Configuration](#configuration).
   On a host with the controller installed, `sudo rampire init` writes a
   suggested `global.toml` instead: it reads the host's memory and every
   libvirt domain, sets aside the boot RAM of each VM with a virtio-mem
   device and all the memory of every other VM, and keeps an eighth of the
   host (2 to 8 GiB) as `host_reserve`. It never overwrites an existing
   `global.toml`.

3. Prepare each VM and attest it ([Adding a VM](#adding-a-vm)).

4. Install the controller. Until there are packages (roadmap O2), build
   and install from a checkout
   ([development](development.md#installing-a-development-build)), then:

   ```bash
   sudo rampire apply                    # check the draft and switch to it
   sudo systemctl enable --now rampire
   ```

   Every VM in `vms.d/` needs its attestation in
   `/etc/rampire/attestations/<vm_name>.attestation.json`.

### Changing the configuration

`/etc/rampire` is a draft. The controller runs only a copy that
`rampire apply` has checked (decision D10), so a mistake in the draft
never reaches it, through a restart or a reboot:

```bash
sudo nano /etc/rampire/vms.d/VM.toml
sudo rampire diff       # what would change
sudo rampire apply      # check, then switch
sudo rampire rollback   # back to the copy before
```

`rampire set` changes settings without an editor, keeping the file's
comments, then applies:

```bash
sudo rampire set webserver priority=150 max_ram=12GB
sudo rampire set global host_reserve=6GB
```

The target is a VM's name, or `global` for `global.toml`. A value is
written as a whole number, `true`/`false`, or text (`"12GB"`). Nothing is
written unless the changed file is valid and the whole configuration still
loads. `--no-apply` changes the draft only.

`apply` copies the draft, checks the copy like `rampire check` (the
settings and every VM's attestation against the live VM) while the
controller keeps running, shows the difference and asks. Only then does
it keep the copy under `/var/lib/rampire/applied/`, point `current` at it
and restart the controller. A draft that fails changes nothing.
`--detailed-exitcode` exits 0 when nothing changed, 2 when something
changed and 1 on failure. `rollback` returns to the copy before the
current one, through the same checks. Each change is recorded under `/var/lib/rampire/evidence/`; the
`retention` setting decides how many copies and records are kept, never
removing the current copy or the one before it.

The daemon resolves `current` once when it starts, so an apply never
changes the files under a running controller.

[Usage](usage.md) walks through this for people new to the program.

## Configuration

One directory holds `global.toml`, one file per VM in `vms.d/`, review
profiles in `reviews/` and attestations in `attestations/`. A missing
`global.toml` means every default. An empty `vms.d/` is valid: the
controller runs and logs that it manages no VMs. Only `vms.d/*.toml` files
are read, so editor backups and `.rpmnew` files there are ignored.
Comments (`#`) are kept when installed. Unknown or misspelt keys are rejected. Memory sizes are binary
whatever the spelling (`"1GB"` = `"1GiB"` = 2^30 bytes, `"128MB"` =
`"128MiB"`); fractional sizes such as `"8.5GB"` and plain byte counts are
accepted.

### `global.toml`

| Key | Default | Meaning |
|---|---|---|
| `pool` | none | Device memory shared by every VM, excluding fixed boot RAM. Required with more than one VM |
| `host_reserve` | `4GB` | `MemAvailable` the host keeps. Growth is reduced or refused rather than cross it |
| `observe_only` | `false` | Evaluate and log requests, but refuse each one before resizing |
| `poll_interval_seconds` | `5` | Host poll and guest sample interval, 1–10. Freshness bounds are six polls |
| `command_timeout_seconds` | `10` | Limit on each `virsh` command |
| `convergence_timeout_seconds` | `300` | How long a Windows removal may stop moving before it is settled at what the guest returned, and the deadline for each settle command. Linux removals use their own built-in time (below) |
| `libvirt_uri` | `qemu:///system` | The libvirt connection that owns the VMs |
| `retention` | `10` | Applied copies, and records of what each apply did, kept under `/var/lib/rampire`. At least 2, so there is always a copy to roll back to |

A Linux removal that stops moving is settled after 380 s without
movement, not after `convergence_timeout_seconds`. The guest driver
retries a busy removal itself, 50 s, 150 s and 350 s after it first fails
and then every 300 s; the controller waits for the first three retries
and 30 s more, and that window also covers at least one retry after any
later movement. It is built in, from the driver's schedule.

Each VM occupies `max(requested, current)` bytes of the pool. Growth uses
only free pool capacity, highest priority first; among equal priorities the
request the guest issued first is served first, then VM name order. The
controller never takes memory from one VM to give to another. Admission
also keeps 256 MiB above `host_reserve` for changes between
planning and resizing.

### `vms.d/<vm_name>.toml`

One file per VM. The file's name is the libvirt domain name with `.toml`
added: lower-case ASCII letters, digits, `-` and `_`, at most 64
characters, and not `global`, which `rampire set` uses for `global.toml`.
The name isn't repeated inside.

| Key | Meaning |
|---|---|
| `alias` | The virtio-mem device's alias in the domain XML |
| `priority` | Positive; higher is served first from free pool memory |
| `min_ram`, `max_ram` | Limits on total guest RAM, fixed boot RAM included, aligned to the device block size |
| `free_ram` | The memory the guest keeps free above what it uses. A multiple of eight device blocks |
| `guest_os` | `"linux"`, or omitted for Windows |
| `install_path`, `service_account` | Windows only: where the service is installed and the account it runs as |
| `review` | Optional: the review profile in `reviews/` the VM is attested with |
| `telemetry_path` | Optional host file for a diagnostic copy of each publication |

`free_ram` is the only policy size. The guest asks for more when free
memory falls below it and returns memory when it has more than that spare.
The rest follows from it: reclaim never plans below half of it (also the
level treated as critical), memory is returned only once the surplus
exceeds an eighth of it, growth and reclaim both aim a sixteenth of it
above it so ordinary fluctuation causes no resize, and native Low pressure
asks for at least half of it. A Windows guest removes at most one and a
half times `free_ram` per request.

Guests take these settings from the host with every status. Changing a
VM's settings needs only a controller install, never a guest change.

## Adding a VM

1. **Device.** Give the domain a virtio-mem device with a stable alias and
   the options the controller requires. The device's `requested` size in
   the saved definition should be zero: the guest asks for what it needs.

   ```xml
   <maxMemory slots='1' unit='GiB'>16</maxMemory>
   ...
   <memory model='virtio-mem'>
     <target>
       <size unit='GiB'>8</size>
       <node>0</node>
       <block unit='KiB'>2048</block>
       <requested unit='KiB'>0</requested>
     </target>
     <alias name='ua-virtiomem0'/>
   </memory>
   ```

   QEMU must run the device with `dynamic-memslots` and
   `unplugged-inaccessible` on. The attestation reads both from QEMU and
   refuses a VM without them.

2. **Control channel.**

   ```xml
   <channel type='unix'>
     <target type='virtio' name='org.rampire.control.0'/>
     <alias name='ua-virtiomem-control'/>
   </channel>
   ```

   libvirt chooses the host socket; the controller finds it in the domain
   XML. Without the channel the guest agent must run in the guest with file
   commands allowed (RHEL-family guests block them by default).

3. **Review.** Write a review profile, `CONFIG_DIR/reviews/NAME.toml`,
   that records what you checked, starting from
   [deploy/examples/reviews/linux-standard.toml](../deploy/examples/reviews/linux-standard.toml),
   and name it in the VM's file with `review = "NAME"`. Each answer says
   whether tooling detected it or a person confirmed it:

   ```toml
   version = 1
   guest_os = "linux"
   workload_reviewed = { value = true, source = "confirmed" }
   no_vdpa = { value = true, source = "confirmed" }
   # ... and the rest, as in the example
   guest_driver_version = { value = "linux 6.12.86+deb13-amd64", source = "confirmed" }
   ```

   `guest_os` must match the VM's settings. `guest_driver_version` is the
   `viomem` driver version on Windows and the kernel release on Linux.
   `workload_reviewed` can only be confirmed by a person.

4. **Attest.** With the VM running in its final configuration:

   ```bash
   cargo xtask attestation VM ALIAS CONFIG_DIR/reviews/NAME.toml \
     --output CONFIG_DIR/attestations/VM.attestation.json --command-timeout-seconds 60
   ```

   The attestation binds the review to hashes of the live domain XML, QEMU's
   arguments and versions, and the device options. Every resize checks them
   again and refuses on any difference, so attest again after any change
   to the VM's definition. The domain XML is read for every resize; what
   cannot change while QEMU runs (its arguments, QEMU and libvirt versions,
   device options) is collected again when the domain restarts and at
   least every five minutes. A saved definition takes effect only after
   a full shutdown and start of the VM; attest after that.

5. **Settings.** Write `/etc/rampire/vms.d/<vm_name>.toml`, with the
   attestation in `/etc/rampire/attestations/`, and run
   `sudo rampire apply`.

6. **Guest service.** Install it in the guest (below). It waits, saying so
   in its log, until the controller has published its settings.

## Linux guests

The guest service is `rampire-guest`, run by systemd as an unprivileged
dynamic user ([unit](../guest-linux/systemd/rampire-guest.service)). It
has no configuration file.

| Item | Location |
|---|---|
| Binary | `/usr/local/libexec/rampire-guest` |
| Durable request state | `/var/lib/rampire-guest/control.json.request-state` |
| Control files, without the channel | `/var/lib/rampire-guest/control.json` and `control.json.host-status` |

Install with the Debian package (`cargo xtask linux package`), or on a
development VM with `cargo xtask linux deploy` ([tools](tools.md)).

At start the service checks and logs its readiness: virtio-mem driver bound,
pressure stall information and its trigger, how hot-plugged memory is
onlined, the movable zone, swap and the commit model. The same report is
published with each sample's diagnostics, so `metrics` records it.

Memory onlining: the kernel guarantees removal only from the movable zone.
Debian's default onlining (`online`, `contig-zones`) removed memory fully in
measured runs, but if removals come back partial after long uptime, boot the
guest with `memory_hotplug.online_policy=auto-movable`. Avoid
`memhp_default_state=online_movable`: it confines the kernel's own memory to
boot RAM however large the guest grows.

Sizing under `auto-movable`: the kernel puts hot-plugged memory in the
movable zone only up to `auto_movable_ratio` percent (default 301) of the
boot RAM in kernel zones. Past that, new memory is split: about one
part in four (at the default ratio) goes to kernel zones, where removal is
not guaranteed, because kernel-zone memory of the same device raises its
movable allowance. On a live guest with 512 MiB boot, a 4 GiB spike put
640 MiB in `Normal`; that time it was all returned, because the pages
were freed user memory. At the default, a guest can hot-plug about three
times its boot RAM entirely movable, so keep `max_ram` within about four
times boot RAM where removal must be reliable: for example 8 GiB boot
supports `max_ram` up to about 32 GiB. The service publishes `auto_movable_ratio`, `auto_movable_numa_aware` and
`boot_ram_bytes` (from `/sys/firmware/memmap`), and when it receives its
settings it warns if `max_ram` is above that limit. With
`auto_movable_numa_aware=Y` (the default) the ratio also applies to each
NUMA node, so a guest with several nodes may reach its limit earlier.

Boot RAM can be very small, because the guest grows on demand once its
service runs. Measured on Debian 13 (kernel 6.12, 2026-11-13): the kernel
keeps back about 110 MiB of any boot size, the largest part a 64 MiB
`swiotlb` bounce buffer that x86 sets up because hot-pluggable memory
lies above 4 GiB. Untuned, 320 MiB booted cleanly (80 MiB free at idle)
and 256 MiB booted but starved while cloud-init ran. With `swiotlb=8192`
(16 MiB), `exim4` and `unattended-upgrades` disabled and cloud-init
disabled after provisioning, 256 MiB boots in about 10 seconds with
100 MiB free. Keep `swiotlb` rather than turning it off while the guest
has devices limited to 32-bit DMA (IDE, USB). Set `min_ram` to the boot
RAM so an idle guest can return its whole device, and save the device
with `<requested>` 0 so every boot starts on boot RAM alone.

Growth follows use, so a guest grows as a program touches memory, not
when it asks for it. Under the default heuristic overcommit
(`vm.overcommit_memory=0`) the kernel refuses a single allocation larger
than the guest's RAM and swap at that moment, before the guest can ask for
more: on a guest at 790 MiB, one 4 GiB `mmap` failed, while the same
4 GiB taken 512 MiB at a time grew the guest to 4.4 GiB in 21 seconds.
Programs that allocate as they go (ClamAV loading its signatures, for
example) are unaffected. For a workload that reserves a large block up
front, give the guest more boot RAM or a higher `min_ram`, or set
`vm.overcommit_memory=1` in the guest.

How Linux memory is read:

- Total and available are `MemTotal` and `MemAvailable`.
- File cache the guest is re-reading from disk (`workingset_refault_file`)
  counts as in use, so the guest grows until the re-reads stop and gives the
  memory back a step at a time once they have. Cache can therefore take pool
  memory up to `max_ram`.
- Pressure comes from `/proc/pressure/memory`. A trigger (100 ms of stall in
  1 s, or 200 ms in 2 s without `CAP_SYS_RESOURCE`) wakes the service and
  asks for memory at once. Reclaim waits until the ten-second average is
  zero.
- Under strict overcommit (`vm.overcommit_memory=2` with a RAM-scaled limit)
  the kernel's `CommitLimit` and `Committed_AS` are honoured. Otherwise the
  commit reserve never asks for memory.

The unit sets `OOMScoreAdjust=-900`: the service is what asks for more
memory, so it must be among the last processes the kernel kills.

## Windows guests

The Windows service is `RampireGuest`
(`C:\Program Files\RAMpire\rampire-guest.exe`, running as
LocalService). Its install configuration is
`C:\ProgramData\RAMpire\config.json`; limits and policy come from
the host as for Linux. Deploy it with `make windows-deploy` from a checkout
whose `site/windows-build.mk` names the VM. Deployment gives LocalService
access to the control channel's device instance
(`\\.\Global\org.rampire.control.0`) and checks that the service stays
running. Without the port the service uses the control files through the
guest agent.

**Windows on virtio-mem is paused.** The Windows `viomem` driver has a
removal defect, and all work on it is on pause until the driver bug is
repaired. The controller's two Windows-only workarounds (widening the
usable region and extending a stalled shrink) are archived in
[archive/viomem](../archive/viomem/) and no longer run: a Windows removal
that stops moving for the convergence timeout settles at what the guest
kept. Windows guests are moving to Hyper-V Dynamic Memory;
see [the roadmap](roadmap.md), milestone W.

## The `rampire` command

Every operator command runs as root (`sudo rampire ...`); only
`rampire daemon`, which the unit runs, runs as the service account.
Commands that change or check the configuration (`check`, `diff`,
`apply`, `set`, `init`) work on the draft, `/etc/rampire`; the views and
recovery (`status`, `top`, `watch`, `recover`) on the applied copy the
controller runs, so a mistake in the draft doesn't hide what is running.
`--config` or `RAMPIRE_CONFIG` names another directory for both. `rampire --help` and
`rampire COMMAND --help` list every option.

The same command serves a person and a script:

- On a terminal, a command that changes something explains and asks.
  `--yes` answers yes. Without a terminal and without `--yes` it changes
  nothing and says so.
- `--json` writes a versioned document instead of text. It names its
  schema: `rampire.status-report` for `status` and `rampire.check` for
  `check`.
- `rampire --version` names the build: `0.1.0 (build 6b0e2db1a2c3)`.

| Exit code | Meaning |
|---|---|
| 0 | Done |
| 1 | The command failed, or `check` found a problem |
| 2 | The command line was wrong |

## Observing

```bash
sudo rampire top            # every VM and the pool
sudo rampire watch VM       # one VM
sudo rampire status [VM]    # the controller's status, as JSON
sudo rampire check          # settings and attestations against the live VMs
sudo rampire doctor         # what the host needs, with the fix for each problem
sudo rampire logs --follow  # the controller's journal
cargo xtask metrics --config /etc/rampire
```

The controller logs every decision to the journal
(`sudo rampire logs`). See [tools](tools.md) for the
recorder's files and incident bundles.

## Recovering

**A latched controller.** When a resize ends in a state the controller
cannot prove safe, it latches (`actuation_latched` in the journal, `watch`
and `metrics`) and stops resizing until an operator clears it. Install the
current controller and attestation, then:

```bash
sudo rampire recover VM "why it is safe to resume"
```

It shows what it would do and asks. Then it stops the unit, settles a
stuck shrink at what the guest kept, records the reason in the VM's state
(as the service account, so the controller can still write it) and starts
the unit again. `--yes` answers for a script.

**A stopped controller.** The unit is fail-stop (`Restart=no`): a failure
needs a person to look. Read `systemctl status` and the journal, fix the
cause, then `systemctl start`. Durable state means a pending request is
resumed, not lost.

**A guest service restart or guest reboot.** The guest's one unresolved
request is durable and is sent again unchanged until the host resolves it.
Never delete request state to get past an error: the request must still
receive its result.

**Attestation drift.** A resize refused with `live compatibility evidence
drifted: FIELD` means the VM changed since it was attested. Check that the
change was intended, attest again and install.

**Startup memory.** A guest reboot keeps QEMU's live device size. Only a
full shutdown and start applies the saved definition (`requested` zero).
Do not clear request state to reduce RAM.

## Upgrading

Order: **controller first, then guest services.** Every document the
controller and the guests exchange or keep names its schema and version,
and every one is at version 1. A reader refuses a schema or version it
doesn't know, so it never guesses. Until the first release, install the
controller and the guests from the same build; from release 0.1.0 a
controller serves guests from its own minor release and the one before it
(see [the roadmap](roadmap.md#versions-and-releases)).

1. If the release notes say the attestation format changed, attest every
   VM again first.
2. Install the new controller, then `sudo rampire apply` and
   `sudo systemctl restart rampire`.
3. Check with `sudo rampire status`.
4. Upgrade each guest service (package or `make windows-deploy`).
