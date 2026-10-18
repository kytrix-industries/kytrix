//! Signed account amounts and unsigned balances.

use serde::{Deserialize, Serialize};

use crate::{
    asset::AssetId,
    decimal::{Quantity, SignedQuantity},
    error::ModelError,
};

/// A signed asset amount such as a fee, funding payment or realized pnl.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetAmount {
    #[serde(rename = "asset")]
    pub asset: AssetId,
    #[serde(rename = "value")]
    pub value: SignedQuantity,
}

impl AssetAmount {
    /// Returns the absolute value of this amount.
    pub const fn magnitude(&self) -> Quantity {
        self.value.magnitude()
    }
}

/// An unsigned asset balance with its available part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetBalance {
    #[serde(rename = "asset")]
    pub asset: AssetId,
    #[serde(rename = "total")]
    pub total: Quantity,
    #[serde(rename = "available")]
    pub available: Quantity,
}

impl AssetBalance {
    /// Validates that the available part never exceeds the total.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.available > self.total {
            return Err(ModelError::InvalidField {
                field: "asset_balance.available",
                value: "above_total".into(),
            });
        }
        Ok(())
    }
}
