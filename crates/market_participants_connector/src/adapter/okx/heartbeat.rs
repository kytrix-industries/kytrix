//! The keepalive OKX expects from a connection that stops carrying data.

use std::time::Duration;

use crate::connection::Keepalive;

/// OKX closes a connection that has carried neither data nor a client `ping` for 30
/// seconds, and answers the text `ping` with `pong`.
///
/// The message is therefore sent well inside that window, and a connection that leaves two
/// messages in a row unanswered is treated as dead rather than kept alive on paper.
pub(super) const KEEPALIVE: Keepalive = Keepalive {
    message: "ping",
    answer: "pong",
    interval: Duration::from_secs(20),
    maximum_missed: 2,
};

/// How long an OKX connection may stay silent and still be trusted.
///
/// The venue pushes or answers the keepalive every 20 seconds, so three of those answer for
/// a connection that is alive.
pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);
