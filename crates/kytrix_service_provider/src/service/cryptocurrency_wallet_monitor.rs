use std::cmp::Ordering;

use ahash::AHashSet;
use kytrix_proto::types::{
    CryptocurrencyWalletMonitoringNetwork as ProtoCryptocurrencyWalletMonitoringNetwork,
    CryptocurrencyWalletMonitoringSelection as ProtoCryptocurrencyWalletMonitoringSelection,
    CryptocurrencyWalletMonitoringSelectionStatus as ProtoCryptocurrencyWalletMonitoringSelectionStatus,
};
use network::transport::error::TransportError;
use reqwest::StatusCode;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;

use crate::service::engine::{ErrorCode, KytrixServiceError, RpcCallError};
pub const MAX_CRYPTOCURRENCY_WALLET_MONITORING_SELECTIONS: usize = 5_000;
pub const MAX_CRYPTOCURRENCY_WALLET_MONITORING_ADDRESS_BYTES: usize = 44;
pub(super) const MAX_TRANSFER_AMOUNT_BYTES: usize = 334;
const MAX_TRANSFER_FRACTION_DIGITS: usize = 255;
pub(super) const MAX_ETHEREUM_INTEGER: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";
pub(super) const MAX_SOLANA_INTEGER: &str = "18446744073709551615";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CryptocurrencyWalletMonitoringNetwork {
    #[serde(rename = "ethereum")]
    Ethereum,
    #[serde(rename = "solana")]
    Solana,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum CryptocurrencyWalletMonitoringSelectionStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CryptocurrencyWalletMonitoringSelection {
    pub network: CryptocurrencyWalletMonitoringNetwork,
    pub address: SmolStr,
    pub status: CryptocurrencyWalletMonitoringSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CryptocurrencyWalletMonitoringSelectionsResponse {
    pub selections: Vec<CryptocurrencyWalletMonitoringSelection>,
    pub cryptocurrency_wallet_monitoring_address_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CryptocurrencyWalletMonitoringApiError {
    #[error("the cryptocurrency_wallet_monitoring address is invalid")]
    InvalidAddress,
    #[error(
        "the cryptocurrency_wallet_monitoring address is already selected or the quota is full"
    )]
    Conflict,
    #[error("the cryptocurrency_wallet_monitoring selection was not found")]
    NotFound,
    #[error("cryptocurrency_wallet_monitoring monitoring is unavailable")]
    Unavailable,
    #[error("CryptocurrencyWalletMonitoring API authentication failed")]
    Authentication,
    #[error("CryptocurrencyWalletMonitoring API request failed: {0}")]
    Network(SmolStr),
    #[error("CryptocurrencyWalletMonitoring API service error: {0}")]
    Service(KytrixServiceError),
    #[error("CryptocurrencyWalletMonitoring API transport error: {0}")]
    Transport(TransportError),
    #[error("CryptocurrencyWalletMonitoring API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("CryptocurrencyWalletMonitoring API response is too large")]
    ResponseTooLarge,
    #[error("CryptocurrencyWalletMonitoring API returned invalid data")]
    InvalidResponse,
    #[error("CryptocurrencyWalletMonitoring API returned HTTP {0}")]
    Server(StatusCode),
    #[error("CryptocurrencyWalletMonitoring API request timed out")]
    Timeout,
    #[error("CryptocurrencyWalletMonitoring API account session is closed")]
    Closed,
    #[error("CryptocurrencyWalletMonitoring API account session changed")]
    AccountChanged,
    #[error("cryptocurrency_wallet_monitoring monitor account session is not ready")]
    NotReady,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CryptocurrencyWalletMonitoringMutationUnknown {
    #[error("the cryptocurrency_wallet_monitoring mutation timed out")]
    Timeout,
    #[error("the cryptocurrency_wallet_monitoring mutation transport result is unknown")]
    Transport,
    #[error("the cryptocurrency_wallet_monitoring mutation response was lost")]
    ResponseLost,
    #[error("the cryptocurrency_wallet_monitoring mutation response was invalid")]
    InvalidResponse,
    #[error("the cryptocurrency_wallet_monitoring mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the cryptocurrency_wallet_monitoring mutation account session changed")]
    AccountChanged,
    #[error("the cryptocurrency_wallet_monitoring mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CryptocurrencyWalletMonitoringMutationOutcome<T> {
    Confirmed(T),
    Rejected(CryptocurrencyWalletMonitoringApiError),
    Unknown(CryptocurrencyWalletMonitoringMutationUnknown),
}

pub(super) fn decode_cryptocurrency_wallet_monitoring_selection(
    selection: ProtoCryptocurrencyWalletMonitoringSelection,
) -> Result<CryptocurrencyWalletMonitoringSelection, CryptocurrencyWalletMonitoringApiError> {
    let network = match ProtoCryptocurrencyWalletMonitoringNetwork::try_from(selection.network)
        .map_err(|_| CryptocurrencyWalletMonitoringApiError::InvalidResponse)?
    {
        ProtoCryptocurrencyWalletMonitoringNetwork::Ethereum => {
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        }
        ProtoCryptocurrencyWalletMonitoringNetwork::Solana => {
            CryptocurrencyWalletMonitoringNetwork::Solana
        }
        ProtoCryptocurrencyWalletMonitoringNetwork::Unspecified => {
            return Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse);
        }
    };
    let status =
        match ProtoCryptocurrencyWalletMonitoringSelectionStatus::try_from(selection.status)
            .map_err(|_| CryptocurrencyWalletMonitoringApiError::InvalidResponse)?
        {
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Active => {
                CryptocurrencyWalletMonitoringSelectionStatus::Active
            }
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Paused => {
                CryptocurrencyWalletMonitoringSelectionStatus::Paused
            }
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Unspecified => {
                return Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse);
            }
        };
    let value = CryptocurrencyWalletMonitoringSelection {
        network,
        address: selection.address.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub(super) fn map_rpc_error(error: KytrixServiceError) -> CryptocurrencyWalletMonitoringApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            CryptocurrencyWalletMonitoringApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => {
            CryptocurrencyWalletMonitoringApiError::InvalidAddress
        }
        KytrixServiceError::Server(ErrorCode::NotFound) => {
            CryptocurrencyWalletMonitoringApiError::NotFound
        }
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => {
            CryptocurrencyWalletMonitoringApiError::Conflict
        }
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            CryptocurrencyWalletMonitoringApiError::Timeout
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => {
            CryptocurrencyWalletMonitoringApiError::NotReady
        }
        KytrixServiceError::Server(_) => CryptocurrencyWalletMonitoringApiError::Unavailable,
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            CryptocurrencyWalletMonitoringApiError::Authentication
        }
        KytrixServiceError::Closed => CryptocurrencyWalletMonitoringApiError::Closed,
        other => CryptocurrencyWalletMonitoringApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(
    error: RpcCallError,
) -> CryptocurrencyWalletMonitoringMutationOutcome<T> {
    let (error, possibly_submitted) = error.into_parts();
    if !possibly_submitted {
        return CryptocurrencyWalletMonitoringMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
                CryptocurrencyWalletMonitoringMutationUnknown::Timeout,
            )
        }
        KytrixServiceError::Closed => CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
            CryptocurrencyWalletMonitoringMutationUnknown::ServiceClosed,
        ),
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
                CryptocurrencyWalletMonitoringMutationUnknown::AccountChanged,
            )
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
                CryptocurrencyWalletMonitoringMutationUnknown::ResponseLost,
            )
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => {
            CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
                CryptocurrencyWalletMonitoringMutationUnknown::ResponseLost,
            )
        }
        KytrixServiceError::Write(_) => CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
            CryptocurrencyWalletMonitoringMutationUnknown::Transport,
        ),
        error => CryptocurrencyWalletMonitoringMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub(super) fn validate_selections(
    response: &CryptocurrencyWalletMonitoringSelectionsResponse,
) -> Result<(), CryptocurrencyWalletMonitoringApiError> {
    if !(0..=MAX_CRYPTOCURRENCY_WALLET_MONITORING_SELECTIONS)
        .contains(&response.cryptocurrency_wallet_monitoring_address_limit)
        || response.selections.len() > MAX_CRYPTOCURRENCY_WALLET_MONITORING_SELECTIONS
    {
        return Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse);
    }
    let mut addresses = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !addresses.insert((selection.network, selection.address.as_str())) {
            return Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_selection(
    selection: &CryptocurrencyWalletMonitoringSelection,
) -> Result<(), CryptocurrencyWalletMonitoringApiError> {
    if cryptocurrency_wallet_monitoring_network(&selection.address) != Some(selection.network)
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse);
    }
    Ok(())
}

pub(super) fn sort_selections(selections: &mut [CryptocurrencyWalletMonitoringSelection]) {
    selections.sort_by(compare_cryptocurrency_wallet_monitoring_selections);
}

pub fn compare_cryptocurrency_wallet_monitoring_selections(
    left: &CryptocurrencyWalletMonitoringSelection,
    right: &CryptocurrencyWalletMonitoringSelection,
) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.network.cmp(&right.network))
        .then_with(|| left.address.cmp(&right.address))
}

pub fn cryptocurrency_wallet_monitoring_network(
    value: &str,
) -> Option<CryptocurrencyWalletMonitoringNetwork> {
    if value.len() > MAX_CRYPTOCURRENCY_WALLET_MONITORING_ADDRESS_BYTES {
        return None;
    }
    if value.len() == 42
        && value.starts_with("0x")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Some(CryptocurrencyWalletMonitoringNetwork::Ethereum);
    }
    let mut key = [0_u8; 32];
    (bs58::decode(value).onto(&mut key).ok() == Some(key.len())
        && bs58::encode(key).into_string() == value)
        .then_some(CryptocurrencyWalletMonitoringNetwork::Solana)
}

pub(super) fn valid_transfer_amount(
    value: &str,
    network: CryptocurrencyWalletMonitoringNetwork,
) -> bool {
    if value.is_empty() || value.len() > MAX_TRANSFER_AMOUNT_BYTES || value.starts_with('-') {
        return false;
    }
    let mut parts = value.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || (integer.len() > 1 && integer.starts_with('0'))
        || parts.next().is_some()
    {
        return false;
    }
    let maximum = match network {
        CryptocurrencyWalletMonitoringNetwork::Ethereum => MAX_ETHEREUM_INTEGER,
        CryptocurrencyWalletMonitoringNetwork::Solana => MAX_SOLANA_INTEGER,
    };
    if integer.len() > maximum.len()
        || (integer.len() == maximum.len() && integer > maximum)
        || (integer == maximum && fraction.is_some())
        || fraction.is_some_and(|fraction| {
            fraction.is_empty()
                || fraction.len() > MAX_TRANSFER_FRACTION_DIGITS
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                || fraction.ends_with('0')
        })
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{
        CryptocurrencyWalletMonitoringApiError, CryptocurrencyWalletMonitoringMutationOutcome,
        CryptocurrencyWalletMonitoringMutationUnknown, CryptocurrencyWalletMonitoringNetwork,
        CryptocurrencyWalletMonitoringSelection, CryptocurrencyWalletMonitoringSelectionStatus,
        CryptocurrencyWalletMonitoringSelectionsResponse, ErrorCode, KytrixServiceError,
        MAX_CRYPTOCURRENCY_WALLET_MONITORING_ADDRESS_BYTES,
        MAX_CRYPTOCURRENCY_WALLET_MONITORING_SELECTIONS, MAX_ETHEREUM_INTEGER, MAX_SOLANA_INTEGER,
        RpcCallError, cryptocurrency_wallet_monitoring_network, map_rpc_error, mutation_rpc_error,
        sort_selections, valid_transfer_amount, validate_selections,
    };

    fn selection(
        address: &str,
        status: CryptocurrencyWalletMonitoringSelectionStatus,
    ) -> CryptocurrencyWalletMonitoringSelection {
        CryptocurrencyWalletMonitoringSelection {
            network: CryptocurrencyWalletMonitoringNetwork::Ethereum,
            address: address.into(),
            status,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn rpc_errors_preserve_conflict_and_unknown_mutation_semantics() {
        assert_eq!(
            map_rpc_error(KytrixServiceError::Server(ErrorCode::QuotaExceeded)),
            CryptocurrencyWalletMonitoringApiError::Conflict
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError {
                error: KytrixServiceError::Server(ErrorCode::MutationUnknown),
                possibly_submitted: true,
            }),
            CryptocurrencyWalletMonitoringMutationOutcome::Unknown(
                CryptocurrencyWalletMonitoringMutationUnknown::ResponseLost
            )
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError::not_submitted(KytrixServiceError::Timeout)),
            CryptocurrencyWalletMonitoringMutationOutcome::Rejected(
                CryptocurrencyWalletMonitoringApiError::Timeout
            )
        );
    }

    #[test]
    fn cryptocurrency_wallet_monitoring_contract_validates_canonical_addresses_and_amounts() {
        assert_eq!(
            cryptocurrency_wallet_monitoring_network("0x0123456789abcdef0123456789abcdef01234567"),
            Some(CryptocurrencyWalletMonitoringNetwork::Ethereum)
        );
        assert_eq!(
            cryptocurrency_wallet_monitoring_network("11111111111111111111111111111111"),
            Some(CryptocurrencyWalletMonitoringNetwork::Solana)
        );
        assert!(
            cryptocurrency_wallet_monitoring_network(
                &"1".repeat(MAX_CRYPTOCURRENCY_WALLET_MONITORING_ADDRESS_BYTES + 1)
            )
            .is_none()
        );
        assert!(
            cryptocurrency_wallet_monitoring_network("0x0123456789ABCDEF0123456789abcdef01234567")
                .is_none()
        );
        assert!(valid_transfer_amount(
            "100.000001",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "1e6",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "01",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "1.0",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "-1",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
    }

    #[test]
    fn cryptocurrency_wallet_monitoring_contract_accepts_canonical_zero_amount() {
        assert!(valid_transfer_amount(
            "0",
            CryptocurrencyWalletMonitoringNetwork::Ethereum
        ));
    }

    #[test]
    fn ethereum_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_ETHEREUM_INTEGER}.1"),
            CryptocurrencyWalletMonitoringNetwork::Ethereum,
        ));
    }

    #[test]
    fn solana_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_SOLANA_INTEGER}.1"),
            CryptocurrencyWalletMonitoringNetwork::Solana,
        ));
    }

    #[test]
    fn selection_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_selections(&CryptocurrencyWalletMonitoringSelectionsResponse {
                selections: Vec::new(),
                cryptocurrency_wallet_monitoring_address_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_accepts_paused_history_above_the_current_limit() {
        assert_eq!(
            validate_selections(&CryptocurrencyWalletMonitoringSelectionsResponse {
                selections: vec![
                    selection(
                        "0x0123456789abcdef0123456789abcdef01234567",
                        CryptocurrencyWalletMonitoringSelectionStatus::Paused,
                    ),
                    selection(
                        "0x89abcdef0123456789abcdef0123456789abcdef",
                        CryptocurrencyWalletMonitoringSelectionStatus::Paused,
                    ),
                ],
                cryptocurrency_wallet_monitoring_address_limit: 1,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_rejects_a_limit_above_the_process_bound() {
        assert_eq!(
            validate_selections(&CryptocurrencyWalletMonitoringSelectionsResponse {
                selections: Vec::new(),
                cryptocurrency_wallet_monitoring_address_limit:
                    MAX_CRYPTOCURRENCY_WALLET_MONITORING_SELECTIONS + 1,
            }),
            Err(CryptocurrencyWalletMonitoringApiError::InvalidResponse)
        );
    }

    #[test]
    fn selections_are_ordered_by_recency_then_stable_identity() {
        let selection =
            |address: &str, created_at, updated_at| CryptocurrencyWalletMonitoringSelection {
                network: CryptocurrencyWalletMonitoringNetwork::Ethereum,
                address: address.into(),
                status: CryptocurrencyWalletMonitoringSelectionStatus::Active,
                created_at,
                updated_at,
            };
        let mut selections = vec![
            selection("0x3333333333333333333333333333333333333333", 3, 4),
            selection("0x2222222222222222222222222222222222222222", 2, 5),
            selection("0x1111111111111111111111111111111111111111", 2, 5),
            selection("0x4444444444444444444444444444444444444444", 1, 4),
        ];

        sort_selections(&mut selections);

        assert_eq!(
            selections
                .iter()
                .map(|selection| selection.address.as_str())
                .collect::<Vec<_>>(),
            [
                "0x1111111111111111111111111111111111111111",
                "0x2222222222222222222222222222222222222222",
                "0x3333333333333333333333333333333333333333",
                "0x4444444444444444444444444444444444444444",
            ]
        );
    }
}
