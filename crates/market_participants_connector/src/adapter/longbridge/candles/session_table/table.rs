//! Which window an instant fell in, and where that window's grid puts its bucket.
//!
//! The table is the venue's own session table rather than a market-participant calendar this
//! build does not have, and it states its windows in the market's wall clock. One of
//! them crosses the market's midnight, which is why a window states *where* it closes
//! rather than *which day* it closes on: the overnight session opens at 20:00 and runs
//! to 04:00 of the next morning, so both the evening's buckets and the small hours' are
//! counted from 20:00 — one grid the venue publishes as one net.

use jiff::{Zoned, civil, tz::TimeZone};

/// The zone the venue opens its US candles in (§4.8).
pub(crate) const MARKET_ZONE: &str = "America/New_York";

/// The market-local session windows of an ordinary US trading day (§4.6).
///
/// The four windows are wall-clock times in [`MARKET_ZONE`] and come from the venue's
/// own session table rather than from a market-participant calendar this build does not have:
/// pre-market opens at 04:00 and closes where the regular session opens, the regular
/// session is 09:30 to 16:00, the post session is 16:00 to 20:00, and the overnight
/// session opens at 20:00 and runs to **04:00 of the next morning**.
///
/// The overnight window is the measurement's and the venue's own table's, and it is
/// **one** window rather than the two the venue's own `US` entry lists
/// (`00:00–04:00` and `20:00–23:59:59`), because that is what the venue publishes:
/// the 45-minute overnight candles the live run read were `20:00 / 20:45 / 21:30 /
/// 22:15 / 23:00 / 23:45 / 00:30`, whose successive distances are 2700 seconds —
/// 23:45 to **00:30** included. A grid that restarted at midnight would have produced
/// `00:00` there, and one that dropped the small hours would have produced nothing.
///
/// So the session is one window spanning the market's day boundary, which is what a
/// bucket is counted from: a 01:15 print belongs to the bucket that opened at 00:30
/// rather than one counted from 00:00, and the evening's grid is the small hours' grid.
/// §4.6 and §14.1 record how that was measured and why the two-window reading is wrong.
pub(crate) const NORMAL_SESSION_TABLE: SessionTable = SessionTable::new(
    (4, 0, 9, 30),
    (9, 30, 16, 0),
    (16, 0, 20, 0),
    ((20, 0), (4, 0)),
);

/// The market-local session windows of a half trading day (§4.6, decision 14).
///
/// The venue closes early on the days its own calendar names: the whole table is
/// replaced rather than the regular session shortened, because the post session of a
/// half day opens where the regular one closes — 13:00 — and a grid that kept 16:00
/// as the post session's start would put every afternoon candle in the wrong bucket.
/// The pre-market window and the overnight window are the ordinary day's: a half day
/// changes when the venue stops trading in the afternoon, not when its evening session
/// begins nor how far into the next morning it runs.
pub(crate) const HALF_DAY_SESSION_TABLE: SessionTable = SessionTable::new(
    (4, 0, 9, 30),
    (9, 30, 13, 0),
    (13, 0, 17, 0),
    ((20, 0), (4, 0)),
);

/// The end of the market's own day, in minutes past midnight.
///
/// A window that closes at the end of the day closes at the last instant the day has. One
/// session of this venue's table does more than that — its overnight window runs into the
/// next morning — which is why a window states its close as an optional hour of the
/// following day ([`WindowClose`]) rather than as a minute of its own.
const END_OF_DAY_MINUTE: i32 = 24 * 60;

/// The session an instant fell in, as the venue's own numbering.
///
/// The numbers are the venue's `TradeSession` values (§4.7): the same four the wire
/// carries, so a push's own session and the session its bucket was computed from are one
/// vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MarketSession {
    /// The regular session, which is the only one a daily candle can be built from.
    Normal,
    /// The session before the regular one.
    Pre,
    /// The session after the regular one.
    Post,
    /// The session that runs from the evening into the next morning.
    Overnight,
}

/// Where one session window closes, on the market's own clock.
///
/// A window that closes on the day it opened on states that hour outright. A window that
/// runs past midnight — the venue's overnight session, which opens at 20:00 and ends at
/// 04:00 the next morning — states the hour of the **following** day instead, because
/// that is the fact and a table that restated it as "24:00" would have to invent an
/// opening at 00:00 that the venue's own series does not publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClose {
    /// The close falls on the window's own day, at this hour and minute.
    SameDay(i8, i8),
    /// The close falls on the following day, at this hour and minute.
    NextDay(i8, i8),
}

/// One market-local window, as its `(open hour, open minute)` and where it closes.
pub(crate) type SessionWindow = ((i8, i8), WindowClose);

/// The session one instant matched, with the window it matched.
///
/// The window is what a fixed-length bucket is counted from, and the answer carries the
/// open instant it resolved to rather than the wall-clock pair it was stated as: a window
/// that ran past midnight opened on the *previous* market day, so a caller that re-read
/// the pair against the instant's own date would anchor the bucket a day late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatchedWindow {
    /// The session kind this window belongs to, in the venue's own numbering.
    pub session: MarketSession,
    /// The market-local instant the window opened at, in milliseconds.
    pub open: i64,
}

/// The market-local session windows of one trading day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionTable {
    /// The pre-market window.
    pre: SessionWindow,
    /// The regular session's window, the only one a daily candle is built inside.
    normal: SessionWindow,
    /// The post-market window.
    post: SessionWindow,
    /// The overnight window, which runs into the following morning.
    overnight: SessionWindow,
}

impl SessionTable {
    /// States the windows of one trading day.
    ///
    /// The overnight window is passed as its own `(open, close)` pair because it is the
    /// one window whose close falls on the next day: `((20, 0), (4, 0))` is the venue's
    /// session, and the type is what says which day each hour belongs to.
    pub(crate) const fn new(
        pre: (i8, i8, i8, i8),
        normal: (i8, i8, i8, i8),
        post: (i8, i8, i8, i8),
        overnight: ((i8, i8), (i8, i8)),
    ) -> Self {
        Self {
            pre: ((pre.0, pre.1), WindowClose::SameDay(pre.2, pre.3)),
            normal: (
                (normal.0, normal.1),
                WindowClose::SameDay(normal.2, normal.3),
            ),
            post: ((post.0, post.1), WindowClose::SameDay(post.2, post.3)),
            overnight: (
                overnight.0,
                WindowClose::NextDay(overnight.1.0, overnight.1.1),
            ),
        }
    }

    /// Returns the market-local bounds of one session, as its window.
    ///
    /// This is what a test reads to state which table one date was bucketed by.
    #[cfg(test)]
    pub(crate) const fn session(&self, session: MarketSession) -> SessionWindow {
        match session {
            MarketSession::Pre => self.pre,
            MarketSession::Normal => self.normal,
            MarketSession::Post => self.post,
            MarketSession::Overnight => self.overnight,
        }
    }

    /// Returns the window one market-local instant fell in, if it fell in one.
    ///
    /// The windows are half-open ranges in the market's own wall clock, which is what
    /// makes 09:30:00 the regular session's first instant rather than the pre session's
    /// last. The overnight session is the one window that spans midnight: a minute before
    /// 20:00 is not in it and a minute after is, and so is every minute up to 04:00 of the
    /// following day.
    ///
    /// An instant inside no window — the afternoon of a half day after its post session
    /// closed, or any hour a table does not cover — falls in no session at all.
    pub(crate) fn window_of(
        &self,
        local: civil::Time,
        date: civil::Date,
        zone: &TimeZone,
    ) -> Option<MatchedWindow> {
        let minute = i32::from(local.hour()) * 60 + i32::from(local.minute());
        let windows = [
            (MarketSession::Pre, self.pre),
            (MarketSession::Normal, self.normal),
            (MarketSession::Post, self.post),
            (MarketSession::Overnight, self.overnight),
        ];
        for (session, ((open_hour, open_minute), close)) in windows {
            let open = i32::from(open_hour) * 60 + i32::from(open_minute);
            let matched = match close {
                WindowClose::SameDay(close_hour, close_minute) => {
                    let close = i32::from(close_hour) * 60 + i32::from(close_minute);
                    // The end of the day is the last instant the day has, which is why no
                    // minute ever reaches a window that closes there.
                    (open..close.min(END_OF_DAY_MINUTE)).contains(&minute)
                }
                // A window that runs past midnight has two same-day halves — before the
                // market's day and after it — and the close is the first hour of the next
                // day, so the instant's own date is not the window's date for the first.
                WindowClose::NextDay(close_hour, close_minute) => {
                    let close = i32::from(close_hour) * 60 + i32::from(close_minute);
                    (open..END_OF_DAY_MINUTE).contains(&minute) || (0..close).contains(&minute)
                }
            };
            if matched {
                // The open belongs to the window's own market day: for a window that runs
                // past midnight, an instant before the window's own open is an instant of
                // the following day, so the window opened the day before.
                let opens_on = match (close, minute < open) {
                    (WindowClose::NextDay(..), true) => date.yesterday().ok()?,
                    _ => date,
                };
                let open = opens_on
                    .at(open_hour, open_minute, 0, 0)
                    .to_zoned(zone.clone())
                    .ok()?;
                return Some(MatchedWindow {
                    session,
                    open: open.timestamp().as_millisecond(),
                });
            }
        }
        None
    }

    /// Returns where one market-local instant's fixed-length candle starts.
    ///
    /// The bucket is the matched window's own open plus a whole number of intervals: a
    /// one-hour candle of the regular session therefore opens at 09:30, 10:30 and so on
    /// rather than at a UTC hour, and the overnight session's grid counts a 00:30 print
    /// from 20:00 of the previous evening — the grid the venue publishes across midnight.
    /// An instant outside every window has no bucket.
    pub(crate) fn fixed_bucket(
        &self,
        local: Zoned,
        interval_millis: i64,
        zone: &TimeZone,
    ) -> Option<i64> {
        let matched = self.window_of(local.time(), local.date(), zone)?;
        // A clock that moved backwards inside one window leaves an instant before its
        // window's open; it belongs to the window that just opened, so the interval it
        // falls in is the first one.
        let elapsed = (local.timestamp().as_millisecond() - matched.open).max(0);
        Some(matched.open + (elapsed / interval_millis) * interval_millis)
    }
}

impl MarketSession {
    /// Reports whether this session is the regular one.
    ///
    /// The venue's own `is_intraday` is this question asked of the wire's numbers, and
    /// three rules depend on it: a daily candle is only ever built inside the regular
    /// session, a non-regular push can only reach a candle below the daily scale, and a
    /// quote push is only merged while the regular session is open.
    pub(crate) const fn is_normal(self) -> bool {
        matches!(self, Self::Normal)
    }
}
