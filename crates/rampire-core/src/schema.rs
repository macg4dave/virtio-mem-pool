//! Every JSON document names its schema beside its version (decision D20),
//! so a reader can tell documents apart. A document whose `schema` is
//! missing or names another document is refused when it is parsed.

/// Defines a unit type that is written as the schema name and refuses any
/// other name when read. A document holds it as its first field, `schema`.
#[macro_export]
macro_rules! schema_name {
    ($(#[$meta:meta])* $vis:vis struct $type:ident = $name:literal;) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        $vis struct $type;

        impl ::serde::Serialize for $type {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str($name)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $type {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let name =
                    <::std::string::String as ::serde::Deserialize>::deserialize(deserializer)?;
                if name == $name {
                    Ok($type)
                } else {
                    Err(<D::Error as ::serde::de::Error>::custom(format!(
                        "expected a {} document, found {name:?}",
                        $name
                    )))
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    schema_name! {
        struct ExampleSchema = "rampire.example";
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Example {
        schema: ExampleSchema,
        version: u16,
    }

    #[test]
    fn writes_the_name_and_refuses_another() {
        let example = Example {
            schema: ExampleSchema,
            version: 1,
        };
        let json = serde_json::to_string(&example).unwrap();
        assert_eq!(json, r#"{"schema":"rampire.example","version":1}"#);
        assert_eq!(serde_json::from_str::<Example>(&json).unwrap(), example);

        let other = serde_json::from_str::<Example>(r#"{"schema":"rampire.other","version":1}"#);
        assert!(other.unwrap_err().to_string().contains("rampire.example"));
        assert!(serde_json::from_str::<Example>(r#"{"version":1}"#).is_err());
    }
}
