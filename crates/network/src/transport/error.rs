//! Errors produced by HTTP operations, WebSocket sessions, and logical subscriptions.

use smol_str::SmolStr;
use thiserror::Error;

/// Why a WebSocket upgrade never produced a session.
///
/// The handshake is the one operation where the proxy, DNS, TCP, TLS, and HTTP legs
/// collapse into a single failure, and the legs call for different reactions: a
/// timeout is worth repeating as it stands, a refusal is not, and a broken transport
/// has to say which leg broke. Naming the leg is what keeps that decision — and the
/// sentence a user reads — from being one blanket "could not connect".
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UpgradeFailure {
    /// The attempt ran past the connect budget.
    #[error("the connect attempt timed out")]
    Timeout,
    /// No usable response arrived: the proxy, DNS, TCP, TLS, or HTTP leg broke.
    #[error("the transport failed: {detail}")]
    Transport { detail: SmolStr },
    /// The peer answered, but not with the protocol switch the handshake needs.
    #[error("the peer answered {status} instead of switching protocols")]
    Rejected { status: u16 },
    /// The peer switched protocols, but the answer cannot start a session.
    #[error("the peer handshake is not usable: {detail}")]
    Handshake { detail: SmolStr },
}

/// Typed failures shared by HTTP operations and logical subscriptions.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("failed to configure the selected proxy route")]
    ProxyConfiguration,
    #[error("failed to construct the HTTP client")]
    HttpClientConfiguration,
    #[error("the HTTP client pool is unavailable")]
    ClientPoolUnavailable,
    #[error("the network request timed out")]
    RequestTimedOut,
    #[error("failed while reading the response body")]
    ResponseReadFailed,
    #[error("response body exceeds the {limit}-byte limit")]
    ResponseBodyTooLarge { limit: usize },
    #[error("operation cancelled")]
    Cancelled,
    #[error("connection closed")]
    Disconnected,
    #[error("subscriber could not keep up with the stream")]
    SlowSubscriber,
    #[error("the configured network route changed")]
    RouteChanged,
    #[error("the authentication generation changed")]
    CredentialChanged,
    #[error("failed to restore a consistent subscription snapshot")]
    ResyncFailed,
    #[error("protocol error while attempting to {operation}")]
    Protocol { operation: &'static str },
    #[error("the remote subscription was rejected ({code}): {message}")]
    SubscriptionRejected { code: SmolStr, message: SmolStr },
    #[error("subscription closed")]
    Closed,
    #[error("subscription capacity must be greater than zero")]
    InvalidSubscriptionCapacity,
    #[error("the WebSocket endpoint is invalid")]
    InvalidWebSocketEndpoint,
    #[error("failed to establish the WebSocket connection: {failure}")]
    WebSocketConnect { failure: UpgradeFailure },
    #[error("the WebSocket connection failed")]
    WebSocketIo,
}
