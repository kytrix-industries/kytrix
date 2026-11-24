use std::rc::Rc;

use i18n::{embedded, language::codes::LanguageCode, translate::Translate};
use time::{Date, Month, UtcOffset};
use ui_core::{math::px, paint::color::Color, text::TextContent};
use widget::test_support::WidgetTestApp;

use super::{
    geometry::TimeLabelKind,
    text::{
        ChartTextCache, REGULAR_WEIGHT, TEXT_CACHE_CAPACITY, format_axis_labels, format_axis_time,
        format_hover_time, format_percent_integer, format_percent_one_decimal, label_style,
    },
};

fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(embedded::load().expect("the embedded locales should load")),
        language,
    )
}

fn axis(timestamp: i64, label: TimeLabelKind, language: LanguageCode) -> String {
    format_axis_time(timestamp, label, UtcOffset::UTC, &translate(language)).to_string()
}

fn axis_in(
    timestamp: i64,
    label: TimeLabelKind,
    zone: UtcOffset,
    language: LanguageCode,
) -> String {
    format_axis_time(timestamp, label, zone, &translate(language)).to_string()
}

fn hover(timestamp: i64, language: LanguageCode) -> String {
    format_hover_time(timestamp, UtcOffset::UTC, &translate(language)).to_string()
}

fn january_fifth_eight() -> i64 {
    Date::from_calendar_date(2026, Month::January, 5)
        .expect("the fixture date is valid")
        .with_hms(8, 0, 0)
        .expect("the fixture time is valid")
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

fn midnight() -> i64 {
    Date::from_calendar_date(2026, Month::January, 5)
        .expect("the fixture date is valid")
        .midnight()
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

fn axis_style() -> ui_core::text::TextStyle {
    label_style(px(12.0), px(14.0), Color::WHITE, REGULAR_WEIGHT)
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

#[test]
fn the_text_cache_returns_the_line_it_measured() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();

    let first = cache.content(app.app_mut(), "8:00 AM", &style);
    let same = cache.content(app.app_mut(), "8:00 AM", &style);

    assert_eq!(first, same);
    assert_eq!(cache.cached_len(), 1);
}

#[test]
fn the_text_cache_keeps_a_line_per_style() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();
    let coloured = label_style(px(12.0), px(14.0), Color::BLACK, REGULAR_WEIGHT);

    let first = cache.content(app.app_mut(), "8:00 AM", &style);
    let second = cache.content(app.app_mut(), "8:00 AM", &coloured);

    assert_ne!(first, second);
    assert_eq!(cache.cached_len(), 2);
}

#[test]
fn the_text_cache_keeps_a_bounded_lru_window() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();
    for index in 0..TEXT_CACHE_CAPACITY {
        cache.content(app.app_mut(), &format!("label-{index}"), &style);
    }
    assert_eq!(cache.cached_len(), TEXT_CACHE_CAPACITY);

    cache.content(app.app_mut(), "label-0", &style);
    cache.content(app.app_mut(), "label-1", &style);
    cache.content(app.app_mut(), "new-label", &style);

    assert_eq!(cache.cached_len(), TEXT_CACHE_CAPACITY);
    assert!(
        cache.cached("label-0"),
        "a label that was read again was kept"
    );
    assert!(
        cache.cached("label-1"),
        "a label that was read again survived the eviction window"
    );
    assert!(
        !cache.cached("label-2"),
        "the least recently used label was the one dropped"
    );
    assert!(cache.cached("new-label"));
}

#[test]
fn the_text_cache_measures_the_line_it_caches() {
    let mut app = WidgetTestApp::new();
    let mut cache = ChartTextCache::default();
    let style = axis_style();

    let width = cache.width(app.app_mut(), "100%", &style);
    let measured = app
        .app_mut()
        .measure_text(&TextContent::new("100%", style), None)
        .width;

    assert_eq!(width, measured);
    assert!(width > px(0.0));
}
