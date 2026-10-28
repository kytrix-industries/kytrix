use std::cmp::Ordering;

use ahash::AHashSet;
use kytrix_proto::types::v1::{
    SolanaWalletSelection as ProtoSolanaWalletSelection,
    SolanaWalletSelectionStatus as ProtoSolanaWalletSelectionStatus,
};
use network::transport::error::TransportError;
use reqwest::StatusCode;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;

use crate::service::engine::{ErrorCode, KytrixServiceError, RpcCallError};

pub const MAX_SOLANA_WALLET_SELECTIONS: usize = 5_000;
pub const MAX_SOLANA_WALLET_ADDRESS_BYTES: usize = 44;
const MAX_TRANSFER_AMOUNT_BYTES: usize = 334;
const MAX_TRANSFER_FRACTION_DIGITS: usize = 255;
pub const MAX_SOLANA_INTEGER: &str = "18446744073709551615";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum SolanaWalletSelectionStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SolanaWalletSelection {
    pub address: SmolStr,
    pub status: SolanaWalletSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SolanaWalletSelectionsResponse {
    pub selections: Vec<SolanaWalletSelection>,
    pub solana_wallet_monitor_address_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SolanaWalletApiError {
    #[error("the Solana wallet address is invalid")]
    InvalidAddress,
    #[error("the Solana wallet address is already selected or the quota is full")]
    Conflict,
    #[error("the Solana wallet selection was not found")]
    NotFound,
    #[error("Solana wallet monitoring is unavailable")]
    Unavailable,
    #[error("Solana wallet API authentication failed")]
    Authentication,
    #[error("Solana wallet API request failed: {0}")]
    Network(SmolStr),
    #[error("Solana wallet API service error: {0}")]
    Service(KytrixServiceError),
    #[error("Solana wallet API transport error: {0}")]
    Transport(TransportError),
    #[error("Solana wallet API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("Solana wallet API response is too large")]
    ResponseTooLarge,
    #[error("Solana wallet API returned invalid data")]
    InvalidResponse,
    #[error("Solana wallet API returned HTTP {0}")]
    Server(StatusCode),
    #[error("Solana wallet API request timed out")]
    Timeout,
    #[error("Solana wallet API account session is closed")]
    Closed,
    #[error("Solana wallet API account session changed")]
    AccountChanged,
    #[error("Solana wallet monitoring is not ready")]
    NotReady,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SolanaWalletMutationUnknown {
    #[error("the Solana wallet mutation timed out")]
    Timeout,
    #[error("the Solana wallet mutation transport result is unknown")]
    Transport,
    #[error("the Solana wallet mutation response was lost")]
    ResponseLost,
    #[error("the Solana wallet mutation response was invalid")]
    InvalidResponse,
    #[error("the Solana wallet mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the Solana wallet mutation account session changed")]
    AccountChanged,
    #[error("the Solana wallet mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SolanaWalletMutationOutcome<T> {
    Confirmed(T),
    Rejected(SolanaWalletApiError),
    Unknown(SolanaWalletMutationUnknown),
}

pub fn decode_solana_wallet_selection(
    selection: ProtoSolanaWalletSelection,
) -> Result<SolanaWalletSelection, SolanaWalletApiError> {
    let status = match ProtoSolanaWalletSelectionStatus::try_from(selection.status)
        .map_err(|_| SolanaWalletApiError::InvalidResponse)?
    {
        ProtoSolanaWalletSelectionStatus::Active => SolanaWalletSelectionStatus::Active,
        ProtoSolanaWalletSelectionStatus::Paused => SolanaWalletSelectionStatus::Paused,
        ProtoSolanaWalletSelectionStatus::Unspecified => {
            return Err(SolanaWalletApiError::InvalidResponse);
        }
    };
    let value = SolanaWalletSelection {
        address: selection.address.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub fn map_rpc_error(error: KytrixServiceError) -> SolanaWalletApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            SolanaWalletApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => {
            SolanaWalletApiError::InvalidAddress
        }
        KytrixServiceError::Server(ErrorCode::NotFound) => SolanaWalletApiError::NotFound,
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => SolanaWalletApiError::Conflict,
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            SolanaWalletApiError::Timeout
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => SolanaWalletApiError::NotReady,
        KytrixServiceError::Server(_) => SolanaWalletApiError::Unavailable,
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            SolanaWalletApiError::Authentication
        }
        KytrixServiceError::Closed => SolanaWalletApiError::Closed,
        other => SolanaWalletApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(error: RpcCallError) -> SolanaWalletMutationOutcome<T> {
    let (error, possibly_submitted) = error.into_parts();
    if !possibly_submitted {
        return SolanaWalletMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::Timeout)
        }
        KytrixServiceError::Closed => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::ServiceClosed)
        }
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::AccountChanged)
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Write(_) => {
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::Transport)
        }
        error => SolanaWalletMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub fn validate_solana_wallet_selections(
    response: &SolanaWalletSelectionsResponse,
) -> Result<(), SolanaWalletApiError> {
    if !(0..=MAX_SOLANA_WALLET_SELECTIONS).contains(&response.solana_wallet_monitor_address_limit)
        || response.selections.len() > MAX_SOLANA_WALLET_SELECTIONS
    {
        return Err(SolanaWalletApiError::InvalidResponse);
    }
    let mut addresses = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !addresses.insert(selection.address.as_str()) {
            return Err(SolanaWalletApiError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_selection(selection: &SolanaWalletSelection) -> Result<(), SolanaWalletApiError> {
    if solana_wallet_address(&selection.address).is_none()
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(SolanaWalletApiError::InvalidResponse);
    }
    Ok(())
}

pub fn sort_solana_wallet_selections(selections: &mut [SolanaWalletSelection]) {
    selections.sort_by(compare_solana_wallet_selections);
}

pub fn compare_solana_wallet_selections(
    left: &SolanaWalletSelection,
    right: &SolanaWalletSelection,
) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.address.cmp(&right.address))
}

/// Returns the address when it is a canonical base58 encoding of 32 bytes.
pub fn solana_wallet_address(value: &str) -> Option<&str> {
    if value.len() > MAX_SOLANA_WALLET_ADDRESS_BYTES {
        return None;
    }
    let mut key = [0_u8; 32];
    (bs58::decode(value).onto(&mut key).ok() == Some(key.len())
        && bs58::encode(key).into_string() == value)
        .then_some(value)
}

pub fn valid_solana_wallet_transfer_amount(value: &str) -> bool {
    valid_transfer_amount(value, MAX_SOLANA_INTEGER)
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

    fn selection(address: &str, status: SolanaWalletSelectionStatus) -> SolanaWalletSelection {
        SolanaWalletSelection {
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
            SolanaWalletApiError::Conflict
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError {
                error: KytrixServiceError::Server(ErrorCode::MutationUnknown),
                possibly_submitted: true,
            }),
            SolanaWalletMutationOutcome::Unknown(SolanaWalletMutationUnknown::ResponseLost)
        );
        assert_eq!(
            mutation_rpc_error::<()>(RpcCallError::not_submitted(KytrixServiceError::Timeout)),
            SolanaWalletMutationOutcome::Rejected(SolanaWalletApiError::Timeout)
        );
    }

    #[test]
    fn contract_validates_canonical_addresses_and_amounts() {
        assert_eq!(
            solana_wallet_address("11111111111111111111111111111111"),
            Some("11111111111111111111111111111111")
        );
        assert!(solana_wallet_address(&"1".repeat(MAX_SOLANA_WALLET_ADDRESS_BYTES + 1)).is_none());
        assert!(solana_wallet_address("0OIl").is_none());
        assert!(valid_solana_wallet_transfer_amount("100.000001"));
        assert!(!valid_solana_wallet_transfer_amount("1e6"));
        assert!(!valid_solana_wallet_transfer_amount("01"));
        assert!(!valid_solana_wallet_transfer_amount("1.0"));
        assert!(!valid_solana_wallet_transfer_amount("-1"));
    }

    #[test]
    fn contract_accepts_canonical_zero_amount() {
        assert!(valid_solana_wallet_transfer_amount("0"));
    }

    #[test]
    fn maximum_integer_rejects_a_fraction() {
        assert!(!valid_solana_wallet_transfer_amount(&format!(
            "{MAX_SOLANA_INTEGER}.1"
        )));
    }

    #[test]
    fn selection_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_solana_wallet_selections(&SolanaWalletSelectionsResponse {
                selections: Vec::new(),
                solana_wallet_monitor_address_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_accepts_paused_history_above_the_current_limit() {
        assert_eq!(
            validate_solana_wallet_selections(&SolanaWalletSelectionsResponse {
                selections: vec![
                    selection(
                        "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
                        SolanaWalletSelectionStatus::Paused,
                    ),
                    selection(
                        "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
                        SolanaWalletSelectionStatus::Paused,
                    ),
                ],
                solana_wallet_monitor_address_limit: 1,
            }),
            Ok(())
        );
    }

    #[test]
    fn selection_contract_rejects_a_limit_above_the_process_bound() {
        assert_eq!(
            validate_solana_wallet_selections(&SolanaWalletSelectionsResponse {
                selections: Vec::new(),
                solana_wallet_monitor_address_limit: MAX_SOLANA_WALLET_SELECTIONS + 1,
            }),
            Err(SolanaWalletApiError::InvalidResponse)
        );
    }

    #[test]
    fn selection_contract_rejects_an_ethereum_address() {
        assert_eq!(
            validate_solana_wallet_selections(&SolanaWalletSelectionsResponse {
                selections: vec![selection(
                    "0x0123456789abcdef0123456789abcdef01234567",
                    SolanaWalletSelectionStatus::Active,
                )],
                solana_wallet_monitor_address_limit: 1,
            }),
            Err(SolanaWalletApiError::InvalidResponse)
        );
    }

    #[test]
    fn selections_are_ordered_by_recency_then_stable_identity() {
        let selection = |address: &str, created_at, updated_at| SolanaWalletSelection {
            address: address.into(),
            status: SolanaWalletSelectionStatus::Active,
            created_at,
            updated_at,
        };
        let mut selections = vec![
            selection("CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8", 3, 4),
            selection("8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR", 2, 5),
            selection("4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi", 2, 5),
            selection("GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq", 1, 4),
        ];

        sort_solana_wallet_selections(&mut selections);

        assert_eq!(
            selections
                .iter()
                .map(|selection| selection.address.as_str())
                .collect::<Vec<_>>(),
            [
                "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
                "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
                "CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8",
                "GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq",
            ]
        );
    }
}
