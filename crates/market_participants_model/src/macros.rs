//! Internal declarative macros shared by the model modules.
//!
//! These macros are crate-internal helpers: they encode a repeated shape that
//! would otherwise be duplicated per module, and they are never re-exported.

/// Generates the deserialization path of a value type whose fields are private.
///
/// The wire struct mirrors every field as an `Option`, so the value type itself
/// only ever exists after its own `validate` ran; a missing field is reported by
/// the value validation instead of serde.
#[macro_export]
macro_rules! validating_wire {
    ($name:ident, $wire:ident, $($field:ident as $wire_field:literal : $ty:ty),+ $(,)?) => {
        #[derive(::serde::Deserialize)]
        struct $wire {
            $(
                #[serde(rename = $wire_field)]
                $field: Option<$ty>,
            )+
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                let wire = $wire::deserialize(deserializer)?;
                let value = Self {
                    $($field: wire.$field,)+
                };
                value.validate().map_err(::serde::de::Error::custom)?;
                Ok(value)
            }
        }
    };
}
