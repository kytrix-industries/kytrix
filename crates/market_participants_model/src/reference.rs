//! Canonical price and external reference terms.
//!
//! `PriceReferenceId` and `ExternalReferenceId` are derived from the reference
//! economic terms only: the underlying or external reference identity, the quote
//! asset, the price basis, the observation semantics, the aggregation, the
//! session and settlement conventions and the terms schema version. Oracle
//! endpoints, refresh rates, fallbacks, transports and adapter implementation
//! details are listing metadata and never take part in the identity.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    asset::{AssetId, UnderlyingRef},
    decimal::Ratio,
    error::{IndexField, ModelError},
    identifier::{
        CanonicalEncoder, CanonicalTerms, ExternalReferenceIdentity, IdentifierNamespace,
        IdentifierValue, PriceReferenceIdentity, ReferenceSourceId, ReferenceTermsVersion,
        TimeZoneId, TradingCalendarId,
    },
    listing::venue::PriceBasis,
};

crate::identifier::identity_type!(PriceReferenceId, PriceReferenceIdentity);
crate::identifier::identity_type!(ExternalReferenceId, ExternalReferenceIdentity);

/// The economic value semantics of a reference observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReferenceObservationKind {
    #[serde(rename = "mark")]
    Mark,
    #[serde(rename = "index")]
    Index,
    #[serde(rename = "fixing")]
    Fixing,
}

impl ReferenceObservationKind {
    /// Returns the fixed canonical label of this observation kind.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mark => "mark",
            Self::Index => "index",
            Self::Fixing => "fixing",
        }
    }
}

impl CanonicalTerms for ReferenceObservationKind {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// The source and value semantics of a reference observation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReferenceObservation {
    #[serde(rename = "source")]
    pub source: ReferenceSourceId,
    #[serde(rename = "kind")]
    pub kind: ReferenceObservationKind,
}

impl CanonicalTerms for ReferenceObservation {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.source.as_str());
        encoder.write_terms(&self.kind);
    }
}

/// One weighted component of a composite reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReferenceComponent {
    #[serde(rename = "underlying")]
    pub underlying: UnderlyingRef,
    #[serde(rename = "weight")]
    pub weight: Ratio,
}

impl CanonicalTerms for ReferenceComponent {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.underlying);
        encoder.write_terms(&self.weight);
    }
}

/// How a reference aggregates its components.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "aggregation")]
pub enum ReferenceAggregation {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "composite")]
    Composite {
        #[serde(rename = "components")]
        components: Vec<ReferenceComponent>,
    },
}

impl ReferenceAggregation {
    /// Validates the aggregation.
    ///
    /// A composite reference needs at least one component, every component
    /// weight must be positive, and the components must be sorted by their
    /// canonical underlying order without repeating an underlying.
    pub fn validate(&self) -> Result<(), ModelError> {
        let Self::Composite { components } = self else {
            return Ok(());
        };
        if components.is_empty() {
            return Err(ModelError::EmptyCollection {
                field: "reference.components",
            });
        }
        if components
            .iter()
            .any(|component| component.weight.is_zero())
        {
            return Err(ModelError::NonPositiveQuantity {
                field: "reference.component.weight",
            });
        }
        let underlyings: Vec<&UnderlyingRef> = components
            .iter()
            .map(|component| &component.underlying)
            .collect();
        if underlyings.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ModelError::UnsortedCollection {
                field: "reference.components",
            });
        }
        Ok(())
    }

    /// Returns the components of a composite reference.
    pub fn components(&self) -> &[ReferenceComponent] {
        match self {
            Self::Direct => &[],
            Self::Composite { components } => components,
        }
    }
}

impl CanonicalTerms for ReferenceAggregation {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Direct => encoder.write_label("direct"),
            Self::Composite { components } => {
                encoder.write_label("composite");
                encoder.write_terms_slice(components);
            }
        }
    }
}

/// The session and time convention of a reference observation.
///
/// A 24x7 crypto or prediction reference still states its convention explicitly;
/// a missing convention never means "always open".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "convention")]
pub enum ReferenceSessionConvention {
    #[serde(rename = "continuous")]
    Continuous,
    #[serde(rename = "trading_calendar")]
    TradingCalendar {
        #[serde(rename = "calendar")]
        calendar: TradingCalendarId,
        #[serde(rename = "timezone")]
        timezone: TimeZoneId,
    },
}

impl ReferenceSessionConvention {
    /// Returns the calendar identity of this convention.
    pub fn calendar(&self) -> SmolStr {
        match self {
            Self::Continuous => SmolStr::new_static("continuous"),
            Self::TradingCalendar { calendar, .. } => SmolStr::new(calendar.as_str()),
        }
    }
}

impl CanonicalTerms for ReferenceSessionConvention {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Continuous => encoder.write_label("continuous"),
            Self::TradingCalendar { calendar, timezone } => {
                encoder.write_label("trading-calendar");
                encoder.write_label(calendar.as_str());
                encoder.write_label(timezone.as_str());
            }
        }
    }
}

/// How a reference value becomes a settlement value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReferenceSettlementConvention {
    #[serde(rename = "continuous")]
    Continuous,
    #[serde(rename = "official_fixing")]
    OfficialFixing,
    #[serde(rename = "settlement_window")]
    SettlementWindow,
}

impl ReferenceSettlementConvention {
    /// Returns the fixed canonical label of this settlement convention.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Continuous => "continuous",
            Self::OfficialFixing => "official-fixing",
            Self::SettlementWindow => "settlement-window",
        }
    }

    /// Returns `true` when this convention observes an official fixing.
    pub const fn is_official_fixing(self) -> bool {
        matches!(self, Self::OfficialFixing)
    }
}

impl CanonicalTerms for ReferenceSettlementConvention {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// The canonical terms of an internal price reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct PriceReferenceTerms {
    #[serde(rename = "underlying")]
    pub underlying: UnderlyingRef,
    #[serde(rename = "quote_asset")]
    pub quote_asset: AssetId,
    #[serde(rename = "basis")]
    pub basis: PriceBasis,
    #[serde(rename = "observation")]
    pub observation: ReferenceObservation,
    #[serde(rename = "aggregation")]
    pub aggregation: ReferenceAggregation,
    #[serde(rename = "session")]
    pub session: ReferenceSessionConvention,
    #[serde(rename = "settlement")]
    pub settlement: ReferenceSettlementConvention,
    #[serde(rename = "schema_version")]
    pub schema_version: ReferenceTermsVersion,
}

impl PriceReferenceTerms {
    /// Validates the reference terms.
    ///
    /// An index underlying must be priced with a matching `PerIndexPoint` basis,
    /// because an index is not an asset.
    pub fn validate(&self) -> Result<(), ModelError> {
        if let UnderlyingRef::Index(index) = &self.underlying {
            match &self.basis {
                PriceBasis::PerIndexPoint { index: basis } if basis == index => {}
                PriceBasis::PerIndexPoint { .. } => {
                    return Err(ModelError::IndexMismatch {
                        field: IndexField::ReferenceBasis,
                    });
                }
                _ => return Err(ModelError::IncompatiblePriceBasis),
            }
        }
        self.aggregation.validate()
    }

    /// Derives the canonical price reference identity.
    pub fn reference_id(&self) -> Result<PriceReferenceId, ModelError> {
        self.validate()?;
        Ok(PriceReferenceId::of_terms(self))
    }
}

impl CanonicalTerms for PriceReferenceTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("price-reference");
        encoder.write_terms(&self.underlying);
        encoder.write_terms(&self.quote_asset);
        encoder.write_terms(&self.basis);
        encoder.write_terms(&self.observation);
        encoder.write_terms(&self.aggregation);
        encoder.write_terms(&self.session);
        encoder.write_terms(&self.settlement);
        encoder.write_label(self.schema_version.as_str());
    }
}

/// The canonical terms of an external reference such as a HIP-3 custom oracle.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ExternalReferenceTerms {
    #[serde(rename = "issuer")]
    pub issuer: IdentifierNamespace,
    #[serde(rename = "reference")]
    pub reference: IdentifierValue,
    #[serde(rename = "quote_asset")]
    pub quote_asset: AssetId,
    #[serde(rename = "basis")]
    pub basis: PriceBasis,
    #[serde(rename = "observation")]
    pub observation: ReferenceObservation,
    #[serde(rename = "aggregation")]
    pub aggregation: ReferenceAggregation,
    #[serde(rename = "session")]
    pub session: ReferenceSessionConvention,
    #[serde(rename = "settlement")]
    pub settlement: ReferenceSettlementConvention,
    #[serde(rename = "schema_version")]
    pub schema_version: ReferenceTermsVersion,
}

impl ExternalReferenceTerms {
    /// Validates the external reference terms.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.aggregation.validate()
    }

    /// Derives the canonical external reference identity.
    pub fn reference_id(&self) -> Result<ExternalReferenceId, ModelError> {
        self.validate()?;
        Ok(ExternalReferenceId::of_terms(self))
    }
}

impl CanonicalTerms for ExternalReferenceTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("external-reference");
        encoder.write_label(self.issuer.as_str());
        encoder.write_label(self.reference.as_str());
        encoder.write_terms(&self.quote_asset);
        encoder.write_terms(&self.basis);
        encoder.write_terms(&self.observation);
        encoder.write_terms(&self.aggregation);
        encoder.write_terms(&self.session);
        encoder.write_terms(&self.settlement);
        encoder.write_label(self.schema_version.as_str());
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        asset::{
            AssetId, CryptoTokenId, CryptoTokenOrigin, CurrencyId, IndexId, Iso4217Code,
            TokenReference, UnderlyingRef,
        },
        decimal::Ratio,
        error::{IndexField, ModelError},
        identifier::{
            CanonicalTerms, IdentifierNamespace, IdentifierValue, ReferenceSourceId,
            ReferenceTermsVersion, TimeZoneId, TokenIssuerId, TradingCalendarId,
        },
        listing::venue::PriceBasis,
        reference::{
            ExternalReferenceTerms, PriceReferenceId, PriceReferenceTerms, ReferenceAggregation,
            ReferenceComponent, ReferenceObservation, ReferenceObservationKind,
            ReferenceSessionConvention, ReferenceSettlementConvention,
        },
    };

    fn usd() -> AssetId {
        AssetId::Currency(CurrencyId::new(
            Iso4217Code::new("USD").expect("USD should be valid"),
        ))
    }

    fn token(code: &str) -> AssetId {
        AssetId::CryptoToken(
            CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
                token: TokenReference::Native {
                    code: IdentifierValue::new(code).expect("code should be valid"),
                },
            })
            .expect("token should be valid"),
        )
    }

    fn index(namespace: &str, code: &str) -> IndexId {
        IndexId::new(
            IdentifierNamespace::new(namespace).expect("namespace should be valid"),
            IdentifierValue::new(code).expect("code should be valid"),
        )
    }

    fn terms() -> PriceReferenceTerms {
        PriceReferenceTerms {
            underlying: UnderlyingRef::Asset(token("BTC")),
            quote_asset: usd(),
            basis: PriceBasis::PerAssetUnit,
            observation: ReferenceObservation {
                source: ReferenceSourceId::new("binance").expect("source should be valid"),
                kind: ReferenceObservationKind::Mark,
            },
            aggregation: ReferenceAggregation::Direct,
            session: ReferenceSessionConvention::Continuous,
            settlement: ReferenceSettlementConvention::Continuous,
            schema_version: ReferenceTermsVersion::new("v1").expect("version should be valid"),
        }
    }

    /// The canonical price reference terms are an identity, not a description, so
    /// the digest of one fixed reference is pinned: a change to the encoding or to
    /// the identity namespace moves this literal.
    #[test]
    fn price_reference_identity_should_be_a_pinned_digest() {
        assert_eq!(
            terms()
                .reference_id()
                .expect("reference should be valid")
                .as_str(),
            "v1/b6f91babcfd81ecefa15bca203b1c1b83c6626c217145b14c34140a4caee863e",
            "the full reference terms derive a pinned identity"
        );
    }

    #[test]
    fn price_reference_identity_should_be_stable_for_the_same_terms() {
        let first = terms().reference_id().expect("reference should be valid");
        let second = terms().reference_id().expect("reference should be valid");

        assert_eq!(first, second);
        assert!(first.as_str().starts_with("v1/"));
    }

    #[test]
    fn price_reference_identity_should_change_with_observation_semantics() {
        let original = terms().reference_id().expect("reference should be valid");
        let mut changed = terms();
        changed.observation.kind = ReferenceObservationKind::Index;

        assert_ne!(
            original,
            changed.reference_id().expect("reference should be valid")
        );
    }

    #[test]
    fn price_reference_identity_should_change_with_the_schema_version() {
        let original = terms().reference_id().expect("reference should be valid");
        let mut changed = terms();
        changed.schema_version = ReferenceTermsVersion::new("v2").expect("version should be valid");

        assert_ne!(
            original,
            changed.reference_id().expect("reference should be valid")
        );
    }

    #[test]
    fn index_reference_should_require_a_matching_per_index_point_basis() {
        let mut index_terms = terms();
        index_terms.underlying = UnderlyingRef::Index(index("sp", "500"));
        index_terms.basis = PriceBasis::PerAssetUnit;

        assert_eq!(
            index_terms.reference_id(),
            Err(ModelError::IncompatiblePriceBasis)
        );

        index_terms.basis = PriceBasis::PerIndexPoint {
            index: index("sp", "500"),
        };
        assert!(index_terms.reference_id().is_ok());

        index_terms.basis = PriceBasis::PerIndexPoint {
            index: index("sp", "100"),
        };
        assert_eq!(
            index_terms.reference_id(),
            Err(ModelError::IndexMismatch {
                field: IndexField::ReferenceBasis
            })
        );
    }

    #[test]
    fn composite_reference_should_require_sorted_unique_components() {
        let component = |code: &str, weight: &str| ReferenceComponent {
            underlying: UnderlyingRef::Asset(token(code)),
            weight: weight.parse::<Ratio>().expect("weight should parse"),
        };
        let mut composite = terms();
        composite.aggregation = ReferenceAggregation::Composite {
            components: vec![component("ETH", "0.5"), component("BTC", "0.5")],
        };

        assert_eq!(
            composite.reference_id(),
            Err(ModelError::UnsortedCollection {
                field: "reference.components"
            })
        );

        composite.aggregation = ReferenceAggregation::Composite {
            components: vec![component("BTC", "0.5"), component("ETH", "0.5")],
        };
        assert!(composite.reference_id().is_ok());

        composite.aggregation = ReferenceAggregation::Composite {
            components: vec![component("BTC", "0")],
        };
        assert_eq!(
            composite.reference_id(),
            Err(ModelError::NonPositiveQuantity {
                field: "reference.component.weight"
            })
        );
    }

    #[test]
    fn trading_calendar_session_should_encode_its_timezone() {
        let mut with_calendar = terms();
        with_calendar.session = ReferenceSessionConvention::TradingCalendar {
            calendar: TradingCalendarId::new("us-equity").expect("calendar should be valid"),
            timezone: TimeZoneId::new("America/New_York").expect("timezone should be valid"),
        };
        let first = with_calendar
            .reference_id()
            .expect("reference should be valid");
        with_calendar.session = ReferenceSessionConvention::TradingCalendar {
            calendar: TradingCalendarId::new("us-equity").expect("calendar should be valid"),
            timezone: TimeZoneId::new("America/Chicago").expect("timezone should be valid"),
        };

        assert_ne!(
            first,
            with_calendar
                .reference_id()
                .expect("reference should be valid")
        );
    }

    #[test]
    fn external_reference_should_be_distinct_from_a_price_reference() {
        let external = ExternalReferenceTerms {
            issuer: IdentifierNamespace::new("hyperliquid").expect("issuer should be valid"),
            reference: IdentifierValue::new("xyz-oracle").expect("reference should be valid"),
            quote_asset: usd(),
            basis: PriceBasis::PerAssetUnit,
            observation: ReferenceObservation {
                source: ReferenceSourceId::new("hyperliquid").expect("source should be valid"),
                kind: ReferenceObservationKind::Index,
            },
            aggregation: ReferenceAggregation::Direct,
            session: ReferenceSessionConvention::Continuous,
            settlement: ReferenceSettlementConvention::OfficialFixing,
            schema_version: ReferenceTermsVersion::new("v1").expect("version should be valid"),
        }
        .reference_id()
        .expect("reference should be valid");

        assert!(external.as_str().starts_with("v1/"));
        assert_ne!(
            external.as_str(),
            terms()
                .reference_id()
                .expect("reference should be valid")
                .as_str()
        );
    }

    #[test]
    fn reference_should_round_trip_through_serde() {
        let reference = terms().reference_id().expect("reference should be valid");
        let encoded = serde_json::to_string(&reference).expect("reference should serialize");
        let decoded =
            serde_json::from_str::<PriceReferenceId>(&encoded).expect("reference should decode");

        assert_eq!(decoded, reference);
    }

    #[test]
    fn observation_kind_labels_should_be_distinct() {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        ReferenceObservationKind::Mark.encode(&mut encoder);
        let mut other = crate::identifier::CanonicalEncoder::new();
        ReferenceObservationKind::Fixing.encode(&mut other);

        assert_ne!(encoder.as_bytes(), other.as_bytes());
    }

    #[test]
    fn reference_identities_should_reject_a_malformed_value() {
        assert!(PriceReferenceId::from_identity_value("v1/short").is_err());
        assert!(PriceReferenceId::from_identity_value("not-an-identity").is_err());
    }
}
