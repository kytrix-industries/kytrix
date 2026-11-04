//! The keepalive Polymarket expects from a market-channel connection.
//!
//! The CLOB market channel closes a connection that sends no `PING` for ten
//! seconds. Any data frame restarts the silence window, so the message is only
//! sent while the socket is quiet; a connection that leaves two answers in a row
//! missing is treated as dead.

use std::time::Duration;

use crate::connection::Keepalive;

pub(super) const KEEPALIVE: Keepalive = Keepalive {
    message: "PING",
    answer: "PONG",
    interval: Duration::from_secs(10),
    maximum_missed: 2,
    send_while_active: true,
};

/// How long a market-channel connection may stay silent and still be trusted.
pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);
