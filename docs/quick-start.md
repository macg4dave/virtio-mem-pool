# Quick start

From nothing to one VM whose memory RAMpire manages, in about ten
minutes. You need a KVM host with libvirt (libvirt 10.1 or later, QEMU 8.2
or later) and a Linux VM on it. Every command runs on the host as root
unless it says "in the VM".

## 1. Install

```bash
dnf copr enable macg4dave/rampire && dnf install rampire   # RHEL, AlmaLinux, Rocky, Fedora
apt install rampire                                        # Debian, Ubuntu, after adding the repository
```

[Releasing](releasing.md#installing-from-the-repositories) shows how to
add the apt repository. The packages are also on the
[releases page](https://github.com/macg4dave/rampire/releases)
(`dnf install ./rampire-VERSION.x86_64.rpm`,
`apt install ./rampire_VERSION_amd64.deb`). For a host without a package,
the static tarball has its own `INSTALL.md`.

## 2. Check the host

```bash
rampire doctor
```

Each `problem` line comes with its `fix`. At this point the only
problem should be that nothing is applied yet; the next step deals with
that.

## 3. Set up

```bash
rampire init
systemctl enable --now rampire
```

`init` reads the host's memory and its VMs, writes
`/etc/rampire/global.toml` with a suggested pool and host reserve, and
applies it. The controller now runs, managing no VMs yet.

## 4. Install the guest service, in the VM

```bash
dnf install ./rampire-guest-VERSION.x86_64.rpm   # or: apt install ./rampire-guest_VERSION_amd64.deb
```

It needs systemd 253 or later: Debian 13, Ubuntu 24.04, RHEL 10, current
Fedora.

## 5. Add the VM

```bash
rampire vm add webserver --fix-xml
```

`vm add` reads the VM's definition and:

- adds what it lacks, after showing you the exact change: a virtio-mem
  device and the control channel. The old definition is kept;
- asks the few questions the definition can't answer, such as whether
  the VM's programs cope with memory coming and going, and the guest's
  kernel release (`uname -r` in the VM);
- writes `/etc/rampire/vms.d/webserver.toml`, with limits from the VM's
  boot RAM;
- offers to restart the VM so its new definition takes effect, then
  applies. Applying attests the VM: it records exactly how the VM looks,
  and every resize checks that it still does.

If the VM has no NUMA cell or `maxMemory`, `vm add` says what to add to
its definition; add it with `virsh edit webserver` and run `vm add` again.

## 6. Watch it work

```bash
rampire status
rampire top
```

The VM starts small and grows as its programs need memory. When it goes
quiet it hands memory back for other VMs.

## Next

- [Usage](usage.md): changing settings, checking it works, what to do when
  something goes wrong.
- [Reference](reference.md): every command and setting.
- [Running a fleet](fleet.md): configuration management, metrics and
  scripting.
