use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use virtio_mem_core::{
    parse_virtio_mem_xml_for_alias, settings::GuestOs, CompatibilityEvidence,
    VirtioMemCompatibility,
};

use crate::compatibility_source::CompatibilitySource;
use crate::virsh::VirshCommand;

/// Version 2 names the reviewed guest operating system and drops the
/// development-only trust flag of version 1; a version 1 attestation must be
/// made again from an updated review.
pub const COMPATIBILITY_ATTESTATION_VERSION: u32 = 2;
const MAX_ATTESTATION_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityReview {
    /// The guest operating system reviewed. It must match the VM's settings,
    /// because the driver behaviour the host relies on differs between them.
    pub guest_os: GuestOs,
    pub workload_reviewed: bool,
    pub no_vdpa: bool,
    pub no_rdma_migration: bool,
    pub no_vfio_nvme: bool,
    pub no_mlock: bool,
    pub no_secure_virtualization: bool,
    pub no_unsupported_vhost_user: bool,
    pub balloon_resize_inactive: bool,
    pub memory_slot_budget: u32,
    pub vfio_mapping_budget: u32,
    /// The guest's virtio-mem driver as reviewed: the `viomem` driver
    /// version on Windows (that path is PAUSED until the viomem driver bug is
    /// repaired), the kernel release on Linux. Reviews written
    /// before Linux guests name it `windows_driver_version`.
    #[serde(alias = "windows_driver_version")]
    pub guest_driver_version: String,
}

impl CompatibilityReview {
    fn validate(&self) -> Result<(), String> {
        let confirmations = [
            (self.workload_reviewed, "workload_reviewed"),
            (self.no_vdpa, "no_vdpa"),
            (self.no_rdma_migration, "no_rdma_migration"),
            (self.no_vfio_nvme, "no_vfio_nvme"),
            (self.no_mlock, "no_mlock"),
            (self.no_secure_virtualization, "no_secure_virtualization"),
            (self.no_unsupported_vhost_user, "no_unsupported_vhost_user"),
            (self.balloon_resize_inactive, "balloon_resize_inactive"),
        ];
        if let Some((_, name)) = confirmations.iter().find(|(value, _)| !value) {
            return Err(format!("compatibility review does not confirm {name}"));
        }
        if self.memory_slot_budget == 0 {
            return Err("compatibility review memory_slot_budget must be positive".to_owned());
        }
        if self.vfio_mapping_budget == 0 {
            return Err("compatibility review vfio_mapping_budget must be positive".to_owned());
        }
        if self.guest_driver_version.trim().is_empty() {
            return Err("compatibility review guest_driver_version must be non-empty".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveCompatibilityEvidence {
    pub vm_name: String,
    pub device_alias: String,
    pub domain_xml_sha256: String,
    pub qemu_argv_sha256: String,
    pub libvirt_version_sha256: String,
    pub qemu_version: Value,
    pub dynamic_memslots: bool,
    pub unplugged_inaccessible: bool,
}

impl LiveCompatibilityEvidence {
    fn validate(&self) -> Result<(), String> {
        if self.vm_name.trim().is_empty() {
            return Err("compatibility evidence VM name must be non-empty".to_owned());
        }
        validate_alias(&self.device_alias)?;
        for (name, value) in [
            ("domain_xml_sha256", &self.domain_xml_sha256),
            ("qemu_argv_sha256", &self.qemu_argv_sha256),
            ("libvirt_version_sha256", &self.libvirt_version_sha256),
        ] {
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("compatibility evidence {name} must be SHA-256 hex"));
            }
        }
        let qemu = self
            .qemu_version
            .get("qemu")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                "compatibility evidence qemu_version.qemu must be an object".to_owned()
            })?;
        for component in ["major", "minor", "micro"] {
            if qemu.get(component).and_then(Value::as_u64).is_none() {
                return Err(format!(
                    "compatibility evidence qemu_version.qemu.{component} must be an unsigned integer"
                ));
            }
        }
        if self
            .qemu_version
            .get("package")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err("compatibility evidence qemu_version.package must be a string".to_owned());
        }
        if !self.dynamic_memslots {
            return Err("compatibility evidence does not enable dynamic_memslots".to_owned());
        }
        if !self.unplugged_inaccessible {
            return Err("compatibility evidence does not enable unplugged_inaccessible".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityAttestation {
    pub version: u32,
    pub evidence: LiveCompatibilityEvidence,
    pub review: CompatibilityReview,
    pub fingerprint_sha256: String,
}

impl CompatibilityAttestation {
    pub fn new(
        evidence: LiveCompatibilityEvidence,
        review: CompatibilityReview,
    ) -> Result<Self, String> {
        evidence.validate()?;
        review.validate()?;
        let mut document = Self {
            version: COMPATIBILITY_ATTESTATION_VERSION,
            evidence,
            review,
            fingerprint_sha256: String::new(),
        };
        document.fingerprint_sha256 = document.expected_fingerprint()?;
        Ok(document)
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        let document: Self = serde_json::from_str(json)
            .map_err(|error| format!("parse compatibility attestation: {error}"))?;
        document.validate_integrity()?;
        Ok(document)
    }

    pub fn to_pretty_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|error| format!("serialize compatibility attestation: {error}"))
    }

    pub fn validate_integrity(&self) -> Result<(), String> {
        if self.version != COMPATIBILITY_ATTESTATION_VERSION {
            return Err(format!(
                "unsupported compatibility attestation version {} (expected {COMPATIBILITY_ATTESTATION_VERSION}); attest the VM again",
                self.version
            ));
        }
        self.evidence.validate()?;
        self.review.validate()?;
        let expected = self.expected_fingerprint()?;
        if self.fingerprint_sha256 != expected {
            return Err("compatibility attestation fingerprint is invalid".to_owned());
        }
        Ok(())
    }

    pub fn validate_live(
        &self,
        live: &LiveCompatibilityEvidence,
        guest_os: GuestOs,
    ) -> Result<(), String> {
        self.validate_integrity()?;
        live.validate()?;
        if self.review.guest_os != guest_os {
            return Err(format!(
                "compatibility attestation reviewed a {:?} guest but the VM is configured as {guest_os:?}",
                self.review.guest_os
            ));
        }
        // Each field with its attested and live values, so a drift report
        // identifies what changed.
        let comparisons = [
            (
                "vm_name",
                self.evidence.vm_name.clone(),
                live.vm_name.clone(),
            ),
            (
                "device_alias",
                self.evidence.device_alias.clone(),
                live.device_alias.clone(),
            ),
            (
                "domain_xml",
                self.evidence.domain_xml_sha256.clone(),
                live.domain_xml_sha256.clone(),
            ),
            (
                "qemu_argv",
                self.evidence.qemu_argv_sha256.clone(),
                live.qemu_argv_sha256.clone(),
            ),
            (
                "libvirt_version",
                self.evidence.libvirt_version_sha256.clone(),
                live.libvirt_version_sha256.clone(),
            ),
            (
                "qemu_version",
                self.evidence.qemu_version.to_string(),
                live.qemu_version.to_string(),
            ),
            (
                "dynamic_memslots",
                self.evidence.dynamic_memslots.to_string(),
                live.dynamic_memslots.to_string(),
            ),
            (
                "unplugged_inaccessible",
                self.evidence.unplugged_inaccessible.to_string(),
                live.unplugged_inaccessible.to_string(),
            ),
        ];
        if let Some((field, attested, observed)) = comparisons
            .iter()
            .find(|(_, attested, observed)| attested != observed)
        {
            return Err(format!(
                "live compatibility evidence drifted: {field} (attested {attested}, live {observed})"
            ));
        }
        Ok(())
    }

    fn expected_fingerprint(&self) -> Result<String, String> {
        let payload = serde_json::to_vec(&json!({
            "version": self.version,
            "evidence": self.evidence,
            "review": self.review,
        }))
        .map_err(|error| format!("serialize compatibility fingerprint payload: {error}"))?;
        Ok(sha256_hex(&payload))
    }
}

pub struct VirshCompatibilityEvidenceSource<C> {
    command: C,
    vm_name: String,
    alias: String,
}

impl<C> VirshCompatibilityEvidenceSource<C> {
    pub fn new(command: C, vm_name: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            command,
            vm_name: vm_name.into(),
            alias: alias.into(),
        }
    }
}

impl<C: VirshCommand> VirshCompatibilityEvidenceSource<C> {
    pub fn collect(&self) -> Result<LiveCompatibilityEvidence, String> {
        validate_alias(&self.alias)?;
        let domain_xml = self.run("live domain XML", &["dumpxml", &self.vm_name])?;
        parse_virtio_mem_xml_for_alias(&domain_xml, &self.alias)
            .map_err(|error| format!("validate live domain XML for attestation: {error}"))?;
        let qemu_argv = self.run(
            "live QEMU argv",
            &["domxml-to-native", "qemu-argv", "--domain", &self.vm_name],
        )?;
        let libvirt_version = self.run("libvirt version", &["version"])?;
        let qemu_version = self.qmp("query-version", None)?;
        let dynamic_memslots = self.qom_bool("dynamic-memslots")?;
        let unplugged_inaccessible = self.qom_bool("unplugged-inaccessible")?;
        Ok(LiveCompatibilityEvidence {
            vm_name: self.vm_name.clone(),
            device_alias: self.alias.clone(),
            domain_xml_sha256: sha256_hex(
                scrub_runtime_identity(&scrub_allocation_state(&domain_xml)).as_bytes(),
            ),
            qemu_argv_sha256: sha256_hex(
                scrub_descriptor_numbers(&scrub_qemu_argv(&qemu_argv))
                    .trim()
                    .as_bytes(),
            ),
            libvirt_version_sha256: sha256_hex(libvirt_version.trim().as_bytes()),
            qemu_version,
            dynamic_memslots,
            unplugged_inaccessible,
        })
    }

    fn run(&self, description: &str, arguments: &[&str]) -> Result<String, String> {
        let output = self
            .command
            .run(
                &arguments
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| format!("read {description}: {error}"))?;
        if output.trim().is_empty() {
            Err(format!("read {description}: command returned empty output"))
        } else {
            Ok(output)
        }
    }

    fn qmp(&self, execute: &str, arguments: Option<Value>) -> Result<Value, String> {
        let mut request = json!({"execute": execute});
        if let Some(arguments) = arguments {
            request["arguments"] = arguments;
        }
        let response = self.run(
            "live QMP evidence",
            &["qemu-monitor-command", &self.vm_name, &request.to_string()],
        )?;
        let response: Value = serde_json::from_str(&response)
            .map_err(|error| format!("parse live QMP evidence: {error}"))?;
        response
            .get("return")
            .cloned()
            .ok_or_else(|| "live QMP evidence response is missing return".to_owned())
    }

    fn qom_bool(&self, property: &str) -> Result<bool, String> {
        let path = format!("/machine/peripheral/{}", self.alias);
        let value = self.qmp("qom-get", Some(json!({"path": path, "property": property})))?;
        match value {
            Value::Bool(value) => Ok(value),
            Value::String(value) if matches!(value.as_str(), "on" | "yes" | "true" | "1") => {
                Ok(true)
            }
            Value::String(value) if matches!(value.as_str(), "off" | "no" | "false" | "0") => {
                Ok(false)
            }
            _ => Err(format!(
                "live QMP property {property} has unsupported value {value}"
            )),
        }
    }
}

pub struct AttestedCompatibilitySource<C> {
    live: VirshCompatibilityEvidenceSource<C>,
    path: PathBuf,
    guest_os: GuestOs,
}

impl<C> AttestedCompatibilitySource<C> {
    pub fn new(
        command: C,
        vm_name: impl Into<String>,
        alias: impl Into<String>,
        guest_os: GuestOs,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            live: VirshCompatibilityEvidenceSource::new(command, vm_name, alias),
            path: path.into(),
            guest_os,
        }
    }
}

impl<C: VirshCommand> CompatibilitySource for AttestedCompatibilitySource<C> {
    fn compatibility(&self) -> Result<VirtioMemCompatibility, String> {
        let attestation = read_attestation(&self.path)?;
        let live = self.live.collect()?;
        attestation.validate_live(&live, self.guest_os)?;
        Ok(VirtioMemCompatibility {
            dynamic_memslots: evidence(live.dynamic_memslots),
            unplugged_inaccessible: evidence(live.unplugged_inaccessible),
            workload_review: CompatibilityEvidence::Confirmed,
        })
    }
}

pub fn read_attestation(path: &Path) -> Result<CompatibilityAttestation, String> {
    CompatibilityAttestation::parse(&read_bounded_json(path, "compatibility attestation")?)
}

pub fn read_review(path: &Path) -> Result<CompatibilityReview, String> {
    let json = read_bounded_json(path, "compatibility review")?;
    let review: CompatibilityReview = serde_json::from_str(&json).map_err(|error| {
        format!(
            "parse compatibility review: {error} (reviews now name guest_os, \"windows\" or \"linux\", and no longer carry trusted_development_guest)"
        )
    })?;
    review.validate()?;
    Ok(review)
}

fn read_bounded_json(path: &Path, description: &str) -> Result<String, String> {
    let file = File::open(path)
        .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_ATTESTATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_ATTESTATION_BYTES {
        return Err(format!(
            "{description} {} exceeds {MAX_ATTESTATION_BYTES} bytes",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("read {description} {} as UTF-8: {error}", path.display()))
}

fn evidence(value: bool) -> CompatibilityEvidence {
    if value {
        CompatibilityEvidence::Confirmed
    } else {
        CompatibilityEvidence::Rejected
    }
}

fn validate_alias(alias: &str) -> Result<(), String> {
    if virtio_mem_core::settings::valid_alias(alias) {
        Ok(())
    } else {
        Err("compatibility evidence alias contains unsupported characters".to_owned())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn scrub_allocation_state(xml: &str) -> String {
    let mut scrubbed = xml.to_owned();
    // Libvirt derives the live top-level currentMemory value from base memory
    // plus the virtio-mem allocation. It therefore changes alongside the
    // alias-scoped requested/current fields and is not configuration drift.
    for element in ["currentMemory", "requested", "current"] {
        let mut offset = 0;
        while let Some(start) = scrubbed[offset..].find(&format!("<{element}")) {
            let start = offset + start;
            let name_end = start + element.len() + 1;
            if !scrubbed[name_end..]
                .chars()
                .next()
                .is_some_and(|character| character == '>' || character.is_whitespace())
            {
                offset = name_end;
                continue;
            }
            let Some(open_end) = scrubbed[start..].find('>') else {
                break;
            };
            let content_start = start + open_end + 1;
            let close = format!("</{element}>");
            let Some(close_start) = scrubbed[content_start..].find(&close) else {
                break;
            };
            let close_start = content_start + close_start;
            scrubbed.replace_range(content_start..close_start, "<allocation-state>");
            offset = content_start + "<allocation-state>".len() + close.len();
        }
    }
    scrubbed
}

/// Libvirt assigns a new domain `id` and, for `type='dynamic'` security
/// labels, a new random label every time the domain starts. Neither is
/// configuration, so a VM restart must not look like drift. Static labels
/// stay fingerprint inputs.
fn scrub_runtime_identity(xml: &str) -> String {
    let mut scrubbed = xml.to_owned();
    if let Some(start) = scrubbed.find("<domain") {
        let tag_end = scrubbed[start..]
            .find('>')
            .map_or(scrubbed.len(), |end| start + end);
        for quote in ['\'', '"'] {
            let attribute = format!(" id={quote}");
            if let Some(id) = scrubbed[start..tag_end].find(&attribute) {
                let value_start = start + id + attribute.len();
                if let Some(value_end) = scrubbed[value_start..tag_end].find(quote) {
                    scrubbed.replace_range(start + id..value_start + value_end + 1, "");
                }
                break;
            }
        }
    }
    // Virtio-serial target connection state is runtime status. Preserve its
    // configured name, source socket and address, but not this status attribute.
    let mut channel_offset = 0;
    while let Some(found) = scrubbed[channel_offset..].find("<channel ") {
        let start = channel_offset + found;
        let Some(close) = scrubbed[start..].find("</channel>") else {
            break;
        };
        let end = start + close;
        let mut block = scrubbed[start..end].to_owned();
        if let Some(target) = block.find("<target ") {
            let target_end = block[target..]
                .find('>')
                .map_or(block.len(), |i| target + i);
            if block[target..target_end].contains("type='virtio'")
                || block[target..target_end].contains("type=\"virtio\"")
            {
                for quote in ['\'', '"'] {
                    let attribute = format!(" state={quote}");
                    if let Some(at) = block[target..target_end].find(&attribute) {
                        let at = target + at;
                        let value = at + attribute.len();
                        if let Some(close) = block[value..target_end].find(quote) {
                            block.replace_range(at..value + close + 1, "");
                        }
                        break;
                    }
                }
            }
        }
        scrubbed.replace_range(start..end, &block);
        channel_offset = start + block.len() + "</channel>".len();
    }
    let mut offset = 0;
    while let Some(start) = scrubbed[offset..].find("<seclabel") {
        let start = offset + start;
        let Some(end) = scrubbed[start..].find("</seclabel>") else {
            break;
        };
        let end = start + end;
        let open_end = scrubbed[start..end]
            .find('>')
            .map_or(end, |open| start + open);
        let tag = &scrubbed[start..open_end];
        if tag.contains("type='dynamic'") || tag.contains("type=\"dynamic\"") {
            let mut block = scrubbed[start..end].to_owned();
            for element in ["label", "imagelabel"] {
                let open = format!("<{element}>");
                let close = format!("</{element}>");
                if let (Some(content), Some(close_at)) = (block.find(&open), block.find(&close)) {
                    let content = content + open.len();
                    if content <= close_at {
                        block.replace_range(content..close_at, "<runtime-label>");
                    }
                }
            }
            scrubbed.replace_range(start..end, &block);
            offset = start + block.len();
        } else {
            offset = end;
        }
    }
    scrubbed
}

/// Removes the `requested-size` member, value and one adjacent separator from
/// the generated argv. Libvirt omits the member entirely when the request is
/// zero, so blanking only its value would make a zero request look like
/// configuration drift.
fn scrub_qemu_argv(arguments: &str) -> String {
    const KEY: &str = "requested-size";
    let mut scrubbed = arguments.to_owned();
    let mut offset = 0;
    while let Some(found) = scrubbed[offset..].find(KEY) {
        let key_start = offset + found;
        let key_end = key_start + KEY.len();
        let member_start = if scrubbed[..key_start].ends_with('"') {
            key_start - 1
        } else {
            key_start
        };
        let mut separator = key_end;
        while let Some(character) = scrubbed[separator..].chars().next() {
            if character == '"' || character.is_whitespace() {
                separator += character.len_utf8();
            } else {
                break;
            }
        }
        let Some(separator_character) = scrubbed[separator..].chars().next() else {
            break;
        };
        if !matches!(separator_character, '=' | ':') {
            offset = key_end;
            continue;
        }
        let mut value_start = separator + separator_character.len_utf8();
        while let Some(character) = scrubbed[value_start..].chars().next() {
            if character.is_whitespace() {
                value_start += character.len_utf8();
            } else {
                break;
            }
        }
        let quoted = scrubbed[value_start..].starts_with('"');
        if quoted {
            value_start += 1;
        }
        let mut member_end = scrubbed[value_start..]
            .find(|character: char| {
                (quoted && character == '"')
                    || (!quoted
                        && (character == ',' || character == '}' || character.is_whitespace()))
            })
            .map_or(scrubbed.len(), |end| value_start + end);
        if quoted && member_end < scrubbed.len() {
            member_end += 1;
        }
        let (remove_start, remove_end) = if scrubbed[member_end..].starts_with(',') {
            (member_start, member_end + 1)
        } else if scrubbed[..member_start].ends_with(',') {
            (member_start - 1, member_end)
        } else {
            (member_start, member_end)
        };
        scrubbed.replace_range(remove_start..remove_end, "");
        offset = remove_start;
    }
    scrubbed
}

/// Blanks the file-descriptor numbers libvirt puts in the generated argv
/// (`"fd":"38"`, `vhostfd=40`, `fds=a:b`). They are libvirtd's own open
/// descriptors at generation time and differ between two calls on an
/// unchanged domain.
fn scrub_descriptor_numbers(arguments: &str) -> String {
    const KEYS: [&str; 4] = ["fd", "vhostfd", "fds", "vhostfds"];
    let mut scrubbed = String::with_capacity(arguments.len());
    let mut rest = arguments;
    'scan: while !rest.is_empty() {
        for key in KEYS {
            for (prefix, quoted) in [
                (format!("\"{key}\":\""), true),
                (format!("\"{key}\":"), false),
                (format!(",{key}="), false),
                (format!(" {key}="), false),
            ] {
                let Some(after) = rest.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                let length = after
                    .find(|character: char| !(character.is_ascii_digit() || character == ':'))
                    .unwrap_or(after.len());
                let closed = !quoted || after[length..].starts_with('"');
                if length > 0 && closed {
                    scrubbed.push_str(&prefix);
                    scrubbed.push('#');
                    rest = &after[length..];
                    continue 'scan;
                }
            }
        }
        let character = rest.chars().next().expect("non-empty");
        scrubbed.push(character);
        rest = &rest[character.len_utf8()..];
    }
    scrubbed
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::virsh::VirshError;

    struct FakeVirsh {
        responses: RefCell<VecDeque<Result<String, VirshError>>>,
    }
    impl FakeVirsh {
        fn new(values: impl IntoIterator<Item = String>) -> Self {
            Self {
                responses: RefCell::new(values.into_iter().map(Ok).collect()),
            }
        }
    }
    impl VirshCommand for FakeVirsh {
        fn run(&self, _arguments: &[String]) -> Result<String, VirshError> {
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| VirshError::Failed {
                    status: "fixture exhausted".to_owned(),
                    stderr: String::new(),
                })?
        }
    }

    fn review() -> CompatibilityReview {
        CompatibilityReview {
            guest_os: GuestOs::Windows,
            workload_reviewed: true,
            no_vdpa: true,
            no_rdma_migration: true,
            no_vfio_nvme: true,
            no_mlock: true,
            no_secure_virtualization: true,
            no_unsupported_vhost_user: true,
            balloon_resize_inactive: true,
            memory_slot_budget: 32,
            vfio_mapping_budget: 8,
            guest_driver_version: "100.101.104.20800".to_owned(),
        }
    }

    #[test]
    fn a_review_written_for_windows_only_is_still_read() {
        let current = serde_json::to_value(review()).unwrap();
        assert!(current.get("windows_driver_version").is_none());
        let mut earlier = current.clone();
        let fields = earlier.as_object_mut().unwrap();
        let version = fields.remove("guest_driver_version").unwrap();
        fields.insert("windows_driver_version".to_owned(), version);
        assert_eq!(
            serde_json::from_value::<CompatibilityReview>(earlier).unwrap(),
            review()
        );
        let mut both = current;
        both["windows_driver_version"] = serde_json::json!("1");
        assert!(serde_json::from_value::<CompatibilityReview>(both).is_err());
    }

    fn live() -> LiveCompatibilityEvidence {
        LiveCompatibilityEvidence {
            vm_name: "guest".to_owned(),
            device_alias: "memory0".to_owned(),
            domain_xml_sha256: "a".repeat(64),
            qemu_argv_sha256: "b".repeat(64),
            libvirt_version_sha256: "c".repeat(64),
            qemu_version: json!({"qemu":{"major":9,"minor":2,"micro":0},"package":"qemu-9.2"}),
            dynamic_memslots: true,
            unplugged_inaccessible: true,
        }
    }

    #[test]
    fn round_trip_accepts_exact_reviewed_evidence() {
        let document = CompatibilityAttestation::new(live(), review()).expect("valid attestation");
        let parsed = CompatibilityAttestation::parse(&document.to_pretty_json().expect("JSON"))
            .expect("valid parsed attestation");
        assert_eq!(parsed.validate_live(&live(), GuestOs::Windows), Ok(()));
    }

    #[test]
    fn rejects_every_review_or_live_drift_category() {
        let document = CompatibilityAttestation::new(live(), review()).expect("valid attestation");
        let mut changes = Vec::new();
        let mut changed = live();
        changed.device_alias = "other".to_owned();
        changes.push(changed);
        let mut changed = live();
        changed.domain_xml_sha256 = "d".repeat(64);
        changes.push(changed);
        let mut changed = live();
        changed.qemu_argv_sha256 = "e".repeat(64);
        changes.push(changed);
        let mut changed = live();
        changed.libvirt_version_sha256 = "f".repeat(64);
        changes.push(changed);
        let mut changed = live();
        changed.qemu_version = json!({"qemu":{"major":10,"minor":0,"micro":0},"package":"qemu-10"});
        changes.push(changed);
        let mut changed = live();
        changed.dynamic_memslots = false;
        changes.push(changed);
        let mut changed = live();
        changed.unplugged_inaccessible = false;
        changes.push(changed);
        for changed in changes {
            assert!(document.validate_live(&changed, GuestOs::Windows).is_err());
        }

        assert!(document.validate_live(&live(), GuestOs::Linux).is_err());

        let mut tampered = document;
        tampered.review.vfio_mapping_budget += 1;
        assert!(tampered.validate_integrity().is_err());

        let mut unreviewed = review();
        unreviewed.no_vfio_nvme = false;
        assert!(CompatibilityAttestation::new(live(), unreviewed).is_err());

        let mut unsupported = CompatibilityAttestation::new(live(), review())
            .expect("valid attestation before version change");
        unsupported.version += 1;
        assert!(unsupported.validate_integrity().is_err());
    }

    #[test]
    fn channel_connection_status_is_runtime_but_binding_is_attested() {
        let xml = "<domain><devices><channel type='unix'><source path='/wake'/><target type='virtio' name='org.virtio-mem.wake.0' state='connected'/><address port='2'/></channel></devices></domain>";
        assert_eq!(
            scrub_runtime_identity(xml),
            scrub_runtime_identity(&xml.replace("connected", "disconnected"))
        );
        assert_eq!(
            scrub_runtime_identity(xml),
            scrub_runtime_identity(&xml.replace(" state='connected'", ""))
        );
        for changed in [
            xml.replace("/wake", "/other"),
            xml.replace("wake.0", "other.0"),
            xml.replace("port='2'", "port='3'"),
        ] {
            assert_ne!(
                scrub_runtime_identity(xml),
                scrub_runtime_identity(&changed)
            );
        }
    }

    #[test]
    fn allocation_progress_does_not_change_domain_fingerprint() {
        let before = "<domain><currentMemory>8</currentMemory><memory><target><requested unit='MiB'>2</requested><current unit='MiB'>0</current></target></memory></domain>";
        let after = "<domain><currentMemory>12</currentMemory><memory><target><requested unit='MiB'>4</requested><current unit='MiB'>4</current></target></memory></domain>";
        assert_eq!(
            sha256_hex(scrub_allocation_state(before).as_bytes()),
            sha256_hex(scrub_allocation_state(after).as_bytes())
        );
        assert_eq!(
            sha256_hex(scrub_qemu_argv("-device virtio-mem,requested-size=1G,node=0").as_bytes()),
            sha256_hex(scrub_qemu_argv("-device virtio-mem,requested-size=4G,node=0").as_bytes())
        );
        assert_eq!(
            sha256_hex(
                scrub_qemu_argv(
                    r#"-device '{"driver":"virtio-mem-pci","requested-size":1071644672,"node":0}'"#
                )
                .as_bytes()
            ),
            sha256_hex(
                scrub_qemu_argv(
                    r#"-device '{"driver":"virtio-mem-pci","requested-size":1073741824,"node":0}'"#
                )
                .as_bytes()
            )
        );
        // Libvirt omits the member when the request is zero.
        let zero = r#"-device '{"driver":"virtio-mem-pci","node":0,"memdev":"m0","id":"memory0"}'"#;
        for argv in [
            r#"-device '{"driver":"virtio-mem-pci","node":0,"requested-size":1073741824,"memdev":"m0","id":"memory0"}'"#,
            r#"-device '{"driver":"virtio-mem-pci","node":0,"memdev":"m0","id":"memory0","requested-size":2097152}'"#,
            r#"-device '{"driver":"virtio-mem-pci","node":0,"memdev":"m0","id":"memory0","requested-size":"4194304"}'"#,
        ] {
            assert_eq!(scrub_qemu_argv(argv), scrub_qemu_argv(zero), "{argv}");
        }
        assert_eq!(
            scrub_qemu_argv("-device virtio-mem,node=0,requested-size=1G,id=m"),
            scrub_qemu_argv("-device virtio-mem,node=0,id=m")
        );
        assert_eq!(
            scrub_allocation_state(before),
            scrub_allocation_state(after)
        );
        assert_eq!(
            scrub_descriptor_numbers(
                r#"-netdev '{"type":"tap","fd":"34","vhost":true,"vhostfd":"38","id":"n"}'"#
            ),
            scrub_descriptor_numbers(
                r#"-netdev '{"type":"tap","fd":"38","vhost":true,"vhostfd":"40","id":"n"}'"#
            )
        );
        assert_eq!(
            scrub_descriptor_numbers("-netdev tap,fds=3:4,vhostfds=5:6,id=n -m size=8"),
            "-netdev tap,fds=#,vhostfds=#,id=n -m size=8"
        );
        // Other members, and paths that merely contain the key, are kept.
        for argv in [
            r#"{"type":"tap","fd":"38","id":"n1"}"#,
            "-chardev socket,path=/run/fd=3",
        ] {
            assert_ne!(
                scrub_descriptor_numbers(argv),
                scrub_descriptor_numbers(&argv.replace('1', "2").replace("/run", "/var"))
            );
        }
    }

    #[test]
    fn domain_restart_identity_does_not_change_domain_fingerprint() {
        let started = |id: u32, label: &str| {
            format!(
                "<domain type='kvm' id='{id}'><name>vm</name><seclabel type='dynamic' model='selinux' relabel='yes'><label>system_u:system_r:svirt_t:s0:{label}</label><imagelabel>system_u:object_r:svirt_image_t:s0:{label}</imagelabel></seclabel><seclabel type='dynamic' model='dac' relabel='yes'><label>+107:+107</label><imagelabel>+107:+107</imagelabel></seclabel></domain>"
            )
        };
        assert_eq!(
            scrub_runtime_identity(&started(3, "c1,c2")),
            scrub_runtime_identity(&started(10, "c148,c988"))
        );
        assert!(!scrub_runtime_identity(&started(3, "c1,c2")).contains("id="));
        // Configuration around them is still covered.
        assert_ne!(
            scrub_runtime_identity(&started(3, "c1,c2")),
            scrub_runtime_identity(&started(3, "c1,c2").replace("<name>vm", "<name>other"))
        );
        let fixed = |label: &str| {
            format!("<domain id='1'><seclabel type='static' model='selinux'><label>{label}</label></seclabel></domain>")
        };
        assert_ne!(
            scrub_runtime_identity(&fixed("s0:c1")),
            scrub_runtime_identity(&fixed("s0:c2"))
        );
        // A device id elsewhere is not the domain id.
        assert!(
            scrub_runtime_identity("<domain id='1'><memory id='m'/></domain>").contains("id='m'")
        );
    }

    #[test]
    fn similarly_prefixed_xml_elements_remain_fingerprint_inputs() {
        let before = "<domain><currentMemorySlots>8</currentMemorySlots></domain>";
        let after = "<domain><currentMemorySlots>16</currentMemorySlots></domain>";

        assert_ne!(
            scrub_allocation_state(before),
            scrub_allocation_state(after)
        );
    }

    #[test]
    fn collector_hashes_full_configuration_and_parses_qmp_booleans() {
        let source = VirshCompatibilityEvidenceSource::new(FakeVirsh::new([
            "<domain><memory model='virtio-mem' dynamic-memslots='on' unplugged-inaccessible='on'><target><size unit='GiB'>8</size><block unit='MiB'>2</block><requested unit='GiB'>1</requested><current unit='GiB'>1</current></target><alias name='memory0'/></memory></domain>".to_owned(),
            "qemu-system -machine q35 -object memory-backend-file,id=mem0".to_owned(),
            "Compiled against library: libvirt 11.0.0".to_owned(),
            r#"{"return":{"qemu":{"major":9,"minor":2,"micro":0},"package":"qemu-9.2"}}"#.to_owned(),
            r#"{"return":true}"#.to_owned(), r#"{"return":"on"}"#.to_owned(),
        ]), "guest", "memory0");
        let evidence = source.collect().expect("complete evidence");
        assert!(evidence.dynamic_memslots && evidence.unplugged_inaccessible);
        assert_eq!(evidence.domain_xml_sha256.len(), 64);
    }
}
