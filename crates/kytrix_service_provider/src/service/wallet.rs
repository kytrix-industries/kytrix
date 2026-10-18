use std::cmp::Ordering;

use ahash::AHashSet;
use kytrix_proto::types::{
    WalletNetwork as ProtoWalletNetwork, WalletSelection as ProtoWalletSelection,
    WalletSelectionStatus as ProtoWalletSelectionStatus,
};
use network::transport::error::TransportError;
use reqwest::StatusCode;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;

use crate::service::engine::{ErrorCode, KytrixServiceError, RpcCallError};
pub const MAX_WALLET_SELECTIONS: usize = 5_000;
pub const MAX_WALLET_ADDRESS_BYTES: usize = 44;
pub(super) const MAX_TRANSFER_AMOUNT_BYTES: usize = 334;
const MAX_TRANSFER_FRACTION_DIGITS: usize = 255;
pub(super) const MAX_ETHEREUM_INTEGER: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";
pub(super) const MAX_SOLANA_INTEGER: &str = "18446744073709551615";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WalletNetwork {
    #[serde(rename = "ethereum")]
    Ethereum,
    #[serde(rename = "solana")]
    Solana,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum WalletSelectionStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WalletSelection {
    pub network: WalletNetwork,
    pub address: SmolStr,
    pub status: WalletSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WalletSelectionsResponse {
    pub selections: Vec<WalletSelection>,
    pub wallet_address_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WalletApiError {
    #[error("the wallet address is invalid")]
    InvalidAddress,
    #[error("the wallet address is already selected or the quota is full")]
    Conflict,
    #[error("the wallet selection was not found")]
    NotFound,
    #[error("wallet monitoring is unavailable")]
    Unavailable,
    #[error("Wallet API authentication failed")]
    Authentication,
    #[error("Wallet API request failed: {0}")]
    Network(SmolStr),
    #[error("Wallet API service error: {0}")]
    Service(KytrixServiceError),
    #[error("Wallet API transport error: {0}")]
    Transport(TransportError),
    #[error("Wallet API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("Wallet API response is too large")]
    ResponseTooLarge,
    #[error("Wallet API returned invalid data")]
    InvalidResponse,
    #[error("Wallet API returned HTTP {0}")]
    Server(StatusCode),
    #[error("Wallet API request timed out")]
    Timeout,
    #[error("Wallet API account session is closed")]
    Closed,
    #[error("Wallet API account session changed")]
    AccountChanged,
    #[error("wallet monitor account session is not ready")]
    NotReady,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WalletMutationUnknown {
    #[error("the wallet mutation timed out")]
    Timeout,
    #[error("the wallet mutation transport result is unknown")]
    Transport,
    #[error("the wallet mutation response was lost")]
    ResponseLost,
    #[error("the wallet mutation response was invalid")]
    InvalidResponse,
    #[error("the wallet mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the wallet mutation account session changed")]
    AccountChanged,
    #[error("the wallet mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalletMutationOutcome<T> {
    Confirmed(T),
    Rejected(WalletApiError),
    Unknown(WalletMutationUnknown),
}

pub(super) fn decode_wallet_selection(
    selection: ProtoWalletSelection,
) -> Result<WalletSelection, WalletApiError> {
    let network = match ProtoWalletNetwork::try_from(selection.network)
        .map_err(|_| WalletApiError::InvalidResponse)?
    {
        ProtoWalletNetwork::Ethereum => WalletNetwork::Ethereum,
        ProtoWalletNetwork::Solana => WalletNetwork::Solana,
        ProtoWalletNetwork::Unspecified => return Err(WalletApiError::InvalidResponse),
    };
    let status = match ProtoWalletSelectionStatus::try_from(selection.status)
        .map_err(|_| WalletApiError::InvalidResponse)?
    {
        ProtoWalletSelectionStatus::Active => WalletSelectionStatus::Active,
        ProtoWalletSelectionStatus::Paused => WalletSelectionStatus::Paused,
        ProtoWalletSelectionStatus::Unspecified => return Err(WalletApiError::InvalidResponse),
    };
    let value = WalletSelection {
        network,
        address: selection.address.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub(super) fn map_rpc_error(error: KytrixServiceError) -> WalletApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            WalletApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => WalletApiError::InvalidAddress,
        KytrixServiceError::Server(ErrorCode::NotFound) => WalletApiError::NotFound,
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => WalletApiError::Conflict,
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            WalletApiError::Timeout
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => WalletApiError::NotReady,
        KytrixServiceError::Server(_) => WalletApiError::Unavailable,
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            WalletApiError::Authentication
        }
        KytrixServiceError::Closed => WalletApiError::Closed,
        other => WalletApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(error: RpcCallError) -> WalletMutationOutcome<T> {
    let (error, possibly_submitted) = error.into_parts();
    if !possibly_submitted {
        return WalletMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::Timeout)
        }
        KytrixServiceError::Closed => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::ServiceClosed)
        }
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::AccountChanged)
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Write(_) => {
            WalletMutationOutcome::Unknown(WalletMutationUnknown::Transport)
        }
        error => WalletMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub(super) fn validate_selections(
    response: &WalletSelectionsResponse,
) -> Result<(), WalletApiError> {
    if !(0..=MAX_WALLET_SELECTIONS).contains(&response.wallet_address_limit)
        || response.selections.len() > MAX_WALLET_SELECTIONS
    {
        return Err(WalletApiError::InvalidResponse);
    }
    let mut addresses = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !addresses.insert((selection.network, selection.address.as_str())) {
            return Err(WalletApiError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_selection(selection: &WalletSelection) -> Result<(), WalletApiError> {
    if wallet_network(&selection.address) != Some(selection.network)
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(WalletApiError::InvalidResponse);
    }
    Ok(())
}

pub(super) fn sort_selections(selections: &mut [WalletSelection]) {
    selections.sort_by(compare_wallet_selections);
}

pub fn compare_wallet_selections(left: &WalletSelection, right: &WalletSelection) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.network.cmp(&right.network))
        .then_with(|| left.address.cmp(&right.address))
}

pub fn wallet_network(value: &str) -> Option<WalletNetwork> {
    if value.len() > MAX_WALLET_ADDRESS_BYTES {
        return None;
    }
    if value.len() == 42
        && value.starts_with("0x")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Some(WalletNetwork::Ethereum);
    }
    let mut key = [0_u8; 32];
    (bs58::decode(value).onto(&mut key).ok() == Some(key.len())
        && bs58::encode(key).into_string() == value)
        .then_some(WalletNetwork::Solana)
}

pub(super) fn valid_transfer_amount(value: &str, network: WalletNetwork) -> bool {
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
        WalletNetwork::Ethereum => MAX_ETHEREUM_INTEGER,
        WalletNetwork::Solana => MAX_SOLANA_INTEGER,
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
        ErrorCode, KytrixServiceError, MAX_ETHEREUM_INTEGER, MAX_SOLANA_INTEGER,
        MAX_WALLET_ADDRESS_BYTES, MAX_WALLET_SELECTIONS, RpcCallError, WalletApiError,
        WalletMutationOutcome, WalletMutationUnknown, WalletNetwork, WalletSelection,
        WalletSelectionStatus, WalletSelectionsResponse, map_rpc_error, mutation_rpc_error,
        sort_selections, valid_transfer_amount, validate_selections, wallet_network,
    };

    fn selection(address: &str, status: WalletSelectionStatus) -> WalletSelection {
        WalletSelection {
            network: WalletNetwork::Ethereum,
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
            WalletApiError::Conflict
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError {
                error: KytrixServiceError::Server(ErrorCode::MutationUnknown),
                possibly_submitted: true,
            }),
            WalletMutationOutcome::Unknown(WalletMutationUnknown::ResponseLost)
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError::not_submitted(KytrixServiceError::Timeout)),
            WalletMutationOutcome::Rejected(WalletApiError::Timeout)
        );
    }

    #[test]
    fn wallet_contract_validates_canonical_addresses_and_amounts() {
        assert_eq!(
            wallet_network("0x0123456789abcdef0123456789abcdef01234567"),
            Some(WalletNetwork::Ethereum)
        );
        assert_eq!(
            wallet_network("11111111111111111111111111111111"),
            Some(WalletNetwork::Solana)
        );
        assert!(wallet_network(&"1".repeat(MAX_WALLET_ADDRESS_BYTES + 1)).is_none());
        assert!(wallet_network("0x0123456789ABCDEF0123456789abcdef01234567").is_none());
        assert!(valid_transfer_amount("100.000001", WalletNetwork::Ethereum));
        assert!(!valid_transfer_amount("1e6", WalletNetwork::Ethereum));
        assert!(!valid_transfer_amount("01", WalletNetwork::Ethereum));
        assert!(!valid_transfer_amount("1.0", WalletNetwork::Ethereum));
        assert!(!valid_transfer_amount("-1", WalletNetwork::Ethereum));
    }

    #[test]
    fn wallet_contract_accepts_canonical_zero_amount() {
        assert!(valid_transfer_amount("0", WalletNetwork::Ethereum));
    }

    #[test]
    fn ethereum_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_ETHEREUM_INTEGER}.1"),
            WalletNetwork::Ethereum,
        ));
    }

    #[test]
    fn solana_maximum_integer_rejects_a_fraction() {
        assert!(!valid_transfer_amount(
            &format!("{MAX_SOLANA_INTEGER}.1"),
            WalletNetwork::Solana,
        ));
    }

    #[test]
    fn selection_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_selections(&WalletSelectionsResponse {
                selections: Vec::new(),
                wallet_address_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_accepts_paused_history_above_the_current_limit() {
        assert_eq!(
            validate_selections(&WalletSelectionsResponse {
                selections: vec![
                    selection(
                        "0x0123456789abcdef0123456789abcdef01234567",
                        WalletSelectionStatus::Paused,
                    ),
                    selection(
                        "0x89abcdef0123456789abcdef0123456789abcdef",
                        WalletSelectionStatus::Paused,
                    ),
                ],
                wallet_address_limit: 1,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_rejects_a_limit_above_the_process_bound() {
        assert_eq!(
            validate_selections(&WalletSelectionsResponse {
                selections: Vec::new(),
                wallet_address_limit: MAX_WALLET_SELECTIONS + 1,
            }),
            Err(WalletApiError::InvalidResponse)
        );
    }

    #[test]
    fn selections_are_ordered_by_recency_then_stable_identity() {
        let selection = |address: &str, created_at, updated_at| WalletSelection {
            network: WalletNetwork::Ethereum,
            address: address.into(),
            status: WalletSelectionStatus::Active,
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
