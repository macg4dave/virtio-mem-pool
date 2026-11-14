# Development

How to build, check and test this repository, and which checks need which
platform. [The roadmap](roadmap.md) holds intended design and open work;
[the Rust standards](../.github/prompts/rust.md) hold coding rules;
[administration](administration.md) covers installing and running the
product; the roadmap also holds
[the rule for importing third-party code](roadmap.md#importing-third-party-code).

## Workspace

| Crate | Path | Builds on | Owns |
|---|---|---|---|
| `rampire-core` | `crates/rampire-core` | any | Policy, request protocol, wire and settings formats |
| `rampire-guest` | `crates/rampire-guest` | any | Guest request store, worker, transports, service loop |
| `rampire-host` | `host/` | Linux | The controller: settings resolution, runtime, libvirt, attestation, the CLI until O1.2 |
| `rampire-ops` | `crates/rampire-ops` | Linux | Operator views: controller status, the pool, one VM live |
| `rampire` | `rampire/` | Linux | The `rampire` binary: the daemon and operator commands |
| `rampire-guest-linux` | `guest-linux/` | Linux | Linux guest service and `rampire-workload` |
| `rampire-guest-windows` | `guest-windows/` | Windows (native) | Windows service and its workload |
| `xtask` | `tools/xtask` | Linux | Gate, deploy, observe, qualify (`cargo xtask`) |

`xtask` depends on the host and operations crates, so it builds only on Linux. The core
and shared guest crates build anywhere.

## The gate

Every change passes the gate before review. One command runs it from any
workstation:

```bash
tools/gate.sh
```

It checks whitespace in the working tree against `HEAD`, then on a Linux
machine runs `cargo xtask gate checks`: `cargo fmt --check`, a release
build, all tests and Clippy with warnings denied for every Linux-buildable
crate, and `bash -n` on every shell script. On a Windows workstation it
also runs the Windows guest's tests and Clippy natively.

On a Linux machine with this checkout, it runs there. Anywhere else it
needs a Linux build host, named in the environment or in `site/gate.env`
(untracked; copy [deploy/examples/gate.env.example](../deploy/examples/gate.env.example)):

| Variable | Meaning |
|---|---|
| `RAMPIRE_GATE_HOST` | SSH destination of the Linux build host |
| `RAMPIRE_GATE_DIR` | Absolute directory on it that the gate owns: `src/` is replaced each run, `target/` is kept |
| `RAMPIRE_SSH` | SSH client to use, if not `ssh` |

The working tree is sent as it is, uncommitted and untracked (not ignored)
files included, with the repository's line endings. The build host needs
Rust with `rustfmt` and Clippy, and `bash`.

The individual steps are `cargo xtask gate format|build|test|lint`.
`cargo xtask gate local` is `checks` plus `git diff --check`, for a Linux
checkout. `cargo xtask gate all` adds the native Windows gate below.

### Checking one crate

```bash
cargo test -p rampire-core          # anywhere
cargo test -p rampire-guest         # anywhere
cargo test -p rampire-guest-windows # Windows
cargo test -p rampire-host     # Linux
```

Unit tests read only [fixtures](../fixtures/config) and temporary files.
None needs a VM, a running service or a site's configuration. The host's
`documented_examples_load` test also loads
[deploy/examples](../deploy/examples), so the examples stay valid.

## Platform-specific checks

| Check | Where | Command |
|---|---|---|
| Host, xtask, Linux guest | Linux (or `tools/gate.sh`) | `cargo xtask gate checks` |
| Windows guest unit tests and Clippy | Windows workstation | part of `tools/gate.sh` there |
| Windows release build, native tests, fetched binaries | Windows build VM | `make windows-native-gate` |
| Service control, sampling, ACLs, atomic replacement | A Windows guest | Deploy and observe; mocks do not prove them |
| Resizing, onlining, pressure, cache behaviour | A live VM under the controller | `cargo xtask metrics`, qualification |

The Windows targets in the `Makefile` read the site's build VM from
`site/windows-build.mk` (copy
[windows-build.mk.example](../deploy/examples/windows-build.mk.example)),
or from the file named by `WINDOWS_SITE`. `make windows-native-gate` checks
the toolchain, synchronises the working tree, release-builds, runs the
native tests, `fmt` and Clippy, then fetches the binaries and records them
in `build.json`.

## Build identity

A deployed binary names the exact source it came from. The identity is the
commit (`<12 hex>`), or for a working tree that differs from it
`<commit>-dirty.<12 hex of the source digest>`: a SHA-256 over every
tracked and untracked, non-ignored file. The build compiles it in through
`RAMPIRE_BUILD_ID`; a build without it reports `unversioned`.

- Linux guest: `cargo xtask linux build` builds the release binary and
  checks that `rampire-guest version` reports the checkout's identity.
- Windows guest: `sync` stamps the remote workspace, `build` compiles the
  stamp in, `fetch` verifies the binaries by SHA-256 and writes
  `build.json`. `make windows-status` compares the installed service with
  that record.

Every guest publication carries its build identity, so `watch`, `metrics`
and captures show which build produced each sample.

## Development guests

`cargo xtask linux deploy SSH_DESTINATION --output PATH --apply` installs the
stamped Linux guest, its unit, the RAM monitor and the workload on a
development VM over SSH (the account needs passwordless `sudo`). It runs on
a Linux checkout that can reach the guest. Without `--apply` it records
what it would do. `status` and `uninstall` complete it; see
[tools](tools.md). Production installs use packages (roadmap M4).

Windows deployment is `make windows-deploy` with the site's VM settings.

## Live tests

Live tests change real VMs and the host's memory. They need the owner's
authorisation, a bounded duration and bounded memory. Record them with
`cargo xtask metrics` (or `qualification`) so the result is evidence, not
recollection, and open `tools/linux/watch-ram.sh` (Linux guest) or
`tools/windows/Watch-Ram.ps1` (Windows guest) to watch them. Results go in
the roadmap with the capture's path under `.artifacts/`.

Every VM a test needs is made and removed with `cargo xtask testvm`
([tools](tools.md#test-vms)). A test that needs a VM no scenario
describes gets a new scenario in `tools/testvm`, not a VM made by hand.

## Site configuration

Nothing site-specific is tracked. VM names, hosts, SSH aliases, paths and
pool sizes live in a site's own files: on a host, its installed
configuration; in a checkout, the untracked `site/` directory. The tracked
[deploy/examples](../deploy/examples) are neutral templates.

## Installing a development build

Until there are packages (roadmap O2), a development host installs from a
checkout:

```bash
export RAMPIRE_BUILD_ID=$(git rev-parse --short=12 HEAD)$(git diff --quiet HEAD -- || echo -dirty)
cargo build --release --locked -p rampire -p xtask
sudo systemctl stop rampire 2>/dev/null; sudo systemctl reset-failed rampire 2>/dev/null
target/release/xtask host-deploy --config CONFIG_DIR --attestation-dir CONFIG_DIR/attestations \
  --output .artifacts/deploy/host-deploy-$(date +%Y%m%dT%H%M%S).json \
  --command-timeout-seconds 60 --apply --elevate
sudo /usr/bin/rampire apply --yes
sudo systemctl enable --now rampire
```

`host-deploy` checks every attestation against the live VMs and installs
the binary as `/usr/bin/rampire`, `rampire.service`, the polkit
rule and the draft configuration in `/etc/rampire`, keeping a backup of
what it replaces. `rampire apply` then makes the copy the unit runs.
`RAMPIRE_BUILD_ID` is what `rampire --version` reports.
