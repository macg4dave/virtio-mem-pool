//! Checked conversions at external memory-unit boundaries.

pub const BYTES_PER_KIB: u64 = 1 << 10;

/// Convert an external KiB value to the canonical byte unit.
pub fn kibibytes_to_bytes(value_kib: u64) -> Option<u64> {
    value_kib.checked_mul(BYTES_PER_KIB)
}
/// Convert canonical bytes to an exact external KiB value.
pub fn bytes_to_kibibytes(value_bytes: u64) -> Option<u64> {
    value_bytes
        .is_multiple_of(BYTES_PER_KIB)
        .then_some(value_bytes / BYTES_PER_KIB)
}

const SIZE_EXAMPLES: &str = "write a size such as \"512MB\", \"128MiB\" or \"3GB\"";

/// Parse a human-written size such as `"8GB"`, `"8.5 GiB"`, `"1024MB"` or
/// `"512"` (bytes). Units are binary whatever their spelling (`GB` = `GiB` =
/// 2^30) because device sizes are block multiples, and the result must be a
/// whole number of bytes.
pub fn parse_byte_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        _ => return Err(format!("unknown size unit in {text:?}; {SIZE_EXAMPLES}")),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty()
        || !(whole.chars().chain(fraction.chars())).all(|c| c.is_ascii_digit())
        || fraction.len() > 18
    {
        return Err(format!(
            "invalid size {text:?}; expected e.g. \"8GB\" or \"1024MB\""
        ));
    }
    let scale = 10u128.pow(fraction.len() as u32);
    let digits: u128 = format!("{whole}{fraction}")
        .parse()
        .map_err(|_| format!("size {text:?} is too large"))?;
    let scaled = digits
        .checked_shl(shift)
        .filter(|value| value >> shift == digits)
        .ok_or_else(|| format!("size {text:?} is too large"))?;
    if scaled % scale != 0 {
        return Err(format!("size {text:?} is not a whole number of bytes"));
    }
    u64::try_from(scaled / scale).map_err(|_| format!("size {text:?} is too large"))
}

/// Serde field adapter: reads a byte count as a number or a size string
/// (see [`parse_byte_size`]) and always writes a plain number.
pub mod byte_size {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(*value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        deserializer.deserialize_any(ByteSize)
    }

    struct ByteSize;
    impl Visitor<'_> for ByteSize {
        type Value = u64;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a byte count or a size such as \"8GB\"")
        }
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
            Ok(value)
        }
        // TOML integers arrive signed.
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<u64, E> {
            u64::try_from(value).map_err(|_| E::custom("a byte count cannot be negative"))
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<u64, E> {
            super::parse_byte_size(value).map_err(E::custom)
        }
    }

    /// The same for an optional field; pair with `#[serde(default)]`.
    pub mod option {
        use serde::Deserializer;

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<u64>, D::Error> {
            super::deserialize(deserializer).map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_human_sizes_as_binary_units() {
        const GIB: u64 = 1 << 30;
        assert_eq!(parse_byte_size("8GB"), Ok(8 * GIB));
        assert_eq!(parse_byte_size("8.5GB"), Ok(8 * GIB + GIB / 2));
        assert_eq!(parse_byte_size("1024MB"), Ok(GIB));
        assert_eq!(parse_byte_size(" 2 GiB "), Ok(2 * GIB));
        assert_eq!(parse_byte_size("256mb"), Ok(256 << 20));
        assert_eq!(parse_byte_size("0"), Ok(0));
        assert_eq!(parse_byte_size("4096"), Ok(4096));
        assert_eq!(parse_byte_size(".5K"), Ok(512));
        // The spellings people write in the settings files.
        assert_eq!(parse_byte_size("512MB"), Ok(512 << 20));
        assert_eq!(parse_byte_size("128MiB"), Ok(128 << 20));
        assert_eq!(parse_byte_size("3GB"), Ok(3 * GIB));
        assert_eq!(parse_byte_size("3 GiB"), Ok(3 * GIB));
        assert_eq!(parse_byte_size("1.5 gb"), Ok(GIB + GIB / 2));
        assert_eq!(parse_byte_size("1TB"), Ok(1024 * GIB));
        for bad in [
            "",
            "GB",
            "8XB",
            "1.5B",
            "8..5GB",
            "-1GB",
            "1.0000001KB",
            "20000000TB",
        ] {
            assert!(parse_byte_size(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn byte_size_fields_read_numbers_or_strings_and_write_numbers() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Field {
            #[serde(with = "byte_size")]
            bytes: u64,
        }
        let from_text: Field = serde_json::from_str(r#"{"bytes":"1.5GB"}"#).unwrap();
        let from_number: Field = serde_json::from_str(r#"{"bytes":1610612736}"#).unwrap();
        assert_eq!(from_text, from_number);
        assert_eq!(
            serde_json::to_string(&from_text).unwrap(),
            r#"{"bytes":1610612736}"#
        );
        assert!(serde_json::from_str::<Field>(r#"{"bytes":-1}"#).is_err());
    }

    #[test]
    fn converts_kibibytes_with_checked_arithmetic() {
        assert_eq!(kibibytes_to_bytes(2), Some(2048));
        assert_eq!(kibibytes_to_bytes(u64::MAX), None);
    }

    #[test]
    fn converts_only_exact_byte_values_to_kibibytes() {
        assert_eq!(bytes_to_kibibytes(2048), Some(2));
        assert_eq!(bytes_to_kibibytes(2047), None);
        assert_eq!(bytes_to_kibibytes(u64::MAX), None);
    }

    #[test]
    fn exact_round_trip_preserves_maximum_representable_kibibytes() {
        let value = u64::MAX / BYTES_PER_KIB;
        assert_eq!(
            bytes_to_kibibytes(kibibytes_to_bytes(value).unwrap()),
            Some(value)
        );
    }
}
