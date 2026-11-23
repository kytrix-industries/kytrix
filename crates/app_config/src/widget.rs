use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VersionedWidgetConfig {
    schema_version: WidgetConfigSchemaVersion,
    payload: String,
}

impl VersionedWidgetConfig {
    pub fn encode<C: WidgetConfig>(config: &C) -> Result<Self, WidgetConfigError> {
        Ok(Self {
            schema_version: C::SCHEMA_VERSION,
            payload: serde_json::to_string(config).map_err(WidgetConfigError)?,
        })
    }

    pub const fn schema_version(&self) -> WidgetConfigSchemaVersion {
        self.schema_version
    }

    pub fn decode<C: WidgetConfig>(self) -> C {
        if self.schema_version != C::SCHEMA_VERSION {
            return C::default();
        }

        serde_json::from_str(&self.payload).unwrap_or_default()
    }
}

pub trait WidgetConfig: Clone + Default + Serialize + DeserializeOwned + 'static {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion;
}

impl WidgetConfig for () {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Debug, thiserror::Error)]
#[error("failed to encode widget config: {0}")]
pub struct WidgetConfigError(#[source] serde_json::Error);

pub fn decode_or_default<C: WidgetConfig>(config: Option<VersionedWidgetConfig>) -> C {
    config.map_or_else(C::default, VersionedWidgetConfig::decode)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WidgetConfigSchemaVersion(u32);

impl WidgetConfigSchemaVersion {
    pub const V1: Self = Self(1);

    #[track_caller]
    pub const fn new_static(value: u32) -> Self {
        if value == 0 {
            panic!("widget config schema version must be greater than zero");
        }
        Self(value)
    }

    pub const fn new(value: u32) -> Result<Self, InvalidWidgetConfigSchemaVersion> {
        if value == 0 {
            Err(InvalidWidgetConfigSchemaVersion)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for WidgetConfigSchemaVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl Serialize for WidgetConfigSchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for WidgetConfigSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidWidgetConfigSchemaVersion;

impl std::fmt::Display for InvalidWidgetConfigSchemaVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("widget config schema version must be greater than zero")
    }
}

impl std::error::Error for InvalidWidgetConfigSchemaVersion {}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
        market::selection::InstrumentSelection,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    struct CurrentConfig {
        interval_seconds: u64,
    }

    impl WidgetConfig for CurrentConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = match WidgetConfigSchemaVersion::new(2) {
            Ok(version) => version,
            Err(_) => WidgetConfigSchemaVersion::V1,
        };
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct ListingConfig {
        selection: InstrumentSelection,
    }

    impl Default for ListingConfig {
        fn default() -> Self {
            Self {
                selection: listing_selection("core/spot", "00"),
            }
        }
    }

    impl WidgetConfig for ListingConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }

    fn listing_selection(scope: &str, identity: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: VenueListingKey::new(
                VenueRoute::new(
                    VenueId::Binance,
                    VenueScopeId::new(scope).expect("the scope should be canonical"),
                ),
                VenueListingId::from_identity_value(&format!(
                    "listing-id/v1/{}",
                    identity.repeat(32)
                ))
                .expect("the listing id should decode"),
            ),
        }
    }

    #[test]
    fn decode_should_reset_a_config_written_under_another_schema_version() {
        let stored = VersionedWidgetConfig {
            schema_version: WidgetConfigSchemaVersion::V1,
            payload: r#"{"interval_minutes":5}"#.to_owned(),
        };

        assert_eq!(stored.decode::<CurrentConfig>(), CurrentConfig::default());
    }

    #[test]
    fn decode_should_reset_a_config_from_a_future_schema_version() {
        let future_version =
            WidgetConfigSchemaVersion::new(3).expect("three is a valid non-zero schema version");
        let stored = VersionedWidgetConfig {
            schema_version: future_version,
            payload: r#"{"interval_seconds":30}"#.to_owned(),
        };

        assert_eq!(
            stored.decode::<CurrentConfig>(),
            CurrentConfig::default(),
            "a version this build does not support is not migrated, it is reset"
        );
    }

    #[test]
    fn encode_and_decode_should_round_trip_current_config() {
        let config = CurrentConfig {
            interval_seconds: 30,
        };

        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");

        assert_eq!(stored.decode::<CurrentConfig>(), config);
    }

    #[test]
    fn decode_should_reset_a_malformed_payload() {
        let stored = VersionedWidgetConfig {
            schema_version: CurrentConfig::SCHEMA_VERSION,
            payload: "{".to_owned(),
        };

        assert_eq!(stored.decode::<CurrentConfig>(), CurrentConfig::default());
    }

    #[test]
    fn decode_should_reset_a_payload_of_the_wrong_type() {
        let stored = VersionedWidgetConfig {
            schema_version: CurrentConfig::SCHEMA_VERSION,
            payload: r#"{"interval_seconds":"thirty"}"#.to_owned(),
        };

        assert_eq!(stored.decode::<CurrentConfig>(), CurrentConfig::default());
    }

    #[test]
    fn decode_should_reset_a_payload_that_does_not_carry_the_required_listing() {
        let current = ListingConfig {
            selection: listing_selection("core/perp", "ab"),
        };
        let stored = VersionedWidgetConfig::encode(&current).expect("the config should encode");

        assert_eq!(stored.decode::<ListingConfig>(), current);
        assert_ne!(
            current,
            ListingConfig::default(),
            "a selection that decoded is not the selection the widget falls back to"
        );

        for removed in [
            concat!(
                r#"{"route":{"venue":"binance","scope":"spot"},"#,
                r#""instrument":{"symbol":"BTCUSDT"}}"#,
            ),
            r#"{"selection":{"route":{"venue":"binance","scope":"spot"}}}"#,
            r#"{"selection":{}}"#,
            "{}",
        ] {
            assert!(
                serde_json::from_str::<ListingConfig>(removed).is_err(),
                "the listing is required to decode this config: {removed}"
            );

            let stored = VersionedWidgetConfig {
                schema_version: ListingConfig::SCHEMA_VERSION,
                payload: removed.to_owned(),
            };

            assert_eq!(
                stored.decode::<ListingConfig>(),
                ListingConfig::default(),
                "{removed}"
            );
        }
    }

    #[test]
    fn decode_or_default_should_answer_for_every_stored_state() {
        assert_eq!(
            decode_or_default::<CurrentConfig>(None),
            CurrentConfig::default(),
            "a pane that never stored a config gets the defaults"
        );
        assert_eq!(
            decode_or_default::<CurrentConfig>(Some(VersionedWidgetConfig {
                schema_version: WidgetConfigSchemaVersion::V1,
                payload: "{}".to_owned(),
            })),
            CurrentConfig::default(),
            "a config from another version gets the defaults"
        );
        assert_eq!(
            decode_or_default::<CurrentConfig>(Some(
                VersionedWidgetConfig::encode(&CurrentConfig {
                    interval_seconds: 30
                })
                .expect("config should encode")
            )),
            CurrentConfig {
                interval_seconds: 30
            }
        );
    }

    #[test]
    fn encoding_the_same_config_twice_should_produce_the_same_envelope() {
        let config = CurrentConfig {
            interval_seconds: 30,
        };

        assert_eq!(
            VersionedWidgetConfig::encode(&config).expect("config should encode"),
            VersionedWidgetConfig::encode(&config).expect("config should encode"),
            "a pane compares envelopes to decide whether its config was replaced"
        );
    }

    #[test]
    fn envelope_should_reject_legacy_value_field() {
        let error = serde_json::from_value::<VersionedWidgetConfig>(json!({
            "schema_version": 2,
            "value": { "interval_seconds": 30 }
        }))
        .expect_err("legacy value envelope must be rejected");

        assert!(error.to_string().contains("missing field"));
    }

    #[test]
    fn envelope_should_ignore_unknown_fields() {
        let stored = serde_json::from_value::<VersionedWidgetConfig>(json!({
            "schema_version": 1,
            "payload": "{}",
            "extra": true,
        }))
        .expect("unknown envelope fields should be ignored");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
    }

    #[test]
    fn config_schema_version_should_reject_zero() {
        assert_eq!(
            WidgetConfigSchemaVersion::new(0),
            Err(InvalidWidgetConfigSchemaVersion)
        );
    }

    #[test]
    fn config_schema_version_should_support_const_static_construction() {
        const SCHEMA_V2: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::new_static(2);

        assert_eq!(SCHEMA_V2.get(), 2);
    }

    #[test]
    fn config_schema_version_should_reject_zero_when_decoded() {
        let error = serde_json::from_value::<WidgetConfigSchemaVersion>(json!(0))
            .expect_err("a zero schema version must be rejected while decoding");

        assert!(
            error
                .to_string()
                .contains("widget config schema version must be greater than zero")
        );
    }
}
