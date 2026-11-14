use rampire_core::VirtioMemCompatibility;

/// Supplies compatibility evidence immediately before resize preparation.
pub trait CompatibilitySource {
    fn compatibility(&self) -> Result<VirtioMemCompatibility, String>;
}

/// Deterministic evidence available only to unit tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedCompatibilitySource {
    evidence: VirtioMemCompatibility,
}

#[cfg(test)]
impl FixedCompatibilitySource {
    /// Construct a source that always returns the supplied evidence.
    pub const fn new(evidence: VirtioMemCompatibility) -> Self {
        Self { evidence }
    }
}

#[cfg(test)]
impl CompatibilitySource for FixedCompatibilitySource {
    fn compatibility(&self) -> Result<VirtioMemCompatibility, String> {
        Ok(self.evidence)
    }
}
