use std::cmp::Ordering;

use ahash::AHashSet;
use kytrix_proto::types::v1::{
    EthereumWalletSelection as ProtoEthereumWalletSelection,
    EthereumWalletSelectionStatus as ProtoEthereumWalletSelectionStatus,
};
use network::transport::error::TransportError;
use reqwest::StatusCode;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;

use crate::service::engine::{ErrorCode, KytrixServiceError, RpcCallError};

pub const MAX_ETHEREUM_WALLET_SELECTIONS: usize = 5_000;
pub const MAX_ETHEREUM_WALLET_ADDRESS_BYTES: usize = 42;
const MAX_TRANSFER_AMOUNT_BYTES: usize = 334;
const MAX_TRANSFER_FRACTION_DIGITS: usize = 255;
pub const MAX_ETHEREUM_INTEGER: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum EthereumWalletSelectionStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct EthereumWalletSelection {
    pub address: SmolStr,
    pub status: EthereumWalletSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct EthereumWalletSelectionsResponse {
    pub selections: Vec<EthereumWalletSelection>,
    pub ethereum_wallet_monitor_address_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EthereumWalletApiError {
    #[error("the Ethereum wallet address is invalid")]
    InvalidAddress,
    #[error("the Ethereum wallet address is already selected or the quota is full")]
    Conflict,
    #[error("the Ethereum wallet selection was not found")]
    NotFound,
    #[error("Ethereum wallet monitoring is unavailable")]
    Unavailable,
    #[error("Ethereum wallet API authentication failed")]
    Authentication,
    #[error("Ethereum wallet API request failed: {0}")]
    Network(SmolStr),
    #[error("Ethereum wallet API service error: {0}")]
    Service(KytrixServiceError),
    #[error("Ethereum wallet API transport error: {0}")]
    Transport(TransportError),
    #[error("Ethereum wallet API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("Ethereum wallet API response is too large")]
    ResponseTooLarge,
    #[error("Ethereum wallet API returned invalid data")]
    InvalidResponse,
    #[error("Ethereum wallet API returned HTTP {0}")]
    Server(StatusCode),
    #[error("Ethereum wallet API request timed out")]
    Timeout,
    #[error("Ethereum wallet API account session is closed")]
    Closed,
    #[error("Ethereum wallet API account session changed")]
    AccountChanged,
    #[error("Ethereum wallet monitoring is not ready")]
    NotReady,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EthereumWalletMutationUnknown {
    #[error("the Ethereum wallet mutation timed out")]
    Timeout,
    #[error("the Ethereum wallet mutation transport result is unknown")]
    Transport,
    #[error("the Ethereum wallet mutation response was lost")]
    ResponseLost,
    #[error("the Ethereum wallet mutation response was invalid")]
    InvalidResponse,
    #[error("the Ethereum wallet mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the Ethereum wallet mutation account session changed")]
    AccountChanged,
    #[error("the Ethereum wallet mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EthereumWalletMutationOutcome<T> {
    Confirmed(T),
    Rejected(EthereumWalletApiError),
    Unknown(EthereumWalletMutationUnknown),
}

pub fn decode_ethereum_wallet_selection(
    selection: ProtoEthereumWalletSelection,
) -> Result<EthereumWalletSelection, EthereumWalletApiError> {
    let status = match ProtoEthereumWalletSelectionStatus::try_from(selection.status)
        .map_err(|_| EthereumWalletApiError::InvalidResponse)?
    {
        ProtoEthereumWalletSelectionStatus::Active => EthereumWalletSelectionStatus::Active,
        ProtoEthereumWalletSelectionStatus::Paused => EthereumWalletSelectionStatus::Paused,
        ProtoEthereumWalletSelectionStatus::Unspecified => {
            return Err(EthereumWalletApiError::InvalidResponse);
        }
    };
    let value = EthereumWalletSelection {
        address: selection.address.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub fn map_rpc_error(error: KytrixServiceError) -> EthereumWalletApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            EthereumWalletApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => {
            EthereumWalletApiError::InvalidAddress
        }
        KytrixServiceError::Server(ErrorCode::NotFound) => EthereumWalletApiError::NotFound,
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => EthereumWalletApiError::Conflict,
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            EthereumWalletApiError::Timeout
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => EthereumWalletApiError::NotReady,
        KytrixServiceError::Server(_) => EthereumWalletApiError::Unavailable,
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            EthereumWalletApiError::Authentication
        }
        KytrixServiceError::Closed => EthereumWalletApiError::Closed,
        other => EthereumWalletApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(error: RpcCallError) -> EthereumWalletMutationOutcome<T> {
    let (error, possibly_submitted) = error.into_parts();
    if !possibly_submitted {
        return EthereumWalletMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::Timeout)
        }
        KytrixServiceError::Closed => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::ServiceClosed)
        }
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::AccountChanged)
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Write(_) => {
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::Transport)
        }
        error => EthereumWalletMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub fn validate_ethereum_wallet_selections(
    response: &EthereumWalletSelectionsResponse,
) -> Result<(), EthereumWalletApiError> {
    if !(0..=MAX_ETHEREUM_WALLET_SELECTIONS)
        .contains(&response.ethereum_wallet_monitor_address_limit)
        || response.selections.len() > MAX_ETHEREUM_WALLET_SELECTIONS
    {
        return Err(EthereumWalletApiError::InvalidResponse);
    }
    let mut addresses = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !addresses.insert(selection.address.as_str()) {
            return Err(EthereumWalletApiError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_selection(selection: &EthereumWalletSelection) -> Result<(), EthereumWalletApiError> {
    if ethereum_wallet_address(&selection.address).is_none()
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(EthereumWalletApiError::InvalidResponse);
    }
    Ok(())
}

pub fn sort_ethereum_wallet_selections(selections: &mut [EthereumWalletSelection]) {
    selections.sort_by(compare_ethereum_wallet_selections);
}

pub fn compare_ethereum_wallet_selections(
    left: &EthereumWalletSelection,
    right: &EthereumWalletSelection,
) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.address.cmp(&right.address))
}

/// Returns the address when it is a canonical, lowercase Ethereum address.
pub fn ethereum_wallet_address(value: &str) -> Option<&str> {
    (value.len() == MAX_ETHEREUM_WALLET_ADDRESS_BYTES
        && value.starts_with("0x")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(value)
}

pub fn valid_ethereum_wallet_transfer_amount(value: &str) -> bool {
    valid_transfer_amount(value, MAX_ETHEREUM_INTEGER)
}

fn valid_transfer_amount(value: &str, maximum: &str) -> bool {
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
    use super::*;

    fn selection(address: &str, status: EthereumWalletSelectionStatus) -> EthereumWalletSelection {
        EthereumWalletSelection {
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
            EthereumWalletApiError::Conflict
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError {
                error: KytrixServiceError::Server(ErrorCode::MutationUnknown),
                possibly_submitted: true,
            }),
            EthereumWalletMutationOutcome::Unknown(EthereumWalletMutationUnknown::ResponseLost)
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError::not_submitted(KytrixServiceError::Timeout)),
            EthereumWalletMutationOutcome::Rejected(EthereumWalletApiError::Timeout)
        );
    }

    #[test]
    fn contract_validates_canonical_addresses_and_amounts() {
        assert_eq!(
            ethereum_wallet_address("0x0123456789abcdef0123456789abcdef01234567"),
            Some("0x0123456789abcdef0123456789abcdef01234567")
        );
        assert!(
            ethereum_wallet_address(&"1".repeat(MAX_ETHEREUM_WALLET_ADDRESS_BYTES + 1)).is_none()
        );
        assert!(ethereum_wallet_address("0x0123456789ABCDEF0123456789abcdef01234567").is_none());
        assert!(ethereum_wallet_address("0x0123456789abcdef0123456789abcdef0123456").is_none());
        assert!(valid_ethereum_wallet_transfer_amount("100.000001"));
        assert!(!valid_ethereum_wallet_transfer_amount("1e6"));
        assert!(!valid_ethereum_wallet_transfer_amount("01"));
        assert!(!valid_ethereum_wallet_transfer_amount("1.0"));
        assert!(!valid_ethereum_wallet_transfer_amount("-1"));
    }

    #[test]
    fn contract_accepts_canonical_zero_amount() {
        assert!(valid_ethereum_wallet_transfer_amount("0"));
    }

    #[test]
    fn maximum_integer_rejects_a_fraction() {
        assert!(!valid_ethereum_wallet_transfer_amount(&format!(
            "{MAX_ETHEREUM_INTEGER}.1"
        )));
    }

    #[test]
    fn selection_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_ethereum_wallet_selections(&EthereumWalletSelectionsResponse {
                selections: Vec::new(),
                ethereum_wallet_monitor_address_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_accepts_paused_history_above_the_current_limit() {
        assert_eq!(
            validate_ethereum_wallet_selections(&EthereumWalletSelectionsResponse {
                selections: vec![
                    selection(
                        "0x0123456789abcdef0123456789abcdef01234567",
                        EthereumWalletSelectionStatus::Paused,
                    ),
                    selection(
                        "0x89abcdef0123456789abcdef0123456789abcdef",
                        EthereumWalletSelectionStatus::Paused,
                    ),
                ],
                ethereum_wallet_monitor_address_limit: 1,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_rejects_a_limit_above_the_process_bound() {
        assert_eq!(
            validate_ethereum_wallet_selections(&EthereumWalletSelectionsResponse {
                selections: Vec::new(),
                ethereum_wallet_monitor_address_limit: MAX_ETHEREUM_WALLET_SELECTIONS + 1,
            }),
            Err(EthereumWalletApiError::InvalidResponse)
        );
    }

    #[test]
    fn selection_contract_rejects_a_non_canonical_address() {
        assert_eq!(
            validate_ethereum_wallet_selections(&EthereumWalletSelectionsResponse {
                selections: vec![selection(
                    "0x0123456789ABCDEF0123456789abcdef01234567",
                    EthereumWalletSelectionStatus::Active,
                )],
                ethereum_wallet_monitor_address_limit: 1,
            }),
            Err(EthereumWalletApiError::InvalidResponse)
        );
    }

    #[test]
    fn selections_are_ordered_by_recency_then_stable_identity() {
        let selection = |address: &str, created_at, updated_at| EthereumWalletSelection {
            address: address.into(),
            status: EthereumWalletSelectionStatus::Active,
            created_at,
            updated_at,
        };
        let mut selections = vec![
            selection("0x3333333333333333333333333333333333333333", 3, 4),
            selection("0x2222222222222222222222222222222222222222", 2, 5),
            selection("0x1111111111111111111111111111111111111111", 2, 5),
            selection("0x4444444444444444444444444444444444444444", 1, 4),
        ];

        sort_ethereum_wallet_selections(&mut selections);

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
