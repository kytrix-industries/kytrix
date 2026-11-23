use jiff::{Timestamp, Zoned, civil, tz::TimeZone};

use crate::market::timeframe::TimeFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandleBucket {
    Start(i64),
    Calendar {
        date: civil::Date,
        unit: CalendarUnit,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarUnit {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl CandleBucket {
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
