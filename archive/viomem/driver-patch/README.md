# Full-region viomem removal experiment

> **PAUSED.** This work is on pause until the upstream `viomem` removal bug
> is repaired. Windows guests are moving to Hyper-V Dynamic Memory
> instead; see [the roadmap](../../../docs/roadmap.md), milestone W. Kept for
> reference and for resuming.

`full-region-removal.patch` changes the upstream Windows kernel driver, not
this repository's Rust service. It applies to
`virtio-win/kvm-guest-drivers-windows` commit
`f7c8beb249df12f52ba4b9dba95f1edb515c7aeb`.

The proposal is recorded in
[#1696](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1696#issuecomment-6470149285)
and explicitly linked to our restricted-window report,
[#1706](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1706#issuecomment-6470260177).

The patch removes the unused longest-set-run helpers from the removal path.
It searches `[addr, addr + region_size - 1]` and separately requests at most
`plugged_size - requested_size` bytes. Each call is capped at the documented
MDL limit of 4 GiB minus one page, rounded down to device blocks. The existing
contiguous-chunk and hot-remove flags, returned-range processing, bitmap
updates and rejected-UNPLUG rollback remain in use. A converged or growth
target does not enter the removal allocator.

This relies on the driver's existing valid device-geometry assumptions.
It does not add retries after a zero-progress result or change the growth
path reported in #1707. Windows can still refuse to hot-remove memory.

## Apply and run regression tests

From this service repository, using a fresh upstream checkout:

```bash
git clone https://github.com/virtio-win/kvm-guest-drivers-windows.git .artifacts/viomem-driver/source
git -C .artifacts/viomem-driver/source checkout f7c8beb249df12f52ba4b9dba95f1edb515c7aeb
git -C .artifacts/viomem-driver/source apply --check "$PWD/archive/viomem/driver-patch/full-region-removal.patch"
git -C .artifacts/viomem-driver/source apply "$PWD/archive/viomem/driver-patch/full-region-removal.patch"
python3 archive/viomem/driver-patch/test-removal.py .artifacts/viomem-driver/source/viomem/sys/viomem.c
```

The runner extracts the actual upstream removal function and compiles it with
the model in `removal-harness.c`, using `-Wall -Wextra -Werror`. Pass
`--sanitize` when address/undefined sanitiser libraries are available.
The model supplies removable chunks within the requested physical interval;
it does not implement or prove Windows memory evacuation behaviour.

Eight cases cover a blocked first block with a removable last block,
fragmented bitmaps, disjoint chunks, partial results, NULL allocation,
rejected UNPLUG rollback, large allocations and non-shrink targets.
The unpatched source fails the restricted-window regression.

## Validation and remaining work

The patched function passes all eight model cases on Linux/GCC and native
Windows x64/MSVC with warnings treated as errors. Evidence is in
`.artifacts/viomem-driver/patched-test.log`, `unpatched-test.log` and
`native-harness.log`. The isolated patched checkout is under
`.artifacts/viomem-driver/source`; it is ignored by the service repository.

The guest has Visual Studio 2022, but the standard Windows Kits installation
has no WDK kernel headers, WDK build directory or Inf2Cat. No `viomem.sys`
has been built or installed. Follow the upstream
[driver build instructions](https://virtio-win.github.io/Development/Building-the-drivers-using-Windows-11-24H2-EWDK)
to supply the driver toolchain. The VM's live firmware configuration enables
Secure Boot, and its current viomem driver is signed, version
`100.102.104.29400`. Loading a replacement requires a compatible signing
and deployment plan before any live driver trial.

Keep the host workaround until a patched-driver trial proves progress at a
fixed target. The trial must prevent the host workaround from supplying new
lower targets, trace requested/current bytes and returned ranges, and cover
fragmentation and a reduction larger than one MDL. Model tests alone do not
establish those results.

During preparation, the Windows service minimum RAM was temporarily raised
from 6 GiB to 12 GiB to request live growth. Its original configuration is
saved at
`C:\ProgramData\VirtioMemService\config.before-viomem-driver-test.json`.
This is an operational guest setting; the repository deployment settings
remain unchanged. Restore the saved configuration and restart only the
Windows service when the temporary minimum is no longer wanted.

Automatic growth was blocked by an old outstanding Remove request and stale
host telemetry. After the operator stopped only the Linux controller, a live
device increase to 4,429,185,024 bytes converged. Windows then reported
12,948,123,648 visible bytes (about 12.06 GiB). The extra two 64 MiB blocks
above 4 GiB account for the difference between configured boot RAM and RAM
visible to Windows, so its 12 GiB minimum is met. The VM was not stopped or
rebooted. Evidence is in `.artifacts/viomem-driver/ram-live-growth.json` and
`ram-windows-verification.json`. The Linux controller remains stopped until
the operator restarts `virtio-mem-host@win11_gpu.service`.

## License

`full-region-removal.patch` modifies `viomem/sys/viomem.c` from
`virtio-win/kvm-guest-drivers-windows`, which is under the BSD-3-Clause
license (Copyright 2009-2025 Red Hat, Inc. and/or its affiliates, and
other contributors; see that repository's `LICENSE`). The patch is offered
under the same license so that it can be submitted upstream. The rest of
this directory is under this repository's license, GPL-3.0-or-later.
