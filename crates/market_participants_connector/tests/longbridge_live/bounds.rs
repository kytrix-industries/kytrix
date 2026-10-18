//! Every wait this suite takes, in one place.
//!
//! Each case wraps its own work in [`CASE_TIMEOUT`] and every read of a socket has one of
//! the bounds below, so a venue that stops answering fails the case it is in rather than
//! hanging the run.

use std::time::Duration;

/// The whole-case bound: no case may run longer than this.
pub const CASE_TIMEOUT: Duration = Duration::from_secs(180);
/// The bound one setup step of the adapter case has: a venue load, a session's own
/// handshake.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// The bound one market-data read through the adapter has.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The bound one subscription through the adapter has for its first event: the opening
/// screen alone is a thousand candles, and the subscribe frame follows it.
pub const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(60);
/// The bound one HTTP request (the token endpoint, a symbol file) has.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// The bound one socket dial has.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The bound waiting for one acknowledgement or answer on the socket.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
/// The bound one subscription is observed for.
pub const PUSH_WINDOW: Duration = Duration::from_secs(30);
/// The bound the last read of an observation window still waits, after the window
/// itself has run out: the venue may send the first push in the same instant it
/// acknowledges the subscription, and a window that stopped reading would lose it.
pub const MAX_READ: Duration = Duration::from_secs(3);
/// The bound a reconnected socket is given to finish its handshake.
pub const RECONNECT_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the liveness probe listens for a venue ping before reporting that it
/// could not observe one. The design's own liveness bound is 120 seconds; this is
/// deliberately shorter, because the probe is a case of its own rather than a wait
/// the suite can afford twice.
pub const PING_OBSERVATION: Duration = Duration::from_secs(20);

/// The request timeout the protocol's packet header carries (design §10.3).
pub const REQUEST_TIMEOUT_MILLIS: u16 = 30_000;
/// The AUTH frame's own request id, which no pending request can hold (§10.4).
pub const AUTH_REQUEST_ID: u32 = 0;
/// The count the subscription's opening screen asks for (§4.6).
pub const FIRST_SCREEN_COUNT: i32 = 1_000;
/// The trade-session field that asks the venue for every session (§4.5).
pub const TRADE_SESSION_ALL: i32 = 100;
