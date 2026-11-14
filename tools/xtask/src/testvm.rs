//! Disposable test VMs on the development KVM host (`cargo xtask testvm`).
//!
//! A scenario file (`tools/testvm/*.toml`) says what the VM is: its cloud
//! image, boot RAM, optional virtio-mem device and the commands it runs on
//! first boot. `up` makes a fresh VM from it (a qcow2 overlay on the image
//! and a cloud-init seed carrying the commands and any `--file`), waits for
//! the commands to finish, reports each one's exit status from the serial
//! port, records evidence and removes the VM again unless `--keep`. `down`
//! removes a VM that `up` made, and nothing else: each one carries a
//! metadata element naming its disk and seed.
//!
//! The host's own values (where images go, which bridge VMs join) come from
//! the environment, never from the scenario or the code.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rampire_ops::process;
use serde::Deserialize;

pub const HELP: &str = r#"Test VMs (run as root on the KVM host):
  cargo xtask testvm up SCENARIO --name NAME [--file PATH]... [--keep] [--apply]
  cargo xtask testvm down NAME [--apply]
  cargo xtask testvm list

up options:
  --name NAME      The domain name (a-z, 0-9, - and _).
  --file PATH      Put PATH on the VM's seed; commands find it in $SEED.
  --keep           Leave the VM running afterwards and print its address,
                   for further tests; remove it later with `down`.
  --apply          Make and start the VM; otherwise show what would be done.

down removes a VM that `up` made: it stops RAMpire managing it if it does
(`rampire vm remove`), then removes the domain, its disk and its seed.

Environment (the host's values; see deploy/examples/testvm.env.example):
  RAMPIRE_TESTVM_IMAGES   Folder for disks, seeds and downloaded images.
  RAMPIRE_TESTVM_BRIDGE   Host bridge the VMs join.
  RAMPIRE_TESTVM_SSH_KEY  Optional public key file for the VM's user tester.
"#;

const NAMESPACE: &str = "https://github.com/macg4dave/rampire/testvm";
const MARK: &str = "RAMPIRE-TESTVM";
const USER: &str = "tester";
/// Host memory left free beyond the VM's boot RAM before `up` starts one.
const HOST_MARGIN_MIB: u64 = 1024;
const POLL: Duration = Duration::from_secs(5);

/// What a test VM is and what it runs on first boot.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    /// An `https://` URL (downloaded once into the images folder) or a path.
    image: String,
    boot_mib: u64,
    /// A virtio-mem device of this size, starting empty; 0 for none.
    #[serde(default)]
    device_mib: u64,
    /// Without a device: room for one (`maxMemory` and a NUMA cell), as
    /// `rampire vm add --fix-xml` expects.
    #[serde(default)]
    hotplug_mib: u64,
    #[serde(default = "default_vcpus")]
    vcpus: u32,
    #[serde(default = "default_disk_gib")]
    disk_gib: u64,
    /// Expose the host CPU's virtualisation, for a VM that is a KVM host.
    #[serde(default)]
    nested: bool,
    /// Distribution packages cloud-init installs before the commands.
    #[serde(default)]
    packages: Vec<String>,
    commands: Vec<String>,
    timeout_minutes: u64,
}

fn default_vcpus() -> u32 {
    2
}

fn default_disk_gib() -> u64 {
    10
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Up {
        scenario: PathBuf,
        name: String,
        files: Vec<PathBuf>,
        keep: bool,
        apply: bool,
    },
    Down {
        name: String,
        apply: bool,
    },
    List,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("up") => parse_up(&args[1..]),
        Some("down") => {
            let name = args.get(1).ok_or("testvm down requires a NAME")?;
            valid_name(name)?;
            let apply = match &args[2..] {
                [] => false,
                [flag] if flag == "--apply" => true,
                _ => return Err("testvm down takes only NAME and --apply".to_owned()),
            };
            Ok(Command::Down {
                name: name.clone(),
                apply,
            })
        }
        Some("list") if args.len() == 1 => Ok(Command::List),
        _ => Err(format!("testvm requires up, down or list\n\n{HELP}")),
    }
}

fn parse_up(args: &[String]) -> Result<Command, String> {
    let scenario = args
        .first()
        .filter(|value| !value.starts_with("--"))
        .ok_or("testvm up requires a SCENARIO file")?;
    let (mut name, mut files, mut keep, mut apply) = (None, Vec::new(), false, false);
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--keep" if !keep => keep = true,
            "--apply" if !apply => apply = true,
            option @ ("--name" | "--file") => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{option} requires a value"))?;
                if option == "--file" {
                    files.push(PathBuf::from(value));
                } else if name.replace(value.clone()).is_some() {
                    return Err("--name given twice".to_owned());
                }
                index += 1;
            }
            option => return Err(format!("unknown or duplicate testvm up option: {option}")),
        }
        index += 1;
    }
    let name = name.ok_or("testvm up requires --name")?;
    valid_name(&name)?;
    Ok(Command::Up {
        scenario: PathBuf::from(scenario),
        name,
        files,
        keep,
        apply,
    })
}

fn valid_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    let first_ok = characters
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if first_ok
        && name.len() <= 40
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(format!("test VM names use a-z, 0-9, - and _: {name}"))
    }
}

/// The host's values, from the environment.
struct Site {
    images: PathBuf,
    bridge: String,
    ssh_key: Option<String>,
}

fn site() -> Result<Site, String> {
    let required = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{name} is not set (see deploy/examples/testvm.env.example)"))
    };
    let images = PathBuf::from(required("RAMPIRE_TESTVM_IMAGES")?);
    if !images.is_absolute() || !images.is_dir() {
        return Err(format!(
            "RAMPIRE_TESTVM_IMAGES must be an existing absolute folder: {}",
            images.display()
        ));
    }
    let ssh_key = match std::env::var("RAMPIRE_TESTVM_SSH_KEY") {
        Ok(path) if !path.trim().is_empty() => {
            let key = process::read_file(Path::new(&path))?.trim().to_owned();
            if !key.starts_with("ssh-") || key.contains('\n') {
                return Err(format!("{path} is not one OpenSSH public key"));
            }
            Some(key)
        }
        _ => None,
    };
    Ok(Site {
        images,
        bridge: required("RAMPIRE_TESTVM_BRIDGE")?,
        ssh_key,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    match command {
        Command::Up {
            scenario,
            name,
            files,
            keep,
            apply,
        } => up(scenario, name, files, *keep, *apply, repo),
        Command::Down { name, apply } => {
            require_root()?;
            down(name, *apply, repo)
        }
        Command::List => {
            require_root()?;
            for name in virsh(&["list", "--all", "--name"], repo)?.lines() {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                if let Some(paths) = made_here(name, repo)? {
                    let state = virsh(&["domstate", name], repo)?;
                    println!("{name}\t{}\t{}", state.trim(), paths.disk.display());
                }
            }
            Ok(())
        }
    }
}

fn require_root() -> Result<(), String> {
    if process::effective_uid() == 0 {
        Ok(())
    } else {
        Err("testvm runs as root on the KVM host (sudo target/debug/xtask testvm ...)".to_owned())
    }
}

fn load_scenario(path: &Path) -> Result<Scenario, String> {
    let scenario: Scenario = toml::from_str(&process::read_file(path)?)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if scenario.boot_mib < 512 || scenario.boot_mib % 128 != 0 {
        return Err("boot_mib must be a multiple of 128, at least 512".to_owned());
    }
    for (field, value) in [
        ("device_mib", scenario.device_mib),
        ("hotplug_mib", scenario.hotplug_mib),
    ] {
        if value % 128 != 0 {
            return Err(format!("{field} must be a multiple of 128"));
        }
    }
    if scenario.device_mib > 0 && scenario.hotplug_mib > 0 {
        return Err("give device_mib or hotplug_mib, not both".to_owned());
    }
    if scenario.vcpus == 0 || scenario.disk_gib == 0 || scenario.timeout_minutes == 0 {
        return Err("vcpus, disk_gib and timeout_minutes must be positive".to_owned());
    }
    if scenario.commands.is_empty() {
        return Err("a scenario needs at least one command".to_owned());
    }
    if let Some(package) = scenario.packages.iter().find(|package| {
        package.is_empty()
            || !package
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c))
    }) {
        return Err(format!("not a package name: {package:?}"));
    }
    Ok(scenario)
}

struct Paths {
    disk: PathBuf,
    seed: PathBuf,
}

/// Everything `up` makes one VM from.
struct Plan<'a> {
    name: &'a str,
    hostname: String,
    scenario_path: &'a Path,
    scenario: Scenario,
    files: &'a [PathBuf],
    image: PathBuf,
    paths: Paths,
    serial: PathBuf,
    user_data: String,
    script: String,
    xml: String,
}

fn up(
    scenario_path: &Path,
    name: &str,
    files: &[PathBuf],
    keep: bool,
    apply: bool,
    repo: &Path,
) -> Result<(), String> {
    let scenario = load_scenario(scenario_path)?;
    let site = site()?;
    for file in files {
        if !file.is_file() && !file.is_dir() {
            return Err(format!("--file {} is not a file or folder", file.display()));
        }
    }
    let paths = Paths {
        disk: site.images.join(format!("{name}.qcow2")),
        seed: site.images.join(format!("{name}-seed.iso")),
    };
    let serial = serial_log(name);
    let hostname = name.replace('_', "-");
    let plan = Plan {
        name,
        image: image_path(&scenario.image, &site.images)?,
        user_data: user_data(&hostname, &scenario.packages, site.ssh_key.as_deref()),
        script: first_boot_script(&scenario.commands),
        xml: domain_xml(name, &scenario, &paths, &serial, &site.bridge),
        hostname,
        scenario_path,
        scenario,
        files,
        paths,
        serial,
    };
    describe(&plan);
    if !apply {
        println!(
            "\n{}\n{}\ndry run: pass --apply to make and start it",
            plan.xml, plan.user_data
        );
        return Ok(());
    }
    require_root()?;
    if virsh(&["dominfo", name], repo).is_ok() {
        return Err(format!("a domain named {name} already exists"));
    }
    if plan.paths.disk.exists() || plan.paths.seed.exists() {
        return Err(format!(
            "{} or {} already exists",
            plan.paths.disk.display(),
            plan.paths.seed.display()
        ));
    }
    let available = mem_available_mib()?;
    if available < plan.scenario.boot_mib + HOST_MARGIN_MIB {
        return Err(format!(
            "the host has {available} MiB available; {name} needs {} MiB and {HOST_MARGIN_MIB} MiB to spare",
            plan.scenario.boot_mib
        ));
    }
    if !plan.image.exists() {
        download(&plan.scenario.image, &plan.image, repo)?;
    }
    let image_sha256 = sha256(&plan.image, repo)?;

    let work = site.images.join(format!(".{name}-seed"));
    if work.exists() {
        std::fs::remove_dir_all(&work)
            .map_err(|error| format!("clear {}: {error}", work.display()))?;
    }
    let made = make_vm(&plan, &work, repo);
    if let Err(error) = std::fs::remove_dir_all(&work) {
        eprintln!("warning: could not remove {}: {error}", work.display());
    }
    if let Err(error) = made {
        // Leave nothing half-made behind, including a domain defined but
        // not started.
        if virsh(&["dominfo", name], repo).is_ok() {
            if let Err(cleanup) = virsh(&["undefine", name], repo) {
                eprintln!("warning: {cleanup}");
            }
        }
        if let Err(cleanup) = remove_files(&plan.paths, &plan.serial) {
            eprintln!("warning: {cleanup}");
        }
        return Err(error);
    }
    println!(
        "started {name}; waiting up to {} minutes",
        plan.scenario.timeout_minutes
    );

    let outcome = wait(&plan, repo);
    let evidence = record(&plan, &image_sha256, &outcome, repo)?;
    match &outcome {
        Ok(statuses) => {
            for (number, status) in statuses.iter().enumerate() {
                println!("  step {} exited {status}", number + 1);
            }
        }
        Err(error) => eprintln!("{name}: {error}"),
    }
    println!("evidence: {}", evidence.display());
    if keep {
        match address(name, repo) {
            Some(address) => println!("{name} kept, at {address} (ssh {USER}@{address})"),
            None => println!("{name} kept; its address isn't known yet (no guest agent answer)"),
        }
    } else {
        down(name, true, repo)?;
    }
    outcome.map(|_| ())
}

fn serial_log(name: &str) -> PathBuf {
    PathBuf::from(format!("/var/log/libvirt/qemu/{name}-serial.log"))
}

fn describe(plan: &Plan<'_>) {
    let scenario = &plan.scenario;
    println!("test VM {} from {}", plan.name, plan.scenario_path.display());
    println!("  image:   {} ({})", scenario.image, plan.image.display());
    println!(
        "  disk:    {} ({} GiB overlay)",
        plan.paths.disk.display(),
        scenario.disk_gib
    );
    println!("  seed:    {}", plan.paths.seed.display());
    println!(
        "  memory:  {} MiB boot, virtio-mem device {} MiB, room for one {} MiB",
        scenario.boot_mib, scenario.device_mib, scenario.hotplug_mib
    );
    for (number, command) in scenario.commands.iter().enumerate() {
        println!("  step {}: {command}", number + 1);
    }
}

fn make_vm(plan: &Plan<'_>, work: &Path, repo: &Path) -> Result<(), String> {
    let write = |file: &str, text: &str| {
        std::fs::write(work.join(file), text)
            .map_err(|error| format!("write {}: {error}", work.join(file).display()))
    };
    std::fs::create_dir_all(work).map_err(|error| format!("create {}: {error}", work.display()))?;
    write("user-data", &plan.user_data)?;
    write(
        "meta-data",
        &format!(
            "instance-id: {}-{}\nlocal-hostname: {}\n",
            plan.name,
            process::unix_millis()?,
            plan.hostname
        ),
    )?;
    write("testvm-run.sh", &plan.script)?;
    for file in plan.files {
        let target = work.join(file.file_name().ok_or("a --file has no file name")?);
        copy_tree(file, &target)?;
    }
    let mut xorriso = [
        "-as", "mkisofs", "-quiet", "-volid", "cidata", "-joliet", "-rock", "-output",
    ]
    .into_iter()
    .map(process::os)
    .collect::<Vec<_>>();
    xorriso.push(process::os(&plan.paths.seed));
    xorriso.push(process::os(work));
    process::checked_output("xorriso", &xorriso, repo)?;
    let format = image_format(&plan.image, repo)?;
    process::checked_output(
        "qemu-img",
        &[
            process::os("create"),
            process::os("-q"),
            process::os("-f"),
            process::os("qcow2"),
            process::os("-F"),
            process::os(format),
            process::os("-b"),
            process::os(&plan.image),
            process::os(&plan.paths.disk),
            process::os(format!("{}G", plan.scenario.disk_gib)),
        ],
        repo,
    )?;
    // The definition goes beside the seed's folder, not onto the seed.
    let xml_path = work.with_extension("xml");
    std::fs::write(&xml_path, &plan.xml)
        .map_err(|error| format!("write {}: {error}", xml_path.display()))?;
    let defined = virsh_os(
        &[
            process::os("define"),
            process::os("--validate"),
            process::os(&xml_path),
        ],
        repo,
    );
    if let Err(error) = std::fs::remove_file(&xml_path) {
        eprintln!("warning: could not remove {}: {error}", xml_path.display());
    }
    defined?;
    virsh(&["start", plan.name], repo).map(|_| ())
}

/// Copy a file, or a folder and everything in it, onto the seed.
fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    if source.is_dir() {
        std::fs::create_dir_all(target)
            .map_err(|error| format!("create {}: {error}", target.display()))?;
        let entries = std::fs::read_dir(source)
            .map_err(|error| format!("read {}: {error}", source.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read {}: {error}", source.display()))?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(source, target)
            .map(|_| ())
            .map_err(|error| format!("copy {}: {error}", source.display()))
    }
}

fn image_path(image: &str, images: &Path) -> Result<PathBuf, String> {
    if let Some(rest) = image.strip_prefix("https://") {
        let file = rest
            .rsplit('/')
            .next()
            .filter(|file| {
                !file.is_empty() && !file.starts_with('.') && !file.contains(['?', '#'])
            })
            .ok_or_else(|| format!("no file name in {image}"))?;
        Ok(images.join("testvm-cache").join(file))
    } else if image.contains("://") {
        Err(format!("images are downloaded only over https: {image}"))
    } else {
        let path = PathBuf::from(image);
        if path.is_file() {
            Ok(path)
        } else {
            Err(format!("image {image} is not a file"))
        }
    }
}

fn download(url: &str, path: &Path, repo: &Path) -> Result<(), String> {
    let folder = path.parent().ok_or("image cache has no folder")?;
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("create {}: {error}", folder.display()))?;
    let partial = path.with_extension("partial");
    println!("downloading {url}");
    process::checked_output(
        "curl",
        &[
            process::os("-fsSL"),
            process::os("--proto"),
            process::os("=https"),
            process::os("-o"),
            process::os(&partial),
            process::os(url),
        ],
        repo,
    )?;
    std::fs::rename(&partial, path)
        .map_err(|error| format!("rename {}: {error}", partial.display()))
}

fn sha256(path: &Path, repo: &Path) -> Result<String, String> {
    let text = process::checked_text("sha256sum", &[process::os(path)], repo)?;
    text.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| format!("sha256sum printed nothing for {}", path.display()))
}

fn image_format(image: &Path, repo: &Path) -> Result<String, String> {
    let info = process::checked_text(
        "qemu-img",
        &[
            process::os("info"),
            process::os("--output=json"),
            process::os(image),
        ],
        repo,
    )?;
    let value: serde_json::Value =
        serde_json::from_str(&info).map_err(|error| format!("qemu-img info: {error}"))?;
    match value.get("format").and_then(serde_json::Value::as_str) {
        Some(format @ ("qcow2" | "raw")) => Ok(format.to_owned()),
        other => Err(format!(
            "{}: unsupported image format {other:?}",
            image.display()
        )),
    }
}

fn mem_available_mib() -> Result<u64, String> {
    let meminfo = process::read_file(Path::new("/proc/meminfo"))?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kib| kib / 1024)
        .ok_or_else(|| "no MemAvailable in /proc/meminfo".to_owned())
}

/// cloud-init's user data: a login for tests, the packages, then the
/// scenario's commands from the seed.
fn user_data(hostname: &str, packages: &[String], ssh_key: Option<&str>) -> String {
    let mut text = format!(
        "#cloud-config\nhostname: {hostname}\nusers:\n  - name: {USER}\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    shell: /bin/bash\n"
    );
    if let Some(key) = ssh_key {
        // JSON strings are valid YAML scalars, so this quotes safely.
        text.push_str(&format!(
            "    ssh_authorized_keys: [{}]\n",
            json_string(key)
        ));
    }
    let list = std::iter::once("qemu-guest-agent")
        .chain(packages.iter().map(String::as_str))
        .map(json_string)
        .collect::<Vec<_>>()
        .join(", ");
    text.push_str(&format!("package_update: true\npackages: [{list}]\n"));
    text.push_str(
        "runcmd:\n  - [systemctl, enable, --now, qemu-guest-agent]\n  - [sh, -c, \"mkdir -p /mnt/seed && mount -o ro /dev/disk/by-label/cidata /mnt/seed && sh /mnt/seed/testvm-run.sh\"]\n",
    );
    text
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// Run each command in turn, reporting on the serial port, and stop at the
/// first that fails.
fn first_boot_script(commands: &[String]) -> String {
    let mut script = String::from(
        "#!/bin/sh\n# Written by cargo xtask testvm.\nexec >/dev/ttyS0 2>&1\nexport SEED=/mnt/seed DEBIAN_FRONTEND=noninteractive\ncd /root\n",
    );
    for (index, command) in commands.iter().enumerate() {
        let number = index + 1;
        let quoted = command.replace('\'', r"'\''");
        script.push_str(&format!(
            "printf '\\n{MARK} STEP {number}\\n'\nsh -ec '{quoted}'\nrc=$?\nprintf '\\n{MARK} RC {number} %s\\n' \"$rc\"\n[ \"$rc\" -eq 0 ] || {{ printf '\\n{MARK} DONE failed\\n'; exit 1; }}\n"
        ));
    }
    script.push_str(&format!("printf '\\n{MARK} DONE ok\\n'\n"));
    script
}

fn domain_xml(
    name: &str,
    scenario: &Scenario,
    paths: &Paths,
    serial: &Path,
    bridge: &str,
) -> String {
    let room = scenario.device_mib.max(scenario.hotplug_mib);
    let boot = scenario.boot_mib;
    let last_cpu = scenario.vcpus - 1;
    let (max_memory, numa) = if room > 0 {
        (
            format!(
                "  <maxMemory slots='1' unit='MiB'>{}</maxMemory>\n",
                boot + room
            ),
            format!(
                "    <numa>\n      <cell id='0' cpus='0-{last_cpu}' memory='{boot}' unit='MiB'/>\n    </numa>\n"
            ),
        )
    } else {
        (String::new(), String::new())
    };
    let cpu = if scenario.nested || room > 0 {
        let mode = if scenario.nested {
            "host-passthrough"
        } else {
            "host-model"
        };
        format!("  <cpu mode='{mode}'>\n{numa}  </cpu>\n")
    } else {
        String::new()
    };
    let device = if scenario.device_mib > 0 {
        format!(
            "    <memory model='virtio-mem'>\n      <target>\n        <size unit='MiB'>{}</size>\n        <node>0</node>\n        <block unit='MiB'>2</block>\n        <requested unit='MiB'>0</requested>\n      </target>\n    </memory>\n",
            scenario.device_mib
        )
    } else {
        String::new()
    };
    let memory = boot + scenario.device_mib;
    format!(
        "<domain type='kvm'>
  <name>{name}</name>
  <metadata>
    <testvm:vm xmlns:testvm='{NAMESPACE}'>
      <testvm:disk>{disk}</testvm:disk>
      <testvm:seed>{seed}</testvm:seed>
    </testvm:vm>
  </metadata>
{max_memory}  <memory unit='MiB'>{memory}</memory>
  <currentMemory unit='MiB'>{boot}</currentMemory>
  <vcpu>{vcpus}</vcpu>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
  </os>
  <features>
    <acpi/>
    <apic/>
  </features>
{cpu}  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='{seed}'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <interface type='bridge'>
      <source bridge='{bridge}'/>
      <model type='virtio'/>
    </interface>
    <serial type='file'>
      <source path='{serial}'/>
      <target port='0'/>
    </serial>
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
{device}    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
  </devices>
</domain>
",
        disk = xml_text(&paths.disk.display().to_string()),
        seed = xml_text(&paths.seed.display().to_string()),
        serial = xml_text(&serial.display().to_string()),
        bridge = xml_text(bridge),
        vcpus = scenario.vcpus,
    )
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

/// Wait for the first-boot script's final report; the exit status of each
/// step.
fn wait(plan: &Plan<'_>, repo: &Path) -> Result<Vec<i32>, String> {
    let steps = plan.scenario.commands.len();
    let minutes = plan.scenario.timeout_minutes;
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    loop {
        let report = parse_report(&read_serial(&plan.serial)?);
        match report.done {
            Some(true) if report.statuses.len() == steps => return Ok(report.statuses),
            Some(_) => {
                return Err(format!(
                    "step {} failed (exit statuses {:?}); see the serial log",
                    report.statuses.len(),
                    report.statuses
                ))
            }
            None => {}
        }
        let state = virsh(&["domstate", plan.name], repo)?;
        if state.trim() != "running" {
            return Err(format!(
                "{} is {} before its steps finished",
                plan.name,
                state.trim()
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {minutes} minutes with {} of {steps} steps done",
                report.statuses.len()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// The serial log so far; empty until QEMU has written to it.
fn read_serial(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Report {
    statuses: Vec<i32>,
    done: Option<bool>,
}

fn parse_report(log: &str) -> Report {
    let mut report = Report::default();
    for line in log.lines() {
        let Some(rest) = line.trim().strip_prefix(MARK) else {
            continue;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        match words.as_slice() {
            ["RC", number, status] => {
                if let (Ok(number), Ok(status)) = (number.parse::<usize>(), status.parse::<i32>()) {
                    if number == report.statuses.len() + 1 {
                        report.statuses.push(status);
                    }
                }
            }
            ["DONE", "ok"] => report.done = Some(true),
            ["DONE", _] => report.done = Some(false),
            _ => {}
        }
    }
    report
}

fn record(
    plan: &Plan<'_>,
    image_sha256: &str,
    outcome: &Result<Vec<i32>, String>,
    repo: &Path,
) -> Result<PathBuf, String> {
    let folder = repo.join(".artifacts/testvm");
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("create {}: {error}", folder.display()))?;
    let stem = format!("{}-{}", plan.name, process::unix_millis()?);
    let log = read_serial(&plan.serial)?;
    let log_copy = folder.join(format!("{stem}.serial.log"));
    std::fs::write(&log_copy, &log)
        .map_err(|error| format!("write {}: {error}", log_copy.display()))?;
    let (result, error) = match outcome {
        Ok(_) => ("passed", None),
        Err(error) => ("failed", Some(error.clone())),
    };
    let evidence = serde_json::json!({
        "schema": "rampire.testvm",
        "version": 1,
        "vm": plan.name,
        "scenario": plan.scenario_path.display().to_string(),
        "image": plan.scenario.image,
        "image_path": plan.image.display().to_string(),
        "image_sha256": image_sha256,
        "files": plan.files.iter().map(|file| file.display().to_string()).collect::<Vec<_>>(),
        "commands": plan.scenario.commands,
        "exit_statuses": parse_report(&log).statuses,
        "result": result,
        "error": error,
        "serial_log": log_copy.display().to_string(),
        "finished_unix_ms": process::unix_millis()?,
    });
    let path = folder.join(format!("{stem}.json"));
    let text = serde_json::to_string_pretty(&evidence).map_err(|error| error.to_string())?;
    std::fs::write(&path, text + "\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// The VM's first IPv4 address other than loopback, from its guest agent,
/// which may take a minute to answer.
fn address(name: &str, repo: &Path) -> Option<String> {
    for _ in 0..12 {
        if let Ok(text) = virsh(&["domifaddr", name, "--source", "agent"], repo) {
            let found = text.split_whitespace().find_map(|word| {
                let ip: std::net::Ipv4Addr = word.split_once('/')?.0.parse().ok()?;
                (!ip.is_loopback()).then(|| ip.to_string())
            });
            if found.is_some() {
                return found;
            }
        }
        std::thread::sleep(POLL);
    }
    None
}

/// The disk and seed a VM made by `up` names in its metadata; `None` for a
/// VM made any other way.
fn made_here(name: &str, repo: &Path) -> Result<Option<Paths>, String> {
    let xml = virsh(&["dumpxml", "--inactive", name], repo)?;
    if !xml.contains(NAMESPACE) {
        return Ok(None);
    }
    let element = |tag: &str| {
        let open = format!("<testvm:{tag}>");
        let start = xml.find(&open)? + open.len();
        let end = xml[start..].find('<')? + start;
        Some(PathBuf::from(
            xml[start..end]
                .replace("&apos;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        ))
    };
    match (element("disk"), element("seed")) {
        (Some(disk), Some(seed)) => Ok(Some(Paths { disk, seed })),
        _ => Err(format!(
            "{name} has testvm metadata without its disk and seed"
        )),
    }
}

fn down(name: &str, apply: bool, repo: &Path) -> Result<(), String> {
    let paths = made_here(name, repo)?
        .ok_or_else(|| format!("{name} wasn't made by testvm up; leaving it alone"))?;
    let serial = serial_log(name);
    let managed = Path::new("/etc/rampire/vms.d")
        .join(format!("{name}.toml"))
        .exists();
    println!(
        "remove test VM {name}: {}the domain, {}, {}",
        if managed { "RAMpire's management, " } else { "" },
        paths.disk.display(),
        paths.seed.display()
    );
    if !apply {
        println!("dry run: pass --apply to remove it");
        return Ok(());
    }
    if managed {
        process::checked_output(
            "rampire",
            &[
                process::os("vm"),
                process::os("remove"),
                process::os(name),
                process::os("--yes"),
            ],
            repo,
        )?;
    }
    if virsh(&["domstate", name], repo)?.trim() != "shut off" {
        virsh(&["destroy", name], repo)?;
    }
    virsh(&["undefine", name], repo)?;
    remove_files(&paths, &serial)?;
    println!("removed {name}");
    Ok(())
}

fn remove_files(paths: &Paths, serial: &Path) -> Result<(), String> {
    for path in [paths.disk.as_path(), paths.seed.as_path(), serial] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", path.display())),
        }
    }
    Ok(())
}

fn virsh(args: &[&str], repo: &Path) -> Result<String, String> {
    virsh_os(&args.iter().map(process::os).collect::<Vec<_>>(), repo)
}

fn virsh_os(args: &[OsString], repo: &Path) -> Result<String, String> {
    let mut all = vec![process::os("-c"), process::os("qemu:///system")];
    all.extend(args.iter().cloned());
    process::checked_text("virsh", &all, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn scenario(device_mib: u64, hotplug_mib: u64) -> Scenario {
        Scenario {
            image: "https://example.invalid/image.qcow2".to_owned(),
            boot_mib: 1024,
            device_mib,
            hotplug_mib,
            vcpus: 2,
            disk_gib: 10,
            nested: false,
            packages: Vec::new(),
            commands: vec!["true".to_owned()],
            timeout_minutes: 5,
        }
    }

    fn paths() -> Paths {
        Paths {
            disk: PathBuf::from("/images/t.qcow2"),
            seed: PathBuf::from("/images/t-seed.iso"),
        }
    }

    #[test]
    fn parses_up_down_and_list() {
        assert_eq!(
            parse(&strings(&[
                "up", "s.toml", "--name", "t1", "--file", "a.rpm", "--keep", "--apply"
            ])),
            Ok(Command::Up {
                scenario: PathBuf::from("s.toml"),
                name: "t1".to_owned(),
                files: vec![PathBuf::from("a.rpm")],
                keep: true,
                apply: true,
            })
        );
        assert_eq!(
            parse(&strings(&["down", "t1"])),
            Ok(Command::Down {
                name: "t1".to_owned(),
                apply: false
            })
        );
        assert_eq!(parse(&strings(&["list"])), Ok(Command::List));
        assert!(parse(&strings(&["up", "s.toml"])).is_err());
        assert!(parse(&strings(&["up", "s.toml", "--name", "Bad"])).is_err());
        assert!(parse(&strings(&["down", "t1", "--force"])).is_err());
    }

    #[test]
    fn the_report_reads_steps_in_order_and_the_final_line() {
        let log = "boot noise\nRAMPIRE-TESTVM STEP 1\nRAMPIRE-TESTVM RC 1 0\n[  3.1] kernel\nRAMPIRE-TESTVM RC 3 0\nRAMPIRE-TESTVM RC 2 1\nRAMPIRE-TESTVM DONE failed\n";
        assert_eq!(
            parse_report(log),
            Report {
                statuses: vec![0, 1],
                done: Some(false)
            }
        );
        assert_eq!(
            parse_report("RAMPIRE-TESTVM RC 1 0\nRAMPIRE-TESTVM DONE ok"),
            Report {
                statuses: vec![0],
                done: Some(true)
            }
        );
        assert_eq!(parse_report("RAMPIRE-TESTVM STEP 1"), Report::default());
    }

    #[test]
    fn the_script_quotes_each_command_and_stops_at_a_failure() {
        let script = first_boot_script(&strings(&["echo 'hi'", "false"]));
        assert!(script.contains(r"sh -ec 'echo '\''hi'\'''"));
        assert!(script.contains("RC 2 %s"));
        assert!(script.ends_with("printf '\\nRAMPIRE-TESTVM DONE ok\\n'\n"));
    }

    #[test]
    fn a_device_or_room_for_one_brings_numa_and_max_memory() {
        let serial = Path::new("/var/log/t.log");
        let plain = domain_xml("t", &scenario(0, 0), &paths(), serial, "br0");
        assert!(
            !plain.contains("maxMemory") && !plain.contains("numa") && !plain.contains("virtio-mem")
        );
        let room = domain_xml("t", &scenario(0, 2048), &paths(), serial, "br0");
        assert!(room.contains("<maxMemory slots='1' unit='MiB'>3072</maxMemory>"));
        assert!(room.contains("<cell id='0' cpus='0-1' memory='1024' unit='MiB'/>"));
        assert!(!room.contains("virtio-mem"));
        let device = domain_xml("t", &scenario(2048, 0), &paths(), serial, "br0");
        assert!(device.contains("<memory unit='MiB'>3072</memory>"));
        assert!(device.contains("<memory model='virtio-mem'>"));
        assert!(device.contains("<testvm:disk>/images/t.qcow2</testvm:disk>"));
    }

    #[test]
    fn user_data_quotes_the_key_and_packages() {
        let text = user_data(
            "t-1",
            &strings(&["libvirt-daemon-kvm"]),
            Some("ssh-ed25519 AAAA x@y"),
        );
        assert!(text.starts_with("#cloud-config\nhostname: t-1\n"));
        assert!(text.contains("ssh_authorized_keys: [\"ssh-ed25519 AAAA x@y\"]"));
        assert!(text.contains("packages: [\"qemu-guest-agent\", \"libvirt-daemon-kvm\"]"));
    }

    #[test]
    fn images_are_cached_by_file_name_and_only_over_https() {
        let images = Path::new("/images");
        assert_eq!(
            image_path("https://host/a/b/Image-1.qcow2", images),
            Ok(PathBuf::from("/images/testvm-cache/Image-1.qcow2"))
        );
        assert!(image_path("http://host/a.qcow2", images).is_err());
        assert!(image_path("https://host/dir/", images).is_err());
    }

    #[test]
    fn the_tracked_scenarios_load() {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../testvm");
        let mut loaded = 0;
        for entry in std::fs::read_dir(&folder).expect("tools/testvm") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                load_scenario(&path).unwrap_or_else(|error| panic!("{error}"));
                loaded += 1;
            }
        }
        assert!(loaded > 0);
    }
}
