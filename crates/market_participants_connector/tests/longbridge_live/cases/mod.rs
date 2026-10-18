//! The suite's cases, one module per concern.
//!
//! The division is the concern rather than the case number: [`handshake`] is the
//! connection and the account it authenticates, [`stream`] is the market data the two
//! candlestick commands return, [`push`] is what the venue pushes and what this build's
//! aggregation makes of it, [`quote`] is the real-time quote command and the incremental
//! semantics of its pushes, [`directory`] is the catalog the venue's listings come from,
//! [`socketcase`] is the socket's own lifecycle, and [`adapter`] is the adapter's public
//! path — the case that drives production code rather than the wire.

pub(crate) mod adapter;
pub(crate) mod directory;
pub(crate) mod grid;
pub(crate) mod handshake;
pub(crate) mod push;
pub(crate) mod quote;
pub(crate) mod socketcase;
pub(crate) mod stream;
