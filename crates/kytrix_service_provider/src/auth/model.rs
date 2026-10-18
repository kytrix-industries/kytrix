use std::{fmt, str::FromStr};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

use super::dpop::DpopKey;
use crate::account_profile::AccountProfile;

/// Subscription tier returned by the account OAuth service.
///
/// This value is part of the account JSON response (`kytrix_plan_code`), not
/// the Kytrix websocket protobuf protocol.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum PlanCode {
    #[serde(rename = "free")]
    Free,
    #[serde(rename = "plus")]
    Plus,
    #[serde(rename = "pro")]
    Pro,
}

impl PlanCode {
    pub const ALL: [Self; 3] = [Self::Free, Self::Plus, Self::Pro];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Plus => "plus",
            Self::Pro => "pro",
        }
    }

    pub const fn level(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Plus => 1,
            Self::Pro => 2,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Plus => "Plus",
            Self::Pro => "Pro",
        }
    }
}

impl fmt::Display for PlanCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PlanCode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "free" => Ok(Self::Free),
            "plus" => Ok(Self::Plus),
            "pro" => Ok(Self::Pro),
            _ => Err("unknown plan_code"),
        }
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("the operating system credential store is unavailable")]
    CredentialStore,
    #[error("no desktop login credential is available")]
    MissingCredential,
    #[error("the desktop login credential is invalid")]
    InvalidCredential,
    #[error("failed to initialize the OAuth HTTP client")]
    HttpClient,
    #[error("the OAuth proxy route is invalid")]
    Route,
    #[error("the OAuth request failed")]
    Request,
    #[error("the OAuth refresh rotation outcome is unknown")]
    RefreshOutcomeUnknown,
    #[error("the OAuth service returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("the OAuth response is invalid")]
    InvalidResponse,
    #[error("the OAuth refresh token is no longer valid")]
    InvalidGrant,
    #[error("system time is before the Unix epoch")]
    SystemTime,
    #[error("failed to generate secure DPoP data")]
    SecureRandom,
    #[error("the desktop authentication session is unavailable")]
    SessionClosed,
    #[error("the desktop authentication sequence is exhausted")]
    SequenceExhausted,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state")]
pub(super) enum StoredCredential {
    #[serde(rename = "active")]
    Active {
        user_id: String,
        refresh_token: String,
        private_key: String,
    },
    #[serde(rename = "refresh_outcome_unknown")]
    RefreshOutcomeUnknown,
    #[serde(rename = "invalidated")]
    Invalidated,
}

pub(super) enum LoadedCredential {
    Active {
        user_id: String,
        refresh_token: String,
        key: DpopKey,
    },
    RefreshOutcomeUnknown,
    Invalidated,
}

pub(super) struct Session {
    pub user_id: String,
    pub plan_code: PlanCode,
    pub revision: u64,
    pub credential_epoch: u64,
    pub refresh_token: Option<String>,
    pub key: DpopKey,
    pub access: Option<AccessToken>,
}

pub(super) enum SessionState {
    Unloaded,
    Active(Box<Session>),
    RefreshOutcomeUnknown,
    Invalidated,
    Exhausted,
}

pub(super) struct AccessToken {
    pub value: String,
    pub expires_at: i64,
}

pub(super) struct RefreshedCredential {
    pub refresh_token: String,
    pub access: AccessToken,
    pub plan_code: PlanCode,
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub refresh_token: String,
    pub scope: String,
    pub kytrix_user_id: String,
    pub kytrix_wallet_kind: TokenWalletKind,
    pub kytrix_wallet_address: String,
    pub kytrix_plan_code: PlanCode,
}

#[derive(Clone, Copy, Deserialize)]
pub(super) enum TokenWalletKind {
    #[serde(rename = "ethereum")]
    Ethereum,
    #[serde(rename = "solana")]
    Solana,
}

/// Wallet family that owns [`AccountIdentity::wallet_address`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AccountWalletKind {
    Ethereum,
    Solana,
}

/// Account identity established by a completed OAuth authorization.
///
/// The address is validated against its wallet family before it appears here,
/// so callers can persist the value without re-deriving the family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountIdentity {
    user_id: String,
    wallet_address: String,
    wallet_kind: AccountWalletKind,
}

impl AccountIdentity {
    pub(crate) fn new(
        user_id: String,
        wallet_address: String,
        wallet_kind: AccountWalletKind,
    ) -> Self {
        Self {
            user_id,
            wallet_address,
            wallet_kind,
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn wallet_address(&self) -> &str {
        &self.wallet_address
    }

    pub const fn wallet_kind(&self) -> AccountWalletKind {
        self.wallet_kind
    }
}

#[derive(Deserialize)]
pub(super) struct OAuthErrorBody {
    pub error: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionAvailability {
    #[default]
    Unknown,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SessionEpoch {
    #[default]
    Initial,
    Revision(u64),
    Exhausted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionChange {
    epoch: SessionEpoch,
    availability: SessionAvailability,
}

impl SessionChange {
    pub const fn availability(self) -> SessionAvailability {
        self.availability
    }

    pub(crate) const fn epoch(self) -> Option<u64> {
        match self.epoch {
            SessionEpoch::Revision(epoch) => Some(epoch),
            SessionEpoch::Initial | SessionEpoch::Exhausted => None,
        }
    }

    pub(crate) const fn is_exhausted(self) -> bool {
        matches!(self.epoch, SessionEpoch::Exhausted)
    }

    pub(crate) const fn published(epoch: u64, availability: SessionAvailability) -> Self {
        Self {
            epoch: SessionEpoch::Revision(epoch),
            availability,
        }
    }

    pub(crate) const fn exhausted() -> Self {
        Self {
            epoch: SessionEpoch::Exhausted,
            availability: SessionAvailability::Unavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSessionStatus {
    observed_change: SessionChange,
    availability: SessionAvailability,
}

impl LocalSessionStatus {
    pub(crate) const fn new(
        observed_change: SessionChange,
        availability: SessionAvailability,
    ) -> Self {
        Self {
            observed_change,
            availability,
        }
    }

    pub const fn observed_change(self) -> SessionChange {
        self.observed_change
    }

    pub const fn availability(self) -> SessionAvailability {
        self.availability
    }
}

pub(super) type AuthorizationResult = Result<(AccountProfile, (String, String)), AuthError>;

pub(super) struct AuthorizationProofContext<'a> {
    pub expected: Option<&'a AccountProfile>,
    pub method: &'a str,
    pub request_url: &'a str,
    pub nonce: Option<&'a str>,
    pub refresh_early_secs: i64,
}

pub(super) enum AuthCommand {
    Install {
        response: TokenResponse,
        key: DpopKey,
        respond_to: oneshot::Sender<Result<(), AuthError>>,
    },
    Profile {
        expected_user_id: String,
        respond_to: oneshot::Sender<Result<AccountProfile, AuthError>>,
    },
    CurrentPlan {
        expected_user_id: String,
        respond_to: oneshot::Sender<Result<Option<PlanCode>, AuthError>>,
    },
    Headers {
        expected: Option<AccountProfile>,
        method: String,
        request_url: String,
        respond_to: oneshot::Sender<AuthorizationResult>,
    },
    RenewalHeaders {
        expected: AccountProfile,
        method: String,
        request_url: String,
        nonce: String,
        respond_to: oneshot::Sender<AuthorizationResult>,
    },
    LocalStatus {
        respond_to: oneshot::Sender<Result<LocalSessionStatus, AuthError>>,
    },
}

pub(super) trait AuthAuthority: Send + Sync + 'static {
    fn exchange_authorization_code<'a>(
        &'a self,
        key: &'a DpopKey,
        code: &'a str,
        redirect_uri: &'a str,
        code_verifier: &'a str,
    ) -> BoxFuture<'a, Result<TokenResponse, AuthError>>;

    fn refresh<'a>(
        &'a self,
        user_id: &'a str,
        refresh_token: &'a str,
        key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<RefreshedCredential, AuthError>>;

    fn revoke<'a>(
        &'a self,
        refresh_token: &'a str,
        key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<(), AuthError>>;
}
