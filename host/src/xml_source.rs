use rampire_core::{parse_virtio_mem_xml_for_alias, VirtioMemState};

use crate::runtime::MemoryStateSource;
use crate::virsh::VirshCommand;

pub struct VirshXmlSource<C> {
    command: C,
    vm_name: String,
    alias: String,
}

impl<C> VirshXmlSource<C> {
    pub fn new(command: C, vm_name: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            command,
            vm_name: vm_name.into(),
            alias: alias.into(),
        }
    }
}

impl<C: VirshCommand> MemoryStateSource for VirshXmlSource<C> {
    fn memory_state(&self) -> Result<VirtioMemState, String> {
        let xml = self
            .command
            .run(&["dumpxml".to_owned(), self.vm_name.clone()])
            .map_err(|error| error.to_string())?;
        parse_virtio_mem_xml_for_alias(&xml, &self.alias)
            .map(|snapshot| snapshot.memory)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virsh::VirshError;
    struct Fake {
        xml: String,
    }
    impl VirshCommand for Fake {
        fn run(&self, arguments: &[String]) -> Result<String, VirshError> {
            assert_eq!(arguments, &["dumpxml".to_owned(), "guest".to_owned()]);
            Ok(self.xml.clone())
        }
    }
    #[test]
    fn reads_the_configured_alias_only() {
        let xml = "<domain><memory model='virtio-mem'><target><size unit='MiB'>8</size><block unit='MiB'>2</block><requested unit='MiB'>4</requested><current unit='MiB'>4</current></target><alias name='memory0'/></memory></domain>".to_owned();
        assert_eq!(
            VirshXmlSource::new(Fake { xml }, "guest", "memory0")
                .memory_state()
                .expect("valid state")
                .current_bytes,
            4 * 1024 * 1024
        );
    }
}
