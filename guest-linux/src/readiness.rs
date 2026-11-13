//! What this guest can and cannot do, from the kernel's own files: whether
//! the virtio-mem driver has a device, whether pressure stall information is
//! available, how hot-plugged memory is onlined and how much of it the
//! movable zone can take, and what swap and commit accounting are in force.
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
const AUTO_MOVABLE_RATIO_PATH: &str = "/sys/module/memory_hotplug/parameters/auto_movable_ratio";
const AUTO_MOVABLE_NUMA_AWARE_PATH: &str =
    "/sys/module/memory_hotplug/parameters/auto_movable_numa_aware";
/// The firmware's boot memory map, world-readable. The kernel adds
/// hot-plugged memory to it only when it is plain `System RAM`; virtio-mem's
/// is driver-managed (`System RAM (virtio_mem)`) and never appears.
const FIRMWARE_MEMMAP_PATH: &str = "/sys/firmware/memmap";
/// The firmware map has one entry per range; real maps have tens.
const MAX_MEMMAP_ENTRIES: usize = 4096;
/// The kernel's page size on every architecture this guest is built for.
const PAGE_BYTES: u64 = 4096;
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
    /// `auto_movable_ratio`: the most movable memory auto-movable allows, as
    /// a percentage of the boot memory in kernel zones.
    pub auto_movable_ratio: Option<u64>,
    /// `auto_movable_numa_aware`: the ratio also applies to each node.
    pub auto_movable_numa_aware: Option<bool>,
    /// `System RAM` in the firmware's boot memory map.
    pub boot_ram_bytes: Option<u64>,
    pub zoneinfo: Option<String>,
    pub swap_total_bytes: Option<u64>,
    pub commit: CommitModel,
}

impl Probe {
    pub fn read() -> Self {
        let text = |path: &str, maximum: usize| {
            virtio_mem_guest::files::read_bounded(Path::new(path), maximum)
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
            auto_movable_ratio: word(AUTO_MOVABLE_RATIO_PATH).and_then(|ratio| ratio.parse().ok()),
            auto_movable_numa_aware: word(AUTO_MOVABLE_NUMA_AWARE_PATH).and_then(|flag| match flag
                .as_str()
            {
                "Y" => Some(true),
                "N" => Some(false),
                _ => None,
            }),
            boot_ram_bytes: std::fs::read_dir(FIRMWARE_MEMMAP_PATH)
                .ok()
                .and_then(|entries| {
                    let ranges = entries
                        .take(MAX_MEMMAP_ENTRIES)
                        .map(|entry| {
                            let directory = entry.ok()?.path();
                            let field = |name: &str| {
                                text(directory.join(name).to_str()?, 256)
                                    .map(|value| value.trim().to_owned())
                            };
                            Some((field("start")?, field("end")?, field("type")?))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    boot_ram_bytes(&ranges)
                }),
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
    zone_pages(zoneinfo, "managed", |zone| zone == "Movable")
}

/// Bytes of `System RAM` in firmware memory-map entries of (start, end,
/// type), with inclusive hexadecimal bounds as `/sys/firmware/memmap` gives
/// them. `None` when there is none or an entry is malformed.
pub fn boot_ram_bytes(ranges: &[(String, String, String)]) -> Option<u64> {
    let hex = |value: &str| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok();
    let mut total = 0u64;
    let mut seen = false;
    for (start, end, kind) in ranges {
        if kind != "System RAM" {
            continue;
        }
        let size = hex(end)?.checked_sub(hex(start)?)?.checked_add(1)?;
        total = total.checked_add(size)?;
        seen = true;
    }
    seen.then_some(total)
}

/// Pages held for the contiguous memory allocator, summed over zones. The
/// kernel counts them as movable, not kernel, memory.
fn cma_pages(zoneinfo: &str) -> Option<u64> {
    zone_pages(zoneinfo, "cma", |_| true)
}

/// Sum one per-zone counter over the zones `wanted` selects; `None` when no
/// zone is selected or a counter is unreadable.
fn zone_pages(zoneinfo: &str, counter: &str, wanted: impl Fn(&str) -> bool) -> Option<u64> {
    let mut selected = false;
    let mut seen = false;
    let mut pages = 0u64;
    for line in zoneinfo.lines() {
        if let Some((_, zone)) = line
            .strip_prefix("Node ")
            .and_then(|rest| rest.split_once(", zone"))
        {
            selected = wanted(zone.trim());
            seen |= selected;
        } else if selected {
            let mut words = line.split_whitespace();
            if words.next() == Some(counter) {
                pages = pages.checked_add(words.next()?.parse().ok()?)?;
            }
        }
    }
    seen.then_some(pages)
}

/// Nodes with a zone in `zoneinfo`.
fn node_count(zoneinfo: &str) -> usize {
    let mut nodes: Vec<&str> = zoneinfo
        .lines()
        .filter_map(|line| line.strip_prefix("Node ")?.split_once(','))
        .map(|(node, _)| node.trim())
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes.len()
}

/// Whether the guest can reach `max_ram` (whole-guest RAM) with every
/// hot-plugged byte in the movable zone, where removal is reliable.
///
/// Only the auto-movable policy (with `auto_online_blocks=online`) limits
/// the movable zone. `mm/memory_hotplug.c` (`auto_movable_can_online_movable`)
/// onlines hot-plugged memory movable only while all movable memory, CMA
/// included, stays within `auto_movable_ratio` percent of the kernel-zone
/// memory present at boot, less CMA. Beyond that a block goes to a kernel
/// zone, where removal is not guaranteed. Kernel-zone memory of the same
/// device raises its movable allowance by the ratio, so past the limit new
/// memory is split: one part in `1 + ratio / 100` goes to kernel zones
/// (measured on a live guest, 2026-11-13: 640 MiB of 4 GiB at ratio 301 and
/// 512 MiB boot). Boot RAM is taken from the firmware map; boot memory
/// placed in the movable zone (`movablecore=`) is not counted. Unknown facts
/// give no warning.
pub fn movable_ratio_warning(probe: &Probe, max_ram: u64) -> Option<String> {
    if probe.online_policy.as_deref() != Some("auto-movable")
        || probe.auto_online.as_deref() != Some("online")
    {
        return None;
    }
    let ratio = probe.auto_movable_ratio?;
    let boot = probe.boot_ram_bytes?;
    let zoneinfo = probe.zoneinfo.as_deref()?;
    let cma = cma_pages(zoneinfo)?.checked_mul(PAGE_BYTES)?;
    let kernel = boot.checked_sub(cma)?;
    let movable_limit = u64::try_from(u128::from(kernel) * u128::from(ratio) / 100)
        .ok()?
        .saturating_sub(cma);
    let reachable = boot.checked_add(movable_limit)?;
    if max_ram <= reachable {
        return None;
    }
    let per_node = if probe.auto_movable_numa_aware == Some(true) && node_count(zoneinfo) > 1 {
        "; the ratio also applies to each NUMA node (auto_movable_numa_aware=Y), so a \
         node's limit may be lower still"
    } else {
        ""
    };
    let kernel_at_max =
        u64::try_from(u128::from(max_ram - reachable) * 100 / (100 + u128::from(ratio))).ok()?;
    Some(format!(
        "max_ram={max_ram} is more than auto-movable can keep removable: boot RAM is \
         {boot} bytes and auto_movable_ratio={ratio} allows {movable_limit} bytes of \
         hot-plugged memory in the movable zone, so above {reachable} bytes of guest RAM \
         about one byte in {} of further memory goes to kernel zones, where removal is \
         not guaranteed: up to {kernel_at_max} bytes at max_ram{per_node}. Raise boot \
         memory, lower max_ram or raise memory_hotplug.auto_movable_ratio",
        format_ratio_share(ratio)
    ))
}

/// `1 + ratio / 100`, the share of memory past the limit that goes to kernel
/// zones, as "4" for 300 or "4.01" for 301.
fn format_ratio_share(ratio: u64) -> String {
    let hundredths = 100 + ratio;
    match hundredths % 100 {
        0 => (hundredths / 100).to_string(),
        rest => format!("{}.{rest:02}", hundredths / 100),
    }
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
    // A missing pressure trigger is reported where it is registered
    // (`ProcPressure::open`), once the host's settings have arrived: at start
    // it is still `pending`.
    match auto_online.as_str() {
        "offline" => warnings.push(
            "auto_online_blocks is offline: hot-plugged memory stays unused unless something \
             else onlines it"
                .to_owned(),
        ),
        "online" if online_policy != "auto-movable" => warnings.push(format!(
            "hot-plugged memory is onlined to kernel zones (auto_online_blocks={auto_online}, \
             online_policy={online_policy}): the kernel guarantees removal only from the \
             movable zone. If removals come back partial, boot with \
             memory_hotplug.online_policy=auto-movable"
        )),
        // online_kernel always picks a kernel zone; the online policy applies
        // only to plain "online" (zone_for_pfn_range in mm/memory_hotplug.c).
        "online_kernel" => warnings.push(format!(
            "hot-plugged memory is onlined to kernel zones (auto_online_blocks=online_kernel, \
             online_policy={online_policy}): the kernel guarantees removal only from the \
             movable zone, and online_kernel ignores the online policy. If removals come \
             back partial, boot with memhp_default_state=online and \
             memory_hotplug.online_policy=auto-movable"
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
            "boot_ram_bytes",
            probe
                .boot_ram_bytes
                .map_or_else(unknown, |bytes| bytes.to_string()),
        ),
        (
            "auto_movable_ratio",
            probe
                .auto_movable_ratio
                .map_or_else(unknown, |ratio| ratio.to_string()),
        ),
        (
            "auto_movable_numa_aware",
            probe
                .auto_movable_numa_aware
                .map_or_else(unknown, |aware| aware.to_string()),
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
    let mut environment = assess(&Probe::read()).environment;
    environment.insert(
        "protected_cache_bytes".to_owned(),
        crate::cache::protected_bytes().to_string(),
    );
    environment
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

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
            auto_movable_ratio: Some(301),
            auto_movable_numa_aware: Some(true),
            boot_ram_bytes: Some(GIB),
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
        assert_eq!(fact("boot_ram_bytes"), GIB.to_string());
        assert_eq!(fact("auto_movable_ratio"), "301");
        assert_eq!(fact("auto_movable_numa_aware"), "true");
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
        // The trigger is registered after start; neither state warns here.
        for trigger in ["pending", "none"] {
            assert!(assess(&Probe {
                pressure_trigger: trigger,
                ..ready()
            })
            .warnings
            .is_empty());
        }
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
        }
        // The auto-movable policy places plain "online" memory itself, but
        // online_kernel ignores the policy.
        let auto = |auto_online: &str| Probe {
            auto_online: Some(auto_online.to_owned()),
            online_policy: Some("auto-movable".to_owned()),
            ..ready()
        };
        assert!(assess(&auto("online")).warnings.is_empty());
        warned(auto("online_kernel"), "ignores the online policy");
    }

    #[test]
    fn unreadable_facts_are_published_as_unknown_without_inventing_a_warning() {
        let readiness = assess(&Probe {
            auto_online: None,
            online_policy: None,
            auto_movable_ratio: None,
            auto_movable_numa_aware: None,
            boot_ram_bytes: None,
            zoneinfo: None,
            swap_total_bytes: None,
            ..ready()
        });
        assert!(readiness.warnings.is_empty());
        for name in [
            "auto_online_blocks",
            "online_policy",
            "movable_zone_pages",
            "boot_ram_bytes",
            "auto_movable_ratio",
            "auto_movable_numa_aware",
            "swap_total_bytes",
        ] {
            assert_eq!(readiness.environment[name], "unknown", "{name}");
        }
    }

    #[test]
    fn boot_ram_is_the_system_ram_in_the_firmware_map() {
        let entry = |start: &str, end: &str, kind: &str| {
            (start.to_owned(), end.to_owned(), kind.to_owned())
        };
        let map = [
            entry("0x0", "0x9ffff", "System RAM"),
            entry("0xe0000000", "0xefffffff", "Reserved"),
            entry("0x100000", "0x3fffffff", "System RAM"),
            entry("0x3fb6d000", "0x3fb7efff", "ACPI Tables"),
        ];
        assert_eq!(boot_ram_bytes(&map), Some(0xa_0000 + 0x3ff0_0000));
        assert_eq!(boot_ram_bytes(&map[1..2]), None);
        assert_eq!(boot_ram_bytes(&[entry("0x10", "0x0", "System RAM")]), None);
        assert_eq!(boot_ram_bytes(&[entry("16", "32", "System RAM")]), None);
        // The counter is matched as a whole word, not as a prefix.
        assert_eq!(
            cma_pages("Node 0, zone   Normal\n  cmathing 9\n  cma 4\n"),
            Some(4)
        );
        assert_eq!(node_count(ZONEINFO), 2);
    }

    #[test]
    fn auto_movable_warns_only_when_max_ram_outgrows_the_movable_ratio() {
        // 2 GiB of boot RAM on one node at the default ratio. Hot-plugged
        // memory already in kernel zones does not enlarge the allowance.
        let zoneinfo = "Node 0, zone    DMA32\n  present  524288\n  cma 0\n\
                        Node 0, zone   Normal\n  present  9999999\n  cma 0\n";
        let probe = Probe {
            auto_online: Some("online".to_owned()),
            online_policy: Some("auto-movable".to_owned()),
            boot_ram_bytes: Some(2 * GIB),
            zoneinfo: Some(zoneinfo.to_owned()),
            ..ready()
        };
        let reachable = 2 * GIB + 2 * GIB * 301 / 100;
        assert_eq!(movable_ratio_warning(&probe, reachable), None);
        let warning = movable_ratio_warning(&probe, reachable + 1).expect("warning");
        assert!(warning.contains("removal is not guaranteed"), "{warning}");
        assert!(!warning.contains("NUMA"), "one node: {warning}");
        // Past the limit memory is split 1 : 3.01, so 401 MiB more puts
        // 100 MiB in kernel zones.
        let warning = movable_ratio_warning(&probe, reachable + 401 * (1 << 20)).expect("warning");
        assert!(warning.contains("one byte in 4.01 "), "{warning}");
        assert!(warning.contains("up to 104857600 bytes"), "{warning}");
        assert_eq!(format_ratio_share(300), "4");
        assert_eq!(format_ratio_share(5), "1.05");

        // CMA is movable memory taken out of the kernel's share.
        let cma = 256 << 20;
        let with_cma = Probe {
            zoneinfo: Some(format!(
                "Node 0, zone    DMA32\n  cma {}\n",
                cma / PAGE_BYTES
            )),
            ..probe.clone()
        };
        let reachable = 2 * GIB + (2 * GIB - cma) * 301 / 100 - cma;
        assert_eq!(movable_ratio_warning(&with_cma, reachable), None);
        assert!(movable_ratio_warning(&with_cma, reachable + 1).is_some());

        // Several nodes under the per-node rule say so.
        let numa = Probe {
            zoneinfo: Some(format!("{zoneinfo}Node 1, zone   Normal\n  present 1\n")),
            ..probe.clone()
        };
        let warning = movable_ratio_warning(&numa, 64 * GIB).expect("warning");
        assert!(warning.contains("NUMA"), "{warning}");

        // Only auto-movable onlining is limited, and unknown facts never warn.
        for other in [
            Probe {
                online_policy: Some("contig-zones".to_owned()),
                ..probe.clone()
            },
            Probe {
                auto_online: Some("online_movable".to_owned()),
                ..probe.clone()
            },
            Probe {
                auto_movable_ratio: None,
                ..probe.clone()
            },
            Probe {
                zoneinfo: None,
                ..probe.clone()
            },
            Probe {
                boot_ram_bytes: None,
                ..probe.clone()
            },
        ] {
            assert_eq!(movable_ratio_warning(&other, 64 * GIB), None);
        }
    }
}
