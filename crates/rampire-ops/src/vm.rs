//! `rampire vm add` and `rampire vm remove` (roadmap O3.1 to O3.4, O3.7):
//! read a domain's definition, say what it lacks, fix what can be fixed by
//! hand-checked text edits, and suggest its settings and review.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use rampire_core::MEMORY_CONTROL_CHANNEL_NAME;
use rampire_host::attestation::AnswerSource;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// A virtio-mem device in a domain's definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub alias: Option<String>,
    pub size_bytes: u64,
    pub block_bytes: u64,
    pub requested_bytes: u64,
}

/// What `vm add` reads from a domain's definition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    /// The memory of the NUMA cells: the fixed boot RAM.
    pub boot_bytes: u64,
    pub numa_cells: usize,
    /// `maxMemory`'s slots, when set.
    pub max_memory_slots: Option<u32>,
    /// Other memory devices (DIMMs and the like) that use slots too.
    pub other_memory_devices: usize,
    pub devices: Vec<Device>,
    pub control_channel: bool,
    pub hostdevs: usize,
    pub vdpa: bool,
    pub vhost_user: bool,
    pub launch_security: bool,
    pub locked_memory: bool,
    /// The memory balloon's model; `None` with no balloon element.
    pub balloon: Option<String>,
}

impl Inspection {
    pub fn device(&self) -> Option<&Device> {
        self.devices.first()
    }

    /// Problems that stop RAMpire managing the domain, each with its fix.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut problems = Vec::new();
        if self.numa_cells == 0 {
            problems.push((
                "the domain has no NUMA cell".to_owned(),
                "a virtio-mem device needs one: add <numa><cell id='0' cpus='0-N' memory='BOOT' unit='KiB'/></numa> inside <cpu>".to_owned(),
            ));
        }
        if self.max_memory_slots.is_none() {
            problems.push((
                "the domain has no <maxMemory>".to_owned(),
                "add <maxMemory slots='1' unit='KiB'>TOTAL</maxMemory>, TOTAL being boot RAM plus the device".to_owned(),
            ));
        }
        match self.devices.len() {
            0 => problems.push((
                "the domain has no virtio-mem device".to_owned(),
                "rampire vm add --fix-xml adds one".to_owned(),
            )),
            1 => {}
            _ => problems.push((
                "the domain has more than one virtio-mem device".to_owned(),
                "RAMpire manages one device per VM: remove the others".to_owned(),
            )),
        }
        if let Some(device) = self.device() {
            if device
                .alias
                .as_deref()
                .is_none_or(|alias| !alias.starts_with("ua-"))
            {
                problems.push((
                    "the virtio-mem device has no user alias".to_owned(),
                    "give it <alias name='ua-virtiomem0'/>, so its name is stable".to_owned(),
                ));
            }
        }
        problems
    }

    /// Changes `--fix-xml` would make, in words.
    pub fn fixable(&self) -> Vec<String> {
        let mut changes = Vec::new();
        if !self.control_channel {
            changes.push(format!(
                "add the control channel {MEMORY_CONTROL_CHANNEL_NAME}"
            ));
        }
        match self.device() {
            Some(device) if device.requested_bytes != 0 => changes.push(
                "save the virtio-mem device's requested size as 0, so the VM starts on boot RAM alone".to_owned(),
            ),
            None if self.can_add_device() => {
                changes.push(format!(
                    "add a {} MiB virtio-mem device",
                    default_device_bytes(self.boot_bytes) / MIB
                ));
            }
            _ => {}
        }
        changes
    }

    fn can_add_device(&self) -> bool {
        self.numa_cells > 0
            && self
                .max_memory_slots
                .is_some_and(|slots| slots as usize > self.other_memory_devices)
    }
}

/// A new device's size: three times boot RAM, so a Linux guest grows to
/// about four times its boot RAM and can still return it reliably.
pub fn default_device_bytes(boot_bytes: u64) -> u64 {
    (boot_bytes * 3).div_ceil(GIB) * GIB
}

/// Read a domain's definition.
pub fn inspect(xml: &str) -> Result<Inspection, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut inspection = Inspection::default();
    let mut device: Option<(Device, Option<&'static str>, String)> = None;
    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("domain XML: {error}"))?;
        match event {
            Event::Start(element) => {
                look(&element, &path, &mut inspection, &mut device)?;
                path.push(element.name().as_ref().to_vec());
            }
            Event::Empty(element) => look(&element, &path, &mut inspection, &mut device)?,
            Event::Text(text) => {
                if let Some((current, Some(field), unit)) = device.as_mut() {
                    let raw = text.decode().map_err(|error| error.to_string())?;
                    let bytes = libvirt_bytes(raw.trim(), unit)?;
                    match *field {
                        "size" => current.size_bytes = bytes,
                        "block" => current.block_bytes = bytes,
                        _ => current.requested_bytes = bytes,
                    }
                    device.as_mut().unwrap().1 = None;
                }
            }
            Event::End(element) => {
                path.pop();
                if element.name().as_ref() == b"memory"
                    && path.last().map(Vec::as_slice) == Some(b"devices")
                {
                    if let Some((finished, _, _)) = device.take() {
                        inspection.devices.push(finished);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(inspection)
}

fn look(
    element: &BytesStart<'_>,
    path: &[Vec<u8>],
    inspection: &mut Inspection,
    device: &mut Option<(Device, Option<&'static str>, String)>,
) -> Result<(), String> {
    let name = element.name().as_ref().to_vec();
    let parent = path.last().map(Vec::as_slice);
    match (name.as_slice(), parent) {
        (b"maxMemory", Some(b"domain")) => {
            inspection.max_memory_slots =
                attribute(element, b"slots")?.and_then(|slots| slots.parse().ok());
        }
        (b"cell", Some(b"numa")) => {
            inspection.numa_cells += 1;
            if let Some(memory) = attribute(element, b"memory")? {
                let unit = attribute(element, b"unit")?.unwrap_or_else(|| "KiB".to_owned());
                inspection.boot_bytes += libvirt_bytes(&memory, &unit)?;
            }
        }
        (b"memory", Some(b"devices")) => {
            if attribute(element, b"model")?.as_deref() == Some("virtio-mem") {
                *device = Some((
                    Device {
                        alias: None,
                        size_bytes: 0,
                        block_bytes: 0,
                        requested_bytes: 0,
                    },
                    None,
                    String::new(),
                ));
            } else {
                inspection.other_memory_devices += 1;
            }
        }
        (b"size" | b"block" | b"requested", Some(b"target")) => {
            if let Some((_, field, unit)) = device.as_mut() {
                *field = Some(match name.as_slice() {
                    b"size" => "size",
                    b"block" => "block",
                    _ => "requested",
                });
                *unit = attribute(element, b"unit")?.unwrap_or_else(|| "KiB".to_owned());
            }
        }
        (b"alias", Some(b"memory")) => {
            if let Some((current, _, _)) = device.as_mut() {
                current.alias = attribute(element, b"name")?;
            }
        }
        (b"target", Some(b"channel")) => {
            if attribute(element, b"name")?.as_deref() == Some(MEMORY_CONTROL_CHANNEL_NAME) {
                inspection.control_channel = true;
            }
        }
        (b"hostdev", Some(b"devices")) => inspection.hostdevs += 1,
        (b"interface", Some(b"devices")) => match attribute(element, b"type")?.as_deref() {
            Some("vdpa") => inspection.vdpa = true,
            Some("vhostuser") => inspection.vhost_user = true,
            _ => {}
        },
        (b"filesystem" | b"disk", Some(b"devices")) => {
            if attribute(element, b"type")?.as_deref() == Some("vhostuser") {
                inspection.vhost_user = true;
            }
        }
        (b"launchSecurity", Some(b"domain")) => inspection.launch_security = true,
        (b"locked", Some(b"memoryBacking")) => inspection.locked_memory = true,
        (b"memballoon", Some(b"devices")) => {
            inspection.balloon = Some(attribute(element, b"model")?.unwrap_or_default());
        }
        _ => {}
    }
    Ok(())
}

fn attribute(element: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>, String> {
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute.map_err(|error| error.to_string())?;
        if attribute.key.as_ref() == key {
            return Ok(Some(
                attribute
                    .normalized_value(XmlVersion::Explicit1_0)
                    .map_err(|error| error.to_string())?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn libvirt_bytes(value: &str, unit: &str) -> Result<u64, String> {
    let value: u64 = value
        .parse()
        .map_err(|_| format!("domain XML: {value:?} is not a size"))?;
    let multiplier: u64 = match unit {
        "b" | "bytes" => 1,
        "KB" => 1_000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1_000_000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1_000_000_000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1_000_000_000_000,
        "T" | "TiB" => 1 << 40,
        other => return Err(format!("domain XML: unknown unit {other:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| "domain XML: size overflows".to_owned())
}

/// The definition with every fix in [`Inspection::fixable`] made, or `None`
/// when there is nothing to fix. Edits are plain text, so everything else
/// in the definition stays exactly as it was.
pub fn fix_xml(xml: &str, inspection: &Inspection) -> Result<Option<String>, String> {
    let mut text = xml.to_owned();
    match inspection.device() {
        Some(device) if device.requested_bytes != 0 => {
            let start = text
                .find("<memory model='virtio-mem'")
                .ok_or("can't find the virtio-mem device's text")?;
            let end = start
                + text[start..]
                    .find("</memory>")
                    .ok_or("the virtio-mem device has no end")?;
            let block = &text[start..end];
            let open = block
                .find("<requested")
                .ok_or("no <requested> in the device")?;
            let close = block[open..]
                .find("</requested>")
                .ok_or("no </requested> in the device")?
                + open
                + "</requested>".len();
            let fixed = format!(
                "{}<requested unit='KiB'>0</requested>{}",
                &block[..open],
                &block[close..]
            );
            text.replace_range(start..end, &fixed);
        }
        None if inspection.can_add_device() => {
            let size = default_device_bytes(inspection.boot_bytes);
            insert_device(
                &mut text,
                &format!(
                    "    <memory model='virtio-mem'>\n      <target>\n        <size unit='KiB'>{}</size>\n        <node>0</node>\n        <block unit='KiB'>2048</block>\n        <requested unit='KiB'>0</requested>\n      </target>\n      <alias name='ua-virtiomem0'/>\n    </memory>\n",
                    size / 1024
                ),
            )?;
        }
        _ => {}
    }
    if !inspection.control_channel {
        insert_device(
            &mut text,
            &format!(
                "    <channel type='unix'>\n      <target type='virtio' name='{MEMORY_CONTROL_CHANNEL_NAME}'/>\n      <alias name='ua-rampire-control'/>\n    </channel>\n"
            ),
        )?;
    }
    Ok((text != xml).then_some(text))
}

fn insert_device(text: &mut String, device: &str) -> Result<(), String> {
    let end = text
        .rfind("</devices>")
        .ok_or("the domain has no </devices>")?;
    let line_start = text[..end].rfind('\n').map_or(end, |index| index + 1);
    text.insert_str(line_start, device);
    Ok(())
}

/// Suggested settings for a domain (O3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggested {
    pub min_ram: u64,
    pub max_ram: u64,
    pub free_ram: u64,
}

/// `min_ram` is the boot RAM; `max_ram` the lower of the device ceiling
/// and, for Linux, about four times boot RAM; `free_ram` 512 MiB.
pub fn suggest(inspection: &Inspection, linux: bool) -> Option<Suggested> {
    let device = inspection.device()?;
    let block = device.block_bytes.max(2 * MIB);
    let boot = inspection.boot_bytes / block * block;
    let ceiling = boot + device.size_bytes;
    let max_ram = if linux {
        ceiling.min(boot * 4)
    } else {
        ceiling
    };
    Some(Suggested {
        min_ram: boot,
        max_ram: max_ram / block * block,
        free_ram: 512 * MIB,
    })
}

/// A review answer `vm add` read from the definition, or `None` when only
/// a person can give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub question: &'static str,
    pub answer: Option<(bool, AnswerSource)>,
}

/// The yes-or-no review answers (O3.2): detected where the definition shows
/// them, left to a person otherwise.
pub fn review_answers(inspection: &Inspection) -> Vec<Detected> {
    let seen = |clear: bool| clear.then_some((true, AnswerSource::Detected));
    vec![
        Detected {
            question: "workload_reviewed",
            answer: None,
        },
        Detected {
            question: "no_vdpa",
            answer: Some((!inspection.vdpa, AnswerSource::Detected)),
        },
        Detected {
            question: "no_rdma_migration",
            answer: None,
        },
        Detected {
            question: "no_vfio_nvme",
            answer: seen(inspection.hostdevs == 0),
        },
        Detected {
            question: "no_mlock",
            answer: Some((!inspection.locked_memory, AnswerSource::Detected)),
        },
        Detected {
            question: "no_secure_virtualization",
            answer: Some((!inspection.launch_security, AnswerSource::Detected)),
        },
        Detected {
            question: "no_unsupported_vhost_user",
            answer: seen(!inspection.vhost_user),
        },
        Detected {
            question: "balloon_resize_inactive",
            answer: seen(
                inspection
                    .balloon
                    .as_deref()
                    .is_none_or(|model| model == "none"),
            ),
        },
    ]
}

/// What each review question asks a person.
pub fn question_text(question: &str) -> &'static str {
    match question {
        "workload_reviewed" => {
            "Do the programs in this VM cope with memory being added and taken away?"
        }
        "no_rdma_migration" => "Is the VM never live-migrated with RDMA?",
        "no_vfio_nvme" => {
            "None of the VM's passed-through devices is an NVMe drive, and none pins its memory?"
        }
        "no_unsupported_vhost_user" => {
            "Do the VM's vhost-user devices all support memory being added and removed?"
        }
        "balloon_resize_inactive" => "Is the VM's memory balloon never used to resize it?",
        _ => "Is this true for the VM?",
    }
}

/// The review profile's text for these answers.
pub fn review_toml(
    vm: &str,
    answers: &[(&str, bool, AnswerSource)],
    slots: u32,
    driver: &str,
) -> String {
    let source = |source: AnswerSource| match source {
        AnswerSource::Detected => "detected",
        AnswerSource::Confirmed => "confirmed",
    };
    let mut text = format!(
        "# The review of {vm}, made by `rampire vm add`. \"detected\" answers\n# were read from the VM's definition; \"confirmed\" ones were given by a\n# person.\nversion = 1\nguest_os = \"linux\"\n"
    );
    for (question, value, from) in answers {
        text.push_str(&format!(
            "{question} = {{ value = {value}, source = \"{}\" }}\n",
            source(*from)
        ));
    }
    text.push_str(&format!(
        "memory_slot_budget = {{ value = {slots}, source = \"detected\" }}\nvfio_mapping_budget = {{ value = 1, source = \"detected\" }}\nguest_driver_version = {{ value = \"{driver}\", source = \"confirmed\" }}\n"
    ));
    text
}

/// The VM's settings file.
pub fn vm_toml(alias: &str, priority: u32, review: &str, suggested: &Suggested) -> String {
    format!(
        "# Added by `rampire vm add`. Change a value with `rampire set VM KEY=VALUE`.\nalias = \"{alias}\"\npriority = {priority}\nreview = \"{review}\"\nguest_os = \"linux\"\n# The smallest the VM is made: its boot RAM.\nmin_ram = \"{}MB\"\n# The largest it may grow.\nmax_ram = \"{}MB\"\n# Spare memory it keeps ready.\nfree_ram = \"{}MB\"\n",
        suggested.min_ram / MIB,
        suggested.max_ram / MIB,
        suggested.free_ram / MIB
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGED: &str = "<domain type='kvm'>
  <name>web</name>
  <maxMemory slots='1' unit='KiB'>9437184</maxMemory>
  <memory unit='KiB'>9437184</memory>
  <cpu mode='host-passthrough'>
    <numa>
      <cell id='0' cpus='0-1' memory='1048576' unit='KiB'/>
    </numa>
  </cpu>
  <devices>
    <interface type='bridge'/>
    <channel type='unix'>
      <target type='virtio' name='org.rampire.control.0'/>
    </channel>
    <memballoon model='none'/>
    <memory model='virtio-mem'>
      <target>
        <size unit='KiB'>8388608</size>
        <node>0</node>
        <block unit='KiB'>2048</block>
        <requested unit='KiB'>2097152</requested>
      </target>
      <alias name='ua-virtiomem0'/>
    </memory>
  </devices>
</domain>
";

    const PLAIN: &str = "<domain type='kvm'>
  <name>desk</name>
  <maxMemory slots='2' unit='KiB'>8388608</maxMemory>
  <memory unit='KiB'>2097152</memory>
  <cpu><numa><cell id='0' cpus='0' memory='2097152' unit='KiB'/></numa></cpu>
  <devices>
    <hostdev mode='subsystem' type='pci'/>
    <memballoon model='virtio'/>
  </devices>
</domain>
";

    #[test]
    fn reads_the_device_the_channel_and_the_boot_ram() {
        let web = inspect(MANAGED).unwrap();
        assert_eq!(web.boot_bytes, GIB);
        assert_eq!(web.max_memory_slots, Some(1));
        let device = web.device().unwrap();
        assert_eq!(device.alias.as_deref(), Some("ua-virtiomem0"));
        assert_eq!(
            (
                device.size_bytes,
                device.block_bytes,
                device.requested_bytes
            ),
            (8 * GIB, 2 * MIB, 2 * GIB)
        );
        assert!(web.control_channel);
        assert!(web.problems().is_empty(), "{:?}", web.problems());
        let desk = inspect(PLAIN).unwrap();
        assert!(desk.device().is_none() && !desk.control_channel);
        assert_eq!(desk.hostdevs, 1);
        assert_eq!(desk.balloon.as_deref(), Some("virtio"));
    }

    #[test]
    fn fixes_a_saved_request_and_adds_what_is_missing() {
        let web = inspect(MANAGED).unwrap();
        let fixed = fix_xml(MANAGED, &web).unwrap().expect("a fix");
        assert!(fixed.contains("<requested unit='KiB'>0</requested>"));
        assert_eq!(fixed.lines().count(), MANAGED.lines().count());
        let refixed = inspect(&fixed).unwrap();
        assert_eq!(
            fix_xml(&fixed, &refixed).unwrap(),
            None,
            "fixing is idempotent"
        );

        let desk = inspect(PLAIN).unwrap();
        assert_eq!(desk.fixable().len(), 2);
        let fixed = inspect(&fix_xml(PLAIN, &desk).unwrap().unwrap()).unwrap();
        assert!(fixed.control_channel);
        let device = fixed.device().expect("a device was added");
        assert_eq!(device.size_bytes, 6 * GIB);
        assert_eq!(device.requested_bytes, 0);
        assert!(fixed.problems().is_empty(), "{:?}", fixed.problems());
    }

    #[test]
    fn a_domain_without_numa_or_slots_is_reported_not_edited() {
        let bare = "<domain><memory unit='KiB'>1048576</memory><devices></devices></domain>";
        let inspection = inspect(bare).unwrap();
        let problems: Vec<_> = inspection
            .problems()
            .into_iter()
            .map(|(problem, _)| problem)
            .collect();
        assert!(problems.iter().any(|p| p.contains("NUMA")));
        assert!(problems.iter().any(|p| p.contains("maxMemory")));
        // Only the channel can be added; no device without NUMA and a slot.
        let fixed = inspect(&fix_xml(bare, &inspection).unwrap().unwrap()).unwrap();
        assert!(fixed.control_channel && fixed.device().is_none());
    }

    #[test]
    fn suggests_settings_and_detects_review_answers() {
        let web = inspect(MANAGED).unwrap();
        let linux = suggest(&web, true).unwrap();
        assert_eq!(
            (linux.min_ram, linux.max_ram, linux.free_ram),
            (GIB, 4 * GIB, 512 * MIB)
        );
        assert_eq!(suggest(&web, false).unwrap().max_ram, 9 * GIB);
        let answers = review_answers(&web);
        let unanswered: Vec<_> = answers
            .iter()
            .filter(|answer| answer.answer.is_none())
            .map(|answer| answer.question)
            .collect();
        assert_eq!(unanswered, ["workload_reviewed", "no_rdma_migration"]);
        let desk = review_answers(&inspect(PLAIN).unwrap());
        let open: Vec<_> = desk
            .iter()
            .filter(|a| a.answer.is_none())
            .map(|a| a.question)
            .collect();
        assert!(open.contains(&"no_vfio_nvme") && open.contains(&"balloon_resize_inactive"));
    }

    #[test]
    fn the_written_files_load() {
        let web = inspect(MANAGED).unwrap();
        let suggested = suggest(&web, true).unwrap();
        let vm: rampire_core::settings::VmSettings =
            toml::from_str(&vm_toml("ua-virtiomem0", 100, "web", &suggested)).unwrap();
        assert_eq!(vm.max_ram, 4 * GIB);
        let answers: Vec<_> = review_answers(&web)
            .into_iter()
            .map(|a| {
                let (value, source) = a.answer.unwrap_or((true, AnswerSource::Confirmed));
                (a.question, value, source)
            })
            .collect();
        let text = review_toml("web", &answers, 1, "linux 6.12");
        let root = std::env::temp_dir().join(format!("rampire-vm-review-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("web.toml");
        std::fs::write(&path, &text).unwrap();
        let review = rampire_host::attestation::read_review(&path);
        std::fs::remove_dir_all(&root).unwrap();
        let review = review.expect("valid profile");
        assert_eq!(review.no_vdpa.source, AnswerSource::Detected);
        assert_eq!(review.workload_reviewed.source, AnswerSource::Confirmed);
    }
}
