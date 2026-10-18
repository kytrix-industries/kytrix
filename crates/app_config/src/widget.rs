//! Persisted widget configuration.
//!
//! A widget owns its own configuration type, but the envelope that type is
//! persisted in and the schema version guarding it belong to the configuration
//! vocabulary: a pane's layout stores an envelope, not a widget type. Keeping
//! both here is what lets the layout describe which widget a pane shows without
//! the configuration model depending on `widget`.
//!
//! Reading a widget's persisted config never fails. An envelope that was written
//! under another schema version, or whose payload no longer decodes, yields the
//! widget's default configuration; whether that replaced what was stored is
//! answered by comparing the two envelopes, not by an error. There is no migration
//! between schema versions: a version this build does not support has no meaning
//! here, and inventing one would be the compatibility layer this project does not
//! keep.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Persistable config envelope independent of a concrete widget's config type.
///
/// The payload is the widget's own serialized config, and for every
/// listing-scoped widget that config holds canonical identities: a
/// `VenueListingKey` and its `InstrumentSelection`, a `WatchListKey` entry, a
/// `ServiceTarget` or a link group. Those identities are versioned by
/// `market_participants_model`'s frozen identity encoding, so an incompatible change to that
/// encoding makes a stored payload undecodable even though this envelope's own
/// `schema_version` did not move. That is handled by the current V1 rule and
/// nothing else: the whole config is dropped, the widget's default is restored,
/// and the default is written back to the layout. There is no migration of old
/// identifiers, no fallback to a previous shape and no dual-version parsing —
/// see [`VersionedWidgetConfig::decode`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VersionedWidgetConfig {
    schema_version: WidgetConfigSchemaVersion,
    payload: String,
}

impl VersionedWidgetConfig {
    /// Wraps one concrete configuration in its versioned envelope.
    pub fn encode<C: WidgetConfig>(config: &C) -> Result<Self, WidgetConfigError> {
        Ok(Self {
            schema_version: C::SCHEMA_VERSION,
            payload: serde_json::to_string(config).map_err(WidgetConfigError)?,
        })
    }

    /// Returns the schema version this envelope was written with.
    pub const fn schema_version(&self) -> WidgetConfigSchemaVersion {
        self.schema_version
    }

    /// Decodes the payload into the widget's current configuration type.
    ///
    /// A schema version this build does not support and a payload this build
    /// cannot read both yield `C::default()`: the widget is always creatable, and
    /// the caller learns that its stored config was replaced by comparing this
    /// envelope with the one the widget now reports.
    ///
    /// "Cannot read" covers the identity case as well as the shape case. A
    /// payload written before an incompatible canonical-identity change names a
    /// listing or an instrument selection this build has no identity for, so it
    /// fails to decode exactly like a payload of the wrong shape; the widget then
    /// starts from its defaults and writes them back. Resetting the whole config
    /// — not the offending entry alone — is the accepted cost of keeping one
    /// decode rule, and it is deliberately preferred to migrating old identifiers
    /// or parsing two identity versions at once.
    pub fn decode<C: WidgetConfig>(self) -> C {
        if self.schema_version != C::SCHEMA_VERSION {
            return C::default();
        }

        serde_json::from_str(&self.payload).unwrap_or_default()
    }
}

/// Versioned persisted configuration owned by a concrete widget.
pub trait WidgetConfig: Clone + Default + Serialize + DeserializeOwned + 'static {
    /// The persisted schema version this widget's config is written under.
    ///
    /// Every widget in the workspace is at `V1`, and `V1` is not a compatibility
    /// window: it is the only shape this build reads. A widget whose persisted
    /// fields change incompatibly deletes the old structure and keeps `V1`, and a
    /// canonical-identity change in `market_participants_model` needs no version bump here at
    /// all — the old payload simply stops decoding, and
    /// [`VersionedWidgetConfig::decode`] resets the whole config to its default and
    /// writes it back. No old-identifier migration, fallback or dual-version
    /// parsing is provided by this trait or by any widget implementing it.
    const SCHEMA_VERSION: WidgetConfigSchemaVersion;
}

/// The config of a widget that persists no options.
///
/// Such a widget still needs a config type, so the persisted schema version is
/// owned by the widget rather than inferred from the absence of a config. It is
/// `V1` for the same reason every other widget is: this build reads one shape, and
/// a payload it cannot read becomes the default and is written back rather than
/// migrated.
impl WidgetConfig for () {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A widget configuration could not be written out.
///
/// Only encoding can fail: a configuration that cannot be turned into JSON is a
/// configuration that cannot be persisted at all, and the caller has to decide
/// what to write in its place. Reading never fails — see
/// [`VersionedWidgetConfig::decode`].
#[derive(Debug, thiserror::Error)]
#[error("failed to encode widget config: {0}")]
pub struct WidgetConfigError(#[source] serde_json::Error);

/// Decodes an optional envelope, where an absent envelope means "defaults".
pub fn decode_or_default<C: WidgetConfig>(config: Option<VersionedWidgetConfig>) -> C {
    config.map_or_else(C::default, VersionedWidgetConfig::decode)
}

/// A monotonically increasing schema version for one widget's persisted config.
///
/// This is deliberately separate from application and widget implementation
/// versions. Increment it only when the persisted config schema changes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WidgetConfigSchemaVersion(u32);

impl WidgetConfigSchemaVersion {
    /// The first persisted config schema version.
    pub const V1: Self = Self(1);

    /// Constructs a schema version whose value is known at compile time.
    ///
    /// # Panics
    ///
    /// Panics when `value` is zero.
    #[track_caller]
    pub const fn new_static(value: u32) -> Self {
        if value == 0 {
            panic!("widget config schema version must be greater than zero");
        }
        Self(value)
    }

    /// Constructs a non-zero config schema version.
    pub const fn new(value: u32) -> Result<Self, InvalidWidgetConfigSchemaVersion> {
        if value == 0 {
            Err(InvalidWidgetConfigSchemaVersion)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the numeric schema version.
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

/// A zero config schema version is invalid.
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

    /// Stands in for a listing-scoped widget's config: the selected listing is a
    /// required field, so a payload that names a route and an instrument, or a
    /// selection without its listing key, cannot decode into it at all.
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
