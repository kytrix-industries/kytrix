use std::time::Duration;

use crate::connection::Keepalive;

pub(super) const KEEPALIVE: Keepalive = Keepalive {
    message: "PING",
    answer: "PONG",
    interval: Duration::from_secs(10),
    maximum_missed: 2,
    send_while_active: true,
};

pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);
