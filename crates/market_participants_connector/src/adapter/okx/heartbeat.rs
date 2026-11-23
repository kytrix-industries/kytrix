use std::time::Duration;

use crate::connection::Keepalive;

pub(super) const KEEPALIVE: Keepalive = Keepalive {
    message: "ping",
    answer: "pong",
    interval: Duration::from_secs(20),
    maximum_missed: 2,
    send_while_active: false,
};

pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);
