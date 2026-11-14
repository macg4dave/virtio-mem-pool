# Installing RAMpire from the static tarball

For a host or guest without a RAMpire package. The binaries are static, so
they run on any x86-64 Linux with systemd. Run every command as root.

## The host

From the `rampire-VERSION-x86_64-linux` folder:

```bash
install -m 0755 rampire /usr/bin/rampire
install -m 0644 systemd/rampire.service /etc/systemd/system/
install -m 0644 systemd/50-rampire.rules /etc/polkit-1/rules.d/
install -m 0644 systemd/rampire.sysusers /etc/sysusers.d/rampire.conf
systemd-sysusers /etc/sysusers.d/rampire.conf
```

On Debian and Ubuntu, libvirt's channel sockets belong to `libvirt-qemu`
rather than `qemu`:

```bash
install -D -m 0644 systemd/debian-socket-group.conf \
  /etc/systemd/system/rampire.service.d/socket-group.conf
```

Then:

```bash
systemctl daemon-reload
rampire doctor                    # what the host still needs
rampire init                      # global.toml from this host, then apply
systemctl enable --now rampire
```

`examples/` holds commented settings files and a review profile.

## A Linux guest

From the `rampire-guest-VERSION-x86_64-linux` folder, with systemd 253 or
later:

```bash
install -m 0755 rampire-guest /usr/libexec/rampire-guest
install -m 0755 rampire-watch-ram /usr/bin/rampire-watch-ram
install -m 0644 rampire-guest.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now rampire-guest
```
