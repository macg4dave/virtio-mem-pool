# Usage

How to use virtio-mem-pool once it is installed: what it does, the
commands you run day to day and what each one is for. This guide is the
starting point. [Administration](administration.md) has the full install,
configuration and recovery reference, and [tools](tools.md) has each
observation tool in detail.

## What it does

A KVM host has a fixed amount of RAM to give its VMs. Without this
program each VM gets a size up front and keeps it, used or not. With it,
the VMs share one **pool**:

1. Each VM boots on a small, fixed **boot RAM**.
2. A **guest service** inside the VM watches its own memory. When free
   memory falls below the VM's `free_ram`, it asks the host for more.
   When it has more than `free_ram` spare, it gives some back.
3. The **controller** on the host (`virtio-mem-host`) decides each
   request. It grants growth only from free pool memory, within the VM's
   `min_ram`–`max_ram`, and never below the host's own reserve
   (`host_min_headroom_bytes`). Higher-priority VMs are served first. It
   never takes memory from one VM to give to another.
4. The controller resizes the VM's virtio-mem device and reports to the
   guest what it actually got.

You set three things per VM (`min_ram`, `max_ram`, `free_ram`) and one
pool size for the host. Everything else follows from them.

| Part | Runs on | Name |
|---|---|---|
| Controller | KVM host (RHEL-family, libvirt, QEMU) | `virtio-mem-host@INSTANCE.service` |
| Linux guest service | Each Linux VM | `virtio-mem-guest.service` |
| Windows guest service | Each Windows VM (paused, see below) | `VirtioMemService` |
| Operator tools | Host, from a checkout | `cargo xtask ...` |

**Windows guests:** the Windows virtio-mem path is paused because of an
upstream driver bug, and Windows support is moving to Hyper-V Dynamic
Memory ([roadmap](roadmap.md), milestone W). Use Linux guests for real
work today.

## Quick start

The short path from nothing to a managed Linux VM. Each step links to the
full detail.

1. **Write the configuration.** Copy [deploy/examples](../deploy/examples)
   to a directory of your own (for example `site/config/`) and edit:
   - `global-settings.toml`: `pool_bytes`, the memory all VMs share, and
     `host_min_headroom_bytes`, what the host keeps for itself.
   - `host_list.toml`: one `[[vm]]` per VM, with its libvirt name, the
     device alias and a priority.
   - `<vm_name>_settings.toml` per VM: `min_ram`, `max_ram`, `free_ram`
     and `guest_os = "linux"`.

   See [Configuration](administration.md#configuration).

2. **Prepare the VM.** Give the domain a virtio-mem device with
   `<requested>` 0, and the control channel
   `org.virtio-mem.control.0`. See [Adding a VM](administration.md#adding-a-vm).

3. **Attest the VM.** Write the compatibility review JSON, then, with the
   VM running:

   ```bash
   cargo xtask attestation VM ALIAS REVIEW.json --output VM.attestation.json --command-timeout-seconds 60
   ```

4. **Install the controller.**

   ```bash
   deploy/scripts/install-host.sh CONFIG_DIR INSTANCE VM=VM.attestation.json
   ```

5. **Install the guest service** in the VM, from the Debian package
   (`cargo xtask linux package --output DIR`) or, on a development VM:

   ```bash
   cargo xtask linux deploy user@vm --output .artifacts/linux-deploy.json --apply
   ```

6. **Watch it work.**

   ```bash
   cargo xtask pool --config /etc/virtio-mem-host/host_list.toml
   ```

   Start something memory-hungry in the guest and watch the VM grow, then
   shrink once it finishes.

## Everyday commands

`CONFIG` is the installed host list, `/etc/virtio-mem-host/host_list.toml`.
With several VMs, `--vm NAME` picks one. The `cargo xtask` commands run
from a checkout of this repository on the host.

### Is it healthy?

| Question | Command |
|---|---|
| Is the controller running? | `systemctl status virtio-mem-host@INSTANCE` |
| What has it decided? | `journalctl -u virtio-mem-host@INSTANCE -f` |
| How is the pool shared right now? | `cargo xtask pool --config CONFIG` |
| What is one VM doing? | `cargo xtask watch --config CONFIG --vm NAME` |
| One VM's full state, as JSON | `virtio-mem-host status --config CONFIG --vm NAME` |
| Is the guest service running? | In the guest: `systemctl status virtio-mem-guest` |
| What memory does the guest see? | In the guest: `virtio-mem-watch-ram` (or `tools/linux/watch-ram.sh`) |

`pool` is the best overview: one bar for the pool split by VM, a row per
VM (plugged size, growing or shrinking, RAM in use, pending request), a
history chart and recent grants and refusals. `--once` prints a single
snapshot. From a Windows workstation,
`tools\windows\Watch-Pool.ps1` opens it over SSH ([tools](tools.md#watching-the-pool)).

In `virtio-mem-host status` output, check these fields first:

| Field | Healthy value |
|---|---|
| `requested_bytes` vs `current_bytes` | Equal, or moving towards each other |
| `actuation_latched` | `false` |
| `observe_only` | `false`, unless you set it |
| `guest_input.error` | `null` |
| `recovery_state` | No recovery under way |

### Recording a run

```bash
cargo xtask metrics --config CONFIG --vm NAME [--duration-seconds N]
cargo xtask metrics --config CONFIG --vm all
```

This records samples, controller events and any crash or restart to
`.artifacts/metrics/`. On a terminal it shows a live UI. Press `m` to add
a marker such as "started build" and `q` to stop. `--vm all` records
every VM and writes a combined `pool.csv`. To send someone a record of a
problem, use the run directory, in particular `summary.json` and
`incidents/`. See [Recording a run](tools.md#recording-a-run).

### Changing a VM's limits

Edit `<vm_name>_settings.toml` (or `global-settings.toml`) in your
configuration directory and install again:

```bash
deploy/scripts/install-host.sh CONFIG_DIR INSTANCE VM=ATTESTATION [VM=ATTESTATION ...]
```

The running controller does not reload configuration on its own. Guests
take their settings from the host, so nothing changes inside the guest.

Choosing the sizes:

- **`free_ram`**: the headroom the guest keeps. Larger values grow sooner
  and keep more memory idle. Smaller values pack the pool tighter but
  leave less margin for a sudden spike. It must be a multiple of eight
  device blocks.
- **`min_ram`**: usually the boot RAM, so an idle guest can return its
  whole device.
- **`max_ram`**: for reliable removal on Linux, keep it within about four
  times boot RAM ([why](administration.md#linux-guests)).
- **`priority`** in `host_list.toml`: when the pool is short, VMs with a
  higher priority get memory first.

### Trying it without resizing

Set `observe_only = true` in `global-settings.toml` and install. The
controller evaluates and logs every request, then refuses it before
touching any device. This is useful for a first look at a new VM or a
new pool size.

### Adding another VM

For a new VM, follow [Adding a VM](administration.md#adding-a-vm), then
install with every VM's attestation. For test fleets, two scripts do it
in one step:

```bash
deploy/scripts/clone-vm.sh TEMPLATE NEW HOSTNAME BASE_IMAGE USER_DATA --apply
deploy/scripts/add-linux-vm.sh --apply CONFIG_DIR INSTANCE TEMPLATE NEW user@new
```

`clone-vm.sh` creates a fresh copy of a managed Linux VM, and
`add-linux-vm.sh` installs the guest service, attests the copy and
installs the controller. Without `--apply` each one only prints its plan.

### Changing a VM's definition

Any change to the domain (devices, CPU, QEMU options) makes its
attestation stale, and the controller then refuses resizes with
`live compatibility evidence drifted: FIELD`. Attest again and install:

```bash
deploy/scripts/restart-vm-attest.sh CONFIG_DIR INSTANCE VM ALIAS REVIEW.json
```

This cold-restarts the VM so its saved definition takes effect, attests
it and installs the controller.

### Testing under load

`cargo xtask linux deploy` also installs `virtio-mem-workload` (the Debian
package does not). It allocates
memory in phases (peak, settle, peak again) so you can watch growth and
reclaim:

```bash
virtio-mem-workload --workload-id test1 --mode resident \
  --peak-bytes 2147483648 --retained-bytes 268435456 \
  --max-allocation-bytes 268435456 \
  --peak-hold-seconds 60 --settled-hold-seconds 120 \
  --renewed-hold-seconds 60 --refresh-interval-seconds 5
```

`--mode` is `resident` (memory in use), `committed` (reserved but
untouched) or `file-cache` (page cache, with `--cache-dir`). For a
recorded pass/fail run driven from the host, use
`cargo xtask qualification start VM ALIAS --ssh-target user@vm ...`.

## When something goes wrong

| What you see | What it means | What to do |
|---|---|---|
| Unit `failed` | The controller stops on any fault rather than guess (`Restart=no`) | Read the journal, fix the cause, `systemctl start`. Pending requests resume |
| `actuation_latched` | A resize ended in a state the controller could not prove safe, so it stopped resizing | `deploy/scripts/recover-latch.sh INSTANCE "why it is safe"` ([Recovering](administration.md#recovering)) |
| `live compatibility evidence drifted` | The VM changed since it was attested | Check the change was intended, attest again, install |
| Guest log says it is waiting for settings | No controller has answered yet, or the controller is older than the guest | Start or upgrade the controller first |
| Growth refused, pool has room | The host is at `host_min_headroom_bytes` | Free host memory or lower the reserve |
| Growth refused, pool full | Every byte of `pool_bytes` is in use | Raise `pool_bytes`, lower other VMs' `max_ram`, or wait for reclaim |
| A large single allocation fails in the guest | The kernel refuses it before the guest can ask ([overcommit](administration.md#linux-guests)) | More boot RAM, a higher `min_ram`, or `vm.overcommit_memory=1` |
| Memory comes back slowly or partly | Linux can only remove memory it can move. The driver retries for several minutes | Wait. If it persists, boot with `memory_hotplug.online_policy=auto-movable` |
| A rebooted VM is still large | A reboot keeps QEMU's live size | A full shutdown and start applies `<requested>` 0 |

Never delete the request state files under `/var/lib/virtio-mem-host/`
or `/var/lib/virtio-mem-guest/` to get past an error. Each request must
still receive its result.

## Upgrading

Upgrade the **controller first, then the guest services**. A new
controller serves older guests. A new guest under an older controller may
wait for settings. Details are in [Upgrading](administration.md#upgrading).

## Command reference

### `virtio-mem-host` (on the host)

```text
virtio-mem-host --config FILE
virtio-mem-host status --config FILE [--vm NAME] [--connect URI]
virtio-mem-host clear-latch REASON --config FILE [--vm NAME] [--connect URI] [--apply]
```

| Command | Purpose |
|---|---|
| `--config FILE` | Run the controller. systemd does this; you normally don't |
| `status` | Print one VM's device, request, guest input and latch state as JSON. Read-only |
| `clear-latch` | Clear a latch after review. A dry run unless `--apply`. Use `recover-latch.sh` instead, which stops and starts the unit around it |

### Scripts in `deploy/scripts/`

| Script | Purpose |
|---|---|
| `install-host.sh CONFIG_DIR INSTANCE VM=ATT...` | Build, validate and install the controller and its configuration, then start it |
| `recover-latch.sh INSTANCE REASON` | Clear a latched controller safely |
| `restart-vm-attest.sh CONFIG_DIR INSTANCE VM ALIAS REVIEW` | Cold-restart a VM, attest it and install |
| `clone-vm.sh` | Make a fresh copy of a managed Linux VM |
| `add-linux-vm.sh` | Put a copy under the controller |

Run any script without arguments to see its usage.

### `cargo xtask` (from a checkout)

| Command | Purpose |
|---|---|
| `pool` | Live view of every VM and the pool |
| `watch` | Live view of one VM |
| `metrics` | Record a run (one VM or `--vm all`) |
| `attestation VM ALIAS REVIEW --output PATH` | Attest a running VM |
| `host-deploy INSTANCE --config PATH --attestation VM=PATH...` | Validate and install (what `install-host.sh` calls) |
| `controller-status INSTANCE --host-cli PATH --output PATH` | Run `status` through a given controller binary and save the result as JSON evidence |
| `doctor host` | Check that the host has `virsh` |
| `preflight VM ALIAS` | No-resize checks before a Windows VM is managed |
| `qualification <start\|status\|review>` | Recorded workload runs with pass/fail expectations |
| `linux build \| package \| deploy \| status \| uninstall` | Build and manage the Linux guest service |
| `windows ...` | Build and manage the Windows guest service |
| `gate ...` | Development checks ([development](development.md)) |

`cargo xtask --help` prints every option.

### In the guest

| Command | Purpose |
|---|---|
| `systemctl status virtio-mem-guest` | Service state |
| `journalctl -u virtio-mem-guest` | Readiness report and every request it made |
| `virtio-mem-watch-ram` | Live view of guest-visible memory |
| `virtio-mem-workload ...` | Test workload |
| `tools\windows\Watch-Ram.ps1` | Windows equivalent of the RAM view |
