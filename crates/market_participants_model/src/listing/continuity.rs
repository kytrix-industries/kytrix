//! The continuity terms that prove two listing publications are the same
//! listing, and the canonical identity derived from them.

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    asset::{SecurityIssuerId, ShareClassId},
    error::ModelError,
    identifier::{
        CanonicalEncoder, CanonicalTerms, CtfConditionId, CtfTokenId, EquityLocalCode, Mic,
        NativeSymbol, VenueScopeId,
    },
    listing::{
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        route::{VenueListingKey, VenueRoute},
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidOutcomeId,
            OkxInstrumentType, VenueListingId,
        },
    },
};

/// The listing continuity terms that determine a [`VenueListingId`].
///
/// The terms contain only the typed native identity that proves the continuity
/// of one listing. Display aliases, snapshots, rules, status and array positions
/// never appear here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "terms")]
pub enum ListingContinuityTerms {
    #[serde(rename = "binance")]
    Binance {
        #[serde(rename = "product")]
        product: BinanceProduct,
        #[serde(rename = "instrument_id")]
        instrument_id: NativeSymbol,
    },
    #[serde(rename = "okx")]
    Okx {
        #[serde(rename = "instrument_id")]
        instrument_id: NativeSymbol,
        #[serde(rename = "instrument_type")]
        instrument_type: OkxInstrumentType,
    },
    #[serde(rename = "hyperliquid_asset")]
    HyperliquidAsset {
        #[serde(rename = "product")]
        product: HyperliquidAssetProduct,
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome {
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "outcome_id")]
        outcome_id: HyperliquidOutcomeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "polymarket")]
    Polymarket {
        #[serde(rename = "condition_id")]
        condition_id: CtfConditionId,
        #[serde(rename = "clob_token_id")]
        clob_token_id: CtfTokenId,
    },
    #[serde(rename = "equity")]
    Equity {
        #[serde(rename = "issuer")]
        issuer: SecurityIssuerId,
        #[serde(rename = "share_class")]
        share_class: ShareClassId,
        #[serde(rename = "mic")]
        mic: Mic,
        #[serde(rename = "local_code")]
        local_code: EquityLocalCode,
    },
    #[serde(rename = "future_series")]
    FutureSeries(FutureSeriesIdentifiers),
    #[serde(rename = "option_series")]
    OptionSeries(OptionSeriesIdentifiers),
}

impl ListingContinuityTerms {
    /// Validates the continuity terms of this variant.
    ///
    /// Every variant must determine exactly one listing inside one route.
    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::Binance { .. } | Self::Okx { .. } => Ok(()),
            Self::HyperliquidAsset { .. } | Self::HyperliquidOutcome { .. } => Ok(()),
            Self::Polymarket {
                condition_id,
                clob_token_id,
            } => {
                if condition_id.as_str().is_empty() || clob_token_id.as_str().is_empty() {
                    return Err(ModelError::InvalidListingContinuityTerms {
                        field: "polymarket identifier without a condition or token",
                    });
                }
                Ok(())
            }
            Self::Equity { .. } => Ok(()),
            Self::FutureSeries(series) => series.validate(),
            Self::OptionSeries(series) => series.validate(),
        }
    }

    /// Returns the fixed canonical label of this continuity variant.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Binance { .. } => "binance",
            Self::Okx { .. } => "okx",
            Self::HyperliquidAsset { .. } => "hyperliquid-asset",
            Self::HyperliquidOutcome { .. } => "hyperliquid-outcome",
            Self::Polymarket { .. } => "polymarket",
            Self::Equity { .. } => "equity",
            Self::FutureSeries(_) => "future-series",
            Self::OptionSeries(_) => "option-series",
        }
    }
}

impl CanonicalTerms for ListingContinuityTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
        match self {
            Self::Binance {
                product,
                instrument_id,
            } => {
                encoder.write_label(product.label());
                encoder.write_label(instrument_id.as_str());
            }
            Self::Okx {
                instrument_id,
                instrument_type,
            } => {
                encoder.write_label(instrument_type.label());
                encoder.write_label(instrument_id.as_str());
            }
            Self::HyperliquidAsset {
                product,
                scope,
                asset_id,
            } => {
                encoder.write_label(product.label());
                encoder.write_label(scope.as_str());
                encoder.write_u64(u64::from(asset_id.0));
            }
            Self::HyperliquidOutcome {
                scope,
                outcome_id,
                asset_id,
            } => {
                encoder.write_label(scope.as_str());
                encoder.write_u64(outcome_id.0);
                encoder.write_u64(u64::from(asset_id.0));
            }
            Self::Polymarket {
                condition_id,
                clob_token_id,
            } => {
                encoder.write_label(condition_id.as_str());
                encoder.write_label(clob_token_id.as_str());
            }
            Self::Equity {
                issuer,
                share_class,
                mic,
                local_code,
            } => {
                encoder.write_label(issuer.as_str());
                encoder.write_label(share_class.as_str());
                encoder.write_label(mic.as_str());
                encoder.write_label(local_code.as_str());
            }
            Self::FutureSeries(series) => {
                encoder.write_optional(
                    series.market_participant_series_id.as_ref(),
                    |encoder, value| {
                        encoder.write_label(value.as_str());
                    },
                );
                encoder.write_optional(series.local_symbol.as_ref(), |encoder, value| {
                    encoder.write_label(value.as_str());
                });
                encoder.write_optional(series.contract_code.as_ref(), |encoder, value| {
                    encoder.write_label(value.as_str());
                });
            }
            Self::OptionSeries(series) => {
                encoder.write_optional(series.occ_symbol.as_ref(), |encoder, value| {
                    encoder.write_label(value.as_str());
                });
                encoder.write_optional(
                    series.market_participant_series_id.as_ref(),
                    |encoder, value| {
                        encoder.write_label(value.as_str());
                    },
                );
                encoder.write_optional(series.local_symbol.as_ref(), |encoder, value| {
                    encoder.write_label(value.as_str());
                });
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "terms")]
enum ListingContinuityTermsWire {
    #[serde(rename = "binance")]
    Binance {
        #[serde(rename = "product")]
        product: BinanceProduct,
        #[serde(rename = "instrument_id")]
        instrument_id: NativeSymbol,
    },
    #[serde(rename = "okx")]
    Okx {
        #[serde(rename = "instrument_id")]
        instrument_id: NativeSymbol,
        #[serde(rename = "instrument_type")]
        instrument_type: OkxInstrumentType,
    },
    #[serde(rename = "hyperliquid_asset")]
    HyperliquidAsset {
        #[serde(rename = "product")]
        product: HyperliquidAssetProduct,
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome {
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "outcome_id")]
        outcome_id: HyperliquidOutcomeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "polymarket")]
    Polymarket {
        #[serde(rename = "condition_id")]
        condition_id: CtfConditionId,
        #[serde(rename = "clob_token_id")]
        clob_token_id: CtfTokenId,
    },
    #[serde(rename = "equity")]
    Equity {
        #[serde(rename = "issuer")]
        issuer: SecurityIssuerId,
        #[serde(rename = "share_class")]
        share_class: ShareClassId,
        #[serde(rename = "mic")]
        mic: Mic,
        #[serde(rename = "local_code")]
        local_code: EquityLocalCode,
    },
    #[serde(rename = "future_series")]
    FutureSeries(FutureSeriesIdentifiers),
    #[serde(rename = "option_series")]
    OptionSeries(OptionSeriesIdentifiers),
}

impl From<ListingContinuityTermsWire> for ListingContinuityTerms {
    fn from(wire: ListingContinuityTermsWire) -> Self {
        match wire {
            ListingContinuityTermsWire::Binance {
                product,
                instrument_id,
            } => Self::Binance {
                product,
                instrument_id,
            },
            ListingContinuityTermsWire::Okx {
                instrument_id,
                instrument_type,
            } => Self::Okx {
                instrument_id,
                instrument_type,
            },
            ListingContinuityTermsWire::HyperliquidAsset {
                product,
                scope,
                asset_id,
            } => Self::HyperliquidAsset {
                product,
                scope,
                asset_id,
            },
            ListingContinuityTermsWire::HyperliquidOutcome {
                scope,
                outcome_id,
                asset_id,
            } => Self::HyperliquidOutcome {
                scope,
                outcome_id,
                asset_id,
            },
            ListingContinuityTermsWire::Polymarket {
                condition_id,
                clob_token_id,
            } => Self::Polymarket {
                condition_id,
                clob_token_id,
            },
            ListingContinuityTermsWire::Equity {
                issuer,
                share_class,
                mic,
                local_code,
            } => Self::Equity {
                issuer,
                share_class,
                mic,
                local_code,
            },
            ListingContinuityTermsWire::FutureSeries(series) => Self::FutureSeries(series),
            ListingContinuityTermsWire::OptionSeries(series) => Self::OptionSeries(series),
        }
    }
}

impl<'de> Deserialize<'de> for ListingContinuityTerms {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let terms = Self::from(ListingContinuityTermsWire::deserialize(deserializer)?);
        terms.validate().map_err(de::Error::custom)?;
        Ok(terms)
    }
}

/// The continuity key of one listing: its route and native identity terms.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ListingContinuityKey {
    #[serde(rename = "route")]
    pub route: VenueRoute,
    #[serde(rename = "terms")]
    pub terms: ListingContinuityTerms,
}

/// The canonical terms of a venue listing continuity key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
struct VenueListingTerms {
    #[serde(rename = "route")]
    route: VenueRoute,
    #[serde(rename = "terms")]
    terms: ListingContinuityTerms,
}

impl CanonicalTerms for VenueListingTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("venue-listing");
        encoder.write_label(self.route.venue.as_str());
        encoder.write_label(self.route.scope.as_str());
        encoder.write_terms(&self.terms);
    }
}

impl ListingContinuityKey {
    /// Validates the continuity terms and derives the listing identity.
    pub fn listing_id(&self) -> Result<VenueListingId, ModelError> {
        self.terms.validate()?;
        Ok(VenueListingId::of_terms(&VenueListingTerms {
            route: self.route.clone(),
            terms: self.terms.clone(),
        }))
    }

    /// Validates the continuity terms and derives the full listing key.
    pub fn listing_key(&self) -> Result<VenueListingKey, ModelError> {
        Ok(VenueListingKey {
            route: self.route.clone(),
            listing_id: self.listing_id()?,
        })
    }
}

#[derive(Deserialize)]
struct ListingContinuityKeyWire {
    #[serde(rename = "route")]
    route: VenueRoute,
    #[serde(rename = "terms")]
    terms: ListingContinuityTerms,
}

impl<'de> Deserialize<'de> for ListingContinuityKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ListingContinuityKeyWire::deserialize(deserializer)?;
        Ok(Self {
            route: wire.route,
            terms: wire.terms,
        })
    }
}
