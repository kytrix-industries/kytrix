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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side {
    #[serde(rename = "buy")]
    Buy,
    #[serde(rename = "sell")]
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PositionSide {
    #[serde(rename = "net")]
    Net,
    #[serde(rename = "long")]
    Long,
    #[serde(rename = "short")]
    Short,
}

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
    pub fn asset(&self) -> Option<&AssetId> {
        match self {
            Self::AssetUnits { asset } | Self::Notional { asset } => Some(asset),
            Self::Contracts | Self::OutcomeTokens { .. } => None,
        }
    }

    pub const fn is_notional(&self) -> bool {
        matches!(self, Self::Notional { .. })
    }

    pub fn require_traded(&self, field: &'static str) -> Result<(), ModelError> {
        require_traded_quantity_unit(self, field)
    }

    pub fn require_notional(&self, field: &'static str) -> Result<(), ModelError> {
        require_notional_unit(self, field)
    }

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketQuantity {
    #[serde(rename = "value")]
    pub value: Quantity,
    #[serde(rename = "unit")]
    pub unit: QuantityUnit,
}

impl MarketQuantity {
    pub fn amount(value: Quantity, unit: QuantityUnit) -> Result<Self, ModelError> {
        unit.require_notional("market_amount")?;
        Ok(Self { value, unit })
    }

    pub fn traded(value: Quantity, unit: QuantityUnit) -> Result<Self, ModelError> {
        unit.require_traded("market_quantity")?;
        Ok(Self { value, unit })
    }

    pub fn validate_amount(&self, field: &'static str) -> Result<(), ModelError> {
        self.unit.require_notional(field)
    }

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
