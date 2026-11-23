use std::sync::OnceLock;

use calloop::ping::Ping;

static PING: OnceLock<Ping> = OnceLock::new();

pub(crate) fn install(ping: Ping) {
    let _ = PING.set(ping);
}

pub(crate) fn signal() -> bool {
    match PING.get() {
        Some(ping) => {
            ping.ping();
            true
        }
        None => false,
    }
}
