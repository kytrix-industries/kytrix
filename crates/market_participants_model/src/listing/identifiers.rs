use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    error::ModelError,
    identifier::{
        BoardId, CtfConditionId, EquityLocalCode, IdentifierValue, Mic, NativeSymbol,
        PolymarketEventId, PolymarketMarketId, VenueScopeId,
    },
    instrument::InstrumentKind,
    listing::{
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        route::VenueRoute,
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidOutcomeId, HyperliquidTokenId,
            OkxInstrumentType, PolymarketAssetId, PolymarketProtocolVersion,
        },
    },
    outcome::OutcomeSideId,
    routing::ListingIdentifierKind,
};

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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OkxListingIdentifiers {
    #[serde(rename = "instrument_type")]
    pub instrument_type: OkxInstrumentType,
    #[serde(rename = "instrument_family")]
    pub instrument_family: Option<NativeSymbol>,
    #[serde(rename = "future_series")]
    pub future_series: Option<FutureSeriesIdentifiers>,
    #[serde(rename = "option_series")]
    pub option_series: Option<OptionSeriesIdentifiers>,
}

impl OkxListingIdentifiers {
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

    pub fn scope(&self) -> Option<&VenueScopeId> {
        match self {
            Self::Spot { .. } => None,
            Self::Perpetual { dex, .. } => Some(dex),
            Self::Outcome { venue, .. } => Some(venue),
        }
    }

    pub const fn instrument_kind(&self) -> InstrumentKind {
        match self {
            Self::Spot { .. } => InstrumentKind::Spot,
            Self::Perpetual { .. } => InstrumentKind::Perpetual,
            Self::Outcome { .. } => InstrumentKind::Outcome,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PolymarketListingIdentifiers {
    #[serde(rename = "event_id")]
    pub event_id: Option<PolymarketEventId>,
    #[serde(rename = "market_id")]
    pub market_id: PolymarketMarketId,
    #[serde(rename = "condition_id")]
    pub condition_id: CtfConditionId,
    #[serde(rename = "asset_id")]
    pub asset_id: PolymarketAssetId,
    #[serde(rename = "protocol")]
    pub protocol: PolymarketProtocolVersion,
    #[serde(rename = "outcome_label")]
    pub outcome_label: SmolStr,
}

impl PolymarketListingIdentifiers {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.condition_id.as_str().is_empty() || self.asset_id.as_str().is_empty() {
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

    pub const fn instrument_kind(&self) -> InstrumentKind {
        InstrumentKind::Outcome
    }
}

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

    pub const fn instrument_kind(&self) -> InstrumentKind {
        InstrumentKind::Spot
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EchoValidationStatus {
    Verified,
    NoComparableIdentifier,
    Mismatch {
        kind: ListingIdentifierKind,
        value: SmolStr,
    },
}

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

    pub fn validate_for(&self, route: &VenueRoute, kind: InstrumentKind) -> Result<(), ModelError> {
        self.validate(route)?;
        if self.instrument_kind() == kind {
            return Ok(());
        }
        Err(ModelError::InvalidListingContinuityTerms {
            field: "listing identifiers do not match the instrument kind",
        })
    }

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
