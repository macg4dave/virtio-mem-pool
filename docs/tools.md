# Tools

Observation and development tools. Run `watch`, `metrics` and
`qualification` on the KVM host; they read the controller's journal,
libvirt and each guest's publication. `CONFIG` below is the host list the
controller runs (installed as `/etc/virtio-mem-host/host_list.toml`); with
several VMs, `--vm NAME` picks one. They follow the one
`virtio-mem-host@INSTANCE` unit installed on the host, or the one named
with `--instance INSTANCE`.

## Guest publications

While the controller runs it keeps each guest's latest validated
publication in `/run/virtio-mem-host/<vm_name>.publication.json`, readable
by any local user. The tools read that copy first. Without it (the
controller stopped) they read the guest's control file through the QEMU
guest agent, which works only for a guest on the control files: a guest on
the control channel writes no file, so only the controller's copy shows it.

## Watching the controller

```bash
cargo xtask watch --config CONFIG
```

A live view of one VM: device size, requested and current memory, the
guest's latest publication and the controller's events.

## Recording a run

```bash
cargo xtask metrics --config CONFIG [--duration-seconds N]
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
writes one directory under `.artifacts/metrics/<vm_name>-<UTC time>/` (or
`--output DIR`):

| File | Contents |
|---|---|
| `samples.csv` | Every sample period (`--interval-seconds`): host total/available RAM, headroom over `host_min_headroom_bytes`, swap used, QEMU RSS, controller active, device size/requested/current. Every guest read (`--guest-interval-seconds`, default 5): agent read time and success, publication age, guest available RAM, load, commit and commit headroom, cache, memory signal, pending request. Appended after those: controller unit state, pid, memory, CPU % and restart count; QEMU pid; libvirt domain state (`virsh domstate --reason`); and per guest read the service session, commit peak and kernel paged/non-paged pools. Sizes in MiB. |
| `events.jsonl` | Every controller journal entry written during the run, with its fields split out, plus markers and `metrics_*` lifecycle events: controller failed/stopped/started/restarted, QEMU exited/started/restarted, domain state changes, guest service session changes, guest publication stalled/resumed, guest agent lost/restored and publication read failed/restored. |
| `incidents.jsonl` | One record per incident, with its bundle path, pid and details (exit status, systemd result, panic text, last controller lines, core dump flag, publication gap). Empty when nothing crashed. |
| `incidents/NNN-TIME-COMPONENT.txt` | Evidence bundle per incident, collected at detection: panic text and last controller lines, `systemctl status`, `coredumpctl info` when the controller dumped core, the unit's last 400 journal lines, the QEMU log tail (root only) for VM incidents, the guest's last and first publication around a service restart, and the 60 `samples.csv` rows before detection. |
| `summary.json` | Min/max/mean of the above, time spent with requested ≠ current, guest read failures and stale publications, publication stall and agent outage durations, completed requests by operation and outcome (MiB asked and got), grant→completion and guest issue→completion latency, refusals, event counts, controller pids, memory and CPU, samples per domain state, guest service sessions, incident counts and records, and latches. Rewritten every minute, at exit and after each incident. |

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

### Diagnostic evidence in captures

Each capture now also writes `run.json` and `guest-publications.jsonl`.
`run.json` records the recorder checkout and executable hash, running host
executable hash, requested and installed settings, live and saved domain XML,
host boot ID, and readable initial checkpoint/attestation. Missing permissions
and metadata are recorded as errors rather than requiring elevation. The first
readable guest publication adds its build identity, initial request and
driver version: the on-disk `viomem.sys` version on Windows (not proof of
the loaded driver; paused with the viomem path), the kernel release on Linux. A final status distinguishes a finished run from a recording
interrupted before its manifest could be finalised.

Guest publication snapshots retain every published field when the request,
service session, native signal or policy reason changes, and otherwise every
30 seconds. Pausing recording pauses these snapshots. Policy reason, desired
allocation, safety floor, history readiness, retry delay and guest boot uptime
are also appended to CSV. Desired targets may be unavailable while device
geometry is pending; this is not zero demand. A new service session combined
with decreased guest boot uptime records a separate reboot event; a service
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

Guests now publish version 6, which means the guest takes its settings
from the host. Hosts accept versions 4 to 6 and send settings only to a
version 6 guest or one that has published nothing yet. Upgrade the host
before either guest service. The durable memory request and host-status schemas are
unchanged; diagnostic fields never authorise memory changes. Existing captures
without diagnostic fields remain readable.


## Watching guest RAM

```powershell
powershell -ExecutionPolicy Bypass -File tools\windows\Watch-Ram.ps1
```

`Watch-Ram.ps1` runs inside a Windows guest and reads memory directly with
`GetPerformanceInfo`. It needs no configuration and no running service.

```bash
tools/linux/watch-ram.sh
```

cargo xtask linux deploy SSH_DESTINATION --output PATH [--apply]
cargo xtask linux status SSH_DESTINATION
cargo xtask linux uninstall SSH_DESTINATION --output PATH [--apply] [--discard-state]

Install, inspect or remove the Linux guest service on a development guest
over SSH (the account needs passwordless `sudo`). Deploy builds the stamped
binary first, installs it with the unit, the RAM monitor
(`virtio-mem-watch-ram`) and the qualification workload
(`virtio-mem-workload`), restarts the service and checks what it reads back.
Without `--apply` they only record what they would do. Uninstall keeps the
durable request state unless told to discard it.

cargo xtask linux build

Release-builds the Linux guest service with the checkout's build identity
(`<commit>` or `<commit>-dirty.<source digest>`) compiled in, and checks that
the binary reports it.
