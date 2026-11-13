# Administration

How to install the virtio-mem pool controller on a KVM host, add VMs to it,
install the guest services, recover from faults and upgrade. Development
builds and checks are in [development](development.md); the observation
tools are in [tools](tools.md).

## How it fits together

One controller (`virtio-mem-host`) runs on the KVM host as the systemd unit
`virtio-mem-host@INSTANCE.service`. It manages every VM in its host list
from one pool of virtio-mem device memory. In each VM a guest service
decides when it needs more or less memory and asks; the controller admits
the request within the pool, the VM's limits and the host's own reserve,
resizes the VM's virtio-mem device and reports the result.

Guest and host talk over a dedicated virtio-serial port,
`org.virtio-mem.control.0` (the control channel). A VM without the port is
served through control files that the controller reads and writes with the
QEMU guest agent.

Run one controller per host: separate controllers cannot share a pool.

## Host layout

| Item | Location |
|---|---|
| Controller binary | `/usr/local/libexec/virtio-mem-host` |
| Unit | `/etc/systemd/system/virtio-mem-host@.service` |
| libvirt access rule | `/etc/polkit-1/rules.d/50-virtio-mem-host.rules` |
| Configuration | `/etc/virtio-mem-host/` (`global-settings.toml`, `host_list.toml`, `<vm_name>_settings.toml`) |
| Attestations | `/etc/virtio-mem-host/<vm_name>.attestation.json`, beside the host list |
| Durable request state | `/var/lib/virtio-mem-host/state/<vm_name>-requests.json` (the unit's `StateDirectory`) |
| Guest publications for tools | `/run/virtio-mem-host/<vm_name>.publication.json` |
| Deployment backups | `/var/lib/virtio-mem-host/deployment-backups/` |

A site with another layout overrides `ExecStart` in a drop-in
(`systemctl edit virtio-mem-host@.service`). The controller takes its
attestations from the host list's directory and its state from the unit's
`StateDirectory`, so they follow.

## Installing the controller

Requirements: a RHEL-family (or other systemd) KVM host with libvirt and
QEMU, `virsh` on the path, polkit, and Rust to build the controller.

1. Create the service account once:

   ```bash
   sudo useradd --system --no-create-home --shell /sbin/nologin virtio-mem-host
   ```

   The unit adds the `qemu` group, which owns libvirt's channel sockets.

2. Write the site configuration in a directory of your own, starting from
   [deploy/examples](../deploy/examples). See [Configuration](#configuration).

3. Prepare each VM and attest it ([Adding a VM](#adding-a-vm)).

4. Build and install from a checkout:

   ```bash
   deploy/scripts/install-host.sh CONFIG_DIR INSTANCE VM=ATTESTATION [VM=ATTESTATION ...]
   ```

   It builds the release controller, stops the unit, then runs
   `cargo xtask host-deploy`. That validates the configuration and every
   attestation against the live VMs, installs binary, unit, polkit rule,
   configuration and attestations, keeping a backup of what it replaces,
   and writes JSON evidence under `.artifacts/deploy/`. The script then
   enables and starts the unit. Every VM in the host list needs exactly one
   attestation.

A controller that is running does not reload its configuration. Install
again to change it.

## Configuration

Three kinds of file, in one directory. Unknown or misspelt keys are
rejected. Memory sizes are binary (`"1GB"` is 2^30 bytes); plain byte counts
and fractional sizes such as `"8.5GB"` are accepted.

### `global-settings.toml`

| Key | Default | Meaning |
|---|---|---|
| `pool_bytes` | none | Device memory shared by every VM, excluding fixed boot RAM. Required with more than one VM |
| `host_min_headroom_bytes` | `4GB` | `MemAvailable` the host keeps. Growth is reduced or refused rather than cross it |
| `observe_only` | `false` | Evaluate and log requests, but refuse each one before resizing |
| `poll_interval_seconds` | `5` | Host poll and guest sample interval, 1–10. Freshness bounds are six polls |
| `command_timeout_seconds` | `10` | Limit on each `virsh` command |
| `convergence_timeout_seconds` | `300` | How long a resize may stop moving before it is settled at what the guest returned |
| `libvirt_uri` | `qemu:///system` | The libvirt connection that owns the VMs |

Each VM occupies `max(requested, current)` bytes of the pool. Growth uses
only free pool capacity, highest priority first; the controller never takes
memory from one VM to give to another. Admission also keeps 256 MiB above
`host_min_headroom_bytes` for changes between planning and resizing.

### `host_list.toml`

One `[[vm]]` per VM:

| Key | Meaning |
|---|---|
| `vm_name` | The libvirt domain name: lower-case ASCII letters, digits, `-` and `_`, at most 64 characters |
| `alias` | The virtio-mem device's alias in the domain XML |
| `priority` | Positive; higher is served first from free pool memory, ties in list order |
| `telemetry_path` | Optional host file for a diagnostic copy of each publication |

### `<vm_name>_settings.toml`

| Key | Meaning |
|---|---|
| `vm_name` | Must match the filename and the host list |
| `min_ram`, `max_ram` | Limits on total guest RAM, fixed boot RAM included, aligned to the device block size |
| `free_ram` | The memory the guest keeps free above what it uses. A multiple of eight device blocks |
| `guest_os` | `"linux"`, or omitted for Windows |
| `install_path`, `service_account` | Windows only: where the service is installed and the account it runs as |

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
     <target type='virtio' name='org.virtio-mem.control.0'/>
     <alias name='ua-virtiomem-control'/>
   </channel>
   ```

   libvirt chooses the host socket; the controller finds it in the domain
   XML. Without the channel the guest agent must run in the guest with file
   commands allowed (RHEL-family guests block them by default).

3. **Review.** Write the compatibility review for the VM, a JSON file that
   records what you checked:

   ```json
   {
     "guest_os": "linux",
     "workload_reviewed": true,
     "no_vdpa": true,
     "no_rdma_migration": true,
     "no_vfio_nvme": true,
     "no_mlock": true,
     "no_secure_virtualization": true,
     "no_unsupported_vhost_user": true,
     "balloon_resize_inactive": true,
     "memory_slot_budget": 1,
     "vfio_mapping_budget": 1,
     "guest_driver_version": "linux 6.12.86+deb13-amd64"
   }
   ```

   `guest_os` must match the VM's settings. `guest_driver_version` is the
   `viomem` driver version on Windows and the kernel release on Linux.

4. **Attest.** With the VM running in its final configuration:

   ```bash
   cargo xtask attestation VM ALIAS REVIEW --output VM.attestation.json --command-timeout-seconds 60
   ```

   The attestation binds the review to hashes of the live domain XML, QEMU's
   arguments and versions, and the device options. Every resize collects
   them again and refuses on any difference, so attest again after any
   change to the VM's definition. `deploy/scripts/restart-vm-attest.sh`
   cold-restarts a VM (so a saved definition takes effect), attests it and
   installs the controller in one step.

5. **Settings.** Add the VM to `host_list.toml`, write its
   `<vm_name>_settings.toml` and install the controller with every VM's
   attestation.

6. **Guest service.** Install it in the guest (below). It waits, saying so
   in its log, until the controller has published its settings.

## Linux guests

The guest service is `virtio-mem-guest`, run by systemd as an unprivileged
dynamic user ([unit](../guest-linux/systemd/virtio-mem-guest.service)). It
has no configuration file.

| Item | Location |
|---|---|
| Binary | `/usr/local/libexec/virtio-mem-guest` |
| Durable request state | `/var/lib/virtio-mem-guest/control.json.request-state` |
| Control files, without the channel | `/var/lib/virtio-mem-guest/control.json` and `control.json.host-status` |

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

The Windows service is `VirtioMemService`
(`C:\Program Files\VirtioMemService\virtio-mem-service.exe`, running as
LocalService). Its install configuration is
`C:\ProgramData\VirtioMemService\config.json`; limits and policy come from
the host as for Linux. Deploy it with `make windows-deploy` from a checkout
whose `site/windows-build.mk` names the VM. Deployment gives LocalService
access to the control channel's device instance
(`\\.\Global\org.virtio-mem.control.0`) and checks that the service stays
running. Without the port the service uses the control files through the
guest agent.

**Windows on virtio-mem is paused.** The Windows `viomem` driver has a
removal defect, and all work on it is on pause until the driver bug is
repaired. The controller's two Windows-only workarounds (widening the
usable region and extending a stalled shrink) are archived in
[archive/viomem](../archive/viomem/) and no longer run: a Windows removal
that stops moving for the convergence timeout settles at what the guest
kept, as on Linux. Windows guests are moving to Hyper-V Dynamic Memory;
see [the roadmap](roadmap.md), milestone W.

## Observing

```bash
cargo xtask watch --config /etc/virtio-mem-host/host_list.toml
cargo xtask metrics --config /etc/virtio-mem-host/host_list.toml
virtio-mem-host status --config /etc/virtio-mem-host/host_list.toml --vm VM
```

The controller logs every decision to the journal
(`journalctl -u virtio-mem-host@INSTANCE`). See [tools](tools.md) for the
recorder's files and incident bundles.

## Recovering

**A latched controller.** When a resize ends in a state the controller
cannot prove safe, it latches (`actuation_latched` in the journal, `watch`
and `metrics`) and stops resizing until an operator clears it. Install the
current controller and attestation, then:

```bash
deploy/scripts/recover-latch.sh INSTANCE "why it is safe to resume"
```

It stops the unit, runs `virtio-mem-host clear-latch`, which first settles a
stuck shrink at what the guest kept and records the reason in the state,
and starts the unit again. `VIRTIO_MEM_HOST_BIN`, `VIRTIO_MEM_HOST_CONFIG`
and `VIRTIO_MEM_CONNECT` override the installed paths and libvirt URI.

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

Order: **controller first, then guest services.** A new controller serves
every older guest; a new guest under an older controller may wait for
settings that never come, and says so in its log.

1. Stop nothing by hand: `install-host.sh` stops and restarts the unit.
2. If the release notes say the attestation format changed, attest every
   VM again first. Version 2 (2026-11-13) replaced `trusted_development_guest`
   with `guest_os` in the review; update each review file accordingly.
3. Install the controller.
4. Upgrade each guest service (package or `make windows-deploy`).

### Compatibility

What each side reads and sends is versioned, and each rule is enforced by
the protocol.

| Guest | Publishes | Needs a controller that | Under an older controller |
|---|---|---|---|
| Windows, before 2026-11-13 | version 4 or 5, control files, own `config.json` | reads versions 4–6 (all current) | — |
| Linux or Windows, control files | version 6 | publishes settings with its status | waits for settings, logs that it is waiting |
| Linux or Windows, control channel | version 6 | answers on the control channel | after 30 s without settings on the channel, uses the control files if a fresh status with settings is there (the older controller writes it through the guest agent, which must allow file commands); otherwise keeps waiting |

The controller sends settings only to a guest whose last publication is
version 6 or that has published nothing yet, so a guest built before
settings were published keeps its own configuration. It answers a guest on
whichever route the guest's publications arrive on, and sends every status
over a connected control channel, so a guest waiting there for its first
settings receives them.

| File | Version | Read by |
|---|---|---|
| Compatibility attestation | 2 | Controller and `host-deploy`; version 1 is refused with a message to attest again |
| Host status, memory request | 1 | Both sides |
| Guest publication | 6 (reads 4–6) | Controller and tools |
| Windows `config.json` | schema 9 | Windows service |
