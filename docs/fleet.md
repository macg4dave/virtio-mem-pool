# Running a fleet

RAMpire's command works the same for a script as for a person. This page
collects what automation needs.

## The contract

- Every command runs as root.
- With `--yes`, or without a terminal, a command never asks. Without a
  terminal and without `--yes`, a command that would change something
  changes nothing and says so.
- `--json` writes a versioned document; see [JSON documents](json.md).
- Exit codes: 0 done, 1 failed or a problem found, 2 a wrong command
  line. `rampire apply --detailed-exitcode` exits 0 when nothing changed,
  2 when something changed and 1 on failure.
- `rampire --version` names the build.

## Configuration in git

Keep each host's `global.toml`, `vms.d/` and `reviews/` in git and copy
them to `/etc/rampire`. Then:

```bash
rampire check                                   # the draft against the live VMs
rampire apply --yes --detailed-exitcode         # 0 unchanged, 2 changed, 1 failed
```

A draft that fails its checks changes nothing: the controller keeps
running its last applied copy, through restarts and reboots. A change to
priorities, limits or the pool reloads the controller; adding or removing
a VM restarts it. Each apply is recorded under
`/var/lib/rampire/evidence/`.

[deploy/ansible](../deploy/ansible) is an example Ansible role that does
this and reports "changed" only when something changed.

Attestations are made on the host: `apply` attests a running VM that names
a review profile and has none yet. A VM whose definition changed needs a
person to confirm the change: `rampire vm attest VM`, or
`rampire vm attest --drifted` for every such VM.

## Adding VMs

A fleet adds a VM by writing `vms.d/NAME.toml`, naming a review profile
shared by VMs of that kind, and running `apply`. The VM's definition must
already have its virtio-mem device and control channel; build them into
your VM templates, or run `rampire vm add NAME --fix-xml` once.

[deploy/cloud-init/rampire-guest.yaml](../deploy/cloud-init/rampire-guest.yaml)
installs the guest service when a VM is created.

## Watching

- Metrics: `/run/rampire/metrics.prom`, written every round, for
  node_exporter's textfile collector. Add the collector's user to the
  `rampire` group.
- Journal: every decision is one `key=value` line with a stable `event`
  name; see [Journal events](journal.md).
- `rampire status --json` and `rampire doctor --json` for health checks.

## Upgrading

Upgrade the controller first, then the guests. A controller serves guests
from its own minor release and the one before it; the
[changelog](../CHANGELOG.md) lists each release.
