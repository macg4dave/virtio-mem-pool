//! `rampire init`: suggest a host's global settings from its memory and its
//! libvirt domains.
//!
//! A domain with a virtio-mem device is one RAMpire will manage: only its
//! boot RAM, the memory of its NUMA cells, is set aside. Every other domain
//! keeps all its memory, so all of it is set aside. The pool is what is left
//! of the host's memory after the host's own reserve.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

const GIB: u64 = 1 << 30;

/// The memory one domain needs, read from its definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainMemory {
    pub name: String,
    /// Has a virtio-mem device, so RAMpire can manage it.
    pub resizable: bool,
    /// The domain's `<memory>`.
    pub memory_bytes: u64,
    /// The memory of its NUMA cells: the fixed boot RAM of a resizable VM.
    pub boot_bytes: u64,
}

impl DomainMemory {
    /// What the pool can never use for this domain.
    pub fn set_aside_bytes(&self) -> u64 {
        if self.resizable && self.boot_bytes > 0 {
            self.boot_bytes
        } else {
            self.memory_bytes
        }
    }
}

/// The suggested global settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub host_bytes: u64,
    pub host_reserve_bytes: u64,
    /// `None` when nothing is left for a pool.
    pub pool_bytes: Option<u64>,
}

/// The host keeps an eighth of its memory, between 2 and 8 GiB, in whole
/// GiB. The pool is the rest after every domain's set-aside memory, rounded
/// down to whole GiB.
pub fn suggest(host_bytes: u64, domains: &[DomainMemory]) -> Suggestion {
    let reserve = (host_bytes / 8).clamp(2 * GIB, 8 * GIB) / GIB * GIB;
    let set_aside: u64 = domains.iter().map(DomainMemory::set_aside_bytes).sum();
    let pool = host_bytes.saturating_sub(reserve).saturating_sub(set_aside) / GIB * GIB;
    Suggestion {
        host_bytes,
        host_reserve_bytes: reserve,
        pool_bytes: (pool > 0).then_some(pool),
    }
}

/// `global.toml` for a suggestion, with notes saying where each value came
/// from.
pub fn global_toml(suggestion: &Suggestion, domains: &[DomainMemory]) -> String {
    let gib = |bytes: u64| bytes / GIB;
    let mut text = String::from(
        "# Written by `rampire init` from this host's memory and its VMs.\n\
         # Change a value with `rampire set global KEY=VALUE`, or edit this\n\
         # file and run `rampire apply`.\n\n",
    );
    text.push_str(&format!(
        "# Memory the host keeps for itself: an eighth of its {} GiB, between\n\
         # 2 and 8 GiB.\nhost_reserve = \"{}GB\"\n\n",
        gib(suggestion.host_bytes),
        gib(suggestion.host_reserve_bytes)
    ));
    text.push_str("# The pool every managed VM grows from: the host's memory, less the\n# reserve, less what each VM below needs whatever happens:\n");
    for domain in domains {
        let what = if domain.resizable && domain.boot_bytes > 0 {
            "boot RAM; it can be managed"
        } else {
            "all of it; it has no virtio-mem device"
        };
        text.push_str(&format!(
            "#   {}: {} MiB ({what})\n",
            domain.name,
            domain.set_aside_bytes() >> 20
        ));
    }
    match suggestion.pool_bytes {
        Some(pool) => text.push_str(&format!("pool = \"{}GB\"\n", gib(pool))),
        None => text.push_str("# Nothing is left for a pool on this host.\n"),
    }
    text
}

/// Read a domain's memory from its XML definition.
pub fn parse_domain(name: &str, xml: &str) -> Result<DomainMemory, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut memory_unit = None;
    let mut memory_bytes = None;
    let mut boot_bytes = 0_u64;
    let mut resizable = false;
    let error = |error: &dyn std::fmt::Display| format!("domain {name}: {error}");
    loop {
        let event = reader.read_event_into(&mut buffer).map_err(|e| error(&e))?;
        match event {
            Event::Start(element) => {
                inspect(&element, &path, &mut resizable, &mut boot_bytes, name)?;
                let element_name = element.name().as_ref().to_vec();
                if path == [b"domain".to_vec()] && element_name == b"memory" {
                    memory_unit = Some(attribute(&element, b"unit", name)?);
                }
                path.push(element_name);
            }
            Event::Empty(element) => {
                inspect(&element, &path, &mut resizable, &mut boot_bytes, name)?;
            }
            Event::Text(text) => {
                if let Some(unit) = memory_unit.take() {
                    let raw = text.decode().map_err(|e| error(&e))?;
                    let value = raw
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| error(&format!("<memory> {raw:?} is not a number")))?;
                    memory_bytes = Some(libvirt_bytes(value, unit.as_deref(), name)?);
                }
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(DomainMemory {
        name: name.to_owned(),
        resizable,
        memory_bytes: memory_bytes.ok_or_else(|| error(&"no <memory>"))?,
        boot_bytes,
    })
}

/// Note a virtio-mem device or a NUMA cell.
fn inspect(
    element: &BytesStart<'_>,
    path: &[Vec<u8>],
    resizable: &mut bool,
    boot_bytes: &mut u64,
    name: &str,
) -> Result<(), String> {
    let element_name = element.name().as_ref().to_vec();
    let parent = path.last().map(Vec::as_slice);
    if element_name == b"memory" && parent == Some(b"devices") {
        *resizable |= attribute(element, b"model", name)?.as_deref() == Some("virtio-mem");
    }
    if element_name == b"cell" && parent == Some(b"numa") {
        if let Some(memory) = attribute(element, b"memory", name)? {
            let value = memory
                .parse::<u64>()
                .map_err(|_| format!("domain {name}: NUMA cell memory {memory:?}"))?;
            let unit = attribute(element, b"unit", name)?;
            *boot_bytes += libvirt_bytes(value, unit.as_deref(), name)?;
        }
    }
    Ok(())
}

fn attribute(element: &BytesStart<'_>, key: &[u8], name: &str) -> Result<Option<String>, String> {
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute.map_err(|error| format!("domain {name}: {error}"))?;
        if attribute.key.as_ref() == key {
            let value = attribute
                .normalized_value(XmlVersion::Explicit1_0)
                .map_err(|error| format!("domain {name}: {error}"))?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

/// libvirt's memory units; without one, KiB.
fn libvirt_bytes(value: u64, unit: Option<&str>, name: &str) -> Result<u64, String> {
    let multiplier: u64 = match unit.unwrap_or("KiB") {
        "b" | "bytes" => 1,
        "KB" => 1_000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1_000_000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1_000_000_000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1_000_000_000_000,
        "T" | "TiB" => 1 << 40,
        other => return Err(format!("domain {name}: unknown memory unit {other:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("domain {name}: memory size overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESIZABLE: &str = "<domain type='kvm'><name>web</name>\
        <maxMemory slots='1' unit='KiB'>17825792</maxMemory>\
        <memory unit='KiB'>9437184</memory>\
        <cpu><numa><cell id='0' cpus='0-1' memory='1048576' unit='KiB'/></numa></cpu>\
        <devices><memory model='virtio-mem'><target><size unit='GiB'>8</size>\
        <node>0</node><block unit='KiB'>2048</block><requested unit='KiB'>0</requested>\
        </target><alias name='ua-virtiomem0'/></memory></devices></domain>";
    const FIXED: &str = "<domain type='kvm'><name>desk</name><memory>4194304</memory>\
        <devices><disk type='file'/></devices></domain>";

    #[test]
    fn a_virtio_mem_domain_sets_aside_its_boot_ram_and_another_all_its_memory() {
        let web = parse_domain("web", RESIZABLE).unwrap();
        assert!(web.resizable);
        assert_eq!(web.memory_bytes, 9 * GIB);
        assert_eq!(web.set_aside_bytes(), GIB);
        let desk = parse_domain("desk", FIXED).unwrap();
        assert!(!desk.resizable);
        assert_eq!(desk.set_aside_bytes(), 4 * GIB);
        assert!(parse_domain("bad", "<domain/>").is_err());
    }

    #[test]
    fn the_pool_is_what_is_left_after_the_reserve_and_every_vm() {
        let domains = [
            parse_domain("web", RESIZABLE).unwrap(),
            parse_domain("desk", FIXED).unwrap(),
        ];
        let suggestion = suggest(64 * GIB, &domains);
        assert_eq!(suggestion.host_reserve_bytes, 8 * GIB);
        assert_eq!(suggestion.pool_bytes, Some(51 * GIB));
        assert_eq!(suggest(16 * GIB, &[]).host_reserve_bytes, 2 * GIB);
        assert_eq!(suggest(4 * GIB, &domains).pool_bytes, None);

        let text = global_toml(&suggestion, &domains);
        let globals: rampire_core::settings::GlobalSettings = toml::from_str(&text).unwrap();
        assert_eq!(globals.pool, Some(51 * GIB));
        assert_eq!(globals.host_reserve, 8 * GIB);
        assert!(
            text.contains("#   web: 1024 MiB (boot RAM; it can be managed)"),
            "{text}"
        );
    }
}
