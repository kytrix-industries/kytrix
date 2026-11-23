use serde::{Deserialize, Serialize};

use crate::{
    asset::IndexId,
    identifier::{CanonicalEncoder, CanonicalTerms, VenueListingIdentity},
    instrument::InstrumentKind,
};

crate::identity_type!(VenueListingId, VenueListingIdentity);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidAssetId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidTokenId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HyperliquidOutcomeId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PolymarketProtocolVersion {
    #[serde(rename = "v1")]
    V1,
    #[serde(rename = "v2")]
    V2,
}

impl PolymarketProtocolVersion {
    pub const fn label(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

pub type PolymarketAssetId = crate::identifier::CtfTokenId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HyperliquidAssetProduct {
    #[serde(rename = "spot")]
    Spot,
    #[serde(rename = "perpetual")]
    Perpetual,
}

impl HyperliquidAssetProduct {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
        }
    }

    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
        }
    }
}

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
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
        }
    }

    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
            Self::Future => InstrumentKind::Future,
            Self::Option => InstrumentKind::Option,
        }
    }
}

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
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
        }
    }

    pub const fn instrument_kind(self) -> InstrumentKind {
        match self {
            Self::Spot => InstrumentKind::Spot,
            Self::Perpetual => InstrumentKind::Perpetual,
            Self::Future => InstrumentKind::Future,
            Self::Option => InstrumentKind::Option,
        }
    }
}

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
    pub const fn label(&self) -> &'static str {
        match self {
            Self::PerAssetUnit => "per-asset-unit",
            Self::PerContract => "per-contract",
            Self::PerIndexPoint { .. } => "per-index-point",
            Self::FractionOfCollateral => "fraction-of-collateral",
        }
    }

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
