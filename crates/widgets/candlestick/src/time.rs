//! Calendar arithmetic and time labels for the chart.
//!
//! Two questions are answered here: how a candle's timestamp reads on an axis
//! or under the crosshair, and when the candle the venue is currently building
//! closes. The second drives the countdown next to the latest price, so it has
//! to agree with the market participant's own boundary rather than with a fixed duration:
//! monthly, quarterly and yearly candles advance by calendar month, and OKX opens
//! them on a UTC+8 boundary while the others use UTC.
//!
//! The intervals themselves are the market request's closed set, and how an
//! interval is counted — a second, a calendar month, three of them, twelve — is
//! read here rather than guessed from the venue: `1M` advances by a calendar
//! month and `1Q` by three on every route that publishes one.

use std::time::Duration;

use market_participants_connector::market::timeframe::TimeFrame;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, ToSmolStr, format_smolstr};
use time::{Date, OffsetDateTime, UtcOffset};
use widget::util::format_countdown;

/// The unit one interval is counted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeFrameUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl TimeFrameUnit {
    /// The i18n key the interval picker heads this unit's group with.
    pub(crate) const fn group_key(self) -> &'static str {
        match self {
            Self::Second => "widget.candlestick.seconds",
            Self::Minute => "widget.candlestick.minutes",
            Self::Hour => "widget.candlestick.hours",
            Self::Day => "widget.candlestick.days",
            Self::Week => "widget.candlestick.weeks",
            Self::Month => "widget.candlestick.months",
            Self::Quarter => "widget.candlestick.quarters",
            Self::Year => "widget.candlestick.years",
        }
    }
}

/// The unit groups the interval picker offers, in the order it draws them.
pub(crate) const TIMEFRAME_GROUPS: [TimeFrameUnit; 8] = [
    TimeFrameUnit::Second,
    TimeFrameUnit::Minute,
    TimeFrameUnit::Hour,
    TimeFrameUnit::Day,
    TimeFrameUnit::Week,
    TimeFrameUnit::Month,
    TimeFrameUnit::Quarter,
    TimeFrameUnit::Year,
];

/// Every interval a market request can carry, in the order the picker offers them.
///
/// This is the widget's own table: the closed set a request can express, the order the picker
/// draws intervals in, and what a route that cannot be asked is offered instead. Which of them
/// a route serves is that route's own answer ([`offered_timeframes`]), so the table claims
/// nothing about any venue — `1s` is here because the request can carry it, not because every
/// venue publishes it.
pub(crate) const TIMEFRAMES: [TimeFrame; 25] = [
    TimeFrame::Second1,
    TimeFrame::Minute1,
    TimeFrame::Minute2,
    TimeFrame::Minute3,
    TimeFrame::Minute5,
    TimeFrame::Minute10,
    TimeFrame::Minute15,
    TimeFrame::Minute20,
    TimeFrame::Minute30,
    TimeFrame::Minute45,
    TimeFrame::Hour1,
    TimeFrame::Hour2,
    TimeFrame::Hour3,
    TimeFrame::Hour4,
    TimeFrame::Hour6,
    TimeFrame::Hour8,
    TimeFrame::Hour12,
    TimeFrame::Day1,
    TimeFrame::Day2,
    TimeFrame::Day3,
    TimeFrame::Week1,
    TimeFrame::Month1,
    TimeFrame::Month3,
    TimeFrame::Quarter1,
    TimeFrame::Year1,
];

/// The intervals the picker offers out of [`TIMEFRAMES`] for one route.
///
/// A route's own declaration says which of them it serves, so what the picker offers is that
/// answer read into this table — and because the table is already in display order, filtering
/// it needs no second ordering table.
///
/// `None` is a route this build could not ask. That answer proves nothing about what the venue
/// serves, so the picker offers the closed set and the choice stays the user's: a request is the
/// only answer the chart can get from such a route, and the venue's own rejection of one is what
/// the pane's recovery settles. This is the widget's existing reading of a request rather than a
/// fallback layer of its own.
pub(crate) fn offered_timeframes(published: Option<&[TimeFrame]>) -> Vec<TimeFrame> {
    match published {
        Some(published) => TIMEFRAMES
            .into_iter()
            .filter(|timeframe| published.contains(timeframe))
            .collect(),
        None => TIMEFRAMES.to_vec(),
    }
}

/// The tick and the unit of one interval.
pub(crate) const fn tick_and_unit(timeframe: TimeFrame) -> (u8, TimeFrameUnit) {
    match timeframe {
        TimeFrame::Second1 => (1, TimeFrameUnit::Second),
        TimeFrame::Minute1 => (1, TimeFrameUnit::Minute),
        TimeFrame::Minute2 => (2, TimeFrameUnit::Minute),
        TimeFrame::Minute3 => (3, TimeFrameUnit::Minute),
        TimeFrame::Minute5 => (5, TimeFrameUnit::Minute),
        TimeFrame::Minute10 => (10, TimeFrameUnit::Minute),
        TimeFrame::Minute15 => (15, TimeFrameUnit::Minute),
        TimeFrame::Minute20 => (20, TimeFrameUnit::Minute),
        TimeFrame::Minute30 => (30, TimeFrameUnit::Minute),
        TimeFrame::Minute45 => (45, TimeFrameUnit::Minute),
        TimeFrame::Hour1 => (1, TimeFrameUnit::Hour),
        TimeFrame::Hour2 => (2, TimeFrameUnit::Hour),
        TimeFrame::Hour3 => (3, TimeFrameUnit::Hour),
        TimeFrame::Hour4 => (4, TimeFrameUnit::Hour),
        TimeFrame::Hour6 => (6, TimeFrameUnit::Hour),
        TimeFrame::Hour8 => (8, TimeFrameUnit::Hour),
        TimeFrame::Hour12 => (12, TimeFrameUnit::Hour),
        TimeFrame::Day1 => (1, TimeFrameUnit::Day),
        TimeFrame::Day2 => (2, TimeFrameUnit::Day),
        TimeFrame::Day3 => (3, TimeFrameUnit::Day),
        TimeFrame::Week1 => (1, TimeFrameUnit::Week),
        TimeFrame::Month1 => (1, TimeFrameUnit::Month),
        TimeFrame::Month3 => (3, TimeFrameUnit::Month),
        TimeFrame::Quarter1 => (3, TimeFrameUnit::Quarter),
        TimeFrame::Year1 => (12, TimeFrameUnit::Year),
    }
}

/// The interval the picker identifies by `label`.
///
/// The picker hands back the label it drew, which is the interval's own spelling
/// rather than a position in a list, so this is the one place a label becomes an
/// interval again.
pub(crate) fn timeframe_of_label(label: &str) -> Option<TimeFrame> {
    TIMEFRAMES
        .into_iter()
        .find(|timeframe| timeframe.label() == label)
}

/// The widest label [`remaining_time_label`] can produce for `interval`.
///
/// The price axis is widened to fit it, so the countdown under the latest price
/// cannot push the axis tag out of shape when it grows a digit.
pub(crate) fn maximum_countdown_label(interval: TimeFrame) -> &'static str {
    let (tick, unit) = tick_and_unit(interval);
    match unit {
        TimeFrameUnit::Second | TimeFrameUnit::Minute => "00:00",
        TimeFrameUnit::Hour | TimeFrameUnit::Day => "00:00:00",
        TimeFrameUnit::Week => "000:00:00",
        TimeFrameUnit::Month if tick == 1 => "000:00:00",
        TimeFrameUnit::Month | TimeFrameUnit::Quarter => "0000:00:00",
        TimeFrameUnit::Year => "00000:00:00",
    }
}

/// The time left in the candle that opens at `candle_timestamp`.
pub(crate) fn remaining_time_label(
    candle_timestamp: i64,
    interval: TimeFrame,
    venue: &VenueId,
    now_timestamp: i64,
) -> SmolStr {
    candle_end_timestamp_millis(candle_timestamp, interval, venue).map_or_else(
        || SmolStr::new_static("00:00"),
        |end_timestamp| format_remaining_time(candle_timestamp, end_timestamp, now_timestamp),
    )
}

/// How long to wait before the countdown label reads differently.
///
/// Waking on the label's own transition rather than on a fixed second keeps the
/// countdown aligned after a late timer fires, and never overshoots the candle
/// boundary it is counting down to.
pub(crate) fn countdown_refresh_delay(
    candle_timestamp: i64,
    interval: TimeFrame,
    venue: &VenueId,
    now_timestamp: i64,
) -> Duration {
    let Some(end_timestamp) = candle_end_timestamp_millis(candle_timestamp, interval, venue) else {
        return next_second_boundary_delay(now_timestamp);
    };
    let remaining_millis = i128::from(end_timestamp) - i128::from(now_timestamp);
    if remaining_millis <= 0 {
        return next_second_boundary_delay(now_timestamp);
    }

    let remainder = remaining_millis.rem_euclid(1_000);
    let delay_millis = if remainder == 0 { 1_000 } else { remainder };
    Duration::from_millis(u64::try_from(delay_millis).unwrap_or(1_000))
}

/// How long to wait until the wall clock crosses the next second.
pub(crate) fn next_second_boundary_delay(now_timestamp: i64) -> Duration {
    let elapsed_millis = now_timestamp.rem_euclid(1_000);
    Duration::from_millis(u64::try_from(1_000 - elapsed_millis).unwrap_or(1_000))
}

pub(crate) fn current_timestamp_millis() -> i64 {
    let millis = OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .div_euclid(1_000_000);
    match i64::try_from(millis) {
        Ok(millis) => millis,
        Err(_) if millis.is_negative() => i64::MIN,
        Err(_) => i64::MAX,
    }
}

/// How a timestamp reads on the time axis, at the resolution `interval` needs and in
/// the zone the settings display data in.
pub(crate) fn format_axis_time(timestamp: i64, interval: TimeFrame, zone: UtcOffset) -> SmolStr {
    let Some(datetime) = datetime(timestamp, zone) else {
        return timestamp.to_smolstr();
    };
    let (tick, unit) = tick_and_unit(interval);
    match unit {
        TimeFrameUnit::Second => format_smolstr!(
            "{:02}:{:02}:{:02}",
            datetime.hour(),
            datetime.minute(),
            datetime.second()
        ),
        TimeFrameUnit::Minute => format_smolstr!("{:02}:{:02}", datetime.hour(), datetime.minute()),
        TimeFrameUnit::Hour if tick == 1 => {
            format_smolstr!("{:02}:{:02}", datetime.hour(), datetime.minute())
        }
        TimeFrameUnit::Hour => format_smolstr!(
            "{:02}-{:02} {:02}:00",
            datetime.month() as u8,
            datetime.day(),
            datetime.hour()
        ),
        TimeFrameUnit::Day | TimeFrameUnit::Week => {
            format_smolstr!("{:02}-{:02}", datetime.month() as u8, datetime.day())
        }
        TimeFrameUnit::Month | TimeFrameUnit::Quarter | TimeFrameUnit::Year => {
            format_smolstr!("{:04}-{:02}", datetime.year(), datetime.month() as u8)
        }
    }
}

/// How a timestamp reads under the crosshair, which has room for the full date and
/// is read in the zone the settings display data in.
pub(crate) fn format_crosshair_time(
    timestamp: i64,
    interval: TimeFrame,
    zone: UtcOffset,
) -> SmolStr {
    let Some(datetime) = datetime(timestamp, zone) else {
        return timestamp.to_smolstr();
    };
    match tick_and_unit(interval).1 {
        TimeFrameUnit::Second => format_smolstr!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            datetime.year(),
            datetime.month() as u8,
            datetime.day(),
            datetime.hour(),
            datetime.minute(),
            datetime.second()
        ),
        TimeFrameUnit::Minute => format_smolstr!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            datetime.year(),
            datetime.month() as u8,
            datetime.day(),
            datetime.hour(),
            datetime.minute()
        ),
        TimeFrameUnit::Hour => format_smolstr!(
            "{:04}-{:02}-{:02} {:02}:00",
            datetime.year(),
            datetime.month() as u8,
            datetime.day(),
            datetime.hour()
        ),
        TimeFrameUnit::Day | TimeFrameUnit::Week => format_smolstr!(
            "{:04}-{:02}-{:02}",
            datetime.year(),
            datetime.month() as u8,
            datetime.day()
        ),
        TimeFrameUnit::Month | TimeFrameUnit::Quarter | TimeFrameUnit::Year => {
            format_smolstr!("{:04}-{:02}", datetime.year(), datetime.month() as u8)
        }
    }
}

fn format_remaining_time(start_timestamp: i64, end_timestamp: i64, now_timestamp: i64) -> SmolStr {
    let duration_millis = (i128::from(end_timestamp) - i128::from(start_timestamp)).max(0_i128);
    let remaining_millis =
        (i128::from(end_timestamp) - i128::from(now_timestamp)).clamp(0_i128, duration_millis);
    // The clock is the shared one: it rounds up to the next visible second and hides
    // the hours field while there are no hours left, which is what a countdown of a
    // minute, an hour or a month all want.
    format_countdown(i64::try_from(remaining_millis).unwrap_or(i64::MAX))
}

/// The instant the candle opening at `candle_timestamp` stops being built.
///
/// `None` means the timestamp cannot be advanced — an out-of-range value, or a
/// calendar month that does not exist.
fn candle_end_timestamp_millis(
    candle_timestamp: i64,
    interval: TimeFrame,
    venue: &VenueId,
) -> Option<i64> {
    let (tick, unit) = tick_and_unit(interval);
    if matches!(
        unit,
        TimeFrameUnit::Month | TimeFrameUnit::Quarter | TimeFrameUnit::Year
    ) {
        return add_calendar_months(candle_timestamp, tick, candle_boundary_offset(venue));
    }

    let unit_millis = match unit {
        TimeFrameUnit::Second => 1_000_i64,
        TimeFrameUnit::Minute => 60_000_i64,
        TimeFrameUnit::Hour => 3_600_000_i64,
        TimeFrameUnit::Day => 86_400_000_i64,
        TimeFrameUnit::Week => 604_800_000_i64,
        TimeFrameUnit::Month | TimeFrameUnit::Quarter | TimeFrameUnit::Year => return None,
    };
    unit_millis
        .checked_mul(i64::from(tick))
        .and_then(|duration| candle_timestamp.checked_add(duration))
}

/// The offset whose midnight a venue opens its calendar candles on.
///
/// A calendar boundary is a venue convention and no capability descriptor carries
/// one, so the venue that has its own is named here by the identity it is
/// registered under, never by an inferred product.
fn candle_boundary_offset(venue: &VenueId) -> UtcOffset {
    if *venue == VenueId::Okx {
        return UtcOffset::from_hms(8, 0, 0).unwrap_or(UtcOffset::UTC);
    }
    UtcOffset::UTC
}

/// Advances a timestamp by whole calendar months in `offset`.
///
/// Clamping to the target month's length is what makes the 31st of January plus
/// one month land on the 28th or 29th of February instead of overflowing, and a
/// candle that opened on the last day of its month always lands on the last day
/// of the target month.
fn add_calendar_months(timestamp: i64, months: u8, offset: UtcOffset) -> Option<i64> {
    let datetime = datetime(timestamp, offset)?;
    let source_year = datetime.year();
    let source_month = datetime.month();
    let month_index = i64::from(source_year)
        .checked_mul(12)?
        .checked_add(i64::from(source_month as u8) - 1)?
        .checked_add(i64::from(months))?;
    let target_year = i32::try_from(month_index.div_euclid(12)).ok()?;
    let target_month = u8::try_from(month_index.rem_euclid(12) + 1)
        .ok()
        .and_then(|month| time::Month::try_from(month).ok())?;
    let source_last_day = source_month.length(source_year);
    let target_last_day = target_month.length(target_year);
    let target_day = if datetime.day() == source_last_day {
        target_last_day
    } else {
        datetime.day().min(target_last_day)
    };
    let target_date = Date::from_calendar_date(target_year, target_month, target_day).ok()?;
    let target_millis = datetime
        .replace_date(target_date)
        .unix_timestamp_nanos()
        .div_euclid(1_000_000);
    i64::try_from(target_millis).ok()
}

/// The instant `timestamp` names, read in `zone`, or nothing when either the
/// timestamp or its local reading is outside the calendar.
///
/// The offset can move an instant past the last date the calendar holds, and a
/// caller that formatted that would panic: an instant off the end of the range is
/// one this cannot read.
fn datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
}

#[cfg(test)]
mod tests {
    use market_participants_connector::market::timeframe::TimeFrame;
    use market_participants_model::identifier::VenueId;

    use super::*;

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue should be canonical")
    }

    // The shape of the shared clock is covered by its own tests; what is tested here
    // is that a candle's countdown reads through it with the clamping a candle needs.
    #[test]
    fn remaining_time_should_round_partial_seconds_up() {
        assert_eq!(format_remaining_time(0, 120_000, 58_001), "01:02");
    }

    #[test]
    fn remaining_time_should_read_the_way_the_shared_clock_reads() {
        assert_eq!(format_remaining_time(0, 3_662_000, 0), "01:01:02");
        assert_eq!(format_remaining_time(0, 3_599_000, 0), "59:59");
        assert_eq!(format_remaining_time(0, 3_600_000, 0), "01:00:00");
    }

    #[test]
    fn remaining_time_should_saturate_after_candle_end() {
        assert_eq!(format_remaining_time(0, 60_000, 60_001), "00:00");
    }

    #[test]
    fn remaining_time_should_never_exceed_the_candle_it_belongs_to() {
        // A clock that ran past the next candle's close still counts down to this
        // one's, because the label belongs to the candle being drawn.
        assert_eq!(format_remaining_time(0, 60_000, -600_000), "01:00");
    }

    #[test]
    fn countdown_refresh_delay_should_target_the_next_label_transition() {
        assert_eq!(
            countdown_refresh_delay(0, TimeFrame::Minute5, &venue("binance"), 238_350),
            Duration::from_millis(650)
        );
    }

    #[test]
    fn countdown_refresh_delay_should_realign_after_a_late_wakeup() {
        assert_eq!(
            countdown_refresh_delay(0, TimeFrame::Minute5, &venue("binance"), 239_200),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn countdown_refresh_delay_should_wait_one_second_on_an_exact_boundary() {
        assert_eq!(
            countdown_refresh_delay(0, TimeFrame::Minute5, &venue("binance"), 239_000),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn fixed_interval_end_should_add_the_interval_duration() {
        assert_eq!(
            candle_end_timestamp_millis(1_000, TimeFrame::Minute5, &venue("binance")),
            Some(301_000)
        );
    }

    #[test]
    fn binance_month_end_should_advance_in_utc() {
        assert_eq!(
            candle_end_timestamp_millis(1_704_067_200_000, TimeFrame::Month1, &venue("binance"),),
            Some(1_706_745_600_000)
        );
    }

    #[test]
    fn okx_month_end_should_advance_at_utc_plus_eight_boundary() {
        assert_eq!(
            candle_end_timestamp_millis(1_706_716_800_000, TimeFrame::Month1, &venue("okx")),
            Some(1_709_222_400_000)
        );
    }

    #[test]
    fn okx_three_month_end_should_advance_across_year_boundary() {
        assert_eq!(
            candle_end_timestamp_millis(1_727_712_000_000, TimeFrame::Month3, &venue("okx")),
            Some(1_735_660_800_000)
        );
    }

    /// A quarter and a year are calendar steps too, so they advance the way a month does:
    /// the candle that opened at the first instant of the window closes at the first
    /// instant of the next one.
    #[test]
    fn quarter_and_year_ends_should_advance_by_their_calendar_steps() {
        assert_eq!(
            candle_end_timestamp_millis(1_704_067_200_000, TimeFrame::Quarter1, &venue("binance")),
            Some(1_711_929_600_000)
        );
        assert_eq!(
            candle_end_timestamp_millis(1_704_067_200_000, TimeFrame::Year1, &venue("binance")),
            Some(1_735_689_600_000)
        );
    }

    #[test]
    fn candle_end_should_return_none_on_timestamp_overflow() {
        assert_eq!(
            candle_end_timestamp_millis(i64::MAX, TimeFrame::Minute1, &venue("binance")),
            None
        );
    }

    #[test]
    fn axis_time_should_use_the_resolution_of_the_interval() {
        let timestamp = 1_704_067_200_000;

        assert_eq!(
            [
                format_axis_time(timestamp, TimeFrame::Minute1, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Hour1, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Hour4, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Day1, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Month1, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Quarter1, UtcOffset::UTC),
                format_axis_time(timestamp, TimeFrame::Year1, UtcOffset::UTC),
            ],
            [
                "00:00",
                "00:00",
                "01-01 00:00",
                "01-01",
                "2024-01",
                "2024-01",
                "2024-01"
            ]
        );
    }

    /// The axis and the crosshair read the instant in the zone the settings display data
    /// in, so a candle that opened at midnight UTC opens in the afternoon at UTC+14.
    #[test]
    fn axis_and_crosshair_times_should_read_the_instant_in_the_display_zone() {
        // 2024-01-01T00:00:00Z.
        let timestamp = 1_704_067_200_000;
        let zone = UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset");

        assert_eq!(
            format_axis_time(timestamp, TimeFrame::Minute1, zone),
            "14:00"
        );
        assert_eq!(
            format_crosshair_time(timestamp, TimeFrame::Minute1, zone),
            "2024-01-01 14:00"
        );
        assert_eq!(
            format_axis_time(-1, TimeFrame::Day1, zone),
            "01-01",
            "an instant just before the epoch is already the next day at UTC+14"
        );
        assert_eq!(
            format_axis_time(
                -1,
                TimeFrame::Day1,
                UtcOffset::from_hms(-1, 0, 0).expect("UTC-1 is a valid offset")
            ),
            "12-31"
        );
    }

    /// An offset can move an instant past the last day the calendar holds. That is
    /// not a time this can format, so the axis reads the raw stamp rather than
    /// panicking on a date that is not one.
    #[test]
    fn an_instant_the_display_zone_cannot_hold_should_not_panic() {
        let boundary = time::Date::MAX
            .with_hms(23, 0, 0)
            .expect("the last day has an evening")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the last day is a millisecond timestamp");
        let zone = UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset");

        assert_eq!(
            format_axis_time(timestamp, TimeFrame::Minute1, zone),
            timestamp.to_smolstr()
        );
        assert_eq!(
            format_crosshair_time(timestamp, TimeFrame::Minute1, zone),
            timestamp.to_smolstr()
        );

        let boundary = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");
        let zone = UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset");

        assert_eq!(
            format_axis_time(timestamp, TimeFrame::Minute1, zone),
            timestamp.to_smolstr(),
            "a western offset runs off the other end of the calendar"
        );
        assert_eq!(
            format_crosshair_time(timestamp, TimeFrame::Minute1, zone),
            timestamp.to_smolstr()
        );
    }

    #[test]
    fn crosshair_time_should_include_the_date_at_every_resolution() {
        let timestamp = 1_704_067_200_000;

        assert_eq!(
            [
                format_crosshair_time(timestamp, TimeFrame::Minute1, UtcOffset::UTC),
                format_crosshair_time(timestamp, TimeFrame::Hour1, UtcOffset::UTC),
                format_crosshair_time(timestamp, TimeFrame::Day1, UtcOffset::UTC),
                format_crosshair_time(timestamp, TimeFrame::Quarter1, UtcOffset::UTC),
                format_crosshair_time(timestamp, TimeFrame::Year1, UtcOffset::UTC),
            ],
            [
                "2024-01-01 00:00",
                "2024-01-01 00:00",
                "2024-01-01",
                "2024-01",
                "2024-01",
            ]
        );
    }

    #[test]
    fn maximum_countdown_label_should_reserve_room_for_the_widest_value() {
        assert_eq!(
            [
                maximum_countdown_label(TimeFrame::Minute1),
                maximum_countdown_label(TimeFrame::Day1),
                maximum_countdown_label(TimeFrame::Week1),
                maximum_countdown_label(TimeFrame::Month1),
                maximum_countdown_label(TimeFrame::Month3),
                maximum_countdown_label(TimeFrame::Quarter1),
                maximum_countdown_label(TimeFrame::Year1),
            ],
            [
                "00:00",
                "00:00:00",
                "000:00:00",
                "000:00:00",
                "0000:00:00",
                "0000:00:00",
                "00000:00:00"
            ]
        );
    }

    /// The picker hands back the label it drew, so a label has to name exactly one
    /// interval: two intervals spelled the same would make a choice ambiguous.
    #[test]
    fn every_interval_should_be_identified_by_its_own_label() {
        for timeframe in TIMEFRAMES {
            let identified = TIMEFRAMES
                .into_iter()
                .filter(|candidate| candidate.label() == timeframe.label())
                .count();

            assert_eq!(
                identified,
                1,
                "{} should name one interval",
                timeframe.label()
            );
            assert_eq!(timeframe_of_label(timeframe.label()), Some(timeframe));
        }

        assert_eq!(timeframe_of_label("not-an-interval"), None);
    }

    /// The picker offers a route's own answer, and it offers it in this table's order: a venue
    /// that declares its intervals in some other order still gets a picker that reads the way
    /// every other venue's does.
    #[test]
    fn a_routes_answer_should_be_offered_in_the_widgets_own_order() {
        let offered = offered_timeframes(Some(&[
            TimeFrame::Day1,
            TimeFrame::Minute5,
            TimeFrame::Minute1,
        ]));

        assert_eq!(
            offered.len(),
            3,
            "every published interval is offered: {offered:?}"
        );
        assert_eq!(
            offered,
            vec![TimeFrame::Minute1, TimeFrame::Minute5, TimeFrame::Day1],
            "in the table's order rather than the route's"
        );
    }

    /// A route this build cannot ask proves nothing about what it serves, so the picker keeps
    /// offering the closed set and the request answers for it.
    #[test]
    fn an_unanswerable_route_should_offer_every_interval() {
        assert_eq!(
            offered_timeframes(None),
            TIMEFRAMES.to_vec(),
            "no answer offers exactly what a request can carry"
        );
    }

    /// The picker groups the intervals by the unit they are counted in, so every
    /// interval has to belong to exactly one offered group.
    #[test]
    fn every_interval_should_belong_to_one_offered_group() {
        for timeframe in TIMEFRAMES {
            let (_, unit) = tick_and_unit(timeframe);
            let offered = TIMEFRAME_GROUPS
                .into_iter()
                .filter(|group| *group == unit)
                .count();

            assert_eq!(offered, 1, "{timeframe:?} should belong to one group");
            assert!(
                !unit.group_key().is_empty(),
                "{timeframe:?} should head its group with a key"
            );
        }
    }

    #[test]
    fn intervals_should_advance_by_the_duration_their_unit_names() {
        assert_eq!(
            [
                tick_and_unit(TimeFrame::Second1),
                tick_and_unit(TimeFrame::Minute15),
                tick_and_unit(TimeFrame::Hour8),
                tick_and_unit(TimeFrame::Day3),
                tick_and_unit(TimeFrame::Week1),
                tick_and_unit(TimeFrame::Month3),
                tick_and_unit(TimeFrame::Quarter1),
                tick_and_unit(TimeFrame::Year1),
            ],
            [
                (1, TimeFrameUnit::Second),
                (15, TimeFrameUnit::Minute),
                (8, TimeFrameUnit::Hour),
                (3, TimeFrameUnit::Day),
                (1, TimeFrameUnit::Week),
                (3, TimeFrameUnit::Month),
                (3, TimeFrameUnit::Quarter),
                (12, TimeFrameUnit::Year),
            ]
        );
    }
}
