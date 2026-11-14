# Names

The names table for the clean break ([roadmap](roadmap.md) R1). R2 and R3
rename everything to the right-hand column, once. Nothing is kept under an
old name (decision D9).

Status: **draft, waiting for review.** No code changes until it's
reviewed. The questions at the end need an answer first.

## Rule

- Ours is renamed. `virtio-mem`, `virtio_mem`, `VirtioMem` and
  `VIRTIO_MEM` become `rampire`, `rampire`, `Rampire` and `RAMPIRE`.
- The device's own names stay. These are the kernel and QEMU device and
  its parts: the libvirt model `virtio-mem`, the QEMU device
  `virtio-mem-pci`, the guest driver `virtio_mem`, a site's device alias
  such as `ua-virtio-mem0`, and Rust items that describe the device
  (`virtio_mem.rs`, `virtio_mem_xml.rs`, `VirtioMemCompatibility`,
  the `virtio_mem_driver` readiness fact).
- In prose the product is **RAMpire**. Everywhere else it's lower case
  (`rampire`), or `Rampire` where the language wants CamelCase.

## Project

| What | Now | New |
|---|---|---|
| Product | virtio-mem pool | RAMpire |
| GitHub repository | `macg4dave/virtio-mem-pool` | `macg4dave/rampire` (renamed by the owner, R7) |
| Homepage in packages and units | `https://github.com/macg4dave/virtio-mem-pool` | `https://github.com/macg4dave/rampire` |

## Crates

The workspace directories `host/`, `guest-linux/`, `guest-windows/` and
`tools/xtask/` keep their names; they don't carry ours. The directories
under `crates/` follow their crate names.

| Directory | Now | New | Contents |
|---|---|---|---|
| `crates/virtio-mem-core` → `crates/rampire-core` | `virtio-mem-core` | `rampire-core` | Protocol, settings, pool, units, shared by every side |
| `crates/virtio-mem-guest` → `crates/rampire-guest` | `virtio-mem-guest` | `rampire-guest` | Shared guest code (library) |
| `crates/rampire-ops` (new, O1.1) | — | `rampire-ops` | Operator views: `status`, `top` (today's `pool` and `watch`), controller status. No dependency on a checkout. Used by `rampire` and by `xtask` |
| `host/` | `virtio-mem-host` | `rampire` | The host binary: `rampire daemon` and every operator command (D7) |
| `guest-linux/` | `virtio-mem-guest-linux` | `rampire-guest-linux` | Linux guest service |
| `guest-windows/` | `virtio-mem-guest-windows` | `rampire-guest-windows` | Windows guest service (parked) |
| `tools/xtask/` | `virtio-mem-xtask` | `xtask` | Development only |

## Binaries

| Now | New | Notes |
|---|---|---|
| `virtio-mem-host` | `rampire` | One binary; the unit runs `rampire daemon` |
| `virtio-mem-guest` (Linux) | `rampire-guest` | |
| `virtio-mem-service.exe` (Windows) | `rampire-guest.exe` | Same name as on Linux |
| `virtio-mem-workload` | `rampire-workload` | Test tool, both guests |
| `virtio-mem-watch-ram` (guest `.deb`) | `rampire-watch-ram` | |

## Host

| What | Now | New |
|---|---|---|
| Unit | `virtio-mem-host@.service` | `rampire.service` (no instance, D2) |
| Unit description | `Virtio-mem controller for %i` | `RAMpire memory controller` |
| Service account and group | `virtio-mem-host` | `rampire` |
| Polkit rule file | `50-virtio-mem-host.rules` | `50-rampire.rules` |
| Binary, package install | — | `/usr/bin/rampire` |
| Binary, development install | `/usr/local/libexec/virtio-mem-host` | `/usr/local/bin/rampire` |
| Configuration (the draft) | `/etc/virtio-mem-host/` | `/etc/rampire/` |
| Global settings | `global-settings.toml` | `global.toml` |
| VM list and VM settings | `host_list.toml`, `NAME_settings.toml` | `vms.d/NAME.toml` (R4) |
| Review | JSON review | `reviews/NAME.toml` (R5) |
| Applied configuration | — | `/var/lib/rampire/applied/` (O1.4) |
| Attestations | `/etc/virtio-mem-host/attestations/` | `/var/lib/rampire/attestations/` |
| Evidence | in the checkout | `/var/lib/rampire/evidence/` (O1.5) |
| Durable state | `/var/lib/virtio-mem-host/state` | `/var/lib/rampire/state` |
| Runtime directory | `/run/virtio-mem-host` | `/run/rampire`, mode 0750 (O1.8) |
| Metrics | — | `/run/rampire/metrics.prom` (O4.2) |
| Host package | — | `rampire` (RPM and `.deb`) |

## Guest

| What | Now | New |
|---|---|---|
| Linux unit | `virtio-mem-guest.service` | `rampire-guest.service` |
| Linux unit description | `Virtio-mem guest memory requests` | `RAMpire guest memory requests` |
| Linux binary, package install | `/usr/libexec/virtio-mem-guest` | `/usr/libexec/rampire-guest` |
| Linux binary, development install | `/usr/local/libexec/virtio-mem-guest` | `/usr/local/libexec/rampire-guest` |
| Linux state directory | `/var/lib/virtio-mem-guest/` | `/var/lib/rampire-guest/` |
| Linux control file | `/var/lib/virtio-mem-guest/control.json` | `/var/lib/rampire-guest/control.json` |
| Guest service name on the wire (`service_name`) | `virtio-mem-guest` | `rampire-guest` |
| Guest package | `virtio-mem-guest` (`.deb`) | `rampire-guest` (`.deb` and RPM) |
| Windows service name | `VirtioMemService` | `RampireGuest` |
| Windows install directory | `C:\Program Files\VirtioMemService\` | `C:\Program Files\RAMpire\` |
| Windows data directory | `C:\ProgramData\VirtioMemService\` | `C:\ProgramData\RAMpire\` |

## Wire identifiers (R3, decision D8)

| What | Now | New |
|---|---|---|
| Control channel port | `org.virtio-mem.control.0` | `org.rampire.control.0` |
| Guest device path | `/dev/virtio-ports/org.virtio-mem.control.0` | `/dev/virtio-ports/org.rampire.control.0` |
| Build stamp file | `virtio-mem-build.json` | `rampire-build.json` |

Every schema version restarts at 1:

| Schema | Constant | Now |
|---|---|---|
| Guest publication (raw telemetry) | `RAW_TELEMETRY_VERSION` | 6 |
| Guest settings | `GUEST_SETTINGS_VERSION` | 6 |
| Memory request | `MEMORY_REQUEST_VERSION` | 1 |
| Controller (host) status | `CONTROLLER_STATUS_VERSION` | 5 |
| Attestation | `COMPATIBILITY_ATTESTATION_VERSION` | 2 |
| Checkpoint (durable state) | `CHECKPOINT_VERSION` | 2 |
| Windows service configuration | `WINDOWS_SERVICE_CONFIG_SCHEMA_VERSION` | 9 |
| Build record | `RECORD_SCHEMA_VERSION` | 1 |
| Controller status evidence | `EVIDENCE_VERSION` | 1 |
| Linux deploy evidence | `EVIDENCE_SCHEMA_VERSION` | 1 |
| Qualification summary | `SCHEMA_VERSION` | 4 |

## Environment variables

Every `VIRTIO_MEM_*` variable becomes `RAMPIRE_*` with the rest unchanged,
for example `VIRTIO_MEM_BUILD_ID` → `RAMPIRE_BUILD_ID`, apart from these:

| Now | New | Why |
|---|---|---|
| `VIRTIO_MEM_CONNECT`, `VIRTIO_MEM_HOST_BIN`, `VIRTIO_MEM_HOST_CONFIG` | deleted | Used only by `recover-latch.sh`, deleted in O1.10 |
| `VIRTIO_MEM_POOL_HOST`, `VIRTIO_MEM_POOL_CONFIG`, `VIRTIO_MEM_POOL_CHECKOUT` | `RAMPIRE_TOP_HOST`; the other two deleted | `Watch-Pool.ps1` runs `rampire top` over SSH, with no checkout or config path |

## Development names

| Now | New |
|---|---|
| `/run/virtio-mem-qualification` | `/run/rampire-qualification` |
| `virtio-mem-qualification-artifacts-*` | `rampire-qualification-artifacts-*` |
| Test fixture port `org.virtio-mem.other.0` | `org.rampire.other.0` |

## Questions for review

1. **Schema names.** Today a document carries only a `version` number.
   Nothing in it says what kind of document it is. D8 and R3 talk about
   renaming schema names, but there are none to rename. Recommended: add
   `"schema": "rampire.NAME"` beside `"version": 1` in every JSON document
   (`rampire.publication`, `rampire.request`, `rampire.status`,
   `rampire.attestation`, `rampire.checkpoint`, `rampire.evidence`), so
   tools and the `--json` schemas in O4.1 can tell documents apart.
2. **The Windows service name.** `RampireGuest`, or `RAMpire` to match the
   install directory? It only shows in `services.msc` and `sc.exe`.
3. **The operations library.** A separate `rampire-ops` crate, as above,
   lets `xtask` use the views without building the daemon. The other way
   is to keep them in the `rampire` crate's library, which is one crate
   fewer.
