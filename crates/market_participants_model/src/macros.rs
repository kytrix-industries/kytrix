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
