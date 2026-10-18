//! What a market quantity counts, which side a market event belongs to and the
//! units a comparison expects.

use serde::{Deserialize, Serialize};

use crate::{
    asset::AssetId,
    decimal::Quantity,
    error::{
        ModelError, QuantityUnitExpectation, require_notional_unit, require_traded_quantity_unit,
    },
    identifier::IdentifierValue,
    instrument::{Instrument, InstrumentTerms},
    outcome::{OutcomeGroupId, OutcomeMemberId, OutcomeSideId},
};

/// Which side of the book a public market event belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side {
    #[serde(rename = "buy")]
    Buy,
    #[serde(rename = "sell")]
    Sell,
}

/// The position-mode side of a perpetual event or order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PositionSide {
    #[serde(rename = "net")]
    Net,
    #[serde(rename = "long")]
    Long,
    #[serde(rename = "short")]
    Short,
}

/// What a quantity counts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "unit")]
pub enum QuantityUnit {
    #[serde(rename = "asset_units")]
    AssetUnits {
        #[serde(rename = "asset")]
        asset: AssetId,
    },
    #[serde(rename = "contracts")]
    Contracts,
    #[serde(rename = "outcome_tokens")]
    OutcomeTokens {
        #[serde(rename = "group")]
        group: OutcomeGroupId,
        #[serde(rename = "member")]
        member: OutcomeMemberId,
        #[serde(rename = "side")]
        side: OutcomeSideId,
    },
    #[serde(rename = "notional")]
    Notional {
        #[serde(rename = "asset")]
        asset: AssetId,
    },
}

impl QuantityUnit {
    /// Returns the asset this unit is denominated in, when it has one.
    pub fn asset(&self) -> Option<&AssetId> {
        match self {
            Self::AssetUnits { asset } | Self::Notional { asset } => Some(asset),
            Self::Contracts | Self::OutcomeTokens { .. } => None,
        }
    }

    /// Returns `true` when this unit expresses a monetary amount.
    pub const fn is_notional(&self) -> bool {
        matches!(self, Self::Notional { .. })
    }

    /// Validates that this unit is a traded quantity rather than an amount.
    pub fn require_traded(&self, field: &'static str) -> Result<(), ModelError> {
        require_traded_quantity_unit(self, field)
    }

    /// Validates that this unit is a monetary amount.
    pub fn require_notional(&self, field: &'static str) -> Result<(), ModelError> {
        require_notional_unit(self, field)
    }

    /// Validates that this unit is compatible with a listed instrument.
    ///
    /// An outcome quantity must name the exact group, member and side of the
    /// listed outcome instrument; a spot listing is traded in its base asset, so
    /// neither its quote asset nor a contract count is a traded unit.
    pub fn validate_for_instrument(&self, instrument: &Instrument) -> Result<(), ModelError> {
        match (self, instrument.terms()) {
            (
                Self::OutcomeTokens {
                    group,
                    member,
                    side,
                },
                InstrumentTerms::Outcome(spec),
            ) => {
                if group != spec.group() || member != spec.member() || side != spec.side() {
                    return Err(ModelError::QuantityUnitMismatch {
                        field: "quantity.unit",
                        expected: QuantityUnitExpectation::OutcomeInstrument,
                    });
                }
                Ok(())
            }
            (Self::OutcomeTokens { .. }, _) => Err(ModelError::QuantityUnitMismatch {
                field: "quantity.unit",
                expected: QuantityUnitExpectation::ListedUnit,
            }),
            (Self::AssetUnits { asset }, InstrumentTerms::Spot(spec)) => {
                if asset == spec.quote() {
                    return Err(ModelError::QuantityUnitMismatch {
                        field: "quantity.unit",
                        expected: QuantityUnitExpectation::TradedQuantity,
                    });
                }
                Ok(())
            }
            (Self::Contracts, InstrumentTerms::Spot(_)) => Err(ModelError::QuantityUnitMismatch {
                field: "quantity.unit",
                expected: QuantityUnitExpectation::TradedQuantity,
            }),
            _ => Ok(()),
        }
    }
}

/// A listing-scoped quantity with its declared unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketQuantity {
    #[serde(rename = "value")]
    pub value: Quantity,
    #[serde(rename = "unit")]
    pub unit: QuantityUnit,
}

impl MarketQuantity {
    /// Builds a monetary amount that must be expressed as a notional.
    pub fn amount(value: Quantity, unit: QuantityUnit) -> Result<Self, ModelError> {
        unit.require_notional("market_amount")?;
        Ok(Self { value, unit })
    }

    /// Builds a traded quantity that must not be expressed as an amount.
    pub fn traded(value: Quantity, unit: QuantityUnit) -> Result<Self, ModelError> {
        unit.require_traded("market_quantity")?;
        Ok(Self { value, unit })
    }

    /// Validates that this quantity is a monetary amount.
    pub fn validate_amount(&self, field: &'static str) -> Result<(), ModelError> {
        self.unit.require_notional(field)
    }

    /// Validates that this quantity is a traded quantity.
    pub fn validate_traded(&self, field: &'static str) -> Result<(), ModelError> {
        self.unit.require_traded(field)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id")]
pub enum MarketTradeId {
    #[serde(rename = "numeric")]
    Numeric(u64),
    #[serde(rename = "native")]
    Native(IdentifierValue),
}
