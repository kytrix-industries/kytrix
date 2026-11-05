//! The chart's formatters, asserted at fixed instants and offsets.
//!
//! Month names are English on purpose: the chart is read beside the venue's own and
//! the two axes have to agree. The tests pin the exact strings so a later "improve the
//! format" cannot quietly change what the axis says.

use time::{Date, Month, UtcOffset};

use super::{
    geometry::TimeLabelKind,
    text::{
        format_axis_labels, format_axis_time, format_hover_time, format_percent_integer,
        format_percent_one_decimal,
    },
};

/// The instant 2026-01-05 08:00:00 UTC, in milliseconds.
fn january_fifth_eight() -> i64 {
    Date::from_calendar_date(2026, Month::January, 5)
        .expect("the fixture date is valid")
        .with_hms(8, 0, 0)
        .expect("the fixture time is valid")
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

/// The instant 2026-01-05 00:00:00 UTC, in milliseconds.
fn midnight() -> i64 {
    Date::from_calendar_date(2026, Month::January, 5)
        .expect("the fixture date is valid")
        .midnight()
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

#[test]
fn a_clock_tier_reads_the_time_of_day() {
    assert_eq!(
        format_axis_time(january_fifth_eight(), TimeLabelKind::Clock, UtcOffset::UTC).as_str(),
        "8:00 AM"
    );
}

#[test]
fn a_day_tier_reads_the_date() {
    assert_eq!(
        format_axis_time(january_fifth_eight(), TimeLabelKind::Day, UtcOffset::UTC).as_str(),
        "Jan 5"
    );
}

#[test]
fn a_month_tier_reads_the_month_without_a_year() {
    assert_eq!(
        format_axis_time(january_fifth_eight(), TimeLabelKind::Month, UtcOffset::UTC).as_str(),
        "Jan",
        "the fifteen-month reference labels bare month names"
    );
}

#[test]
fn a_year_disambiguated_month_tier_reads_the_year_too() {
    assert_eq!(
        format_axis_time(
            january_fifth_eight(),
            TimeLabelKind::MonthYear,
            UtcOffset::UTC
        )
        .as_str(),
        "Jan 2026"
    );
}

#[test]
fn the_clock_reads_twelve_hour_at_midnight_and_noon() {
    let at_midnight = format_axis_time(midnight(), TimeLabelKind::Clock, UtcOffset::UTC);
    assert_eq!(at_midnight.as_str(), "12:00 AM");

    let at_noon = format_axis_time(
        midnight() + 12 * 3_600_000,
        TimeLabelKind::Clock,
        UtcOffset::UTC,
    );
    assert_eq!(at_noon.as_str(), "12:00 PM");

    let late = format_axis_time(
        midnight() + 23 * 3_600_000 + 59 * 60_000,
        TimeLabelKind::Clock,
        UtcOffset::UTC,
    );
    assert_eq!(late.as_str(), "11:59 PM");
}

#[test]
fn the_axis_time_is_read_in_the_display_zone() {
    let zone = UtcOffset::from_hms(8, 0, 0).expect("the fixture offset is valid");
    let timestamp = midnight();

    assert_eq!(
        format_axis_time(timestamp, TimeLabelKind::Clock, zone).as_str(),
        "8:00 AM",
        "midnight UTC is eight in the morning at +08:00"
    );
    assert_eq!(
        format_axis_time(
            timestamp,
            TimeLabelKind::Clock,
            UtcOffset::from_hms(-5, 0, 0).expect("valid")
        )
        .as_str(),
        "7:00 PM",
        "and the previous evening at -05:00"
    );
}

#[test]
fn a_repeated_clock_label_is_blanked_while_its_tick_stays() {
    let timestamps = [
        january_fifth_eight(),
        january_fifth_eight() + 3 * 3_600_000,
        january_fifth_eight() + 24 * 3_600_000,
    ];
    let labels = format_axis_labels(&timestamps, TimeLabelKind::Clock, UtcOffset::UTC);

    assert_eq!(labels[0].as_str(), "8:00 AM");
    assert_eq!(labels[1].as_str(), "11:00 AM");
    assert_eq!(
        labels[2].as_str(),
        "",
        "the closing tick repeats the opening clock label and is blanked"
    );
}

#[test]
fn the_hovered_time_carries_the_day_but_not_the_year() {
    assert_eq!(
        format_hover_time(january_fifth_eight(), UtcOffset::UTC).as_str(),
        "Jan 5, 8:00 AM"
    );
}

#[test]
fn every_month_has_its_english_abbreviation() {
    let expected = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    for (index, abbreviation) in expected.into_iter().enumerate() {
        let date =
            Date::from_calendar_date(2026, Month::try_from(index as u8 + 1).expect("a month"), 1)
                .expect("the fixture date is valid");
        let timestamp = date.midnight().assume_utc().unix_timestamp() * 1_000;
        assert_eq!(
            format_axis_time(timestamp, TimeLabelKind::Month, UtcOffset::UTC).as_str(),
            abbreviation
        );
    }
}

#[test]
fn the_hover_label_always_shows_one_decimal() {
    assert_eq!(format_percent_one_decimal(26.5).as_str(), "26.5%");
    assert_eq!(format_percent_one_decimal(9.0).as_str(), "9.0%");
    assert_eq!(format_percent_one_decimal(0.0).as_str(), "0.0%");
    assert_eq!(format_percent_one_decimal(100.0).as_str(), "100.0%");
}

#[test]
fn the_axis_and_legend_round_to_whole_percent() {
    assert_eq!(format_percent_integer(27.4).as_str(), "27%");
    assert_eq!(format_percent_integer(0.0).as_str(), "0%");
    assert_eq!(format_percent_integer(100.0).as_str(), "100%");
}
