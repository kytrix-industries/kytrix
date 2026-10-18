//! The unit-carrying price of one listing.

use serde::{Deserialize, Serialize};

use crate::{asset::AssetId, decimal::Price, error::ModelError, listing::venue::PriceBasis};

/// A listing-scoped price with its quote asset and price basis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketPrice {
    #[serde(rename = "value")]
    pub value: Price,
    #[serde(rename = "quote_asset")]
    pub quote_asset: AssetId,
    #[serde(rename = "basis")]
    pub basis: PriceBasis,
}

impl MarketPrice {
    /// Validates that this price matches a price rule.
    pub fn validate_against(
        &self,
        quote_asset: &AssetId,
        basis: &PriceBasis,
    ) -> Result<(), ModelError> {
        if &self.quote_asset != quote_asset {
            return Err(ModelError::InstrumentTermsMismatch {
                field: "market_price.quote_asset",
            });
        }
        if basis != &self.basis {
            return Err(ModelError::IncompatiblePriceBasis);
        }
        Ok(())
    }
}
