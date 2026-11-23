use serde::{Deserialize, Serialize};

use crate::{
    asset::AssetId,
    decimal::Quantity,
    error::{ModelError, QuantityUnitExpectation},
    instrument::Instrument,
    listing::route::VenueListingKey,
    market::{
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit},
    },
    outcome::{OutcomeGroupId, OutcomeMemberId, OutcomeSideId},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "quantity")]
pub enum OrderQuantity {
    #[serde(rename = "asset_units")]
    AssetUnits {
        #[serde(rename = "asset")]
        asset: AssetId,
        #[serde(rename = "value")]
        value: Quantity,
    },
    #[serde(rename = "contracts")]
    Contracts(Quantity),
    #[serde(rename = "outcome_tokens")]
    OutcomeTokens {
        #[serde(rename = "group")]
        group: OutcomeGroupId,
        #[serde(rename = "member")]
        member: OutcomeMemberId,
        #[serde(rename = "side")]
        side: OutcomeSideId,
        #[serde(rename = "value")]
        value: Quantity,
    },
    #[serde(rename = "notional")]
    Notional {
        #[serde(rename = "asset")]
        asset: AssetId,
        #[serde(rename = "value")]
        value: Quantity,
    },
}

impl OrderQuantity {
    pub fn unit(&self) -> QuantityUnit {
        match self {
            Self::AssetUnits { asset, .. } => QuantityUnit::AssetUnits {
                asset: asset.clone(),
            },
            Self::Contracts(_) => QuantityUnit::Contracts,
            Self::OutcomeTokens {
                group,
                member,
                side,
                ..
            } => QuantityUnit::OutcomeTokens {
                group: *group,
                member: *member,
                side: *side,
            },
            Self::Notional { asset, .. } => QuantityUnit::Notional {
                asset: asset.clone(),
            },
        }
    }

    pub const fn value(&self) -> Quantity {
        match self {
            Self::AssetUnits { value, .. }
            | Self::Contracts(value)
            | Self::OutcomeTokens { value, .. }
            | Self::Notional { value, .. } => *value,
        }
    }

    pub fn validate_against(
        &self,
        listed_unit: &QuantityUnit,
        notional_asset: Option<&AssetId>,
        instrument: &Instrument,
    ) -> Result<(), ModelError> {
        let unit = self.unit();
        if unit.is_notional() {
            if unit.asset() != notional_asset {
                return Err(ModelError::QuantityUnitMismatch {
                    field: "order_quantity",
                    expected: QuantityUnitExpectation::NotionalAsset,
                });
            }
            return unit.validate_for_instrument(instrument);
        }
        if &unit == listed_unit {
            return unit.validate_for_instrument(instrument);
        }
        Err(ModelError::QuantityUnitMismatch {
            field: "order_quantity",
            expected: QuantityUnitExpectation::ListedUnit,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstrumentSelection {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingTicker {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "monotonic_id")]
    pub monotonic_id: Option<i64>,
    #[serde(rename = "open")]
    pub open: MarketPrice,
    #[serde(rename = "high")]
    pub high: Option<MarketPrice>,
    #[serde(rename = "low")]
    pub low: Option<MarketPrice>,
    #[serde(rename = "close")]
    pub close: MarketPrice,
    #[serde(rename = "traded_quantity")]
    pub traded_quantity: Option<MarketQuantity>,
    #[serde(rename = "traded_notional")]
    pub traded_notional: Option<MarketQuantity>,
}

impl ListingTicker {
    pub fn validate(&self) -> Result<(), ModelError> {
        if let (Some(low), Some(high)) = (&self.low, &self.high)
            && low.value > high.value
        {
            return Err(ModelError::InvalidCandlestickPrices);
        }
        if let Some(quantity) = &self.traded_quantity {
            quantity.validate_traded("traded_quantity")?;
        }
        if let Some(notional) = &self.traded_notional {
            notional.validate_amount("traded_notional")?;
        }
        Ok(())
    }
}
