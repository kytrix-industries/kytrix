//! Errors produced by generic network, proxy, and transport operations.

use smol_str::SmolStr;
use thiserror::Error;

use crate::transport::error::UpgradeFailure;

/// Typed failures that do not depend on any market participant or trading capability domain.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum NetworkError {
    #[error("DNS resolution failed for {host}")]
    DnsResolutionFailed { host: SmolStr },
    #[error("TCP connection failed: {endpoint}")]
    TcpConnectionFailed { endpoint: SmolStr },
    #[error("TLS handshake failed: {endpoint}")]
    TlsHandshakeFailed { endpoint: SmolStr },
    #[error("WebSocket upgrade failed for {endpoint}: {failure}")]
    WebSocketUpgradeFailed {
        endpoint: SmolStr,
        failure: UpgradeFailure,
    },
    #[error("proxy connection failed ({proxy_type}): {message}")]
    ProxyConnectionFailed {
        proxy_type: SmolStr,
        message: SmolStr,
    },
    #[error("I/O error: {message}")]
    Io { message: SmolStr },
    #[error("invalid data while attempting to {operation}: {reason}")]
    InvalidData { operation: SmolStr, reason: SmolStr },
    #[error("{operation} exceeded the {limit}-item limit")]
    LimitExceeded { operation: SmolStr, limit: usize },
}
