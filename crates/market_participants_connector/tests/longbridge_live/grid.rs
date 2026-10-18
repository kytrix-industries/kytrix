//! The venue's own session table and bucket grid, as the cases compute them.
//!
//! This is the one place in the suite that states where a bucket starts. It is the
//! adapter's rule restated rather than shared — the adapter's table is crate-private —
//! and the live suite checks the venue against it, so the two must be the same table:
//! **four windows**, one of which spans midnight.
//!
//! The overnight window is the one that matters here, and both of its facts are the
//! venue's own. Its `US` trade-session entry lists the overnight session as two windows
//! (`00:00–04:00` and `20:00–23:59:59`); its published series shows **one continuous
//! grid**: the 45-minute overnight candles the live run read were `20:00 / 20:45 / 21:30
//! / 22:15 / 23:00 / 23:45 / 00:30`, whose successive distances are 2700 seconds across
//! midnight. A grid that restarted at 00:00 would have published `00:00` there, and one
//! that dropped the small hours would have published nothing. So the window runs
//! `20:00 → 04:00 the next morning`, and a bucket is counted from 20:00 of the market day
//! the session belongs to.

use jiff::{Timestamp, Zoned, civil, tz::TimeZone};
use longbridge_proto::quote::TradeSession;

/// The zone every US candle boundary is stated in (§4.8).
pub const MARKET_ZONE: &str = "America/New_York";

/// Where one session window closes, on the market's own clock.
///
/// The overnight session is the one window whose close falls on the following day, and
/// the distinction is not cosmetic: a bucket is counted from the window's open, and for
/// an instant after midnight that open is on the *previous* market day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowEnd {
    /// The window closes on the day it opened on, at this hour and minute.
    SameDay(i32, i32),
    /// The window closes on the following day, at this hour and minute.
    NextDay(i32, i32),
}

/// One session window, as its open and where it closes.
pub type Window = ((i32, i32), WindowEnd);

/// The venue's own session table for an ordinary US trading day (§4.6).
pub const SESSION_WINDOWS: [(TradeSession, Window); 4] = [
    (TradeSession::PreTrade, ((4, 0), WindowEnd::SameDay(9, 30))),
    (
        TradeSession::NormalTrade,
        ((9, 30), WindowEnd::SameDay(16, 0)),
    ),
    (
        TradeSession::PostTrade,
        ((16, 0), WindowEnd::SameDay(20, 0)),
    ),
    (
        TradeSession::OvernightTrade,
        ((20, 0), WindowEnd::NextDay(4, 0)),
    ),
];

/// The half-day table's own windows (§4.6, decision 14).
///
/// A half day replaces the whole table rather than shortening one session: the regular
/// session ends at 13:00 and the post session opens where it closed. The pre-market
/// window and the overnight window are the ordinary day's, which is why 17:00 to 20:00
/// is inside no session at all on a half day.
pub const HALF_DAY_WINDOWS: [(TradeSession, Window); 4] = [
    (TradeSession::PreTrade, ((4, 0), WindowEnd::SameDay(9, 30))),
    (
        TradeSession::NormalTrade,
        ((9, 30), WindowEnd::SameDay(13, 0)),
    ),
    (
        TradeSession::PostTrade,
        ((13, 0), WindowEnd::SameDay(17, 0)),
    ),
    (
        TradeSession::OvernightTrade,
        ((20, 0), WindowEnd::NextDay(4, 0)),
    ),
];

/// The market's own zone.
pub fn market_zone() -> TimeZone {
    TimeZone::get(MARKET_ZONE).expect("the zone database carries America/New_York")
}

/// The market-local wall clock of one epoch-millisecond instant.
pub fn market_local(millis: i64) -> Zoned {
    let timestamp = Timestamp::from_millisecond(millis).expect("a representable instant");
    Zoned::new(timestamp, market_zone())
}

/// The current epoch milliseconds.
pub fn now_millis() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch");
    i64::try_from(since_epoch.as_millis()).expect("a representable instant")
}

/// The market's current date, as `YYYYMMDD`.
pub fn market_date(millis: i64) -> String {
    market_local(millis).strftime("%Y%m%d").to_string()
}

/// The epoch-millisecond instant one market-local wall time names.
pub fn market_instant(date: &str, hour: i8, minute: i8) -> i64 {
    civil::Date::strptime("%Y%m%d", date)
        .expect("a date this suite formatted")
        .at(hour, minute, 0, 0)
        .to_zoned(market_zone())
        .expect("a market-local wall time is a real instant")
        .timestamp()
        .as_millisecond()
}

/// The session one market-local minute falls in, of the ordinary table.
///
/// The windows are half-open in the market's own wall clock, so 09:30:00 is the regular
/// session's first instant rather than the pre session's last, and the overnight window
/// is the one that spans midnight. An instant inside no window falls in no session: the
/// ordinary table's four windows tile the clock, and on a half day the afternoon between
/// the post session's 17:00 close and the overnight window's 20:00 open is the gap.
pub fn session_of(local: &Zoned) -> Option<TradeSession> {
    session_in(&SESSION_WINDOWS, local).map(|(session, _)| session)
}

/// The session one market-local minute falls in, of the half-day table.
pub fn half_day_session_of(local: &Zoned) -> Option<TradeSession> {
    session_in(&HALF_DAY_WINDOWS, local).map(|(session, _)| session)
}

/// The session and window one market-local minute falls in, of one table's windows.
fn session_in(windows: &[(TradeSession, Window)], local: &Zoned) -> Option<(TradeSession, Window)> {
    let minute = i32::from(local.hour()) * 60 + i32::from(local.minute());
    windows
        .iter()
        .find(|(_, ((open_hour, open_minute), end))| {
            let open = open_hour * 60 + open_minute;
            match end {
                WindowEnd::SameDay(close_hour, close_minute) => {
                    let close = close_hour * 60 + close_minute;
                    (open..close).contains(&minute)
                }
                // A window that runs past midnight covers the evening of the day it
                // opened on and the small hours of the next one.
                WindowEnd::NextDay(close_hour, close_minute) => {
                    let close = close_hour * 60 + close_minute;
                    (open..24 * 60).contains(&minute) || (0..close).contains(&minute)
                }
            }
        })
        .copied()
}

/// Where one instant's bucket starts, as the adapter's grid computes it.
///
/// A fixed-length bucket starts at the **matched window's own open** plus a whole number
/// of periods — never at a UTC hour — and a calendar bucket starts at the market's
/// midnight. `None` means the instant is inside no session of the table, which is the
/// case the adapter drops.
pub fn bucket_start(millis: i64, period_millis: Option<i64>) -> Option<i64> {
    bucket_in(&SESSION_WINDOWS, millis, period_millis)
}

/// Where one half-day instant's bucket starts, as the adapter's half-day table computes
/// it.
pub fn half_day_bucket_start(millis: i64, period_millis: Option<i64>) -> Option<i64> {
    bucket_in(&HALF_DAY_WINDOWS, millis, period_millis)
}

/// Where one instant's bucket starts, of one table's windows.
fn bucket_in(
    windows: &[(TradeSession, Window)],
    millis: i64,
    period_millis: Option<i64>,
) -> Option<i64> {
    let local = market_local(millis);
    if period_millis.is_none() {
        return Some(
            civil::Date::new(local.year(), local.month(), local.day())
                .expect("the local date of a measured instant")
                .at(0, 0, 0, 0)
                .to_zoned(market_zone())
                .expect("the market's midnight is a real instant")
                .timestamp()
                .as_millisecond(),
        );
    }
    let period_millis = period_millis?;
    let (_, ((open_hour, open_minute), end)) = session_in(windows, &local)?;
    // The open belongs to the window's own market day: an instant after midnight inside a
    // window that runs past midnight is an instant of the following day, so the window
    // opened the day before.
    let minutes = i32::from(local.hour()) * 60 + i32::from(local.minute());
    let open_minutes = open_hour * 60 + open_minute;
    let opens_on = match (end, minutes < open_minutes) {
        (WindowEnd::NextDay(..), true) => local.date().yesterday().ok()?,
        _ => local.date(),
    };
    let open = opens_on
        .at(
            i8::try_from(open_hour).expect("an hour"),
            i8::try_from(open_minute).expect("a minute"),
            0,
            0,
        )
        .to_zoned(market_zone())
        .expect("the session's open is a real instant")
        .timestamp()
        .as_millisecond();
    Some(open + (millis - open).div_euclid(period_millis) * period_millis)
}
