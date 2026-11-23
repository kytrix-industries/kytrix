use smol_str::SmolStr;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UpgradeFailure {
    #[error("the connect attempt timed out")]
    Timeout,
    #[error("the transport failed: {detail}")]
    Transport { detail: SmolStr },
    #[error("the peer answered {status} instead of switching protocols")]
    Rejected { status: u16 },
    #[error("the peer handshake is not usable: {detail}")]
    Handshake { detail: SmolStr },
}

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
