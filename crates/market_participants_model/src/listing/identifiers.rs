//! The typed native identifiers of one listing on one route.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    error::ModelError,
    identifier::{
        BoardId, CtfConditionId, CtfTokenId, EquityLocalCode, IdentifierValue, Mic, NativeSymbol,
        PolymarketEventId, PolymarketMarketId, VenueScopeId,
    },
    instrument::InstrumentKind,
    listing::{
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        route::VenueRoute,
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidOutcomeId, HyperliquidTokenId,
            OkxInstrumentType,
        },
    },
    outcome::OutcomeSideId,
    routing::ListingIdentifierKind,
};

/// The Binance listing identifiers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct BinanceListingIdentifiers {
    #[serde(rename = "product")]
    pub product: BinanceProduct,
    #[serde(rename = "future_series")]
    pub future_series: Option<FutureSeriesIdentifiers>,
    #[serde(rename = "option_series")]
    pub option_series: Option<OptionSeriesIdentifiers>,
}

impl BinanceListingIdentifiers {
    /// Validates the product and series combination.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.future_series.is_some() && self.product != BinanceProduct::Future {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "binance future series on a non-future product",
            });
        }
        if self.option_series.is_some() && self.product != BinanceProduct::Option {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "binance option series on a non-option product",
            });
        }
        if let Some(series) = &self.future_series {
            series.validate()?;
        }
        if let Some(series) = &self.option_series {
            series.validate()?;
        }
        Ok(())
    }

    /// Returns the instrument kind this identifier must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        self.product.instrument_kind()
    }
}

#[derive(Deserialize)]
struct BinanceListingIdentifiersWire {
    #[serde(rename = "product")]
    product: BinanceProduct,
    #[serde(rename = "future_series")]
    future_series: Option<FutureSeriesIdentifiers>,
    #[serde(rename = "option_series")]
    option_series: Option<OptionSeriesIdentifiers>,
}

impl<'de> Deserialize<'de> for BinanceListingIdentifiers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = BinanceListingIdentifiersWire::deserialize(deserializer)?;
        let identifiers = Self {
            product: wire.product,
            future_series: wire.future_series,
            option_series: wire.option_series,
        };
        identifiers.validate().map_err(serde::de::Error::custom)?;
        Ok(identifiers)
    }
}

/// The OKX listing identifiers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OkxListingIdentifiers {
    #[serde(rename = "instrument_type")]
    pub instrument_type: OkxInstrumentType,
    /// The venue's own instrument family (`instFamily`).
    ///
    /// A perpetual is margined and levelled by family — the venue publishes its
    /// position tiers for a family rather than for one contract — so a perpetual
    /// carries the family it belongs to as part of its identity.
    #[serde(rename = "instrument_family")]
    pub instrument_family: Option<NativeSymbol>,
    #[serde(rename = "future_series")]
    pub future_series: Option<FutureSeriesIdentifiers>,
    #[serde(rename = "option_series")]
    pub option_series: Option<OptionSeriesIdentifiers>,
}

impl OkxListingIdentifiers {
    /// Validates the instrument type, the family and the series combination.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.instrument_family.is_none() && self.instrument_type == OkxInstrumentType::Perpetual
        {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "okx perpetual without an instrument family",
            });
        }
        if self.future_series.is_some() && self.instrument_type != OkxInstrumentType::Future {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "okx future series on a non-future instrument type",
            });
        }
        if self.option_series.is_some() && self.instrument_type != OkxInstrumentType::Option {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "okx option series on a non-option instrument type",
            });
        }
        if let Some(series) = &self.future_series {
            series.validate()?;
        }
        if let Some(series) = &self.option_series {
            series.validate()?;
        }
        Ok(())
    }

    /// Returns the instrument kind this identifier must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        self.instrument_type.instrument_kind()
    }
}

#[derive(Deserialize)]
struct OkxListingIdentifiersWire {
    #[serde(rename = "instrument_type")]
    instrument_type: OkxInstrumentType,
    #[serde(rename = "instrument_family")]
    instrument_family: Option<NativeSymbol>,
    #[serde(rename = "future_series")]
    future_series: Option<FutureSeriesIdentifiers>,
    #[serde(rename = "option_series")]
    option_series: Option<OptionSeriesIdentifiers>,
}

impl<'de> Deserialize<'de> for OkxListingIdentifiers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OkxListingIdentifiersWire::deserialize(deserializer)?;
        let identifiers = Self {
            instrument_type: wire.instrument_type,
            instrument_family: wire.instrument_family,
            future_series: wire.future_series,
            option_series: wire.option_series,
        };
        identifiers.validate().map_err(serde::de::Error::custom)?;
        Ok(identifiers)
    }
}

/// The Hyperliquid typed listing metadata.
///
/// `dex` and `venue` echo the protocol scope: they must equal the route scope
/// and never become a second route identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifiers")]
pub enum HyperliquidListingIdentifiers {
    #[serde(rename = "spot")]
    Spot {
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
        #[serde(rename = "base_token_id")]
        base_token_id: HyperliquidTokenId,
        #[serde(rename = "quote_token_id")]
        quote_token_id: HyperliquidTokenId,
    },
    #[serde(rename = "perpetual")]
    Perpetual {
        #[serde(rename = "dex")]
        dex: VenueScopeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
        #[serde(rename = "coin")]
        coin: NativeSymbol,
    },
    #[serde(rename = "outcome")]
    Outcome {
        #[serde(rename = "venue")]
        venue: VenueScopeId,
        #[serde(rename = "outcome_id")]
        outcome_id: HyperliquidOutcomeId,
        #[serde(rename = "side")]
        side: OutcomeSideId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
        #[serde(rename = "coin")]
        coin: Option<NativeSymbol>,
        #[serde(rename = "token_name")]
        token_name: IdentifierValue,
    },
}

impl HyperliquidListingIdentifiers {
    /// Validates the echoed scope against the route and the required metadata.
    pub fn validate(&self, route: &VenueRoute) -> Result<(), ModelError> {
        match self {
            Self::Spot { .. } => Ok(()),
            Self::Perpetual { dex, coin, .. } => {
                if dex != &route.scope {
                    return Err(ModelError::InvalidListingContinuityTerms {
                        field: "hyperliquid perpetual scope does not match the route",
                    });
                }
                if coin.as_str().is_empty() {
                    return Err(ModelError::InvalidListingContinuityTerms {
                        field: "hyperliquid perpetual without a coin",
                    });
                }
                Ok(())
            }
            Self::Outcome {
                venue,
                coin,
                token_name,
                ..
            } => {
                if venue != &route.scope {
                    return Err(ModelError::InvalidListingContinuityTerms {
                        field: "hyperliquid outcome venue does not match the route",
                    });
                }
                if coin.is_none() && token_name.as_str().is_empty() {
                    return Err(ModelError::InvalidListingContinuityTerms {
                        field: "hyperliquid outcome without a coin or token name",
                    });
                }
                Ok(())
            }
        }
    }

    /// Returns the route scope this identifier claims, when it echoes one.
    pub fn scope(&self) -> Option<&VenueScopeId> {
        match self {
            Self::Spot { .. } => None,
            Self::Perpetual { dex, .. } => Some(dex),
            Self::Outcome { venue, .. } => Some(venue),
        }
    }

    /// Returns the instrument kind this identifier must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        match self {
            Self::Spot { .. } => InstrumentKind::Spot,
            Self::Perpetual { .. } => InstrumentKind::Perpetual,
            Self::Outcome { .. } => InstrumentKind::Outcome,
        }
    }
}

/// The Polymarket typed listing metadata.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PolymarketListingIdentifiers {
    #[serde(rename = "event_id")]
    pub event_id: Option<PolymarketEventId>,
    #[serde(rename = "market_id")]
    pub market_id: PolymarketMarketId,
    #[serde(rename = "condition_id")]
    pub condition_id: CtfConditionId,
    #[serde(rename = "clob_token_id")]
    pub clob_token_id: CtfTokenId,
    #[serde(rename = "outcome_label")]
    pub outcome_label: SmolStr,
}

impl PolymarketListingIdentifiers {
    /// Validates the required CTF identifiers and the outcome label.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.condition_id.as_str().is_empty() || self.clob_token_id.as_str().is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "polymarket listing without a condition or token",
            });
        }
        if self.market_id.as_str().is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "polymarket listing without a market id",
            });
        }
        if self.outcome_label.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "polymarket listing without an outcome label",
            });
        }
        Ok(())
    }

    /// Returns the instrument kind this identifier must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        InstrumentKind::Outcome
    }
}

/// The equity listing metadata of one market segment.
///
/// The segment is named by its ISO 10383 market identifier code, which is the
/// market the listing is quoted on: a venue identity is not part of it, because
/// the venue of the *route* is the venue this build addresses the listing at,
/// and the venue the shares actually trade on is what the MIC says.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EquityListingIdentifiers {
    #[serde(rename = "mic")]
    pub mic: Mic,
    #[serde(rename = "board")]
    pub board: Option<BoardId>,
    #[serde(rename = "ticker")]
    pub ticker: SmolStr,
    #[serde(rename = "local_code")]
    pub local_code: EquityLocalCode,
}

impl EquityListingIdentifiers {
    /// Validates the displayed ticker and the local code.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.ticker.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "equity listing without a ticker",
            });
        }
        if self.local_code.as_str().is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "equity listing without a local code",
            });
        }
        Ok(())
    }

    /// Returns the instrument kind this identifier must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        InstrumentKind::Spot
    }
}

/// The outcome of comparing a listing's typed identifiers with its native symbols.
///
/// The status is a closed enum because the three cases lead to different
/// publication decisions: a contradicted echo refuses the listing, a missing
/// comparison keeps it publishable, and only an exact echo means the typed
/// identifier and the native symbols agree. It describes that relationship and
/// nothing more — in particular, a symbol that echoes a typed identifier stays a
/// search alias and never becomes the name a listing is drawn by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EchoValidationStatus {
    /// Every comparable typed identifier is echoed by a declared symbol.
    Verified,
    /// The listing has no typed identifier with a native display symbol, or it
    /// declares no display symbol to compare against, so no echo exists.
    NoComparableIdentifier,
    /// A typed identifier names a value none of the declared symbols carries.
    Mismatch {
        /// The identifier kind whose echo disagreed.
        kind: ListingIdentifierKind,
        /// The typed value that was not echoed.
        value: SmolStr,
    },
}

/// The authoritative protocol identifiers of one listing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifiers")]
pub enum VenueListingIdentifiers {
    #[serde(rename = "binance")]
    Binance(BinanceListingIdentifiers),
    #[serde(rename = "okx")]
    Okx(OkxListingIdentifiers),
    #[serde(rename = "hyperliquid")]
    Hyperliquid(HyperliquidListingIdentifiers),
    #[serde(rename = "polymarket")]
    Polymarket(PolymarketListingIdentifiers),
    #[serde(rename = "equity")]
    Equity(EquityListingIdentifiers),
    #[serde(rename = "future_series")]
    FutureSeries(FutureSeriesIdentifiers),
    #[serde(rename = "option_series")]
    OptionSeries(OptionSeriesIdentifiers),
}

impl VenueListingIdentifiers {
    /// Validates the product metadata and its route-specific requirements.
    pub fn validate(&self, route: &VenueRoute) -> Result<(), ModelError> {
        match self {
            Self::Binance(identifiers) => identifiers.validate(),
            Self::Okx(identifiers) => identifiers.validate(),
            Self::Hyperliquid(identifiers) => identifiers.validate(route),
            Self::Polymarket(identifiers) => identifiers.validate(),
            Self::Equity(identifiers) => identifiers.validate(),
            Self::FutureSeries(series) => series.validate(),
            Self::OptionSeries(series) => series.validate(),
        }
    }

    /// Validates this identifier against the route and the listed kind.
    pub fn validate_for(&self, route: &VenueRoute, kind: InstrumentKind) -> Result<(), ModelError> {
        self.validate(route)?;
        if self.instrument_kind() == kind {
            return Ok(());
        }
        Err(ModelError::InvalidListingContinuityTerms {
            field: "listing identifiers do not match the instrument kind",
        })
    }

    /// Returns the instrument kind these identifiers must resolve to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        match self {
            Self::Binance(identifiers) => identifiers.instrument_kind(),
            Self::Okx(identifiers) => identifiers.instrument_kind(),
            Self::Hyperliquid(identifiers) => identifiers.instrument_kind(),
            Self::Polymarket(_) => InstrumentKind::Outcome,
            Self::Equity(_) => InstrumentKind::Spot,
            Self::FutureSeries(_) => InstrumentKind::Future,
            Self::OptionSeries(_) => InstrumentKind::Option,
        }
    }

    /// Returns the primary identifier kind of this variant.
    pub const fn primary_kind(&self) -> ListingIdentifierKind {
        match self {
            Self::Binance(_) => ListingIdentifierKind::BinanceInstrument,
            Self::Okx(_) => ListingIdentifierKind::OkxInstrument,
            Self::Hyperliquid(HyperliquidListingIdentifiers::Spot { .. })
            | Self::Hyperliquid(HyperliquidListingIdentifiers::Perpetual { .. }) => {
                ListingIdentifierKind::HyperliquidAsset
            }
            Self::Hyperliquid(HyperliquidListingIdentifiers::Outcome { .. }) => {
                ListingIdentifierKind::HyperliquidOutcome
            }
            Self::Polymarket(_) => ListingIdentifierKind::PolymarketToken,
            Self::Equity(_) => ListingIdentifierKind::EquityLocalCode,
            Self::FutureSeries(_) => ListingIdentifierKind::FutureSeries,
            Self::OptionSeries(_) => ListingIdentifierKind::OccSymbol,
        }
    }
}
