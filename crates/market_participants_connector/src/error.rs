//! Typed errors shared by market-participant adapters and the network gateway.

use bytes::Bytes;
use market_participants_model::{
    decimal::Quantity,
    identifier::{VenueId, VenueScopeId},
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    routing::CapabilityId,
};
pub(crate) use network::error::NetworkError;
use smol_str::SmolStr;
use thiserror::Error;

use crate::protocol::CommandKind;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DisconnectReason {
    #[error("normal closure")]
    Normal,
    #[error("network error: {0}")]
    Network(NetworkError),
    #[error("server closed connection ({code}): {reason}")]
    ServerClosed { code: u16, reason: SmolStr },
    #[error("proxy configuration changed")]
    ProxyChanged,
    #[error("authentication expired")]
    AuthenticationExpired,
    #[error("heartbeat timed out")]
    HeartbeatTimeout,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("account identity is missing")]
    AccountIdentityMissing,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("API key was not found")]
    ApiKeyNotFound,
    /// The venue's own market-data profile grants no permission for the market read.
    ///
    /// It is the venue's permission profile that answers this, never a refused
    /// command: the profile said the account may not read the market at all, so the
    /// request is refused before anything is sent and no error code is guessed at.
    #[error("the credentials have no market-data permission for this market")]
    NoMarketDataPermission,
    #[error("signature is invalid")]
    SignatureInvalid,
    #[error("request timestamp is outside the allowed window")]
    TimestampOutOfRange,
    #[error("IP address is not whitelisted")]
    IpNotWhitelisted,
    #[error("session has expired")]
    SessionExpired,
    #[error("authentication handshake timed out")]
    HandshakeTimeout,
    #[error("authentication response was malformed")]
    ResponseMalformed,
    #[error("authentication service closed the connection")]
    ServiceClosed,
    /// The venue's stored credential set is only partially filled.
    ///
    /// The venue is carried as a typed identity rather than flattened into a
    /// formatted sentence, so the interface states it in the reader's language: a
    /// diagnostic naming the canonical identity would otherwise be interpolated
    /// into a localized one, naming the venue twice — once in each language.
    ///
    /// The scope is carried beside it because a venue that signs its markets apart can
    /// be half configured: the scope names which of the venue's credential sets the
    /// operator has to finish, and without it the interface would send them to a page
    /// with two of them on it.
    #[error("the stored credentials of venue {venue} are incomplete")]
    IncompleteCredentials {
        venue: VenueId,
        scope: Option<SmolStr>,
    },
    /// The stored credentials cannot be turned into a signer at all.
    ///
    /// A typed venue rather than a formatted sentence: the set the user finished
    /// filling in is unusable — a private key that does not parse — and the interface
    /// has to say whose credentials to enter again, in the reader's language.
    #[error("the stored credentials of venue {venue} cannot be used as a signer")]
    UnusableCredentials { venue: VenueId },
    /// The execution's account identity is not a valid account id.
    ///
    /// A typed value rather than a formatted sentence, so the interface can state it
    /// in the reader's language: the identity came from the caller, not from the wire.
    #[error("the account identity {value} is not a valid account id")]
    InvalidAccountIdentity { value: SmolStr },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OrderErrorKind {
    #[error("invalid price: {reason}")]
    InvalidPrice { reason: SmolStr },
    #[error("invalid quantity: {reason}")]
    InvalidQuantity { reason: SmolStr },
    #[error("order was not found")]
    OrderNotFound,
    #[error("order was already filled")]
    OrderAlreadyFilled,
    #[error("order was already canceled")]
    OrderAlreadyCanceled,
    #[error("position limit exceeded")]
    PositionLimitExceeded,
    #[error("market is suspended")]
    MarketSuspended,
    #[error("self-trade protection prevented the order")]
    SelfTradeProtection,
    #[error("other order error ({code}): {message}")]
    Other { code: SmolStr, message: SmolStr },
}

/// Every failure of the market-participants layer.
///
/// A route is always named by its full `VenueRoute` and a capability by its
/// `CapabilityId`: an error never collapses them into a venue name or a free
/// text reason, because the caller has to be able to act on the exact scope that
/// failed.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MarketParticipantError {
    #[error("connection to {route} at {endpoint} failed: {source}")]
    ConnectionFailed {
        route: VenueRoute,
        endpoint: SmolStr,
        source: NetworkError,
    },
    /// A venue-level request failed: the discovery conversation, which addresses the
    /// venue itself because it asks what the venue publishes rather than reading one
    /// of its scopes.
    #[error("request to venue {venue} at {endpoint} failed: {source}")]
    VenueRequestFailed {
        venue: VenueId,
        endpoint: SmolStr,
        source: NetworkError,
    },
    #[error("authentication for {route} account {account:?} failed: {source}")]
    AuthenticationFailed {
        route: VenueRoute,
        account: Option<SmolStr>,
        source: AuthError,
    },
    #[error("rate limit exceeded")]
    RateLimited {
        retry_after_ms: Option<u64>,
        remaining_quota: Option<u64>,
    },
    #[error("market participant rejected request ({code}): {message}")]
    Rejected {
        code: SmolStr,
        message: SmolStr,
        retryable: bool,
        raw_response: Option<Bytes>,
    },
    #[error("order failed: {kind}")]
    OrderError {
        kind: OrderErrorKind,
        raw_data: Option<Bytes>,
    },
    #[error("insufficient {asset}: required {required}, available {available} ({message})")]
    InsufficientBalance {
        asset: SmolStr,
        required: Quantity,
        available: Quantity,
        message: SmolStr,
    },
    #[error("invalid configuration: {0}")]
    Configuration(SmolStr),
    /// The venue's scopes are all statically known, so it publishes no table to read.
    ///
    /// The venue whose loader would have asked is carried as a typed identity for the
    /// same reason as [`AuthError::IncompleteCredentials`]: the refusal is reported
    /// through a localized template, and a diagnostic that spells the canonical
    /// identity would name the venue in the wrong language beside the catalogue's own
    /// name for it.
    #[error("venue {venue} publishes no scope table to read")]
    VenuePublishesNoScopeTable { venue: VenueId },
    /// The venue's stored credential set is only partially filled.
    ///
    /// Carried as a typed venue for the same reason as
    /// [`AuthError::IncompleteCredentials`], and with the scope beside it for the same
    /// reason: a venue that signs its markets apart can be half configured.
    #[error("the stored credentials of venue {venue} are incomplete")]
    IncompleteCredentials {
        venue: VenueId,
        scope: Option<SmolStr>,
    },
    /// The stored credentials cannot be turned into a signer at all.
    ///
    /// A typed venue rather than a formatted sentence: the set the user finished
    /// filling in is unusable — a private key that does not parse — and the interface
    /// has to say whose credentials to enter again, in the reader's language.
    #[error("the stored credentials of venue {venue} cannot be used as a signer")]
    UnusableCredentials { venue: VenueId },
    /// This build carries no adapter or origin for the venue a route names.
    ///
    /// Every venue this build speaks has both, so a venue that reaches here is one
    /// the build was assembled without rather than one a caller picked. The answer
    /// names the venue rather than the part that is missing, because which part is
    /// missing is a fact about the assembly and not about what was asked for — and
    /// it names it as a typed venue, so the interface states it in the reader's
    /// language instead of spelling the identity into a diagnostic.
    #[error("this build speaks no market named {venue}")]
    VenueUnsupported { venue: VenueId },
    #[error("a Tokio runtime is required for this market-participant operation")]
    RuntimeUnavailable,
    #[error(
        "protocol request requires capability {expected:?}, but request context declares {actual:?}"
    )]
    CapabilityMismatch {
        expected: CapabilityId,
        actual: CapabilityId,
    },
    #[error("protocol request route {request} does not match context route {context}")]
    RouteMismatch {
        request: VenueRoute,
        context: VenueRoute,
    },
    #[error("listing {listing:?} does not belong to route {route}")]
    ListingRouteMismatch {
        listing: VenueListingKey,
        route: VenueRoute,
    },
    #[error(
        "command {command:?} is not allowed while listing {listing:?} is {status:?} (within window: {within_window})"
    )]
    ListingActionNotAllowed {
        listing: VenueListingKey,
        status: ListingStatus,
        command: CommandKind,
        within_window: bool,
    },
    #[error("protocol error while attempting to {operation}: {reason}")]
    ProtocolError {
        operation: SmolStr,
        reason: SmolStr,
        raw_data: Option<Bytes>,
    },
    #[error("failed to decode {operation}")]
    DecodeFailed { operation: SmolStr, raw_data: Bytes },
    #[error("{operation} timed out after {timeout_ms}ms")]
    Timeout { operation: SmolStr, timeout_ms: u64 },
    #[error("capability {capability:?} is unsupported for route {route}")]
    UnsupportedCapability {
        route: VenueRoute,
        capability: CapabilityId,
    },
    #[error("{operation} limit {requested} exceeds maximum {maximum}")]
    InvalidLimit {
        operation: SmolStr,
        requested: u16,
        maximum: u16,
    },
    #[error("{scope} of {venue} has not been loaded")]
    VenueNotLoaded { venue: VenueId, scope: VenueScopeId },
    #[error("{timeframe} is unsupported for route {route}")]
    UnsupportedTimeFrame {
        route: VenueRoute,
        timeframe: SmolStr,
    },
    /// A request asked for a price adjustment the route does not publish.
    #[error("{adjustment} is unsupported for route {route}")]
    UnsupportedAdjustment {
        route: VenueRoute,
        adjustment: SmolStr,
    },
    /// A venue's symbol directory could not be fetched or read.
    ///
    /// The venue is carried as a typed identity for the same reason the credential
    /// errors carry one: the refusal reaches a reader through a localized template,
    /// and a diagnostic spelling the canonical identity would name the venue in the
    /// wrong language beside the catalogue's own name for it. The reason is the
    /// diagnostic detail — the address, the status, the concrete cause — and never
    /// what the reader is shown instead of the sentence.
    #[error("the symbol directory of venue {venue} could not be fetched: {reason}")]
    SymbolDirectoryFetchFailed { venue: VenueId, reason: SmolStr },
    #[error("the catalog reported: {0}")]
    Catalog(Box<crate::catalog::error::CatalogError>),
    #[error("the market-participants model rejected a boundary value: {0}")]
    MarketModel(Box<market_participants_model::error::ModelError>),
    #[error("market participant disconnected: {0}")]
    Disconnected(DisconnectReason),
    #[error("market participant service is closed")]
    ServiceClosed,
    #[error("internal market-participant error: {0}")]
    Internal(SmolStr),
}

impl MarketParticipantError {
    pub const fn connection_failed(
        route: VenueRoute,
        endpoint: SmolStr,
        source: NetworkError,
    ) -> Self {
        Self::ConnectionFailed {
            route,
            endpoint,
            source,
        }
    }

    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::ConnectionFailed { .. }
            | Self::VenueRequestFailed { .. }
            | Self::RateLimited { .. }
            | Self::Timeout { .. }
            // Reading a symbol directory is a network read of a remote answer, so a
            // failure of it is worth retrying: nothing about the request itself was
            // refused.
            | Self::SymbolDirectoryFetchFailed { .. }
            | Self::Disconnected(DisconnectReason::Network(_))
            | Self::Disconnected(DisconnectReason::HeartbeatTimeout) => true,
            Self::Rejected { retryable, .. } => *retryable,
            _ => false,
        }
    }

    pub const fn is_network_error(&self) -> bool {
        matches!(
            self,
            Self::ConnectionFailed { .. }
                | Self::VenueRequestFailed { .. }
                | Self::Disconnected(DisconnectReason::Network(_))
                | Self::Disconnected(DisconnectReason::HeartbeatTimeout)
        )
    }

    pub const fn requires_user_action(&self) -> bool {
        matches!(
            self,
            Self::AuthenticationFailed { .. }
                | Self::InsufficientBalance { .. }
                | Self::Configuration(_)
                | Self::RuntimeUnavailable
                | Self::CapabilityMismatch { .. }
                | Self::UnsupportedCapability { .. }
                | Self::UnsupportedAdjustment { .. }
                | Self::VenueNotLoaded { .. }
                | Self::ListingActionNotAllowed { .. }
                | Self::OrderError {
                    kind: OrderErrorKind::InvalidPrice { .. }
                        | OrderErrorKind::InvalidQuantity { .. }
                        | OrderErrorKind::PositionLimitExceeded
                        | OrderErrorKind::MarketSuspended
                        | OrderErrorKind::SelfTradeProtection,
                    ..
                }
        )
    }

    pub const fn is_rate_limit(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    pub const fn suggested_retry_delay_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_ms: Some(delay),
                ..
            } => Some(*delay),
            Self::RateLimited { .. } => Some(1_000),
            Self::ConnectionFailed { .. } | Self::VenueRequestFailed { .. } => Some(500),
            Self::SymbolDirectoryFetchFailed { .. } => Some(500),
            Self::Timeout { .. } => Some(500),
            Self::Rejected {
                retryable: true, ..
            } => Some(1_000),
            Self::Disconnected(DisconnectReason::Network(_))
            | Self::Disconnected(DisconnectReason::HeartbeatTimeout) => Some(500),
            _ => None,
        }
    }

    pub fn market_participant_code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } => Some(code),
            Self::OrderError {
                kind: OrderErrorKind::Other { code, .. },
                ..
            } => Some(code),
            _ => None,
        }
    }

    pub fn raw_data(&self) -> Option<&Bytes> {
        match self {
            Self::Rejected { raw_response, .. } => raw_response.as_ref(),
            Self::OrderError { raw_data, .. } | Self::ProtocolError { raw_data, .. } => {
                raw_data.as_ref()
            }
            Self::DecodeFailed { raw_data, .. } => Some(raw_data),
            _ => None,
        }
    }
}
