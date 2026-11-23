use serde::{Deserialize, Serialize};

use crate::{
    asset::AssetId,
    decimal::{Quantity, SignedQuantity},
    error::ModelError,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetAmount {
    #[serde(rename = "asset")]
    pub asset: AssetId,
    #[serde(rename = "value")]
    pub value: SignedQuantity,
}

impl AssetAmount {
    pub const fn magnitude(&self) -> Quantity {
        self.value.magnitude()
    }
}

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
