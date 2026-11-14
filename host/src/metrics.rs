//! The controller's metrics as a Prometheus text file (roadmap O4.2): no
//! listening socket, so the sandbox doesn't change. node_exporter's textfile
//! collector, or anything else, reads `/run/rampire/metrics.prom`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where the daemon writes its metrics.
pub const METRICS_PATH: &str = "/run/rampire/metrics.prom";

/// One VM as a round observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmMetrics {
    pub name: String,
    pub priority: u32,
    /// Bytes the VM holds of the pool; `None` when unknown this round.
    pub charged_bytes: Option<u64>,
    pub max_ram_bytes: u64,
    /// The controller stopped acting for this VM after a failure.
    pub stopped: bool,
    /// Resizing waits for `rampire recover`.
    pub latched: bool,
}

/// A per-VM metric: its name, its help and how to read it from a VM.
type Family = (&'static str, &'static str, fn(&VmMetrics) -> Option<u64>);

/// The metrics file's text.
pub fn render(
    build: &str,
    pool_bytes: Option<u64>,
    rounds: u64,
    unix_seconds: u64,
    vms: &[VmMetrics],
) -> String {
    let mut text = String::new();
    let mut family = |name: &str, kind: &str, help: &str| {
        let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} {kind}");
        name.to_owned()
    };
    let info = family(
        "rampire_build_info",
        "gauge",
        "The running controller's build.",
    );
    let mut lines = vec![format!("{info}{{build=\"{}\"}} 1", escape(build))];
    let rounds_name = family(
        "rampire_rounds_total",
        "counter",
        "Control rounds since the controller started.",
    );
    lines.push(format!("{rounds_name} {rounds}"));
    let last = family(
        "rampire_last_round_timestamp_seconds",
        "gauge",
        "When the last control round ran.",
    );
    lines.push(format!("{last} {unix_seconds}"));
    text.push_str(&lines.join("\n"));
    text.push('\n');
    if let Some(pool) = pool_bytes {
        let _ = writeln!(
            text,
            "# HELP rampire_pool_bytes Device memory shared by every VM.\n# TYPE rampire_pool_bytes gauge\nrampire_pool_bytes {pool}"
        );
    }
    let per_vm: [Family; 5] = [
        (
            "rampire_vm_charged_bytes",
            "Bytes the VM holds of the pool: max(requested, current).",
            |vm| vm.charged_bytes,
        ),
        ("rampire_vm_max_ram_bytes", "The VM's max_ram.", |vm| {
            Some(vm.max_ram_bytes)
        }),
        (
            "rampire_vm_priority",
            "The VM's priority; higher is served first.",
            |vm| Some(u64::from(vm.priority)),
        ),
        (
            "rampire_vm_stopped",
            "1 when the controller stopped acting for the VM after a failure.",
            |vm| Some(u64::from(vm.stopped)),
        ),
        (
            "rampire_vm_latched",
            "1 when resizing waits for rampire recover.",
            |vm| Some(u64::from(vm.latched)),
        ),
    ];
    for (name, help, value) in per_vm {
        let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} gauge");
        for vm in vms {
            if let Some(value) = value(vm) {
                let _ = writeln!(text, "{name}{{vm=\"{}\"}} {value}", escape(&vm.name));
            }
        }
    }
    text
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Write the file in one step, so a reader never sees half of it.
pub fn write(path: &Path, text: &str) -> Result<(), String> {
    let temporary: PathBuf = path.with_extension("prom.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_every_family_with_help_and_type() {
        let vms = [
            VmMetrics {
                name: "web".to_owned(),
                priority: 100,
                charged_bytes: Some(1 << 30),
                max_ram_bytes: 4 << 30,
                stopped: false,
                latched: false,
            },
            VmMetrics {
                name: "db\"1".to_owned(),
                priority: 50,
                charged_bytes: None,
                max_ram_bytes: 8 << 30,
                stopped: true,
                latched: true,
            },
        ];
        let text = render("0.1.0 (build abc)", Some(15 << 30), 7, 1_794_600_000, &vms);
        assert!(text.contains("rampire_build_info{build=\"0.1.0 (build abc)\"} 1\n"));
        assert!(text.contains("rampire_rounds_total 7\n"));
        assert!(text.contains("rampire_pool_bytes 16106127360\n"));
        assert!(text.contains("rampire_vm_charged_bytes{vm=\"web\"} 1073741824\n"));
        assert!(
            !text.contains("rampire_vm_charged_bytes{vm=\"db"),
            "unknown is left out"
        );
        assert!(text.contains("rampire_vm_latched{vm=\"db\\\"1\"} 1\n"));
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let family = line.split(['{', ' ']).next().unwrap();
            assert!(
                text.contains(&format!("# TYPE {family} ")),
                "{family} has no TYPE"
            );
        }
        assert!(!render("b", None, 0, 0, &[]).contains("rampire_pool_bytes"));
    }

    #[test]
    fn the_file_is_replaced_whole() {
        let path =
            std::env::temp_dir().join(format!("rampire-metrics-{}.prom", std::process::id()));
        write(&path, "a 1\n").unwrap();
        write(&path, "a 2\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(text, "a 2\n");
    }
}
