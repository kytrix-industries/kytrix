//! Where one push belongs on the venue's own candlestick grid.
//!
//! The grid is the venue's, not this build's: every bucket boundary is a wall-clock
//! instant in the market's zone, so a fixed-length candle starts at the session's own
//! open rather than at a UTC hour, and a calendar candle starts at the market's
//! midnight. `jiff` carries the zone database, so the two daylight-saving transitions
//! a US year has are its rules rather than a fixed offset this build would have to
//! restate — and a fixed offset is wrong for exactly the candles a reader looks at
//! around those dates.
//!
//! A session is the unit a bucket is counted from, and one of them is a window that
//! **crosses the market's midnight**: the overnight session opens at 20:00 and runs to
//! 04:00 of the next morning, so the evening's buckets and the small hours' are one grid
//! counted from 20:00. [`table`] holds which window an instant fell in, and [`bucket`]
//! where that window's grid puts the candle — both pure, so a test can state an instant
//! as the market's own wall time and read back the boundary it must produce.

mod bucket;
mod table;
#[cfg(test)]
mod tests;

pub(super) use bucket::{CandleBucket, calendar_bucket, calendar_unit, fixed_interval_millis};
pub(super) use table::{
    HALF_DAY_SESSION_TABLE, MARKET_ZONE, MarketSession, NORMAL_SESSION_TABLE, SessionTable,
};
