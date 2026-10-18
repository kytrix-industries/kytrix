//! The candle one subscription builds from the venue's pushes.
//!
//! The venue publishes quotes and trades and no candles at all, so every candle a reader
//! sees after the first screen is this state machine's own construction. It is a pure
//! value — no socket, no clock, no I/O — because the rules it applies are the venue's and
//! the only way to hold them to it is to state instants on the market's own wall clock and
//! read back the candle they produce.
//!
//! Two of those rules are gates, and each one is a fact about the venue's calendar rather
//! than about one push: a daily-scale candle is never built outside the regular session,
//! and a non-regular push reaches only a candle below the daily scale — the minutes and
//! the hours, up to the venue's `4h`, because the venue's own question is `Period`'s
//! `is_minute()` rather than the interval's name. The third gate — a quote
//! push is merged only while the regular session is open — belongs to the caller that
//! reads a frame into a print, because it decides whether a print exists at all rather
//! than where its bucket starts, and it lives beside the state that owns the session
//! table. The fourth rule is the update table: a print's `trade_type` decides which of a
//! candle's fields it is allowed to touch, and a type the table does not name touches
//! none of them.
//!
//! [`print`] is the vocabulary one push is read in, and [`state`] the state machine the
//! wire feeds it to.

mod print;
mod state;
#[cfg(test)]
mod tests;

pub(crate) use print::{
    CandlePrint, CumulativeDay, QuotePrint, TradePrint, TradeType, UpdatedFields,
};
pub(crate) use state::{CandleAggregator, CandleOutcome};
