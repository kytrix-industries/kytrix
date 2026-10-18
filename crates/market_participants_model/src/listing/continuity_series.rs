//! The verified identifiers of a standardized future or option series.

use serde::Serialize;

use crate::{
    error::ModelError,
    identifier::{ContractCode, LocalSymbol, MarketParticipantSeriesId, OccSymbol},
};

/// The verified series identifiers of a standardized future.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct FutureSeriesIdentifiers {
    #[serde(rename = "market_participant_series_id")]
    pub market_participant_series_id: Option<MarketParticipantSeriesId>,
    #[serde(rename = "local_symbol")]
    pub local_symbol: Option<LocalSymbol>,
    #[serde(rename = "contract_code")]
    pub contract_code: Option<ContractCode>,
}

impl FutureSeriesIdentifiers {
    /// Validates that at least one verified series identifier is present.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "future series without a verified identifier",
            });
        }
        Ok(())
    }

    /// Returns `true` when no series identifier is present.
    pub fn is_empty(&self) -> bool {
        self.market_participant_series_id.is_none()
            && self.local_symbol.is_none()
            && self.contract_code.is_none()
    }
}

/// The verified identifiers of an option series.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OptionSeriesIdentifiers {
    #[serde(rename = "occ_symbol")]
    pub occ_symbol: Option<OccSymbol>,
    #[serde(rename = "market_participant_series_id")]
    pub market_participant_series_id: Option<MarketParticipantSeriesId>,
    #[serde(rename = "local_symbol")]
    pub local_symbol: Option<LocalSymbol>,
}

impl OptionSeriesIdentifiers {
    /// Validates that at least one verified series identifier is present.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "option series without a verified identifier",
            });
        }
        Ok(())
    }

    /// Returns `true` when no series identifier is present.
    pub fn is_empty(&self) -> bool {
        self.occ_symbol.is_none()
            && self.market_participant_series_id.is_none()
            && self.local_symbol.is_none()
    }
}

validating_wire!(FutureSeriesIdentifiers, FutureSeriesWire,
    market_participant_series_id as "market_participant_series_id": MarketParticipantSeriesId,
    local_symbol as "local_symbol": LocalSymbol,
    contract_code as "contract_code": ContractCode,
);
validating_wire!(OptionSeriesIdentifiers, OptionSeriesWire,
    occ_symbol as "occ_symbol": OccSymbol,
    market_participant_series_id as "market_participant_series_id": MarketParticipantSeriesId,
    local_symbol as "local_symbol": LocalSymbol,
);
