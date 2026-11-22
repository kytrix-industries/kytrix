//! The chart's formatters, asserted at fixed instants and offsets.
//!
//! Both interface languages are pinned: the axis and the hovered time have to follow
//! the language showing, not the venue's English spelling. The shaped-line cache is
//! asserted through the engine's own test host, because a lease is what it manages.

use std::{rc::Rc, sync::Arc};

use i18n::{global::embedded, language::codes::LanguageCode, translate::Translate};
use time::{Date, Month, UtcOffset};
use ui_core::paint::color::Color;
use widget::test_support::WidgetTestApp;

use super::{
    geometry::TimeLabelKind,
    text::{
        ChartTextCache, REGULAR_WEIGHT, TEXT_CACHE_CAPACITY, format_axis_labels, format_axis_time,
        format_hover_time, format_percent_integer, format_percent_one_decimal, label_style,
    },
};

/// The catalogue the chart formats through, rendering `language`.
fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(embedded::load().expect("the embedded locales should load")),
        language,
    )
}

/// One axis label at UTC.
fn axis(timestamp: i64, label: TimeLabelKind, language: LanguageCode) -> String {
    format_axis_time(timestamp, label, UtcOffset::UTC, &translate(language)).to_string()
}

/// One axis label in a display zone.
fn axis_in(
    timestamp: i64,
    label: TimeLabelKind,
    zone: UtcOffset,
    language: LanguageCode,
) -> String {
    format_axis_time(timestamp, label, zone, &translate(language)).to_string()
}

/// One hovered time at UTC.
fn hover(timestamp: i64, language: LanguageCode) -> String {
    format_hover_time(timestamp, UtcOffset::UTC, &translate(language)).to_string()
}

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

/// The style the chart's axis labels are shaped in.
fn axis_style() -> ui_core::text::TextStyle {
    label_style(12.0, 14.0, Color::WHITE, REGULAR_WEIGHT)
}

#[test]
fn a_clock_tier_reads_the_time_of_day_in_each_language() {
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::Clock,
            LanguageCode::En
        ),
        "8:00 AM"
    );
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::Clock,
            LanguageCode::ZhCn
        ),
        "08:00",
        "Chinese reads a twenty-four-hour clock without a meridiem"
    );
}

#[test]
fn a_day_tier_reads_the_date_in_each_language() {
    assert_eq!(
        axis(january_fifth_eight(), TimeLabelKind::Day, LanguageCode::En),
        "Jan 5"
    );
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::Day,
            LanguageCode::ZhCn
        ),
        "1月5日"
    );
}

#[test]
fn a_month_tier_reads_the_month_without_a_year() {
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::Month,
            LanguageCode::En
        ),
        "Jan",
        "the fifteen-month reference labels bare month names"
    );
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::Month,
            LanguageCode::ZhCn
        ),
        "1月"
    );
}

#[test]
fn a_year_disambiguated_month_tier_reads_the_year_too() {
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::MonthYear,
            LanguageCode::En
        ),
        "Jan 2026"
    );
    assert_eq!(
        axis(
            january_fifth_eight(),
            TimeLabelKind::MonthYear,
            LanguageCode::ZhCn
        ),
        "2026年1月"
    );
}

#[test]
fn the_clock_reads_midnight_and_noon_in_each_language() {
    assert_eq!(
        axis(midnight(), TimeLabelKind::Clock, LanguageCode::En),
        "12:00 AM"
    );
    assert_eq!(
        axis(midnight(), TimeLabelKind::Clock, LanguageCode::ZhCn),
        "00:00"
    );

    let noon = midnight() + 12 * 3_600_000;
    assert_eq!(
        axis(noon, TimeLabelKind::Clock, LanguageCode::En),
        "12:00 PM"
    );
    assert_eq!(
        axis(noon, TimeLabelKind::Clock, LanguageCode::ZhCn),
        "12:00"
    );

    let late = midnight() + 23 * 3_600_000 + 59 * 60_000;
    assert_eq!(
        axis(late, TimeLabelKind::Clock, LanguageCode::En),
        "11:59 PM"
    );
    assert_eq!(
        axis(late, TimeLabelKind::Clock, LanguageCode::ZhCn),
        "23:59"
    );
}

#[test]
fn the_axis_time_is_read_in_the_display_zone() {
    let zone = UtcOffset::from_hms(8, 0, 0).expect("the fixture offset is valid");
    let timestamp = midnight();

    assert_eq!(
        axis_in(timestamp, TimeLabelKind::Clock, zone, LanguageCode::En),
        "8:00 AM",
        "midnight UTC is eight in the morning at +08:00"
    );
    assert_eq!(
        axis_in(
            timestamp,
            TimeLabelKind::Clock,
            UtcOffset::from_hms(-5, 0, 0).expect("valid"),
            LanguageCode::ZhCn
        ),
        "19:00",
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
    let labels = format_axis_labels(
        &timestamps,
        TimeLabelKind::Clock,
        UtcOffset::UTC,
        &translate(LanguageCode::En),
    );

    assert_eq!(labels[0].as_str(), "8:00 AM");
    assert_eq!(labels[1].as_str(), "11:00 AM");
    assert_eq!(
        labels[2].as_str(),
        "",
        "the closing tick repeats the opening clock label and is blanked"
    );
}

#[test]
fn the_hovered_time_carries_the_day_but_not_the_year_in_each_language() {
    assert_eq!(
        hover(january_fifth_eight(), LanguageCode::En),
        "Jan 5, 8:00 AM"
    );
    assert_eq!(
        hover(january_fifth_eight(), LanguageCode::ZhCn),
        "1月5日 08:00"
    );
}

#[test]
fn every_month_has_its_name_in_each_language() {
    let expected_en = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let expected_zh = [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ];
    for (index, abbreviation) in expected_en.into_iter().enumerate() {
        let date =
            Date::from_calendar_date(2026, Month::try_from(index as u8 + 1).expect("a month"), 1)
                .expect("the fixture date is valid");
        let timestamp = date.midnight().assume_utc().unix_timestamp() * 1_000;
        assert_eq!(
            axis(timestamp, TimeLabelKind::Month, LanguageCode::En),
            abbreviation
        );
        assert_eq!(
            axis(timestamp, TimeLabelKind::Month, LanguageCode::ZhCn),
            expected_zh[index]
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

/// A shaped line is shaped once: the second read of the same text and style is the
/// same handle, which is what makes a hover re-shape nothing.
#[test]
fn the_text_cache_shapes_a_line_once() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();

    let first = cache.line(app.app_mut(), "8:00 AM", &style);
    let same = cache.line(app.app_mut(), "8:00 AM", &style);

    assert!(Arc::ptr_eq(&first, &same));
    assert_eq!(cache.cached_len(), 1);
}

/// A different style is a different line: colour and weight are part of its identity.
#[test]
fn the_text_cache_keeps_a_line_per_style() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();
    let coloured = label_style(12.0, 14.0, Color::BLACK, REGULAR_WEIGHT);

    let first = cache.line(app.app_mut(), "8:00 AM", &style);
    let second = cache.line(app.app_mut(), "8:00 AM", &coloured);

    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(cache.cached_len(), 2);
}

/// The cache is bounded and drops the least recently used label: reads refresh an
/// entry, so the label that falls out is the one furthest from the last read.
#[test]
fn the_text_cache_keeps_a_bounded_lru_window() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();
    let mut second = None;
    for index in 0..TEXT_CACHE_CAPACITY {
        let handle = cache.line(app.app_mut(), &format!("label-{index}"), &style);
        if index == 2 {
            second = Some(handle);
        }
    }
    let second = second.expect("the third label was shaped");
    assert_eq!(cache.cached_len(), TEXT_CACHE_CAPACITY);

    // Two recent reads: `label-0` and `label-1` are no longer the least recently used,
    // so the next miss has to evict `label-2`.
    cache.line(app.app_mut(), "label-0", &style);
    let first = cache.line(app.app_mut(), "label-1", &style);
    cache.line(app.app_mut(), "new-label", &style);

    assert_eq!(cache.cached_len(), TEXT_CACHE_CAPACITY);
    let evicted = cache.line(app.app_mut(), "label-2", &style);
    assert!(
        !Arc::ptr_eq(&evicted, &second),
        "the least recently used label was the one dropped"
    );
    let kept = cache.line(app.app_mut(), "label-1", &style);
    assert!(
        Arc::ptr_eq(&kept, &first),
        "a label that was read again survived the eviction window"
    );
}

/// A handle whose atlas lease moved is re-shaped rather than drawn from reused
/// texture: the engine refuses to encode it, so keeping it would lose the label.
#[test]
fn the_text_cache_reshapes_a_line_whose_atlas_lease_is_gone() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();
    let before = cache.line(app.app_mut(), "8:00 AM", &style);

    // An atlas reset under the same font stack: every handle shaped so far is leased
    // from an atlas that no longer exists.
    let stack = app.app().font_stack().clone();
    app.app_mut().set_font_stack(stack);

    let after = cache.line(app.app_mut(), "8:00 AM", &style);

    assert!(before.is_stale(), "the font stack change moved the lease");
    assert!(
        !Arc::ptr_eq(&before, &after),
        "the stale handle was shaped again"
    );
    assert!(!after.is_stale(), "and the fresh one is drawable");
    assert_eq!(cache.cached_len(), 1);
}

/// The width the cache reports is the shaped line's own advance width.
#[test]
fn the_text_cache_measures_the_line_it_shaped() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();

    let width = cache.width(app.app_mut(), "100%", &style);
    let line = cache.line(app.app_mut(), "100%", &style);

    assert_eq!(width, line.size().width);
    assert!(width > 0.0);
}
