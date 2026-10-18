//! The venue's real-time quotes.
//!
//! One symbol's quote is one whole snapshot of the day so far — open, high, low, last
//! done, and the two traded totals — and the venue offers it two ways: a **pull**
//! (cmd 11) that answers the complete snapshot, and a **subscription** (cmd 6) whose
//! pushes carry only the fields that changed.
//!
//! The two are read together rather than separately, because a push on its own is not a
//! snapshot: a frame that states only the last price would zero every other field, and a
//! row drawn from it would show a change measured against nothing. A subscription
//! therefore opens with the pull — the baseline — and merges every push into it, so what
//! leaves this module is always a complete, drawable snapshot ([`wire`]).
//!
//! The session, the permission gate, the per-command quota and the subscription lease
//! are the ones every market-data path of this venue uses; this module adds the reading
//! of the quote commands and nothing else.

#[cfg(test)]
mod tests;
mod wire;

pub(crate) use wire::{fetch_ticker, subscribe_ticker};
