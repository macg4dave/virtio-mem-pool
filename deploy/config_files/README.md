# Test configuration files

These configuration files are kept here for testing with `win11_gpu`; their
long-term location will change later. Edit here, then apply. Generated files
(attestation, deploy evidence) stay in `.artifacts/`.

| File | Installed as | Apply |
|---|---|---|
| [global-settings.toml](global-settings.toml) | `/etc/virtio-mem-host/global-settings.toml` | `install-host.sh INSTANCE VM=ATTESTATION ...` |
| [host_list.toml](host_list.toml) | `/etc/virtio-mem-host/host_list.toml` | `install-host.sh INSTANCE VM=ATTESTATION ...` |
| [recover-latch.sh](recover-latch.sh) | — | see below |
| [win11_gpu_settings.toml](win11_gpu_settings.toml) | `C:\ProgramData\VirtioMemService` config + service | `make win11-deploy` |

`make win11-deploy` installs the build last fetched by `make win11-build`.
How make reaches the guest (SSH alias, guest checkout, timeouts) is set in
[windows-build.mk](windows-build.mk). Watch the live loop with
`cargo xtask watch --config deploy/config_files/host_list.toml`.

## Recording a stress test

```bash
cargo xtask metrics --config deploy/config_files/host_list.toml [--duration-seconds N]
```

Read-only, like `watch`. On a terminal it opens a live UI: host, device and
guest panels, a memory chart (host available, guest available, device
current and requested), request results and latencies, problem and incident
counters, and the controller's events alongside lifecycle events. A red
`N INCIDENTS` badge in the header means a crash or restart was recorded.
The controller row shows its pid, memory and CPU, or how it exited. `--plain`, or output that is not a terminal, prints
one line per sample instead. It records until `q`/Ctrl+C or
`--duration-seconds`.

| Key | Action |
|---|---|
| `-` / `+` | Sample period (0.5 s – 120 s; default 5 s) |
| `[` / `]` | Guest-agent read period (1 s – 120 s; default 5 s) |
| space | Pause / resume logging (the UI keeps updating; nothing is written) |
| `n` | Sample now |
| `m` | Type a marker (e.g. "started 12 GB allocation"), Enter saves |
| `s` | Save `summary.json` now |
| `w` | Chart window: 5 min, 15 min, 1 h, whole run |
| `q` | Quit |

Period changes, pauses and markers are written to `events.jsonl` as
`"event": "marker"` entries, so the logs show when each happened. Each run
writes one directory under `.artifacts/metrics/win11_gpu-<UTC time>/` (or
`--output DIR`):

| File | Contents |
|---|---|
| `samples.csv` | Every sample period (`--interval-seconds`): host total/available RAM, headroom over `host_min_headroom_bytes`, swap used, QEMU RSS, controller active, device size/requested/current. Every guest read (`--guest-interval-seconds`, default 5): agent read time and success, publication age, Windows available RAM, load, commit and commit headroom, cache, memory signal, pending request. Appended after those: controller unit state, pid, memory, CPU % and restart count; QEMU pid; libvirt domain state (`virsh domstate --reason`); and per guest read the service session, commit peak and kernel paged/non-paged pools. Sizes in MiB. |
| `events.jsonl` | Every controller journal entry written during the run, with its fields split out, plus markers and `metrics_*` lifecycle events: controller failed/stopped/started/restarted, QEMU exited/started/restarted, domain state changes, guest service session changes, guest publication stalled/resumed, guest agent lost/restored and publication read failed/restored. |
| `incidents.jsonl` | One record per incident, with its bundle path, pid and details (exit status, systemd result, panic text, last controller lines, core dump flag, publication gap). Empty when nothing crashed. |
| `incidents/NNN-TIME-COMPONENT.txt` | Evidence bundle per incident, collected at detection: panic text and last controller lines, `systemctl status`, `coredumpctl info` when the controller dumped core, the unit's last 400 journal lines, the QEMU log tail (root only) for VM incidents, the guest's last and first publication around a service restart, and the 60 `samples.csv` rows before detection. |
| `summary.json` | Min/max/mean of the above, time spent with requested ≠ current, guest read failures and stale publications, publication stall and agent outage durations, completed requests by operation and outcome (MiB asked and got), grant→completion and Windows issue→completion latency, refusals, event counts, controller pids, memory and CPU, samples per domain state, guest service sessions, incident counts and records, and latches. Rewritten every minute, at exit and after each incident. |

An incident is a controller that stops other than cleanly (any systemd
`Result` except `success`, including one already failed when recording
starts), a new guest service session (service restart or guest reboot; the
host cannot tell which), a QEMU process exit or replacement, or a domain in
`crashed`/`panicked` state. A `systemctl stop` is an event, not an incident.
Lifecycle events and incidents are written even while logging is paused.
Windows' own record of a service crash stays in the guest: System log,
Service Control Manager 7031/7034, and Application log, Application Error
1000; guest bundles include a `Get-WinEvent` query for the right time.

The recorder uses a persistent libvirt connection for repeated XML/QGA reads.
It appends host memory PSI (`some`/`full` avg10 percentages), swap-in/out pages
per second, major faults per second and sample collection milliseconds to CSV.
Rates are host-wide; the first sample has no rate. Unavailable interfaces leave
blank values. Swap occupancy alone does not indicate active paging.

Summary request totals include terminal PoolEmpty and Rejected results as well
as completed/partial movements. Refusal counts include zero grants and final
headroom rejections. Growth and removal latencies are also separate, and pending
operations show age, original allocation goal and remaining bytes. The last
observed guest request may persist briefly after host completion until the guest
consumes its result. Full error messages remain in events; publication read
failures are distinguished from explicit guest-agent connection loss.

### Diagnostic evidence in metrics captures

Each capture now also writes `run.json` and `guest-publications.jsonl`.
`run.json` records the recorder checkout and executable hash, running host
executable hash, requested and installed settings, live and saved domain XML,
host boot ID, and readable initial checkpoint/attestation. Missing permissions
and metadata are recorded as errors rather than requiring elevation. The first
readable guest publication adds its build identity, initial request and on-disk
`viomem.sys` version when available. File version is not proof of the loaded
driver version. A final status distinguishes a finished run from a recording
interrupted before its manifest could be finalised.

Guest publication snapshots retain every published field when the request,
service session, native signal or policy reason changes, and otherwise every
30 seconds. Pausing recording pauses these snapshots. Policy reason, desired
allocation, safety floor, history readiness, retry delay and Windows boot uptime
are also appended to CSV. Desired targets may be unavailable while device
geometry is pending; this is not zero demand. A new service session combined
with decreased Windows boot uptime records a separate reboot event; a service
restart alone does not establish a reboot.

Host events record the observed request, per-request pool/device/guest/host
limits, all tied limiting factors, alignment loss, command start and duration.
Unchanged pending-device health is reduced to a 30-second heartbeat, retaining
state changes, request identity, idle time and timeout remaining. Existing
completion events identify convergence. Guest result-consumed events add the
sample time at which Windows consumed the result; compare clocks with care
when interpreting host-to-guest timing.

Sustained host reserve shortfall, stale readable publications, device stalls,
and repeated capacity failures create `performance` incident bundles with the
same journal/sample context as lifecycle incidents. Shortfall/staleness requires
30 seconds; a device must first stop moving for 30 seconds and remain stalled
for another 30. Five or more zero grants or rejected commands within a minute
must persist as a condition for 30 seconds. Bundles are emitted once per episode
and rearm after recovery. Durations use the recorder's monotonic clock.

These changes publish guest telemetry version 5. Updated readers accept version
4, but older hosts reject version 5 and its new fields. Upgrade the host before
the Windows service. The durable memory request and host-status schemas are
unchanged; diagnostic fields never authorise memory changes. Existing captures
without diagnostic fields remain readable.

## Recovering a latched controller

When the controller latches (`actuation_latched` in `watch`/`metrics`), it
stops resizing until an operator clears it. Install the current host build
and attestation, then run

```bash
deploy/config_files/recover-latch.sh "why it is safe to resume"
```

It stops the controller, clears the latch and starts the controller again.
`clear-latch` first settles a stuck shrink (requested set to what the guest
kept) when requested ≠ current.

## Configuration ownership

- `global-settings.toml`: shared device pool, Linux headroom, observe-only mode
  and three controller timings. Omitted keys use built-in defaults. A missing
  global file uses all defaults; multiple VMs still require an explicit pool.
- `host_list.toml`: one `[[vm]]` per VM with `vm_name`, libvirt device `alias`
  and `priority`. Optional `telemetry_path` writes diagnostics on the Linux host.
- `<vm_name>_settings.toml`: `min_ram`, `max_ram`, `free_ram`, `install_path`,
  `service_account`, `vm_name` and optional Windows `telemetry_path`. A Linux
  guest instead sets `guest_os = "linux"`; see [Linux guests](#linux-guests).

The host list loads sibling files; paths do not depend on the working directory.
VM names use lower-case ASCII letters, digits, `-` and `_`, beginning with a
letter or digit, at most 64 characters. The exact filename and internal name
must agree. Duplicate entries and unknown keys fail validation.

`min_ram` and `max_ram` describe **total guest RAM**, in binary size units or
bytes, aligned to the live device block size (64 MiB for `win11_gpu`).
The policy derives the device allocation from observed
RAM and live device state; there is no configured boot/base-RAM value. Both
Windows requests and host grants honour these bounds. The underlying VM must
have enough supported memory capacity; impossible limits fail closed. This
change does not edit libvirt VM definitions.

`free_ram` is the memory a guest keeps free above what it is using, and the
only policy size an administrator sets. There is no built-in value. The guest
asks for more when free memory falls below it and returns memory when it has
more than that spare. The other sizes follow from it: reclaim never plans below
half of it (also the level treated as critical), memory is returned only once
the surplus exceeds an eighth of it, and native Low pressure asks for at least
half of it. A Windows guest removes at most one and a half times `free_ram`
per request; a Linux guest may return its whole surplus at once. Use a multiple
of eight device blocks so every derived size stays block aligned.

Reclaim history (1 second) and stop timeout (30 seconds) are built in. One
polling interval drives host polling and guest sampling; freshness, request
lifetime and maximum sample gaps follow it. Device size and block size come
from live state. `/usr/bin/virsh`, the Windows service name and control-file
path are built in. Per-VM state and attestation paths are derived from the
validated name.
Freshness/request lifetime is six poll intervals and the maximum sample gap is
three. Windows service identity is `VirtioMemService`.


### Capacity and stalled removals

Growth admission leaves an additional 256 MiB above the configured host reserve
to absorb changes between planning and execution. The final check still enforces
the configured hard reserve; other host activity can lower available memory
later. Failed or partial guest requests retry with exponential delays from one
to thirty seconds. A transition into native Low or across a numeric safety floor bypasses an Add delay; an
opposite-direction request is also allowed immediately. Retained requests are
always retransmitted unchanged while unresolved.

The unpatched-driver shrink workaround is still present. If it reaches its
minimum target and memory then stops moving for the convergence timeout, the
controller settles at the actual allocation and reports the original request's
result. This releases the request slot for renewed pressure. It does not force
Windows to return unevictable memory. Normal progress is not interrupted by
this idle deadline.

## Optional telemetry and the control channel

Both optional telemetry paths may be omitted. No diagnostic publisher or output
file is created in that case. When configured, the guest path writes Windows
diagnostics and the host path writes a separate Linux diagnostic file. Output
failures are reported without stopping memory control. Provision write access
for the service account if choosing a custom diagnostic directory.

Memory sampling needed by the guest policy and the request/reply channel remain
active. The internal control channel retains its historical filename
`C:\ProgramData\VirtioMemService\telemetry.json` to preserve pending requests;
it is no longer a telemetry setting. Its `.host-status` and request-state
sidecars are derived automatically. Do not use this path or its sidecars as an
optional diagnostic output. `watch` and `metrics` read the control data when
explicitly run and require no telemetry settings.

## Startup memory

A Windows Restart can retain QEMU's live virtio-mem requested size and does not
apply the saved domain definition. To reset the requested allocation to the
saved zero value, use a full guest shutdown followed by a start. The supplied
`restart-vm-attest.sh` performs that cold restart for reviewed geometry changes;
it stops the controller first and refreshes attestation before installation.
Do not clear retained request state to reduce RAM: an unresolved request must
still receive its correlated result. A service-session change alone is not
proof of a VM reboot and must not trigger forced memory removal.

The live block size can therefore differ from the saved block size until a cold
restart. Inspect both live and inactive XML when comparing test runs.


The persistent libvirt definition starts virtio-mem with zero requested bytes.
No startup allocation is stored in these application settings. Windows requests
device RAM when its memory policy needs it; the boot-memory allocation remains
a property of the VM definition. Updating the saved startup setting does not
force-unplug memory from an already running guest.

## Deployment and migration

The installer requires the controller instance and one explicit, current
attestation per VM; it never selects evidence from an old test run:

```bash
deploy/config_files/install-host.sh win11_gpu win11_gpu=/path/to/current-attestation.json
```

`install-host.sh` validates and snapshots the global settings, host list and
all listed VM files, hashes the complete snapshot and installs the same files
under `/etc/virtio-mem-host/`. Existing installed files and obsolete instance
configs are backed up. The unit now reads `host_list.toml`. Run one controller
for the host list: separate controllers cannot share pool accounting.

`make win11-deploy` reads the selected `<vm_name>_settings.toml` and sibling
global settings and generates Windows runtime `config.json` schema 8. This
JSON is a deployment output, not another administrator settings file. Deploy
matching new host and Windows binaries before runtime verification.

The tracked values were carried over: 15 GiB device pool, 4 GiB host headroom,
5/10/300-second timings, priority 100, Windows account and executable path.
The reviewed VM now has explicit total-RAM limits of 6–27 GiB. Its previous
telemetry path remains the internal channel, so no optional output is enabled.

For other deployments, keep copies of the old files before migration. Rename
`host.toml` to `host_list.toml`, use `vm_name` instead of `name`, and rename
`vm-settings.toml` to `<vm_name>_settings.toml` with the new field set. Create
one `<vm_name>_settings.toml` per entry. Remove `global_settings` imports and
move global values into the sibling `global-settings.toml`. Old manifests,
inline VM/global configs and JSON administrator manifests are rejected with a
migration error rather than silently accepted. Review custom policy tuning
against the documented built-in defaults before replacing it. Custom service
names, control paths or checkpoint locations require explicit state migration;
do not discard pending requests, checkpoints or latches to get past validation.
Windows deployment retains configuration/binary/registry backups and refuses
an automatic upgrade from older wire schemas when their policy or channel
settings differ from the generated settings.

## Adding another VM

Create its own `<vm_name>_settings.toml`, add its identity, alias and priority
to `host_list.toml`, and pass one `--attestation VM=PATH` per VM to `host-deploy`.
The installed attestation is `/etc/virtio-mem-host/<vm_name>.attestation.json`;
retained state is `/var/lib/virtio-mem-host/state/<vm_name>-requests.json`.
Deploy the Windows service separately for each guest. `pool_bytes` describes
shared **device** RAM; the per-VM bounds describe total RAM.

## Linux guests

One controller manages Windows and Linux guests from the same host list. A
VM's settings file selects the guest with `guest_os = "linux"`; omitted means
Windows. A Linux settings file holds only `min_ram`, `max_ram`, `free_ram`,
`guest_os` and `vm_name`: `install_path`,
`service_account` and `telemetry_path` are Windows settings and are rejected.
[debian13_settings.toml](debian13_settings.toml) is the tracked example. It is
not yet in `host_list.toml`, so the installed controller does not manage it.

The Linux guest runs `virtio-mem-guest` under systemd
([unit](../../guest-linux/systemd/virtio-mem-guest.service)) with the same policy,
request protocol and wire format as the Windows service. Its identity and
files are built in:

| Item | Linux guest |
|---|---|
| Service identity on the wire | `virtio-mem-guest` |
| Control publication | `/var/lib/virtio-mem-guest/control.json` |
| Host status, request state | `control.json.host-status`, `control.json.request-state` |
| Generated configuration | `/etc/virtio-mem-guest/config.json` |

Generate the configuration from the settings, then install the release binary,
configuration and unit in the guest. The guest also needs `qemu-guest-agent`
running, with file commands allowed, because the host reads and writes the
control files through it.

```bash
cargo xtask linux config deploy/config_files/debian13_settings.toml > config.json
cargo build --release --locked -p virtio-mem-guest-linux
# In the guest:
install -D -m 0755 virtio-mem-guest /usr/local/libexec/virtio-mem-guest
install -D -m 0644 config.json /etc/virtio-mem-guest/config.json
install -m 0644 virtio-mem-guest.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now virtio-mem-guest
```

Linux memory is reported in the shared telemetry fields as follows.

- Physical total and available are `MemTotal` and `MemAvailable`.
- The commit limit is `MemTotal + SwapTotal` and the commit total is that less
  `MemAvailable + SwapFree`. The kernel's `CommitLimit`/`Committed_AS` are not
  used: they are not enforced under the default overcommit heuristic. Strict
  overcommit (`vm.overcommit_memory=2`) is not modelled.
- Native pressure comes from `/proc/pressure/memory` (`some avg10`): 10 % or
  more is Low, exactly zero is High and anything between is Neutral. As on
  Windows, only High permits reclaim and Low is an urgent growth trigger. A
  kernel without pressure stall information reports nothing, which allows
  growth and never reclaim.
- The driver version field carries the kernel release, since the Linux
  virtio-mem driver ships with its kernel.

The guest polls on the configured interval; it has no wake channel yet. Up to
four consecutive failed polls are skipped with reclaim history reset, because
host status and the guest's counters are not read atomically; the fifth ends
the service and systemd restarts it. A retained request survives both.

The host applies the Windows driver workarounds (usable-region widening and
shrink extension) only to Windows guests. For a Linux guest a growth is held
until the guest plugs it, and a removal that has not moved for the convergence
timeout settles at what the guest returned and reports a partial result.

### Prompt memory notifications

Add the optional dedicated port while the controller is stopped, then refresh
compatibility attestation before restarting it. Do not reuse the QEMU guest-agent
port. Libvirt supplies the Unix socket path; the host discovers it in domain XML.

```xml
<channel type='unix'>
  <target type='virtio' name='org.virtio-mem.wake.0'/>
  <alias name='ua-virtiomem-wake'/>
</channel>
```

The host service needs the `qemu` supplementary group to connect to libvirt's
socket. Windows deployment provisions LocalService read/write access only on
the named wake-port device instance, backs up its previous security property
and restarts that port if changed. It does not alter the QEMU guest-agent port.
Installations without this port continue polling and report the missing wake
channel. Polling remains a fallback; native Low pressure, result arrival and
libvirt size-change events can wake earlier. Notification bytes carry no RAM
values or authorisation. The same durable protocol and reserve checks apply.
