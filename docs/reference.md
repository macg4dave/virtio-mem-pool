# Reference

Every `rampire` command and every setting. `rampire --help` and
`rampire COMMAND --help` show the same, from the same definition.

## Options every command takes

| Option | Meaning |
|---|---|
| `--config DIR` | The configuration directory. Commands that change or check it use the draft, `/etc/rampire`; the views and recovery use the applied copy the controller runs. Also `RAMPIRE_CONFIG` |
| `--connect URI` | The libvirt connection; default the configuration's `libvirt_uri` |
| `--yes` | Answer yes to every question |
| `--json` | Write a versioned JSON document ([JSON documents](json.md)) |

Every command but `daemon` runs as root. Exit codes: 0 done, 1 failed or
a problem found, 2 a wrong command line.

## Commands

### Setting up and checking

| Command | What it does |
|---|---|
| `rampire init` | Suggests `global.toml` from the host's memory and its VMs (never over an existing one), creates the configuration folders and applies |
| `rampire doctor` | Checks libvirt 10.1+, QEMU 8.2+ with virtio-mem's options, the service account, the socket group, libvirt access and an applied configuration, with the fix for each problem |
| `rampire check` | Checks the draft and every VM's attestation against the live VMs; changes nothing |

### Changing the configuration

| Command | What it does |
|---|---|
| `rampire diff` | Shows the draft against the applied copy |
| `rampire apply [--detailed-exitcode]` | Copies the draft, checks the copy while the controller runs, shows the difference, asks, then makes it current and reloads or restarts the controller. A failing draft changes nothing |
| `rampire rollback` | Returns to the applied copy before the current one, through the same checks |
| `rampire set TARGET KEY=VALUE... [--no-apply]` | Changes settings in a VM's file, or in `global.toml` with `global`, keeping comments, then applies |

### VMs

| Command | What it does |
|---|---|
| `rampire vm add VM [--fix-xml] [--review PROFILE] [--kernel RELEASE] [--priority N] [--min-ram SIZE] [--max-ram SIZE] [--free-ram SIZE]` | Starts managing a VM: checks its definition (fixing it with `--fix-xml`, keeping the old one), fills in its review from the definition and asks the rest, writes its settings, restarts it if its definition changed, and applies |
| `rampire vm attest VM [--restart] [--no-apply]` | Attests a running VM again from its review profile, then applies. `--restart` cold-restarts it first |
| `rampire vm attest --drifted` | Lists every running VM whose live evidence no longer matches its attestation, asks, and attests them again |
| `rampire vm remove VM` | Stops managing a VM at once. It keeps its memory until its next cold start, which begins on boot RAM alone. Its file is kept in `parked/` |

### Watching

| Command | What it does |
|---|---|
| `rampire status [VM]` | The controller's view of one VM or all of them |
| `rampire top [--once] [--interval-seconds N] [--events N]` | The pool, live |
| `rampire watch [VM] [--once] [--interval-seconds N] [--guest-interval-seconds N] [--events N] [--no-guest]` | One VM, live |
| `rampire logs [-f] [--since TIME]` | The controller's journal |

### Recovering

| Command | What it does |
|---|---|
| `rampire recover VM REASON` | Clears a VM's latch once a person has checked it is safe: stops the controller, settles a stuck removal at what the guest kept, records the reason, starts the controller |
| `rampire daemon` | Runs the controller; the unit runs this |

## Files

| Path | What it holds |
|---|---|
| `/etc/rampire/global.toml` | Settings for the whole host |
| `/etc/rampire/vms.d/VM.toml` | One VM; the file's name is the VM's |
| `/etc/rampire/reviews/PROFILE.toml` | A review profile |
| `/etc/rampire/attestations/VM.attestation.json` | A VM's attestation |
| `/etc/rampire/parked/` | VM files `vm remove` took out |
| `/var/lib/rampire/applied/ID/`, `current` | Applied copies, and the one the controller runs |
| `/var/lib/rampire/evidence/` | What each apply and rollback did |
| `/var/lib/rampire/state/` | Each VM's durable request state |
| `/var/lib/rampire/domain-backups/` | Definitions `vm add --fix-xml` replaced |
| `/run/rampire/metrics.prom` | Metrics, every round |

Sizes are written in quotes with a unit: `"512MB"`, `"3GB"`, `"1.5GB"`.
MB and MiB mean the same (1 GB = 1024 MB).

## `global.toml`

Every setting is optional.

| Setting | Default | Meaning |
|---|---|---|
| `pool` | none | Device memory shared by every VM, on top of each one's boot RAM. Required with more than one VM |
| `host_reserve` | `"4GB"` | Memory the host keeps for itself; growth stops before crossing it |
| `observe_only` | `false` | Decide and log everything, but change no VM's memory |
| `poll_interval_seconds` | `5` | Seconds between checks, 1 to 10 |
| `command_timeout_seconds` | `10` | Limit on each libvirt command |
| `convergence_timeout_seconds` | `300` | How long a Windows removal may stop moving before it is settled |
| `libvirt_uri` | `"qemu:///system"` | The libvirt connection |
| `retention` | `10` | Applied copies and their records kept; at least 2 |

## `vms.d/VM.toml`

| Setting | Meaning |
|---|---|
| `alias` | The VM's virtio-mem device alias |
| `priority` | Who is served first when memory is short; higher first |
| `min_ram` | The smallest the VM is made, boot RAM included |
| `max_ram` | The largest it may grow. For Linux, at most about four times boot RAM |
| `free_ram` | Spare memory the guest keeps; it asks for more below this and returns memory above it. A multiple of eight device blocks |
| `guest_os` | `"linux"`, or omitted for Windows |
| `review` | The review profile in `reviews/` it is attested with |
| `telemetry_path` | Optional host file for a diagnostic copy of each publication |
| `install_path`, `service_account` | Windows only |

`global` is not a VM name: `rampire set` uses it for `global.toml`.

## `reviews/PROFILE.toml`

What RAMpire can't prove from the running VM. Each answer is
`{ value = ..., source = "detected" | "confirmed" }`; `workload_reviewed`
can only be confirmed by a person. See the
[example profile](../deploy/examples/reviews/linux-standard.toml).

| Setting | Meaning |
|---|---|
| `version` | 1 |
| `guest_os` | `"linux"` or `"windows"`, matching the VM |
| `workload_reviewed` | The VM's programs cope with memory being added and taken away |
| `no_vdpa`, `no_rdma_migration`, `no_vfio_nvme`, `no_mlock`, `no_secure_virtualization`, `no_unsupported_vhost_user` | None of these, each of which can hold on to guest memory, is used |
| `balloon_resize_inactive` | The memory balloon isn't used to resize the VM |
| `memory_slot_budget`, `vfio_mapping_budget` | The budgets reviewed for the device |
| `guest_driver_version` | The kernel release on Linux (`"linux 6.12.0"`), the `viomem` driver version on Windows |
