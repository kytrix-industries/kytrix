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
pub const MAX_CRYPTOCURRENCY_WALLET_MONITOR_SELECTIONS: usize = 5_000;
pub const MAX_CRYPTOCURRENCY_WALLET_MONITOR_ADDRESS_BYTES: usize = 44;
pub(super) const MAX_TRANSFER_AMOUNT_BYTES: usize = 334;
const MAX_TRANSFER_FRACTION_DIGITS: usize = 255;
pub(super) const MAX_ETHEREUM_INTEGER: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";
pub(super) const MAX_SOLANA_INTEGER: &str = "18446744073709551615";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CryptocurrencyWalletMonitorNetwork {
    #[serde(rename = "ethereum")]
    Ethereum,
    #[serde(rename = "solana")]
    Solana,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum CryptocurrencyWalletMonitorSelectionStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CryptocurrencyWalletMonitorSelection {
    pub network: CryptocurrencyWalletMonitorNetwork,
    pub address: SmolStr,
    pub status: CryptocurrencyWalletMonitorSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CryptocurrencyWalletMonitorSelectionsResponse {
    pub selections: Vec<CryptocurrencyWalletMonitorSelection>,
    pub cryptocurrency_wallet_monitoring_address_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CryptocurrencyWalletMonitorApiError {
    #[error("the cryptocurrency_wallet_monitor address is invalid")]
    InvalidAddress,
    #[error("the cryptocurrency_wallet_monitor address is already selected or the quota is full")]
    Conflict,
    #[error("the cryptocurrency_wallet_monitor selection was not found")]
    NotFound,
    #[error("cryptocurrency_wallet_monitor is unavailable")]
    Unavailable,
    #[error("CryptocurrencyWalletMonitor API authentication failed")]
    Authentication,
    #[error("CryptocurrencyWalletMonitor API request failed: {0}")]
    Network(SmolStr),
    #[error("CryptocurrencyWalletMonitor API service error: {0}")]
    Service(KytrixServiceError),
    #[error("CryptocurrencyWalletMonitor API transport error: {0}")]
    Transport(TransportError),
    #[error("CryptocurrencyWalletMonitor API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("CryptocurrencyWalletMonitor API response is too large")]
    ResponseTooLarge,
    #[error("CryptocurrencyWalletMonitor API returned invalid data")]
    InvalidResponse,
    #[error("CryptocurrencyWalletMonitor API returned HTTP {0}")]
    Server(StatusCode),
    #[error("CryptocurrencyWalletMonitor API request timed out")]
    Timeout,
    #[error("CryptocurrencyWalletMonitor API account session is closed")]
    Closed,
    #[error("CryptocurrencyWalletMonitor API account session changed")]
    AccountChanged,
    #[error("cryptocurrency_wallet_monitor account session is not ready")]
    NotReady,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CryptocurrencyWalletMonitorMutationUnknown {
    #[error("the cryptocurrency_wallet_monitor mutation timed out")]
    Timeout,
    #[error("the cryptocurrency_wallet_monitor mutation transport result is unknown")]
    Transport,
    #[error("the cryptocurrency_wallet_monitor mutation response was lost")]
    ResponseLost,
    #[error("the cryptocurrency_wallet_monitor mutation response was invalid")]
    InvalidResponse,
    #[error("the cryptocurrency_wallet_monitor mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the cryptocurrency_wallet_monitor mutation account session changed")]
    AccountChanged,
    #[error("the cryptocurrency_wallet_monitor mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CryptocurrencyWalletMonitorMutationOutcome<T> {
    Confirmed(T),
    Rejected(CryptocurrencyWalletMonitorApiError),
    Unknown(CryptocurrencyWalletMonitorMutationUnknown),
}

pub(super) fn decode_cryptocurrency_wallet_monitor_selection(
    selection: ProtoCryptocurrencyWalletMonitoringSelection,
) -> Result<CryptocurrencyWalletMonitorSelection, CryptocurrencyWalletMonitorApiError> {
    let network = match ProtoCryptocurrencyWalletMonitoringNetwork::try_from(selection.network)
        .map_err(|_| CryptocurrencyWalletMonitorApiError::InvalidResponse)?
    {
        ProtoCryptocurrencyWalletMonitoringNetwork::Ethereum => {
            CryptocurrencyWalletMonitorNetwork::Ethereum
        }
        ProtoCryptocurrencyWalletMonitoringNetwork::Solana => {
            CryptocurrencyWalletMonitorNetwork::Solana
        }
        ProtoCryptocurrencyWalletMonitoringNetwork::Unspecified => {
            return Err(CryptocurrencyWalletMonitorApiError::InvalidResponse);
        }
    };
    let status =
        match ProtoCryptocurrencyWalletMonitoringSelectionStatus::try_from(selection.status)
            .map_err(|_| CryptocurrencyWalletMonitorApiError::InvalidResponse)?
        {
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Active => {
                CryptocurrencyWalletMonitorSelectionStatus::Active
            }
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Paused => {
                CryptocurrencyWalletMonitorSelectionStatus::Paused
            }
            ProtoCryptocurrencyWalletMonitoringSelectionStatus::Unspecified => {
                return Err(CryptocurrencyWalletMonitorApiError::InvalidResponse);
            }
        };
    let value = CryptocurrencyWalletMonitorSelection {
        network,
        address: selection.address.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub(super) fn map_rpc_error(error: KytrixServiceError) -> CryptocurrencyWalletMonitorApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            CryptocurrencyWalletMonitorApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => {
            CryptocurrencyWalletMonitorApiError::InvalidAddress
        }
        KytrixServiceError::Server(ErrorCode::NotFound) => {
            CryptocurrencyWalletMonitorApiError::NotFound
        }
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => {
            CryptocurrencyWalletMonitorApiError::Conflict
        }
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            CryptocurrencyWalletMonitorApiError::Timeout
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => {
            CryptocurrencyWalletMonitorApiError::NotReady
        }
        KytrixServiceError::Server(_) => CryptocurrencyWalletMonitorApiError::Unavailable,
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            CryptocurrencyWalletMonitorApiError::Authentication
        }
        KytrixServiceError::Closed => CryptocurrencyWalletMonitorApiError::Closed,
        other => CryptocurrencyWalletMonitorApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(
    error: RpcCallError,
) -> CryptocurrencyWalletMonitorMutationOutcome<T> {
    let (error, possibly_submitted) = error.into_parts();
    if !possibly_submitted {
        return CryptocurrencyWalletMonitorMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            CryptocurrencyWalletMonitorMutationOutcome::Unknown(
                CryptocurrencyWalletMonitorMutationUnknown::Timeout,
            )
        }
        KytrixServiceError::Closed => CryptocurrencyWalletMonitorMutationOutcome::Unknown(
            CryptocurrencyWalletMonitorMutationUnknown::ServiceClosed,
        ),
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            CryptocurrencyWalletMonitorMutationOutcome::Unknown(
                CryptocurrencyWalletMonitorMutationUnknown::AccountChanged,
            )
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            CryptocurrencyWalletMonitorMutationOutcome::Unknown(
                CryptocurrencyWalletMonitorMutationUnknown::ResponseLost,
            )
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => CryptocurrencyWalletMonitorMutationOutcome::Unknown(
            CryptocurrencyWalletMonitorMutationUnknown::ResponseLost,
        ),
        KytrixServiceError::Write(_) => CryptocurrencyWalletMonitorMutationOutcome::Unknown(
            CryptocurrencyWalletMonitorMutationUnknown::Transport,
        ),
        error => CryptocurrencyWalletMonitorMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub(super) fn validate_selections(
    response: &CryptocurrencyWalletMonitorSelectionsResponse,
) -> Result<(), CryptocurrencyWalletMonitorApiError> {
    if !(0..=MAX_CRYPTOCURRENCY_WALLET_MONITOR_SELECTIONS)
        .contains(&response.cryptocurrency_wallet_monitoring_address_limit)
        || response.selections.len() > MAX_CRYPTOCURRENCY_WALLET_MONITOR_SELECTIONS
    {
        return Err(CryptocurrencyWalletMonitorApiError::InvalidResponse);
    }
    let mut addresses = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !addresses.insert((selection.network, selection.address.as_str())) {
            return Err(CryptocurrencyWalletMonitorApiError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_selection(
    selection: &CryptocurrencyWalletMonitorSelection,
) -> Result<(), CryptocurrencyWalletMonitorApiError> {
    if cryptocurrency_wallet_monitor_network(&selection.address) != Some(selection.network)
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(CryptocurrencyWalletMonitorApiError::InvalidResponse);
    }
    Ok(())
}

pub(super) fn sort_selections(selections: &mut [CryptocurrencyWalletMonitorSelection]) {
    selections.sort_by(compare_cryptocurrency_wallet_monitor_selections);
}

pub fn compare_cryptocurrency_wallet_monitor_selections(
    left: &CryptocurrencyWalletMonitorSelection,
    right: &CryptocurrencyWalletMonitorSelection,
) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.network.cmp(&right.network))
        .then_with(|| left.address.cmp(&right.address))
}

pub fn cryptocurrency_wallet_monitor_network(
    value: &str,
) -> Option<CryptocurrencyWalletMonitorNetwork> {
    if value.len() > MAX_CRYPTOCURRENCY_WALLET_MONITOR_ADDRESS_BYTES {
        return None;
    }
    if value.len() == 42
        && value.starts_with("0x")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Some(CryptocurrencyWalletMonitorNetwork::Ethereum);
    }
    let mut key = [0_u8; 32];
    (bs58::decode(value).onto(&mut key).ok() == Some(key.len())
        && bs58::encode(key).into_string() == value)
        .then_some(CryptocurrencyWalletMonitorNetwork::Solana)
}

pub(super) fn valid_transfer_amount(
    value: &str,
    network: CryptocurrencyWalletMonitorNetwork,
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
        CryptocurrencyWalletMonitorNetwork::Ethereum => MAX_ETHEREUM_INTEGER,
        CryptocurrencyWalletMonitorNetwork::Solana => MAX_SOLANA_INTEGER,
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
        CryptocurrencyWalletMonitorApiError, CryptocurrencyWalletMonitorMutationOutcome,
        CryptocurrencyWalletMonitorMutationUnknown, CryptocurrencyWalletMonitorNetwork,
        CryptocurrencyWalletMonitorSelection, CryptocurrencyWalletMonitorSelectionStatus,
        CryptocurrencyWalletMonitorSelectionsResponse, ErrorCode, KytrixServiceError,
        MAX_CRYPTOCURRENCY_WALLET_MONITOR_ADDRESS_BYTES,
        MAX_CRYPTOCURRENCY_WALLET_MONITOR_SELECTIONS, MAX_ETHEREUM_INTEGER, MAX_SOLANA_INTEGER,
        RpcCallError, cryptocurrency_wallet_monitor_network, map_rpc_error, mutation_rpc_error,
        sort_selections, valid_transfer_amount, validate_selections,
    };

    fn selection(
        address: &str,
        status: CryptocurrencyWalletMonitorSelectionStatus,
    ) -> CryptocurrencyWalletMonitorSelection {
        CryptocurrencyWalletMonitorSelection {
            network: CryptocurrencyWalletMonitorNetwork::Ethereum,
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
            CryptocurrencyWalletMonitorApiError::Conflict
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError {
                error: KytrixServiceError::Server(ErrorCode::MutationUnknown),
                possibly_submitted: true,
            }),
            CryptocurrencyWalletMonitorMutationOutcome::Unknown(
                CryptocurrencyWalletMonitorMutationUnknown::ResponseLost
            )
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError::not_submitted(KytrixServiceError::Timeout)),
            CryptocurrencyWalletMonitorMutationOutcome::Rejected(
                CryptocurrencyWalletMonitorApiError::Timeout
            )
        );
    }

    #[test]
    fn cryptocurrency_wallet_monitor_contract_validates_canonical_addresses_and_amounts() {
        assert_eq!(
            cryptocurrency_wallet_monitor_network("0x0123456789abcdef0123456789abcdef01234567"),
            Some(CryptocurrencyWalletMonitorNetwork::Ethereum)
        );
        assert_eq!(
            cryptocurrency_wallet_monitor_network("11111111111111111111111111111111"),
            Some(CryptocurrencyWalletMonitorNetwork::Solana)
        );
        assert!(
            cryptocurrency_wallet_monitor_network(
                &"1".repeat(MAX_CRYPTOCURRENCY_WALLET_MONITOR_ADDRESS_BYTES + 1)
            )
            .is_none()
        );
        assert!(
            cryptocurrency_wallet_monitor_network("0x0123456789ABCDEF0123456789abcdef01234567")
                .is_none()
        );
        assert!(valid_transfer_amount(
            "100.000001",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "1e6",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "01",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "1.0",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
        assert!(!valid_transfer_amount(
            "-1",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
    }

    #[test]
    fn cryptocurrency_wallet_monitor_contract_accepts_canonical_zero_amount() {
        assert!(valid_transfer_amount(
            "0",
            CryptocurrencyWalletMonitorNetwork::Ethereum
        ));
    }

    #[test]
    fn ethereum_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_ETHEREUM_INTEGER}.1"),
            CryptocurrencyWalletMonitorNetwork::Ethereum,
        ));
    }

    #[test]
    fn solana_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_SOLANA_INTEGER}.1"),
            CryptocurrencyWalletMonitorNetwork::Solana,
        ));
    }

    #[test]
    fn selection_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_selections(&CryptocurrencyWalletMonitorSelectionsResponse {
                selections: Vec::new(),
                cryptocurrency_wallet_monitoring_address_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_accepts_paused_history_above_the_current_limit() {
        assert_eq!(
            validate_selections(&CryptocurrencyWalletMonitorSelectionsResponse {
                selections: vec![
                    selection(
                        "0x0123456789abcdef0123456789abcdef01234567",
                        CryptocurrencyWalletMonitorSelectionStatus::Paused,
                    ),
                    selection(
                        "0x89abcdef0123456789abcdef0123456789abcdef",
                        CryptocurrencyWalletMonitorSelectionStatus::Paused,
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
            validate_selections(&CryptocurrencyWalletMonitorSelectionsResponse {
                selections: Vec::new(),
                cryptocurrency_wallet_monitoring_address_limit:
                    MAX_CRYPTOCURRENCY_WALLET_MONITOR_SELECTIONS + 1,
            }),
            Err(CryptocurrencyWalletMonitorApiError::InvalidResponse)
        );
    }

    #[test]
    fn selections_are_ordered_by_recency_then_stable_identity() {
        let selection =
            |address: &str, created_at, updated_at| CryptocurrencyWalletMonitorSelection {
                network: CryptocurrencyWalletMonitorNetwork::Ethereum,
                address: address.into(),
                status: CryptocurrencyWalletMonitorSelectionStatus::Active,
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
