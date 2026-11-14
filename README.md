# RAMpire

<p align="center">
  <img src="docs/Mascot/Hero_Logo_Rampire.png" alt="RAMpire" width="480">
</p>

One controller on a KVM host shares a pool of RAM between many VMs. Each
guest works out when it needs more or less memory and asks for it. The host
admits the request within the pool, the VM's limits and the host's own
reserve, resizes the VM and reports what actually happened.

Linux guests are resized with virtio-mem. Windows guests are moving to
Hyper-V Dynamic Memory (see [the roadmap](docs/roadmap.md), milestone W).

## Status

| Part | State |
|---|---|
| Host controller (`rampire`) | Working on RHEL 10 / libvirt / QEMU, managing several Linux VMs from one pool |
| Linux guest service | Working: PSI pressure triggers, cache floor, readiness report, control channel |
| Windows guest service | Built on the shared guest crate, not deployed. Its virtio-mem path is paused because of an upstream `viomem` driver bug; see [archive/viomem](archive/viomem/README.md) |
| Windows on Hyper-V Dynamic Memory | Not started |

This is development software. Read [administration](docs/administration.md)
before running it on a host you care about.

## Layout

| Path | What it is |
|---|---|
| [crates/rampire-core](crates/rampire-core) | Policy, request protocol, wire and settings formats (any platform) |
| [crates/rampire-guest](crates/rampire-guest) | Shared guest code: request store, worker, transports, service loop |
| [host](host) | The controller daemon and its CLI (Linux) |
| [guest-linux](guest-linux) | Linux guest service, workload tool and Debian package |
| [guest-windows](guest-windows) | Windows guest service and workload tool |
| [tools/xtask](tools/xtask) | Gate, deploy, watch, metrics and qualification (`cargo xtask`) |
| [deploy](deploy) | Example configuration and deployment scripts |
| [archive/viomem](archive/viomem) | Paused Windows virtio-mem workarounds and driver patch |

## Build and check

Every change passes the gate before review:

```bash
tools/gate.sh
```

It runs formatting, a release build, all tests and Clippy (warnings denied)
on a Linux build host, and the Windows guest's checks natively on Windows.
See [development](docs/development.md) for setup and platform details.

## Documentation

| Document | Covers |
|---|---|
| [docs/usage.md](docs/usage.md) | Start here: plain-language guide to the settings, changing them, and checking it works |
| [docs/administration.md](docs/administration.md) | Installing the controller, adding VMs, guest services, recovery, upgrades |
| [docs/development.md](docs/development.md) | Building, the gate, which checks need which platform |
| [docs/tools.md](docs/tools.md) | `watch`, `metrics` and `qualification` |
| [docs/roadmap.md](docs/roadmap.md) | Goals, open work, decisions and the rule for importing third-party code |
| [docs/names.md](docs/names.md) | Every name RAMpire uses, and what it replaced |

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The upstream driver patch in
[archive/viomem/driver-patch](archive/viomem/driver-patch) keeps upstream's
BSD-3-Clause license.
