//! The calendar side of a candle: which period of the market's own calendar it is
//! bounded by, and the instant that period opens at.
//!
//! A calendar candle is not a fixed length — a week is not seven days of trading and a
//! year is not 365 of them — so its boundary is a date in the market's zone rather than
//! an interval counted from a session's open. Resolving that date back to an instant
//! goes through the zone's own rules, which is why the market's midnight rather than
//! UTC's is what a day candle starts at.

use jiff::{Timestamp, Zoned, civil, tz::TimeZone};

use crate::market::timeframe::TimeFrame;

/// Where one instant's candle starts, in the market's own terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandleBucket {
    /// A fixed-length candle: the instant the bucket opened, in milliseconds.
    Start(i64),
    /// A calendar candle: the market-local midnight of the period it belongs to.
    Calendar {
        date: civil::Date,
        unit: CalendarUnit,
    },
}

/// The calendar period a calendar-scale candle is bounded by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarUnit {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl CandleBucket {
    /// The instant this bucket opens at, in the market's own zone.
    ///
    /// A fixed-length bucket carries its opening instant outright. A calendar bucket is
    /// a market-local midnight, which is where the zone's rules matter: the instant is
    /// resolved through `jiff` rather than computed from an offset, so the two days a
    /// US year changes its offset are the ones a reader can check against the venue.
    pub(crate) fn start(self, zone: &TimeZone) -> Option<Zoned> {
        match self {
            Self::Start(millis) => {
                let timestamp = Timestamp::from_millisecond(millis).ok()?;
                Some(Zoned::new(timestamp, zone.clone()))
            }
            Self::Calendar { date, .. } => date.at(0, 0, 0, 0).to_zoned(zone.clone()).ok(),
        }
    }
}

/// The market-local midnight a calendar-scale candle of one instant belongs to.
///
/// The period is read in the market's own zone and then resolved back to an instant
/// through the same zone, so a week starts on a Monday the venue would agree with and a
/// day starts at the market's midnight rather than at UTC's.
pub(crate) fn calendar_bucket(local: Zoned, unit: CalendarUnit) -> Option<civil::Date> {
    let date = local.date();
    Some(match unit {
        CalendarUnit::Day => date,
        CalendarUnit::Week => date
            .checked_sub(jiff::Span::new().days(i64::from(date.weekday().to_monday_zero_offset())))
            .ok()?,
        CalendarUnit::Month => date.first_of_month(),
        CalendarUnit::Quarter => {
            let month = (i32::from(date.month()) - 1) / 3 * 3 + 1;
            date.with()
                .month(i8::try_from(month).ok()?)
                .day(1)
                .build()
                .ok()?
        }
        CalendarUnit::Year => date.with().month(1).day(1).build().ok()?,
    })
}

/// The calendar period one timeframe is bounded by, when it is a calendar one.
pub(crate) const fn calendar_unit(timeframe: TimeFrame) -> Option<CalendarUnit> {
    match timeframe {
        TimeFrame::Day1 => Some(CalendarUnit::Day),
        TimeFrame::Week1 => Some(CalendarUnit::Week),
        TimeFrame::Month1 => Some(CalendarUnit::Month),
        TimeFrame::Quarter1 => Some(CalendarUnit::Quarter),
        TimeFrame::Year1 => Some(CalendarUnit::Year),
        _ => None,
    }
}

/// How long one fixed-length timeframe is, when it is a fixed-length one.
///
/// The durations are read off the venue's own period table rather than derived from the
/// interval's name: a `4h` candle is the venue's `FOUR_HOUR`, which is 240 minutes, and
/// the two are the same number stated twice on purpose — the table is the wire's and
/// this is the grid's.
pub(crate) const fn fixed_interval_millis(timeframe: TimeFrame) -> Option<i64> {
    let minutes = match timeframe {
        TimeFrame::Minute1 => 1,
        TimeFrame::Minute2 => 2,
        TimeFrame::Minute3 => 3,
        TimeFrame::Minute5 => 5,
        TimeFrame::Minute10 => 10,
        TimeFrame::Minute15 => 15,
        TimeFrame::Minute20 => 20,
        TimeFrame::Minute30 => 30,
        TimeFrame::Minute45 => 45,
        TimeFrame::Hour1 => 60,
        TimeFrame::Hour2 => 120,
        TimeFrame::Hour3 => 180,
        TimeFrame::Hour4 => 240,
        _ => return None,
    };
    Some(minutes * 60 * 1_000)
}
