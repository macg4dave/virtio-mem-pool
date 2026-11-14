use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityEvidence {
    Confirmed,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMemCompatibility {
    pub dynamic_memslots: CompatibilityEvidence,
    pub unplugged_inaccessible: CompatibilityEvidence,
    pub workload_review: CompatibilityEvidence,
}

impl VirtioMemCompatibility {
    pub const fn unknown() -> Self {
        Self {
            dynamic_memslots: CompatibilityEvidence::Unknown,
            unplugged_inaccessible: CompatibilityEvidence::Unknown,
            workload_review: CompatibilityEvidence::Unknown,
        }
    }

    pub const fn confirmed() -> Self {
        Self {
            dynamic_memslots: CompatibilityEvidence::Confirmed,
            unplugged_inaccessible: CompatibilityEvidence::Confirmed,
            workload_review: CompatibilityEvidence::Confirmed,
        }
    }

    pub fn merge(self, other: Self) -> Result<Self, VirtioMemCompatibilityError> {
        Ok(Self {
            dynamic_memslots: merge_evidence(
                self.dynamic_memslots,
                other.dynamic_memslots,
                "dynamic-memslots",
            )?,
            unplugged_inaccessible: merge_evidence(
                self.unplugged_inaccessible,
                other.unplugged_inaccessible,
                "unplugged-inaccessible",
            )?,
            workload_review: merge_evidence(
                self.workload_review,
                other.workload_review,
                "workload compatibility review",
            )?,
        })
    }

    pub fn validate_for_resize(self) -> Result<(), VirtioMemCompatibilityError> {
        if self.dynamic_memslots != CompatibilityEvidence::Confirmed {
            return Err(VirtioMemCompatibilityError::InsufficientEvidence(
                "dynamic-memslots must be explicitly enabled",
            ));
        }
        if self.unplugged_inaccessible != CompatibilityEvidence::Confirmed {
            return Err(VirtioMemCompatibilityError::InsufficientEvidence(
                "unplugged-inaccessible must be explicitly enabled",
            ));
        }
        if self.workload_review != CompatibilityEvidence::Confirmed {
            return Err(VirtioMemCompatibilityError::InsufficientEvidence(
                "incompatible workload/device review must be confirmed",
            ));
        }
        Ok(())
    }
}

impl Default for VirtioMemCompatibility {
    fn default() -> Self {
        Self::unknown()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VirtioMemCompatibilityError {
    #[error("virtio-mem compatibility evidence is insufficient: {0}")]
    InsufficientEvidence(&'static str),
    #[error("conflicting virtio-mem compatibility evidence for {0}")]
    ConflictingEvidence(&'static str),
}

fn merge_evidence(
    first: CompatibilityEvidence,
    second: CompatibilityEvidence,
    field: &'static str,
) -> Result<CompatibilityEvidence, VirtioMemCompatibilityError> {
    match (first, second) {
        (CompatibilityEvidence::Unknown, value) | (value, CompatibilityEvidence::Unknown) => {
            Ok(value)
        }
        (left, right) if left == right => Ok(left),
        _ => Err(VirtioMemCompatibilityError::ConflictingEvidence(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_evidence_is_completed_by_an_independent_source() {
        let xml = VirtioMemCompatibility {
            dynamic_memslots: CompatibilityEvidence::Confirmed,
            unplugged_inaccessible: CompatibilityEvidence::Confirmed,
            workload_review: CompatibilityEvidence::Unknown,
        };
        assert_eq!(
            xml.merge(VirtioMemCompatibility::confirmed())
                .expect("independent evidence should merge"),
            VirtioMemCompatibility::confirmed()
        );
    }

    #[test]
    fn conflicting_evidence_is_rejected() {
        let rejected = VirtioMemCompatibility {
            dynamic_memslots: CompatibilityEvidence::Rejected,
            ..VirtioMemCompatibility::unknown()
        };
        assert_eq!(
            rejected.merge(VirtioMemCompatibility::confirmed()),
            Err(VirtioMemCompatibilityError::ConflictingEvidence(
                "dynamic-memslots"
            ))
        );
    }
}
