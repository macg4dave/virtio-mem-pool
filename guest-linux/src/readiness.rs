//! What this guest can and cannot do, from the kernel's own files: whether
//! the virtio-mem driver has a device, whether pressure stall information is
//! available, how hot-plugged memory is onlined, and what swap and commit
//! accounting are in force.
//!
//! The service logs it at start and publishes it with every sample, so a VM
//! that can grow but never shrink says why. It decides nothing: the policy is
//! unchanged by it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::meminfo::CommitModel;

const DRIVER_PATH: &str = "/sys/bus/virtio/drivers/virtio_mem";
const AUTO_ONLINE_PATH: &str = "/sys/devices/system/memory/auto_online_blocks";
const ONLINE_POLICY_PATH: &str = "/sys/module/memory_hotplug/parameters/online_policy";
const ZONEINFO_PATH: &str = "/proc/zoneinfo";
/// Zone information grows with nodes and CPUs; a megabyte is far above both.
const MAX_ZONEINFO_BYTES: usize = 1024 * 1024;

/// The kernel's answers as read, before any judgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// A virtio device is bound to the virtio-mem driver.
    pub driver_bound: bool,
    pub pressure_readable: bool,
    /// The registered pressure trigger, or `none`.
    pub pressure_trigger: &'static str,
    /// `auto_online_blocks`: offline, online, online_kernel or online_movable.
    pub auto_online: Option<String>,
    /// `online_policy`: contig-zones or auto-movable.
    pub online_policy: Option<String>,
    pub zoneinfo: Option<String>,
    pub swap_total_bytes: Option<u64>,
    pub commit: CommitModel,
}

impl Probe {
    pub fn read() -> Self {
        let text = |path: &str, maximum: usize| {
            crate::files::read_bounded(Path::new(path), maximum)
                .ok()
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        };
        let word = |path: &str| text(path, 256).map(|value| value.trim().to_owned());
        Self {
            driver_bound: std::fs::read_dir(DRIVER_PATH).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("virtio"))
                })
            }),
            pressure_readable: text(crate::pressure::PRESSURE_PATH, 4096).is_some(),
            pressure_trigger: crate::pressure::trigger_description(),
            auto_online: word(AUTO_ONLINE_PATH),
            online_policy: word(ONLINE_POLICY_PATH),
            zoneinfo: text(ZONEINFO_PATH, MAX_ZONEINFO_BYTES),
            swap_total_bytes: text(crate::meminfo::MEMINFO_PATH, 64 * 1024).and_then(|meminfo| {
                let kibibytes = meminfo
                    .lines()
                    .find_map(|line| line.strip_prefix("SwapTotal:"))?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse::<u64>()
                    .ok()?;
                kibibytes.checked_mul(1024)
            }),
            commit: CommitModel::current(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// Published facts, name to value. Unreadable facts are `unknown`.
    pub environment: BTreeMap<String, String>,
    /// What an administrator should know before relying on this guest.
    pub warnings: Vec<String>,
}

/// Pages the kernel manages in movable zones, summed over nodes.
pub fn movable_zone_pages(zoneinfo: &str) -> Option<u64> {
    let mut in_movable = false;
    let mut seen = false;
    let mut pages = 0u64;
    for line in zoneinfo.lines() {
        if let Some(zone) = line
            .strip_prefix("Node ")
            .and_then(|rest| rest.split_once(", zone"))
        {
            in_movable = zone.1.trim() == "Movable";
            seen |= in_movable;
        } else if in_movable {
            if let Some(managed) = line.trim().strip_prefix("managed") {
                pages = pages.checked_add(managed.trim().parse().ok()?)?;
            }
        }
    }
    seen.then_some(pages)
}

pub fn assess(probe: &Probe) -> Readiness {
    let unknown = || "unknown".to_owned();
    let auto_online = probe.auto_online.clone().unwrap_or_else(unknown);
    let online_policy = probe.online_policy.clone().unwrap_or_else(unknown);
    let movable = probe.zoneinfo.as_deref().and_then(movable_zone_pages);
    let mut warnings = Vec::new();
    if !probe.driver_bound {
        warnings.push(
            "no device is bound to the virtio-mem driver: this guest cannot be resized".to_owned(),
        );
    }
    if !probe.pressure_readable {
        warnings.push(
            "pressure stall information is unavailable (kernel built without PSI, or psi=0): \
             the guest can ask for memory but never returns any"
                .to_owned(),
        );
    }
    if probe.pressure_readable && probe.pressure_trigger == "none" {
        warnings.push(
            "no pressure trigger could be registered: urgent growth waits for the next poll"
                .to_owned(),
        );
    }
    match auto_online.as_str() {
        "offline" => warnings.push(
            "auto_online_blocks is offline: hot-plugged memory stays unused unless something \
             else onlines it"
                .to_owned(),
        ),
        "online" | "online_kernel" if online_policy != "auto-movable" => warnings.push(format!(
            "hot-plugged memory is onlined to kernel zones (auto_online_blocks={auto_online}, \
             online_policy={online_policy}): the kernel guarantees removal only from the \
             movable zone, so memory may not all be returned"
        )),
        _ => {}
    }
    let environment = [
        (
            "virtio_mem_driver",
            if probe.driver_bound {
                "bound"
            } else {
                "absent"
            }
            .to_owned(),
        ),
        (
            "pressure_signal",
            if probe.pressure_readable {
                "psi"
            } else {
                "none"
            }
            .to_owned(),
        ),
        ("pressure_trigger", probe.pressure_trigger.to_owned()),
        ("auto_online_blocks", auto_online),
        ("online_policy", online_policy),
        (
            "movable_zone_pages",
            movable.map_or_else(unknown, |pages| pages.to_string()),
        ),
        (
            "swap_total_bytes",
            probe
                .swap_total_bytes
                .map_or_else(unknown, |bytes| bytes.to_string()),
        ),
        (
            "commit_limit",
            match probe.commit {
                CommitModel::Enforced => "enforced",
                CommitModel::NotEnforced => "not_enforced",
            }
            .to_owned(),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    Readiness {
        environment,
        warnings,
    }
}

/// The published facts for this moment.
pub fn environment() -> BTreeMap<String, String> {
    assess(&Probe::read()).environment
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONEINFO: &str = "Node 0, zone      DMA\n  pages free     3839\n        managed  3839\n\
        Node 0, zone   Normal\n        present  327680\n        managed  295424\n\
        Node 0, zone  Movable\n  pages free     100\n        present  65536\n        managed  65000\n\
        Node 1, zone  Movable\n        managed  536\n\
        Node 1, zone   Device\n        managed  7\n";

    fn ready() -> Probe {
        Probe {
            driver_bound: true,
            pressure_readable: true,
            pressure_trigger: "100ms/1s",
            auto_online: Some("online_movable".to_owned()),
            online_policy: Some("contig-zones".to_owned()),
            zoneinfo: Some(ZONEINFO.to_owned()),
            swap_total_bytes: Some(0),
            commit: CommitModel::NotEnforced,
        }
    }

    #[test]
    fn movable_pages_are_summed_over_nodes_and_absent_zones_are_unknown() {
        assert_eq!(movable_zone_pages(ZONEINFO), Some(65536));
        assert_eq!(
            movable_zone_pages("Node 0, zone  Movable\n        managed  0\n"),
            Some(0)
        );
        assert_eq!(
            movable_zone_pages("Node 0, zone   Normal\n  managed  5\n"),
            None
        );
        assert_eq!(
            movable_zone_pages("Node 0, zone  Movable\n        managed  many\n"),
            None
        );
    }

    #[test]
    fn a_ready_guest_has_no_warnings_and_publishes_every_fact() {
        let readiness = assess(&ready());
        assert_eq!(readiness.warnings, Vec::<String>::new());
        let fact = |name: &str| readiness.environment[name].as_str();
        assert_eq!(fact("virtio_mem_driver"), "bound");
        assert_eq!(fact("pressure_signal"), "psi");
        assert_eq!(fact("pressure_trigger"), "100ms/1s");
        assert_eq!(fact("auto_online_blocks"), "online_movable");
        assert_eq!(fact("online_policy"), "contig-zones");
        assert_eq!(fact("movable_zone_pages"), "65536");
        assert_eq!(fact("swap_total_bytes"), "0");
        assert_eq!(fact("commit_limit"), "not_enforced");
    }

    #[test]
    fn each_shortcoming_is_named_once() {
        let warned = |probe: Probe, text: &str| {
            let warnings = assess(&probe).warnings;
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains(text), "{warnings:?}");
        };
        warned(
            Probe {
                driver_bound: false,
                ..ready()
            },
            "cannot be resized",
        );
        warned(
            Probe {
                pressure_readable: false,
                ..ready()
            },
            "never returns",
        );
        warned(
            Probe {
                pressure_trigger: "none",
                ..ready()
            },
            "next poll",
        );
        warned(
            Probe {
                auto_online: Some("offline".to_owned()),
                ..ready()
            },
            "stays unused",
        );
        for kernel in ["online", "online_kernel"] {
            warned(
                Probe {
                    auto_online: Some(kernel.to_owned()),
                    ..ready()
                },
                "kernel zones",
            );
            // The auto-movable policy places hot-plugged memory itself.
            let auto = Probe {
                auto_online: Some(kernel.to_owned()),
                online_policy: Some("auto-movable".to_owned()),
                ..ready()
            };
            assert!(assess(&auto).warnings.is_empty());
        }
    }

    #[test]
    fn unreadable_facts_are_published_as_unknown_without_inventing_a_warning() {
        let readiness = assess(&Probe {
            auto_online: None,
            online_policy: None,
            zoneinfo: None,
            swap_total_bytes: None,
            ..ready()
        });
        assert!(readiness.warnings.is_empty());
        for name in [
            "auto_online_blocks",
            "online_policy",
            "movable_zone_pages",
            "swap_total_bytes",
        ] {
            assert_eq!(readiness.environment[name], "unknown", "{name}");
        }
    }
}
