# viomem work (paused)

> **PAUSED until the upstream `viomem` removal bug is repaired.** Nothing in
> this directory is built, tested or deployed. Windows guests are moving to
> Hyper-V Dynamic Memory instead; see [the roadmap](../../docs/roadmap.md),
> milestone W.

The Windows virtio-mem driver (`viomem`, in
[virtio-win/kvm-guest-drivers-windows](https://github.com/virtio-win/kvm-guest-drivers-windows))
cannot reliably return memory: a removal only searches a window that starts
at the lowest plugged block, and growth can stop at the edge of QEMU's
usable region. The upstream reports are
[#1706](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1706)
and [#1707](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1707),
with our proposed fix in
[#1696](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1696).

Paused on 2026-11-13. The last commit with this code active is `e665205`.

## Contents

| Path | What it is | License |
|---|---|---|
| [driver-patch/](driver-patch/) | Our proposed fix to the upstream driver, its regression harness and test runner | Patch: BSD-3-Clause (upstream's); the rest GPL-3.0-or-later |
| [host-workaround/runtime.rs](host-workaround/runtime.rs) | The controller's Windows-only shrink extension and usable-region widening, cut from `host/src/runtime.rs` | GPL-3.0-or-later |
| [host-workaround/resize_sink.rs](host-workaround/resize_sink.rs) | The `virsh` side of the widening (pause, request the ceiling, restore, resume) | GPL-3.0-or-later |
| [host-workaround/tests.rs](host-workaround/tests.rs) | Their unit tests and test fakes | GPL-3.0-or-later |

The `host-workaround` files are excerpts, each section headed with the
file and line range it came from in `e665205`. They do not compile on their
own.

## What still mentions viomem

These stay in the build, each marked `PAUSED (viomem)`:

- the Windows guest still reads the `viomem.sys` version for its
  diagnostics ([guest-windows/src/diagnostics.rs](../../guest-windows/src/diagnostics.rs));
- the Windows removal step in the guest policy
  ([crates/virtio-mem-core/src/settings.rs](../../crates/virtio-mem-core/src/settings.rs));
- the `usable_region_widened` event in `xtask watch` and `metrics`, for
  older journals.

## Resuming

Resume only when both hold:

1. A virtio-win release contains a removal fix (ours or another), and
2. it passes `driver-patch/test-removal.py` and a recorded removal on a
   Windows VM under the controller with no workaround.

If a fixed driver makes the workarounds unnecessary, delete
`host-workaround/` rather than restoring it. If a workaround is still
needed, restore it from `e665205` (`git show e665205:host/src/runtime.rs`),
fenced to the driver versions that need it.
