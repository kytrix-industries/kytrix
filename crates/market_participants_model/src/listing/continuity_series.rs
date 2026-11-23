use serde::Serialize;

use crate::{
    error::ModelError,
    identifier::{ContractCode, LocalSymbol, MarketParticipantSeriesId, OccSymbol},
};

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
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "future series without a verified identifier",
            });
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.market_participant_series_id.is_none()
            && self.local_symbol.is_none()
            && self.contract_code.is_none()
    }
}

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
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.is_empty() {
            return Err(ModelError::InvalidListingContinuityTerms {
                field: "option series without a verified identifier",
            });
        }
        Ok(())
    }

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
