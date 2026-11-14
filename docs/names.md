# Names

The names table for the clean break ([roadmap](roadmap.md) R1). R2 and R3
rename everything to the right-hand column, once. Nothing is kept under an
old name (decision D9).

Reviewed by the owner on 2026-11-14 (decisions D20 to D22 in the
[roadmap](roadmap.md#decisions)).

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

Every document RAMpire reads or writes names its schema in a `schema`
field, written first, beside its `version`. Every version restarted at 1
(decision D20). A reader refuses a document whose `schema` is missing or
names another document, or whose `version` it doesn't know. The marker
types come from `rampire_core::schema_name!`.

| Document | `schema` | Version constant | Was |
|---|---|---|---|
| Guest publication | `rampire.publication` | `RAW_TELEMETRY_VERSION` | 6 (read 4 to 6) |
| Host status, sent to a guest | `rampire.host-status` | `MEMORY_REQUEST_VERSION` | 1 |
| Guest's retained request state | `rampire.guest-state` | `GUEST_STATE_VERSION` | no version |
| Controller status for a VM | `rampire.status` | `CONTROLLER_STATUS_VERSION` | 5 |
| Checkpoint (durable state) | `rampire.checkpoint` | `CHECKPOINT_VERSION` | 2 |
| Attestation | `rampire.attestation` | `COMPATIBILITY_ATTESTATION_VERSION` | 2 |
| Windows service configuration | `rampire.windows-config` | `WINDOWS_SERVICE_CONFIG_VERSION` | 9, as `schema_version` |

A memory request travels inside a publication or a status, not as a
document of its own, so it keeps its `version` and has no `schema`.
`GUEST_SETTINGS_VERSION` and the host's choice of which guests to send
settings to are gone: every guest reads them.

The records `xtask` writes for development (the Windows build record and
stamp, deploy and preflight evidence, metrics and qualification runs) are
not RAMpire documents and keep their own versions. A record that moves into
`rampire`, such as apply evidence in O1.5, gets a schema then.

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

## Decisions

| ID | Decision |
|---|---|
| D20 | Every JSON document carries `"schema": "rampire.NAME"` beside `"version"` |
| D21 | The Windows service is `RampireGuest` |
| D22 | The operator views live in their own crate, `rampire-ops` |
