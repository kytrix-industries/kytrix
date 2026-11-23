use jiff::{civil, tz::TimeZone};

use super::{
    CandleBucket, HALF_DAY_SESSION_TABLE, MARKET_ZONE, MarketSession, NORMAL_SESSION_TABLE,
    bucket::CalendarUnit,
    calendar_bucket, calendar_unit, fixed_interval_millis,
    table::{MatchedWindow, SessionTable, WindowClose},
};
use crate::market::timeframe::TimeFrame;

fn local(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> jiff::Zoned {
    civil::date(year, month, day)
        .at(hour, minute, 0, 0)
        .to_zoned(TimeZone::get(MARKET_ZONE).expect("the market zone is in the database"))
        .expect("the fixture wall time is a real instant")
}

fn zone() -> TimeZone {
    TimeZone::get(MARKET_ZONE).expect("the market zone is in the database")
}

fn session_of(table: SessionTable, hour: i8, minute: i8, second: i8) -> Option<MatchedWindow> {
    let date = civil::date(2026, 6, 10);
    table.window_of(civil::time(hour, minute, second, 0), date, &zone())
}

#[test]
fn the_overnight_session_spans_midnight() {
    let table = NORMAL_SESSION_TABLE;
    assert_eq!(
        table.session(MarketSession::Overnight),
        ((20, 0), WindowClose::NextDay(4, 0)),
        "the window opens at 20:00 and closes at 04:00 of the following day"
    );
    for (hour, minute) in [(20, 0), (23, 0), (23, 59), (0, 1), (2, 30), (3, 59)] {
        assert_eq!(
            session_of(table, hour, minute, 0).map(|matched| matched.session),
            Some(MarketSession::Overnight),
            "{hour:02}:{minute:02} is inside the overnight session"
        );
    }
    assert_eq!(
        session_of(table, 4, 0, 0).map(|matched| matched.session),
        Some(MarketSession::Pre),
        "and the pre-market window is where classification moves on"
    );
    assert_eq!(
        session_of(table, 19, 59, 0).map(|matched| matched.session),
        Some(MarketSession::Post),
        "the minute before the window is still the post session"
    );
}

#[test]
fn the_overnight_grid_is_counted_from_the_evening_before() {
    let table = NORMAL_SESSION_TABLE;
    let hour = fixed_interval_millis(TimeFrame::Hour1).expect("an hour is fixed length");
    let evening = table
        .fixed_bucket(local(2026, 6, 10, 20, 0), hour, &zone())
        .expect("20:00 opens the overnight session");
    assert_eq!(
        evening,
        local(2026, 6, 10, 20, 0).timestamp().as_millisecond(),
        "the grid opens at 20:00 of the day the session belongs to"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 10, 23, 30), hour, &zone()),
        Some(evening + 3 * 60 * 60 * 1_000),
        "23:30 is three hourly buckets into it"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 10, 23, 45), hour, &zone()),
        Some(evening + 3 * 60 * 60 * 1_000),
        "and 23:45, the venue's own last evening bucket, rests on the same grid"
    );

    let after_midnight = local(2026, 6, 11, 0, 0).timestamp().as_millisecond();
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 0, 0), hour, &zone()),
        Some(evening + 4 * 60 * 60 * 1_000),
        "00:00 is four hourly buckets into the evening's grid"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 3, 0), hour, &zone()),
        Some(evening + 7 * 60 * 60 * 1_000),
        "and 03:00 is seven"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 3, 59), hour, &zone()),
        Some(evening + 7 * 60 * 60 * 1_000),
        "03:59 is still inside the bucket that opened at 03:00"
    );
    assert!(
        after_midnight > evening,
        "the fixture dates are the right way round"
    );

    let forty_five =
        fixed_interval_millis(TimeFrame::Minute45).expect("45 minutes are fixed length");
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 10, 23, 45), forty_five, &zone()),
        Some(evening + 5 * 45 * 60 * 1_000),
        "23:45 is the grid's sixth 45-minute bucket"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 0, 30), forty_five, &zone()),
        Some(evening + 6 * 45 * 60 * 1_000),
        "and 00:30 is the seventh — the candle the venue publishes after 23:45"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 0, 39), forty_five, &zone()),
        Some(evening + 6 * 45 * 60 * 1_000),
        "00:39 is inside that bucket rather than at its start"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 11, 1, 15), forty_five, &zone()),
        Some(evening + 7 * 45 * 60 * 1_000),
        "and 01:15 is the grid's eighth bucket: the venue's own series names it, so a print \
         there opens a candle rather than joining the 00:30 one"
    );
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 10, 21, 50), forty_five, &zone()),
        Some(evening + 2 * 45 * 60 * 1_000),
        "a 21:50 print lands on the 20:00 grid's 21:30 bucket"
    );
}

#[test]
fn the_sessions_are_the_markets_own_clock() {
    let table = NORMAL_SESSION_TABLE;
    for (hour, minute, second, session, reason) in [
        (
            4,
            0,
            0,
            MarketSession::Pre,
            "04:00 is the pre session's first instant",
        ),
        (
            9,
            29,
            59,
            MarketSession::Pre,
            "and its last is the minute before the regular open",
        ),
        (
            9,
            30,
            0,
            MarketSession::Normal,
            "09:30 belongs to the regular session",
        ),
        (15, 59, 59, MarketSession::Normal, "which runs to 16:00"),
        (
            16,
            0,
            0,
            MarketSession::Post,
            "16:00 opens the post session",
        ),
        (19, 59, 59, MarketSession::Post, "which runs to 20:00"),
        (
            20,
            0,
            0,
            MarketSession::Overnight,
            "where the overnight session opens",
        ),
        (
            23,
            59,
            59,
            MarketSession::Overnight,
            "and runs through the market day's end",
        ),
        (0, 0, 0, MarketSession::Overnight, "into the next morning"),
        (
            3,
            59,
            59,
            MarketSession::Overnight,
            "up to the pre session's open",
        ),
        (
            4,
            0,
            0,
            MarketSession::Pre,
            "where the new day's pre session takes over",
        ),
    ] {
        assert_eq!(
            session_of(table, hour, minute, second).map(|matched| matched.session),
            Some(session),
            "{hour:02}:{minute:02}:{second:02} — {reason}"
        );
    }
    assert_eq!(
        HALF_DAY_SESSION_TABLE
            .window_of(
                civil::time(17, 30, 0, 0),
                civil::date(2026, 11, 27),
                &zone()
            )
            .map(|matched| matched.session),
        None,
        "a half day's post session closes at 17:00 and its overnight session opens at \
         20:00, so the afternoon between them is inside no session at all"
    );
}

#[test]
fn a_fixed_length_bucket_starts_at_its_sessions_open() {
    let table = NORMAL_SESSION_TABLE;
    let hour = fixed_interval_millis(TimeFrame::Hour1).expect("an hour is fixed length");

    let open = table
        .fixed_bucket(local(2026, 6, 10, 9, 30), hour, &zone())
        .expect("09:30 is in the session");
    let halfway = table
        .fixed_bucket(local(2026, 6, 10, 10, 0), hour, &zone())
        .expect("10:00 is in the session");
    let next = table
        .fixed_bucket(local(2026, 6, 10, 10, 30), hour, &zone())
        .expect("10:30 is in the session");

    assert_eq!(
        halfway, open,
        "10:00 belongs to the candle that opened at 09:30"
    );
    assert_eq!(next - open, hour, "and 10:30 opens the candle after it");

    let four = fixed_interval_millis(TimeFrame::Hour4).expect("four hours are fixed length");
    assert_eq!(
        table.fixed_bucket(local(2026, 6, 10, 13, 0), four, &zone()),
        table.fixed_bucket(local(2026, 6, 10, 9, 30), four, &zone()),
        "13:00 is still inside the candle that opened at 09:30"
    );
    let afternoon = table
        .fixed_bucket(local(2026, 6, 10, 13, 30), four, &zone())
        .expect("13:30 is in the session");
    assert_ne!(afternoon, open, "13:30 opens the next four-hour candle");
    let post = table
        .fixed_bucket(local(2026, 6, 10, 16, 0), four, &zone())
        .expect("16:00 is in the post session");
    assert_ne!(
        post, afternoon,
        "the post session's grid starts at its own open"
    );
    assert_eq!(
        post,
        local(2026, 6, 10, 16, 0).timestamp().as_millisecond(),
        "which is 16:00 itself"
    );
}

#[test]
fn the_half_day_table_starts_the_post_session_at_thirteen() {
    let hour = fixed_interval_millis(TimeFrame::Hour1).expect("an hour is fixed length");
    let four = fixed_interval_millis(TimeFrame::Hour4).expect("four hours are fixed length");
    let half_open = HALF_DAY_SESSION_TABLE
        .fixed_bucket(local(2026, 11, 27, 13, 0), hour, &zone())
        .expect("13:00 opens the post session of a half day");
    let half_later = HALF_DAY_SESSION_TABLE
        .fixed_bucket(local(2026, 11, 27, 15, 0), hour, &zone())
        .expect("15:00 is in that post session");
    let ordinary = NORMAL_SESSION_TABLE
        .fixed_bucket(local(2026, 11, 27, 15, 0), hour, &zone())
        .expect("15:00 is in an ordinary day's regular session");
    let ordinary_post = NORMAL_SESSION_TABLE
        .fixed_bucket(local(2026, 11, 27, 16, 0), hour, &zone())
        .expect("16:00 opens an ordinary day's post session");

    assert_eq!(
        half_open,
        local(2026, 11, 27, 13, 0).timestamp().as_millisecond(),
        "13:00 is the half day's own bucket: an hour grid starts at its session's open"
    );
    assert_eq!(
        half_later - half_open,
        2 * 60 * 60 * 1_000,
        "and 15:00 is two hourly buckets into it"
    );
    assert_ne!(
        half_later, ordinary,
        "which is not the grid an ordinary day's regular session would have used"
    );
    assert_ne!(
        HALF_DAY_SESSION_TABLE
            .fixed_bucket(local(2026, 11, 27, 16, 0), four, &zone())
            .expect("16:00 is still inside the half day's post session"),
        ordinary_post,
        "and a four-hour candle of that afternoon rests on 13:00, not on 16:00"
    );
    assert_eq!(
        session_of(HALF_DAY_SESSION_TABLE, 12, 59, 0).map(|matched| matched.session),
        Some(MarketSession::Normal),
        "the regular session of a half day runs until 13:00"
    );
    assert_eq!(
        session_of(HALF_DAY_SESSION_TABLE, 13, 0, 0).map(|matched| matched.session),
        Some(MarketSession::Post)
    );
    assert_eq!(
        session_of(HALF_DAY_SESSION_TABLE, 17, 0, 0).map(|matched| matched.session),
        None,
        "and its post session ends at 17:00"
    );
    assert_eq!(
        session_of(HALF_DAY_SESSION_TABLE, 20, 0, 0).map(|matched| matched.session),
        Some(MarketSession::Overnight),
        "while the day's evening window is the ordinary one"
    );
    assert_eq!(
        session_of(HALF_DAY_SESSION_TABLE, 1, 0, 0).map(|matched| matched.session),
        Some(MarketSession::Overnight),
        "and so are its small hours"
    );
}

#[test]
fn a_calendar_bucket_is_the_markets_own_boundary() {
    assert_eq!(
        calendar_bucket(local(2026, 6, 10, 15, 45), CalendarUnit::Day),
        Some(civil::date(2026, 6, 10))
    );
    assert_eq!(
        calendar_bucket(local(2026, 6, 10, 15, 45), CalendarUnit::Week),
        Some(civil::date(2026, 6, 8)),
        "2026-06-10 is a Wednesday, so its week starts on Monday the 8th"
    );
    assert_eq!(
        calendar_bucket(local(2026, 6, 10, 15, 45), CalendarUnit::Month),
        Some(civil::date(2026, 6, 1))
    );
    assert_eq!(
        calendar_bucket(local(2026, 6, 10, 15, 45), CalendarUnit::Quarter),
        Some(civil::date(2026, 4, 1)),
        "June is in the second quarter"
    );
    assert_eq!(
        calendar_bucket(local(2026, 8, 1, 9, 0), CalendarUnit::Quarter),
        Some(civil::date(2026, 7, 1)),
        "and August is in the third"
    );
    assert_eq!(
        calendar_bucket(local(2026, 6, 10, 15, 45), CalendarUnit::Year),
        Some(civil::date(2026, 1, 1))
    );
}

#[test]
fn the_bucket_boundaries_follow_the_markets_daylight_saving() {
    assert_eq!(
        local(2026, 3, 6, 9, 30).timestamp().as_millisecond(),
        1_772_807_400_000,
        "2026-03-06T09:30 in New York is 14:30 UTC, because the offset is still -05:00"
    );
    assert_eq!(
        local(2026, 3, 9, 9, 30).timestamp().as_millisecond(),
        1_773_063_000_000,
        "2026-03-09T09:30 in New York is 13:30 UTC, because the offset is now -04:00"
    );
    assert_eq!(
        local(2026, 11, 2, 9, 30).timestamp().as_millisecond(),
        1_793_629_800_000,
        "2026-11-02T09:30 in New York is 14:30 UTC, because the offset is back to -05:00"
    );

    let four = fixed_interval_millis(TimeFrame::Hour4).expect("four hours are fixed length");
    for (label, day) in [("the Friday before", 6), ("the Monday after", 9)] {
        let bucket = NORMAL_SESSION_TABLE
            .fixed_bucket(local(2026, 3, day, 9, 30), four, &zone())
            .unwrap_or_else(|| panic!("{label} the March transition is a trading day"));
        assert_eq!(
            bucket,
            local(2026, 3, day, 9, 30).timestamp().as_millisecond(),
            "{label} the March transition opens its four-hour candle at 09:30 local"
        );
    }

    for (date, expected, reason) in [
        (civil::date(2026, 3, 8), 1_772_946_000_000, "05:00 UTC"),
        (civil::date(2026, 11, 1), 1_793_505_600_000, "04:00 UTC"),
    ] {
        let midnight = CandleBucket::Calendar {
            date,
            unit: CalendarUnit::Day,
        }
        .start(&zone())
        .expect("the market has a midnight on a transition day");
        assert_eq!(
            midnight.timestamp().as_millisecond(),
            expected,
            "{date}T00:00 in New York is {reason}"
        );
        assert_eq!(
            calendar_bucket(
                local(date.year(), date.month(), date.day(), 15, 0),
                CalendarUnit::Day
            ),
            Some(date),
            "and the afternoon of {date} is still that local day"
        );
    }
}

#[test]
fn the_two_kinds_of_period_are_the_venues_own() {
    for (timeframe, unit) in [
        (TimeFrame::Day1, CalendarUnit::Day),
        (TimeFrame::Week1, CalendarUnit::Week),
        (TimeFrame::Month1, CalendarUnit::Month),
        (TimeFrame::Quarter1, CalendarUnit::Quarter),
        (TimeFrame::Year1, CalendarUnit::Year),
    ] {
        assert_eq!(calendar_unit(timeframe), Some(unit));
        assert_eq!(
            fixed_interval_millis(timeframe),
            None,
            "{timeframe:?} is not a fixed-length interval"
        );
    }
    assert_eq!(
        fixed_interval_millis(TimeFrame::Minute45),
        Some(45 * 60 * 1_000)
    );
    assert_eq!(
        fixed_interval_millis(TimeFrame::Hour4),
        Some(4 * 60 * 60 * 1_000)
    );
    assert_eq!(
        fixed_interval_millis(TimeFrame::Second1),
        None,
        "the model has a second interval and the venue publishes none"
    );
}
