//! The candle wire: the requests it sends and the stream it answers with.
//!
//! The requests are the venue's own commands — the history of one market, a subscription's
//! opening screen, the subscription itself and the calendar a half day comes from — and the
//! stream merges the pushes they lead to into candles. What the venue says about the market
//! and what this build makes of it are one module because the second is a reading of the
//! first: the frames carry the venue's numbers, and every number is turned into this
//! build's vocabulary in one place.
//!
//! The shape of one inbound frame is not this module's: the ticker wire reads the same
//! frames, so the frame and its readers live beside both of them
//! ([`crate::adapter::longbridge::wire`]) and [`stream`] is everything the candle wire
//! sends and makes of what comes back.

mod stream;
#[cfg(test)]
mod tests;

pub(crate) use stream::{fetch_candlesticks, subscribe_candlesticks};
