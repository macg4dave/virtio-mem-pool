//! The controller's systemd unit: one per host (decision D2).

/// The controller's unit.
pub const CONTROLLER_UNIT: &str = "rampire.service";

/// Refuse any unit but the controller's.
pub fn require_controller_unit(unit: &str) -> Result<(), String> {
    if unit == CONTROLLER_UNIT {
        Ok(())
    } else {
        Err(format!(
            "{unit} is not the controller unit {CONTROLLER_UNIT}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_controller_unit_is_accepted() {
        assert_eq!(require_controller_unit("rampire.service"), Ok(()));
        for unit in ["rampire@main.service", "other.service", ""] {
            assert!(require_controller_unit(unit).is_err(), "{unit}");
        }
    }
}
