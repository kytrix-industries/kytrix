//! The keepalive Hyperliquid expects from a connection it has not spoken on.

use std::time::Duration;

use crate::connection::Keepalive;

/// Hyperliquid closes any connection it has not itself sent a message on for 60 seconds,
/// and answers the JSON `{"method":"ping"}` with `{"channel":"pong"}`.
///
/// The message is therefore sent well inside that window, and a connection that leaves two
/// messages in a row unanswered is treated as dead rather than kept alive on paper.
pub(super) const KEEPALIVE: Keepalive = Keepalive {
    message: r#"{"method":"ping"}"#,
    answer: r#"{"channel":"pong"}"#,
    interval: Duration::from_secs(20),
    maximum_missed: 2,
};

/// How long a Hyperliquid connection may stay silent and still be trusted.
///
/// The venue answers the keepalive every 20 seconds, so three of those answer for a
/// connection that is alive.
pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);
