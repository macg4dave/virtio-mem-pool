//! The Debian package for the Linux guest service (`cargo xtask linux
//! package`). It holds the stamped service binary, its systemd unit and the
//! RAM monitor. The guest has no configuration file: the host publishes its
//! settings. Removing the package keeps the durable request state, so a
//! reinstall resumes an unresolved request; purging discards it.
//!
//! The archive is assembled here (an `ar` of `debian-binary` and two gzip
//! tarballs made by GNU tar), so building needs no Debian tooling.

use std::path::{Path, PathBuf};

use crate::process;

const PACKAGE: &str = "virtio-mem-guest";
const DEVELOPMENT_BINARY: &str = "/usr/local/libexec/virtio-mem-guest";
const PACKAGED_BINARY: &str = "/usr/libexec/virtio-mem-guest";
const PACKAGED_UNIT: &str = "/usr/lib/systemd/system/virtio-mem-guest.service";
const PACKAGED_MONITOR: &str = "/usr/bin/virtio-mem-watch-ram";
const HOMEPAGE: &str = "https://github.com/macg4dave/virtio-mem-pool";
/// `OpenFile=` in the unit, which hands the service the control channel,
/// first appeared in systemd 253.
const DEPENDS: &str = "libc6, systemd (>= 253)";

/// What goes into one package.
pub struct Contents<'a> {
    pub build_id: &'a str,
    pub cargo_version: &'a str,
    pub architecture: &'a str,
    pub maintainer: &'a str,
    pub binary: &'a [u8],
    pub unit: &'a [u8],
    pub monitor: &'a [u8],
}

/// Build `PACKAGE_VERSION_ARCH.deb` in `output`; returns its path.
pub fn build(contents: &Contents<'_>, output: &Path, repo: &Path) -> Result<PathBuf, String> {
    let version = version(contents.cargo_version, contents.build_id)?;
    let unit = packaged_unit(contents.unit)?;
    let staging = output.join(format!(".{PACKAGE}-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| format!("clear {}: {error}", staging.display()))?;
    }
    let result = (|| {
        let data = staging.join("data");
        let control = staging.join("control");
        for (path, bytes, mode) in [
            (PACKAGED_BINARY, contents.binary, 0o755),
            (PACKAGED_UNIT, unit.as_slice(), 0o644),
            (PACKAGED_MONITOR, contents.monitor, 0o755),
            (
                "/usr/share/doc/virtio-mem-guest/copyright",
                COPYRIGHT.as_bytes(),
                0o644,
            ),
        ] {
            write(&data.join(path.trim_start_matches('/')), bytes, mode)?;
        }
        let installed_kib = [contents.binary.len(), unit.len(), contents.monitor.len()]
            .iter()
            .map(|bytes| bytes.div_ceil(1024))
            .sum::<usize>()
            + 1;
        let control_text = control_file(
            &version,
            contents.architecture,
            contents.maintainer,
            installed_kib,
        );
        write(&control.join("control"), control_text.as_bytes(), 0o644)?;
        for (name, script) in [("postinst", POSTINST), ("prerm", PRERM), ("postrm", POSTRM)] {
            write(&control.join(name), script.as_bytes(), 0o755)?;
        }
        let data_tar = tarball(&data, &staging.join("data.tar.gz"), repo)?;
        let control_tar = tarball(&control, &staging.join("control.tar.gz"), repo)?;
        let archive = ar(&[
            ("debian-binary", b"2.0\n".to_vec()),
            ("control.tar.gz", control_tar),
            ("data.tar.gz", data_tar),
        ])?;
        let path = output.join(format!("{PACKAGE}_{version}_{}.deb", contents.architecture));
        std::fs::write(&path, archive)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        Ok(path)
    })();
    // Best effort: the package, or the error above, is the result.
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// A Debian version for a stamped build: the crate version, then the build
/// identity, whose `-` Debian reserves for the revision.
pub fn version(cargo_version: &str, build_id: &str) -> Result<String, String> {
    let valid = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    };
    if !valid(cargo_version) || !valid(build_id) || cargo_version.contains('-') {
        return Err(format!(
            "cannot make a Debian version from {cargo_version:?} and build {build_id:?}"
        ));
    }
    Ok(format!(
        "{cargo_version}+git{}-1",
        build_id.replace('-', ".")
    ))
}

/// The repository unit with the binary at its packaged path.
pub fn packaged_unit(unit: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(unit).map_err(|_| "the guest unit is not UTF-8")?;
    let development = format!("ExecStart={DEVELOPMENT_BINARY} ");
    if text.matches(&development).count() != 1 {
        return Err(format!(
            "the guest unit must have exactly one {development:?} line"
        ));
    }
    Ok(text
        .replace(&development, &format!("ExecStart={PACKAGED_BINARY} "))
        .replace('\r', "")
        .into_bytes())
}

/// The Debian architecture name for this build's target.
pub fn architecture() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => Err(format!("no Debian architecture for {other}")),
    }
}

fn control_file(
    version: &str,
    architecture: &str,
    maintainer: &str,
    installed_kib: usize,
) -> String {
    format!(
        "Package: {PACKAGE}
Version: {version}
Architecture: {architecture}
Maintainer: {maintainer}
Installed-Size: {installed_kib}
Depends: {DEPENDS}
Section: admin
Priority: optional
Homepage: {HOMEPAGE}
Description: virtio-mem guest memory requests
 Asks the KVM host's virtio-mem pool controller for memory when the guest
 needs more and returns it when the guest has spare. Talks to the host
 over the virtio-serial port org.virtio-mem.control.0, or through control
 files the QEMU guest agent carries when the VM has no such port.
"
    )
}

const COPYRIGHT: &str = "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: virtio-mem-pool
Source: https://github.com/macg4dave/virtio-mem-pool

Files: *
Copyright: 2026 macg4dave
License: GPL-3+

License: GPL-3+
 This program is free software: you can redistribute it and/or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License, or
 (at your option) any later version.
 .
 This program is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 GNU General Public License for more details.
 .
 On Debian systems, the complete text of the GNU General Public License
 version 3 can be found in \"/usr/share/common-licenses/GPL-3\".
";

/// Enable on first install only, so an administrator's `disable` survives
/// upgrades; restart onto the new binary every time. The request state
/// carries over a restart.
const POSTINST: &str = "#!/bin/sh
set -e
if [ \"$1\" = configure ] || [ \"$1\" = abort-upgrade ]; then
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload
        if [ -z \"$2\" ]; then
            systemctl enable virtio-mem-guest.service
        fi
        if systemctl is-enabled --quiet virtio-mem-guest.service; then
            systemctl restart virtio-mem-guest.service
        fi
    fi
fi
";

const PRERM: &str = "#!/bin/sh
set -e
if [ \"$1\" = remove ] && [ -d /run/systemd/system ]; then
    systemctl disable --now virtio-mem-guest.service
fi
";

/// Removal keeps the durable request state: the host may already have
/// admitted the request it holds. Purging is the explicit discard.
const POSTRM: &str = "#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
if [ \"$1\" = purge ]; then
    rm -rf /var/lib/private/virtio-mem-guest /var/lib/virtio-mem-guest
fi
";

fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// A gzip tarball of `directory`'s contents, owned by root and in a stable
/// order, as dpkg expects.
fn tarball(directory: &Path, output: &Path, repo: &Path) -> Result<Vec<u8>, String> {
    process::run(
        "tar",
        &[
            "--create",
            "--gzip",
            "--file",
            &output.to_string_lossy(),
            "--directory",
            &directory.to_string_lossy(),
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--sort=name",
            "--format=gnu",
            ".",
        ],
        repo,
    )?;
    std::fs::read(output).map_err(|error| format!("read {}: {error}", output.display()))
}

/// A Unix `ar` archive of `members`, the container format of a `.deb`.
pub fn ar(members: &[(&str, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut archive = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        if name.len() > 15 || name.contains('/') {
            return Err(format!("ar member name {name:?} is not a short file name"));
        }
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            "100644",
            bytes.len()
        );
        debug_assert_eq!(header.len(), 60);
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            archive.push(b'\n');
        }
    }
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamped_build_has_a_valid_debian_version() {
        assert_eq!(
            version("0.1.0", "6134275b136c-dirty.cdf3554aac2a"),
            Ok("0.1.0+git6134275b136c.dirty.cdf3554aac2a-1".to_owned())
        );
        assert_eq!(
            version("0.1.0", "6134275b136c"),
            Ok("0.1.0+git6134275b136c-1".to_owned())
        );
        for (cargo, build) in [
            ("0.1.0", "unversioned build"),
            ("0.1.0-rc1", "abc"),
            ("", "abc"),
        ] {
            assert!(version(cargo, build).is_err(), "{cargo} {build}");
        }
    }

    #[test]
    fn the_packaged_unit_runs_the_packaged_binary() {
        let unit = std::fs::read(
            crate::repo_root()
                .expect("root")
                .join("guest-linux/systemd/virtio-mem-guest.service"),
        )
        .expect("unit");
        let packaged = String::from_utf8(packaged_unit(&unit).expect("packaged")).unwrap();
        assert!(packaged.contains("ExecStart=/usr/libexec/virtio-mem-guest run"));
        assert!(!packaged.contains("/usr/local/"));
        assert!(!packaged.contains('\r'));
        assert!(packaged_unit(b"[Service]\nExecStart=/bin/true\n").is_err());
    }

    #[test]
    fn members_are_laid_out_as_ar_expects() {
        let archive = ar(&[("debian-binary", b"2.0\n".to_vec()), ("odd", b"x".to_vec())]).unwrap();
        assert!(archive.starts_with(b"!<arch>\n"));
        let first = &archive[8..68];
        assert_eq!(&first[..16], b"debian-binary/  ");
        assert_eq!(&first[48..58], b"4         ");
        assert_eq!(&first[58..], b"`\n");
        assert_eq!(&archive[68..72], b"2.0\n");
        // An odd-sized member is padded to an even offset.
        assert_eq!(archive.len(), 8 + 60 + 4 + 60 + 2);
        assert!(ar(&[("a/b", Vec::new())]).is_err());
        assert!(ar(&[("a-very-long-member-name", Vec::new())]).is_err());
    }

    #[test]
    fn removal_keeps_request_state_and_purge_discards_it() {
        let purge = POSTRM.split("if [ \"$1\" = purge ]").nth(1).unwrap();
        assert!(
            purge.contains("rm -rf /var/lib/private/virtio-mem-guest /var/lib/virtio-mem-guest")
        );
        assert_eq!(POSTRM.matches("rm -rf").count(), 1);
        assert!(PRERM.contains("[ \"$1\" = remove ]"));
        let control = control_file("1", "amd64", "M <m@example.org>", 10);
        assert!(control.contains("Depends: libc6, systemd (>= 253)\n"));
        assert!(control.ends_with('\n'));
    }
}
