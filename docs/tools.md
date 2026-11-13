cargo xtask metrics --config deploy/config_files/host_list.toml

powershell -ExecutionPolicy Bypass -File C:\Users\Dave\Watch-Ram.ps1

cargo xtask watch --config deploy/config_files/host_list.toml

Metrics loads the host list and its sibling settings files. It reads the built-in
guest control publication over QGA; optional telemetry paths can be omitted.

`Watch-Ram.ps1` reads Windows memory directly with `GetPerformanceInfo`. It
does not load configuration or require the service or telemetry output.

The Windows deployment helper also provisions access to the optional
`org.virtio-mem.wake.0` serial port for LocalService. It saves the previous device
security property with the deployment backup; a missing port retains polling.
See [the deployment guide](../deploy/config_files/README.md#prompt-memory-notifications)
for the VM channel and host group requirements. RAM growth requests cover the
calculated shortage; reclaim uses a 1-second continuous history window.

Metrics captures include `run.json` provenance and `guest-publications.jsonl`
state-change/30-second snapshots. CSV includes guest policy explanations;
sustained performance problems produce incident bundles alongside crashes.
See [capture diagnostics](../deploy/config_files/README.md#diagnostic-evidence-in-metrics-captures).

tools/linux/watch-ram.sh

The Linux counterpart of `Watch-Ram.ps1`, run inside the guest without root.
It reads `/proc/meminfo` on every tick and shows total, in-use and available
RAM, file cache and how much of it the guest service is keeping, memory
pressure, the page-cache refault rate, a history graph and the last changes
in total RAM. `--csv` prints one row per sample; `-h` lists the options.

cargo xtask linux config deploy/config_files/debian13_settings.toml

Prints the Linux guest service configuration generated from a VM settings file
and its sibling global settings. See [Linux guests](../deploy/config_files/README.md#linux-guests).

cargo xtask linux build

Release-builds the Linux guest service with the checkout's build identity
(`<commit>` or `<commit>-dirty.<source digest>`) compiled in, and checks that
the binary reports it.
