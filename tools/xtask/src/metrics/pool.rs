//! `metrics --vm all`: every VM the controller manages, recorded together.
//!
//! Each VM gets an ordinary recorder (a child `metrics --vm NAME --plain`) in
//! its own directory, with its samples, events and incidents. This process
//! adds `pool.csv`, one row per interval with every device side by side, the
//! pool's charge and free bytes and the host's available memory, so how the
//! VMs' demands meet in the pool can be read from one file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_mem_core::VirtioMemState;
use virtio_mem_host::config::HostSettings;
use virtio_mem_host::virsh::Virsh;

use super::{compact_utc, iso_utc, unix_millis, COMMAND_TIMEOUT};
use crate::watch;

/// Options this mode takes; everything else is for single-VM recording.
struct Options {
    config: PathBuf,
    output: Option<PathBuf>,
    interval: Duration,
    duration: Option<Duration>,
    /// Passed through to every child unchanged.
    forwarded: Vec<String>,
}

/// Whether `args` ask for every VM (`--vm all`).
pub fn requested(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == "--vm" && pair[1] == "all")
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        config: PathBuf::new(),
        output: None,
        interval: Duration::from_secs(1),
        duration: None,
        forwarded: Vec::new(),
    };
    let mut config = None;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--plain" {
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{option} requires a value"))?;
        let seconds = |minimum: f64| {
            value
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= minimum)
                .map(Duration::from_secs_f64)
                .ok_or_else(|| format!("{option} requires a number of seconds >= {minimum}"))
        };
        match option {
            "--vm" if value == "all" => {}
            "--config" => config = Some(PathBuf::from(value)),
            "--output" => options.output = Some(PathBuf::from(value)),
            "--interval-seconds" => {
                options.interval = seconds(0.5)?;
                options.forwarded.extend([option.to_owned(), value.clone()]);
            }
            "--duration-seconds" => {
                options.duration = Some(seconds(1.0)?);
                options.forwarded.extend([option.to_owned(), value.clone()]);
            }
            "--instance" | "--connect" | "--guest-interval-seconds" => {
                options.forwarded.extend([option.to_owned(), value.clone()]);
            }
            _ => return Err(format!("metrics --vm all does not take {option}")),
        }
        index += 2;
    }
    options.config = config.ok_or("metrics requires --config")?;
    Ok(options)
}

/// The arguments for one VM's recorder.
fn child_arguments(options: &Options, vm: &str, directory: &Path) -> Vec<String> {
    let mut arguments = vec![
        "metrics".to_owned(),
        "--config".to_owned(),
        options.config.display().to_string(),
        "--vm".to_owned(),
        vm.to_owned(),
        "--plain".to_owned(),
        "--output".to_owned(),
        directory.display().to_string(),
    ];
    arguments.extend(options.forwarded.iter().cloned());
    arguments
}

/// One VM's device in a pool row: `None` when its state could not be read.
type Device = Option<VirtioMemState>;

fn header(vms: &[String]) -> String {
    let mut line = "time_utc,elapsed_s,host_available_bytes,pool_bytes,pool_charged_bytes,\
                    pool_free_bytes"
        .to_owned();
    for vm in vms {
        line.push_str(&format!(",{vm}_requested_bytes,{vm}_current_bytes"));
    }
    line
}

/// What the pool charges: `max(requested, current)` per device, as the
/// controller counts it; `None` while any device is unreadable.
fn charged(devices: &[Device]) -> Option<u64> {
    devices.iter().try_fold(0_u64, |total, device| {
        let device = device.as_ref()?;
        total.checked_add(device.requested_bytes.max(device.current_bytes))
    })
}

fn row(
    unix_millis: u64,
    elapsed: Duration,
    host_available: u64,
    pool_bytes: Option<u64>,
    devices: &[Device],
) -> String {
    let charge = charged(devices);
    let cell = |value: Option<u64>| value.map_or_else(String::new, |value| value.to_string());
    let mut line = format!(
        "{},{:.1},{host_available},{},{},{}",
        iso_utc(unix_millis),
        elapsed.as_secs_f64(),
        cell(pool_bytes),
        cell(charge),
        cell(
            pool_bytes
                .zip(charge)
                .map(|(pool, charge)| pool.saturating_sub(charge))
        ),
    );
    for device in devices {
        line.push_str(&format!(
            ",{},{}",
            cell(device.map(|device| device.requested_bytes)),
            cell(device.map(|device| device.current_bytes))
        ));
    }
    line
}

/// The console line for a row: host, pool and each VM's current size.
fn progress(
    vms: &[String],
    host_available: u64,
    pool_bytes: Option<u64>,
    devices: &[Device],
) -> String {
    let gib = |bytes: u64| format!("{:.1}G", bytes as f64 / f64::from(1 << 30));
    let mut line = format!("host avail {}", gib(host_available));
    if let (Some(pool), Some(charge)) = (pool_bytes, charged(devices)) {
        line.push_str(&format!(
            " | pool {}/{} free {}",
            gib(charge),
            gib(pool),
            gib(pool.saturating_sub(charge))
        ));
    }
    for (vm, device) in vms.iter().zip(devices) {
        match device {
            Some(device) if device.requested_bytes != device.current_bytes => {
                line.push_str(&format!(
                    " | {vm} {}->{}",
                    gib(device.current_bytes),
                    gib(device.requested_bytes)
                ))
            }
            Some(device) => line.push_str(&format!(" | {vm} {}", gib(device.current_bytes))),
            None => line.push_str(&format!(" | {vm} ?")),
        }
    }
    line
}

pub fn run(args: &[String], repo: &Path) -> Result<(), String> {
    let options = parse(args)?;
    let settings = HostSettings::load(&options.config)
        .map_err(|error| format!("{}: {error}", options.config.display()))?;
    let vms: Vec<(String, String)> = settings
        .vms
        .iter()
        .map(|vm| (vm.vm_name.clone(), vm.alias.clone()))
        .collect();
    let names: Vec<String> = vms.iter().map(|(name, _)| name.clone()).collect();
    let started_millis = unix_millis();
    let directory = options.output.clone().unwrap_or_else(|| {
        repo.join(".artifacts/metrics")
            .join(format!("pool-{}", compact_utc(started_millis)))
    });
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("install signal handler: {error}"))?;
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("locate this executable: {error}"))?;
    let mut children = Vec::new();
    for name in &names {
        let child_directory = directory.join(name);
        std::fs::create_dir_all(&child_directory)
            .map_err(|error| format!("create {}: {error}", child_directory.display()))?;
        let console = std::fs::File::create(child_directory.join("console.log"))
            .map_err(|error| format!("create console log for {name}: {error}"))?;
        let child = std::process::Command::new(&executable)
            .args(child_arguments(&options, name, &child_directory))
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(console)
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("start the recorder for {name}: {error}"))?;
        children.push((name.clone(), child));
    }
    println!(
        "recording {} VMs to {} (Ctrl+C to stop)",
        names.len(),
        directory.display()
    );
    let sampled = sample(&options, &settings, &vms, &directory, &stop);
    stop_children(&mut children);
    sampled?;
    println!("recorded in {}", directory.display());
    Ok(())
}

fn sample(
    options: &Options,
    settings: &HostSettings,
    vms: &[(String, String)],
    directory: &Path,
    stop: &AtomicBool,
) -> Result<(), String> {
    let names: Vec<String> = vms.iter().map(|(name, _)| name.clone()).collect();
    let connect = options
        .forwarded
        .windows(2)
        .find(|pair| pair[0] == "--connect")
        .map_or("qemu:///system", |pair| pair[1].as_str())
        .to_owned();
    let virsh = Virsh::persistent_with_connection("virsh", COMMAND_TIMEOUT, connect);
    let path = directory.join("pool.csv");
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(&path)
            .map_err(|error| format!("create {}: {error}", path.display()))?,
    );
    let write = |error: std::io::Error| format!("write {}: {error}", path.display());
    writeln!(file, "{}", header(&names)).map_err(write)?;
    let clock = Instant::now();
    while !stop.load(Ordering::Acquire)
        && options
            .duration
            .is_none_or(|duration| clock.elapsed() < duration)
    {
        let started = Instant::now();
        let host = watch::read_meminfo()?;
        let devices: Vec<Device> = vms
            .iter()
            .map(|(name, alias)| watch::device_state(&virsh, name, alias))
            .collect();
        writeln!(
            file,
            "{}",
            row(
                unix_millis(),
                clock.elapsed(),
                host.available,
                settings.pool_bytes,
                &devices
            )
        )
        .map_err(write)?;
        file.flush().map_err(write)?;
        println!(
            "{} {}",
            &iso_utc(unix_millis())[11..19],
            progress(&names, host.available, settings.pool_bytes, &devices)
        );
        let remaining = options.interval.saturating_sub(started.elapsed());
        let until = Instant::now() + remaining;
        while Instant::now() < until && !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(100).min(until - Instant::now()));
        }
    }
    Ok(())
}

/// Ask every recorder to finish its files, then wait for it.
fn stop_children(children: &mut [(String, Child)]) {
    for (name, child) in children.iter_mut() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            continue;
        }
        let asked = std::process::Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        if !asked.is_ok_and(|status| status.success()) {
            // Best effort: a recorder that cannot be asked is stopped.
            eprintln!("could not ask the recorder for {name} to stop; killing it");
            if let Err(error) = child.kill() {
                eprintln!("could not stop the recorder for {name}: {error}");
            }
        }
    }
    for (name, child) in children.iter_mut() {
        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("recorder for {name} ended with {status}"),
            Err(error) => eprintln!("recorder for {name}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn device(requested: u64, current: u64) -> Device {
        Some(VirtioMemState {
            size_bytes: 15 * GIB,
            block_size_bytes: 2 << 20,
            requested_bytes: requested,
            current_bytes: current,
        })
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn only_vm_all_selects_the_pool_recorder() {
        assert!(requested(&strings(&["--config", "c", "--vm", "all"])));
        assert!(!requested(&strings(&["--config", "c", "--vm", "safe"])));
        assert!(!requested(&strings(&["--config", "all"])));
    }

    #[test]
    fn children_get_their_vm_and_directory_and_the_shared_options() {
        let options = parse(&strings(&[
            "--config",
            "/etc/h.toml",
            "--vm",
            "all",
            "--plain",
            "--duration-seconds",
            "60",
            "--instance",
            "main",
        ]))
        .expect("options");
        assert_eq!(options.duration, Some(Duration::from_secs(60)));
        assert_eq!(
            child_arguments(&options, "a", Path::new("/out/a")),
            strings(&[
                "metrics",
                "--config",
                "/etc/h.toml",
                "--vm",
                "a",
                "--plain",
                "--output",
                "/out/a",
                "--duration-seconds",
                "60",
                "--instance",
                "main",
            ])
        );
        assert!(parse(&strings(&["--vm", "all"])).is_err(), "needs --config");
        assert!(parse(&strings(&["--config", "c", "--vm", "all", "--bogus", "1"])).is_err());
    }

    #[test]
    fn a_pool_row_charges_each_device_as_the_controller_does() {
        let vms = strings(&["a", "b"]);
        // a is shrinking (still charged at current), b is growing.
        let devices = [device(GIB, 2 * GIB), device(3 * GIB, GIB)];
        let line = row(
            0,
            Duration::from_secs(2),
            10 * GIB,
            Some(15 * GIB),
            &devices,
        );
        let cells: Vec<&str> = line.split(',').collect();
        assert_eq!(header(&vms).split(',').count(), cells.len());
        assert_eq!(cells[1], "2.0");
        assert_eq!(cells[4], (5 * GIB).to_string(), "charged");
        assert_eq!(cells[5], (10 * GIB).to_string(), "free");
        assert_eq!(
            cells[6..],
            [GIB, 2 * GIB, 3 * GIB, GIB].map(|v| v.to_string())
        );
        // An unreadable device leaves the pool cells empty, not wrong.
        let line = row(
            0,
            Duration::ZERO,
            GIB,
            Some(15 * GIB),
            &[device(GIB, GIB), None],
        );
        let cells: Vec<&str> = line.split(',').collect();
        assert_eq!((cells[4], cells[5]), ("", ""));
        assert_eq!(cells[8..], ["", ""]);
        let text = progress(&vms, 10 * GIB, Some(15 * GIB), &devices);
        assert_eq!(
            text,
            "host avail 10.0G | pool 5.0G/15.0G free 10.0G | a 2.0G->1.0G | b 1.0G->3.0G"
        );
    }
}
