//! The controller's systemd unit.

use std::path::Path;
use std::time::Duration;

use crate::process;

/// The deployment layout: unit `rampire@INSTANCE.service` runs the
/// configuration in `/etc/rampire`.
/// Returns INSTANCE for a controller unit name.
pub fn controller_instance(unit: &str) -> Result<&str, String> {
    unit.strip_prefix("rampire@")
        .and_then(|rest| rest.strip_suffix(".service"))
        .filter(|instance| valid_instance(instance))
        .ok_or_else(|| format!("{unit} is not a rampire@INSTANCE.service unit"))
}

/// The controller unit to observe when none was named: the one
/// `rampire@INSTANCE` unit systemd knows on this host, since one
/// controller manages every VM in its configuration. With none or several,
/// `fallback` (the unit named after the VM) is kept.
pub fn installed_controller_unit(fallback: &str, cwd: &Path) -> String {
    let listing = process::bounded_text(
        "systemctl",
        &[
            process::os("list-units"),
            process::os("--all"),
            process::os("--plain"),
            process::os("--no-legend"),
            process::os("rampire@*.service"),
        ],
        cwd,
        Duration::from_secs(10),
    );
    // Best effort: without systemctl the fallback is what was used before.
    listing.map_or_else(
        |_| fallback.to_owned(),
        |text| only_controller_unit(&text).unwrap_or_else(|| fallback.to_owned()),
    )
}

/// The single controller unit in a `systemctl list-units --plain` listing.
fn only_controller_unit(listing: &str) -> Option<String> {
    let mut units = listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| controller_instance(unit).is_ok());
    let unit = units.next()?;
    units.next().is_none().then(|| unit.to_owned())
}

/// An instance name that is safe in a unit name and on a command line.
pub fn valid_instance(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_units_name_a_deployment_instance() {
        assert_eq!(controller_instance("rampire@pool.service"), Ok("pool"));
        for unit in [
            "rampire@.service",
            "rampire@a/b.service",
            "other@pool.service",
            "rampire@pool",
        ] {
            assert!(controller_instance(unit).is_err(), "{unit}");
        }
    }

    #[test]
    fn the_only_installed_controller_is_the_default_one() {
        let one = "rampire@main.service loaded active running Virtio-mem controller for main\n";
        assert_eq!(
            only_controller_unit(one).as_deref(),
            Some("rampire@main.service")
        );
        // Several controllers, or none, leave the choice to the caller.
        let two = format!("{one}rampire@lab.service loaded inactive dead x\n");
        assert_eq!(only_controller_unit(&two), None);
        assert_eq!(only_controller_unit(""), None);
        assert_eq!(
            only_controller_unit("other@main.service loaded active running x\n"),
            None
        );
    }
}
