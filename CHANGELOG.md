# Changelog

Every release of RAMpire, newest first. Versions follow semantic
versioning. Upgrade the controller first, then the guests: a controller
serves guests from its own minor release and the one before it.

## Unreleased: 0.1.0

The first release under the name RAMpire. Nothing is migrated from the
earlier virtio-mem pool controller: remove an old install and install
again (decision D9).

- One command, `rampire`: `init`, `status`, `top`, `watch`, `check`,
  `diff`, `apply`, `rollback`, `set`, `logs`, `doctor` and `recover`, and
  `daemon` for the unit. Every command takes `--json` and `--yes`, and
  exit codes are documented.
- The controller runs only a copy of the configuration that
  `rampire apply` checked, so a mistake in `/etc/rampire` never reaches
  it. Each apply is recorded under `/var/lib/rampire/evidence`.
- Configuration is `global.toml` and one `vms.d/NAME.toml` per VM. Review
  profiles are TOML, with a source for every answer.
- One unit, `rampire.service`, ordered after libvirt's single or split
  daemons.
- Host and guest packages as RPM and `.deb`, and static tarballs.
- Every document names its schema and starts at version 1. The control
  channel port is `org.rampire.control.0`.
