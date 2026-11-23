use jiff::{Timestamp, Zoned, civil, tz::TimeZone};
use longbridge_proto::quote::TradeSession;

pub const MARKET_ZONE: &str = "America/New_York";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowEnd {
    SameDay(i32, i32),
    NextDay(i32, i32),
}

pub type Window = ((i32, i32), WindowEnd);

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

pub fn market_zone() -> TimeZone {
    TimeZone::get(MARKET_ZONE).expect("the zone database carries America/New_York")
}

pub fn market_local(millis: i64) -> Zoned {
    let timestamp = Timestamp::from_millisecond(millis).expect("a representable instant");
    Zoned::new(timestamp, market_zone())
}

pub fn now_millis() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch");
    i64::try_from(since_epoch.as_millis()).expect("a representable instant")
}

pub fn market_date(millis: i64) -> String {
    market_local(millis).strftime("%Y%m%d").to_string()
}

pub fn market_instant(date: &str, hour: i8, minute: i8) -> i64 {
    civil::Date::strptime("%Y%m%d", date)
        .expect("a date this suite formatted")
        .at(hour, minute, 0, 0)
        .to_zoned(market_zone())
        .expect("a market-local wall time is a real instant")
        .timestamp()
        .as_millisecond()
}

pub fn session_of(local: &Zoned) -> Option<TradeSession> {
    session_in(&SESSION_WINDOWS, local).map(|(session, _)| session)
}

pub fn half_day_session_of(local: &Zoned) -> Option<TradeSession> {
    session_in(&HALF_DAY_WINDOWS, local).map(|(session, _)| session)
}

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
                WindowEnd::NextDay(close_hour, close_minute) => {
                    let close = close_hour * 60 + close_minute;
                    (open..24 * 60).contains(&minute) || (0..close).contains(&minute)
                }
            }
        })
        .copied()
}

pub fn bucket_start(millis: i64, period_millis: Option<i64>) -> Option<i64> {
    bucket_in(&SESSION_WINDOWS, millis, period_millis)
}

pub fn half_day_bucket_start(millis: i64, period_millis: Option<i64>) -> Option<i64> {
    bucket_in(&HALF_DAY_WINDOWS, millis, period_millis)
}

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
