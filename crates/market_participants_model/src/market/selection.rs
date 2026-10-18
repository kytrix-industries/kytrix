//! Order quantities and the exact listing a consumer selected.

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

/// The quantity of an order command.
///
/// A notional order is the only amount a command may carry, and it is the only
/// way to express a cash quantity.
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
    /// Returns the unit this order quantity is expressed in.
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
                group: group.clone(),
                member: member.clone(),
                side: side.clone(),
            },
            Self::Notional { asset, .. } => QuantityUnit::Notional {
                asset: asset.clone(),
            },
        }
    }

    /// Returns the ordered quantity value.
    pub const fn value(&self) -> Quantity {
        match self {
            Self::AssetUnits { value, .. }
            | Self::Contracts(value)
            | Self::OutcomeTokens { value, .. }
            | Self::Notional { value, .. } => *value,
        }
    }

    /// Validates this order quantity against the resolved listing rules.
    ///
    /// A traded quantity must equal the listed unit. A notional is accepted only
    /// when the listing declares a notional asset and the order names it, and it
    /// is then validated against the instrument like any other unit.
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
/// The exact listing a consumer selected.
///
/// A selection never carries an instrument and a route separately: the full
/// listing key is the only thing a command or a widget propagates.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstrumentSelection {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
}

/// One normalized ticker for one listing.
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
    /// Validates the optional OHLC range and both volume units.
    ///
    /// A venue may omit the session high and low, so the range is only checked
    /// when both bounds were published.
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
