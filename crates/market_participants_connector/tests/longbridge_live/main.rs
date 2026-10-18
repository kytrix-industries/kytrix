//! The Longbridge venue's live protocol suite (design §12.9, a mandatory item).
//!
//! Every case here is ignored by default, because every case talks to the real
//! venue: `cargo test --workspace` therefore never touches the network. Run them
//! with
//!
//! ```powershell
//! cargo test -p market_participants_connector --test longbridge_live -- --ignored --nocapture
//! ```
//!
//! What this suite is for is the one thing a fixture cannot prove. The venue's wire
//! — a custom frame around protobuf, the one-time-password handshake, the
//! market-data profile, the two candlestick commands, the real-time quote command and
//! the two pushes — is implemented in this repository, so a fixture can only show that
//! this build agrees with itself. Each case below sends the venue what the design says
//! it sends, measures what comes back, prints every measurement, and asserts the fact
//! the design states about it. A failing assertion here means a fact in §4 is wrong,
//! not that the test needs loosening.
//!
//! Three rules shape the code:
//!
//! * **The suite is read-only.** It sends AUTH, the profile query, the trading-day
//!   query, the two candlestick queries, the real-time quote query and a
//!   subscription; it never places an order
//!   and never changes any state. Subscriptions are dropped by closing the socket,
//!   and every socket is closed when its case ends.
//! * **Credentials are read, never written.** The three values come from the
//!   environment, or from the workspace root's `longbridge-test.env`, or from the
//!   file `KYTRIX_LONGBRIDGE_TEST_CREDENTIALS` names. No value ever reaches an
//!   assertion message, a print, or a log line: what is printed is a length or a
//!   `us_`-prefix verdict and nothing else.
//! * **Every wait is bounded.** Each case wraps its work in `tokio::time::timeout`,
//!   and each read of the socket has its own bound, so a venue that stops answering
//!   fails the case instead of hanging the run.
//!
//! Eighteen of the cases speak the wire themselves rather than driving the adapter,
//! because the adapter's framing, its handshake and its command encoders are
//! crate-private: an integration test cannot reach them, and the facts under test are
//! the venue's answers rather than this build's internals. Driving the same commands by
//! hand is what makes the measurements independent of the code they are meant to check
//! — and the other two are the other half: cases 17 and 20 walk the adapter's **public**
//! path, so the signed OTP request, the pooled handshake, the permission gate and the
//! candle and ticker commands are exercised by the code the application runs.
//!
//! One thing the live run forced on this suite is stated where it is implemented
//! rather than here: the venue's `/v1/socket/token` refused an unsigned request with
//! its own code, so this suite signs that one request the way §4.9 states. That
//! divergence, and the response envelope that came with it, is a finding the suite
//! reports; see the crate's delivery notes and §14.1 of the design.
//!
//! The modules are the suite's concerns rather than its cases alone: [`bounds`] holds
//! every wait, [`credentials`] the three values, [`client`] the line a run takes,
//! [`wire`] the codec, [`socket`] the one dialled socket, [`grid`] **the** session table
//! and bucket grid, [`describe`] the words the prints are made of, and [`cases`] the
//! twenty cases themselves.

mod bounds;
mod cases;
mod client;
mod credentials;
mod describe;
mod grid;
mod http;
mod socket;
mod symbols;
mod wire;

/// Asks the venue for one socket's password, or reports that this run has no credentials.
///
/// A run without credentials **skips** rather than fails: the three values are the
/// operator's, and a checkout without them is a checkout that cannot run the live suite
/// rather than one whose suite is broken. What is printed is the case's own name and the
/// names of the three variables — never a value.
#[macro_export]
macro_rules! skip_without_credentials {
    ($case:expr, $credentials:expr) => {
        match $credentials {
            Some(credentials) => credentials,
            None => {
                println!(
                    "SKIP {}: no {} / {} / {} in the environment and no readable {} at the \
                     workspace root",
                    $case,
                    $crate::credentials::API_KEY_ENV,
                    $crate::credentials::APP_SECRET_ENV,
                    $crate::credentials::ACCESS_TOKEN_ENV,
                    $crate::credentials::CREDENTIALS_FILE
                );
                return;
            }
        }
    };
}
