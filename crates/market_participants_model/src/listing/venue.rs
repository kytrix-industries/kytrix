//! The venue listing identity and the venue-issued value types that never take
//! part in the economic instrument identity.

use serde::{Deserialize, Serialize};

use crate::{
    asset::IndexId,
    identifier::{CanonicalEncoder, CanonicalTerms, VenueListingIdentity},
    instrument::InstrumentKind,
};

crate::identity_type!(VenueListingId, VenueListingIdentity);

/// The Hyperliquid asset index of a spot or perpetual coin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidAssetId(pub u32);

/// The Hyperliquid spot token index of a balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidTokenId(pub u32);

/// The Hyperliquid HIP-4 outcome index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidOutcomeId(pub u64);

/// The Hyperliquid product family of an asset identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HyperliquidAssetProduct {
    #[serde(rename = "spot")]
    Spot,
    #[serde(rename = "perpetual")]
    Perpetual,
}

impl HyperliquidAssetProduct {
    /// Returns the fixed canonical label of this product.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
        }
    }

    /// Returns the instrument kind an instrument of this product must have.
    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
        }
    }
}

/// The Binance product family of an instrument identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BinanceProduct {
    #[serde(rename = "spot")]
    Spot,
    #[serde(rename = "perpetual")]
    Perpetual,
    #[serde(rename = "future")]
    Future,
    #[serde(rename = "option")]
    Option,
}

impl BinanceProduct {
    /// Returns the fixed canonical label of this product.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
        }
    }

    /// Returns the instrument kind an instrument of this product must have.
    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
            Self::Future => InstrumentKind::Future,
            Self::Option => InstrumentKind::Option,
        }
    }
}

/// The OKX instrument type of an instrument identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OkxInstrumentType {
    #[serde(rename = "spot")]
    Spot,
    #[serde(rename = "perpetual")]
    Perpetual,
    #[serde(rename = "future")]
    Future,
    #[serde(rename = "option")]
    Option,
}

impl OkxInstrumentType {
    /// Returns the fixed canonical label of this instrument type.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
        }
    }

    /// Returns the instrument kind an instrument of this type must have.
    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
            Self::Future => InstrumentKind::Future,
            Self::Option => InstrumentKind::Option,
        }
    }
}

/// How a numeric price is expressed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "basis")]
pub enum PriceBasis {
    #[serde(rename = "per_asset_unit")]
    PerAssetUnit,
    #[serde(rename = "per_contract")]
    PerContract,
    #[serde(rename = "per_index_point")]
    PerIndexPoint {
        #[serde(rename = "index")]
        index: IndexId,
    },
    #[serde(rename = "fraction_of_collateral")]
    FractionOfCollateral,
}

impl PriceBasis {
    /// Returns the fixed canonical label of this basis.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::PerAssetUnit => "per-asset-unit",
            Self::PerContract => "per-contract",
            Self::PerIndexPoint { .. } => "per-index-point",
            Self::FractionOfCollateral => "fraction-of-collateral",
        }
    }

    /// Returns the index of an index-point basis.
    pub fn index(&self) -> Option<&IndexId> {
        match self {
            Self::PerIndexPoint { index } => Some(index),
            _ => None,
        }
    }
}

impl CanonicalTerms for PriceBasis {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
        if let Self::PerIndexPoint { index } = self {
            encoder.write_terms(index);
        }
    }
}
