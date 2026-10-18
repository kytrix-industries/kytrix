//! The venue's candlestick reads and subscriptions.
//!
//! One authenticated socket carries the whole market-data surface of this venue, so
//! every entry point here starts the same way: the session's permission gate, which
//! refuses a market-data command the account's own profile grants no permission for
//! before any frame exists, and the per-command throttle the profile installed. What
//! follows differs by direction.
//!
//! A **fetch** pages the venue's history (cmd 27) and answers with the candles it
//! returned, sorted ascending because the widget reads a short page as "the newest edge
//! is reached" and a page out of order would break that judgement. The four shapes the
//! caller can state are §4.5's own, and `date`/`minute` are the market's local wall time
//! rather than UTC: the venue has no idea what zone the caller is in, so this build
//! converts through the market's own zone database.
//!
//! A **subscription** reads the venue's opening screen (cmd 19) *before* it registers
//! (cmd 6), then merges the venue's quote and trade pushes into candles of its own
//! ([`aggregator`]). The order matters: `subscribe_with_id` sends the subscribe frame as
//! soon as the lease exists, and pushes that arrived while the screen was being read
//! would be lost to a full broadcast channel. The half-day table (cmd 9) is read lazily,
//! on the first push, and refreshed once a day.
//!
//! The venue never pushes candles, so nothing here waits for one: it waits for the
//! venue's pushes and builds them.

mod aggregator;
mod push_state;
mod session_table;
mod wire;

pub(super) use wire::{fetch_candlesticks, subscribe_candlesticks};
