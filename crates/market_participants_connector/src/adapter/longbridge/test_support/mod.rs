//! The loopback Longbridge venue this module's tests drive, and the clients that read
//! it.
//!
//! `answers` is what a fixture is told to answer with and what it read back, `venue` is
//! the fixture itself and `serve` the two sockets it runs, and `client` is the other
//! side: the configuration, the credentials, the route and the listing every case is
//! stated in terms of.

mod answers;
mod client;
mod serve;
mod venue;

pub(super) use answers::{
    AuthAnswer, CalendarAnswer, CandleAnswer, FixtureCandle, ProfileAnswer, QuoteAnswer,
    SUBSCRIPTION_REFUSAL_MESSAGE, TokenAnswer,
};
pub(super) use client::{
    APP_SECRET, credential_values, credentials, execution, resolved_listing, route,
};
pub(super) use venue::LongbridgeFixture;
