//! What the chart's labels say, and the shaped-text cache they are drawn from.

use market_participants_model::decimal::Tick;
use time::UtcOffset;
use ui_core::paint::color::Color;
use widget::test_support::WidgetTestApp;

use super::text::{
    ChartTextCache, NOT_A_READING, format_axis_time, format_compact, format_price, format_quantity,
    format_ratio, format_share, format_tooltip_time, label_style, labelled_value,
};

fn tick(value: &str) -> Tick {
    value.parse().expect("a tick should parse")
}

fn zone(hours: i8) -> UtcOffset {
    UtcOffset::from_hms(hours, 0, 0).expect("the fixture offset is valid")
}

#[test]
fn a_value_should_be_rounded_to_the_instrument_tick() {
    assert_eq!(
        format_quantity(1234.5678, tick("0.01")).as_str(),
        "1,234.57"
    );
}

#[test]
fn a_tick_should_keep_its_own_decimals() {
    assert_eq!(format_price(2.5, tick("0.001")).as_str(), "2.500");
}

#[test]
fn a_value_that_is_not_a_reading_should_be_named_as_such() {
    assert_eq!(format_quantity(f64::NAN, tick("0.01")), NOT_A_READING);
    assert_eq!(format_quantity(-1.0, tick("0.01")), NOT_A_READING);
    assert_eq!(format_price(f64::INFINITY, tick("0.01")), NOT_A_READING);
}

#[test]
fn a_value_with_a_tick_should_use_the_tick_not_a_short_form() {
    assert_eq!(
        format_quantity(2_500_000.0, tick("0.1")).as_str(),
        "2,500,000.0"
    );
}

#[test]
fn a_compact_label_should_use_a_readable_suffix() {
    assert_eq!(format_compact(1_500.0, 100.0).as_str(), "1.5K");
    assert_eq!(format_compact(2_500_000.0, 100_000.0).as_str(), "2.5M");
    assert_eq!(
        format_compact(3_000_000_000.0, 100_000_000.0).as_str(),
        "3B"
    );
}

#[test]
fn neighbouring_compact_labels_should_stay_distinct() {
    let step = 100_000.0;
    let first = format_compact(1_000_000.0, step);
    let second = format_compact(1_100_000.0, step);

    assert_ne!(first, second, "two grid lines read as two numbers");
}

#[test]
fn a_suffix_that_would_swallow_a_step_should_be_dropped() {
    // A thousand-wide step cannot be written under a "K": both ends of it would print
    // as "1K".
    assert_ne!(
        format_compact(1_000.0, 1_000.0),
        format_compact(2_000.0, 1_000.0)
    );
}

#[test]
fn a_ratio_should_be_written_with_three_decimals() {
    assert_eq!(format_ratio(1.5).as_str(), "1.500");
    assert_eq!(format_ratio(0.000_01).as_str(), "1.00e-5");
}

#[test]
fn a_share_should_be_written_as_a_percentage() {
    assert_eq!(format_share(0.5).as_str(), "50.00%");
    assert_eq!(format_share(0.123_4).as_str(), "12.34%");
}

#[test]
fn the_axis_should_omit_the_year_and_the_tooltip_should_not() {
    // 2024-03-05T06:07:08Z
    let timestamp = 1_709_618_828_000;
    let axis = format_axis_time(timestamp, zone(0));
    let tooltip = format_tooltip_time(timestamp, zone(0));

    assert_eq!(axis.as_str(), "03-05 06:07");
    assert_eq!(tooltip.as_str(), "2024-03-05 06:07");
}

#[test]
fn the_axis_and_the_tooltip_should_read_the_display_zone() {
    let timestamp = 1_709_618_828_000;

    assert_eq!(format_axis_time(timestamp, zone(8)).as_str(), "03-05 14:07");
}

#[test]
fn a_timestamp_outside_the_calendar_should_not_panic() {
    assert_eq!(format_axis_time(i64::MAX, zone(0)).as_str(), "--");
}

#[test]
fn an_instant_the_display_zone_cannot_hold_should_not_panic() {
    // The last instant the calendar holds, moved past its end by a positive offset.
    let last = time::OffsetDateTime::UNIX_EPOCH.unix_timestamp() * 1_000;
    let _ = format_axis_time(last, zone(14));
    let _ = format_tooltip_time(i64::MAX, zone(-12));
}

#[test]
fn a_labelled_value_should_name_the_series_before_the_number() {
    assert_eq!(
        labelled_value("Long accounts", "12.34%").as_str(),
        "Long accounts: 12.34%"
    );
}

#[test]
fn an_empty_cache_should_hold_nothing_stale() {
    let mut cache = ChartTextCache::default();

    assert!(!cache.has_stale());
}

/// A shaped line leases glyph tiles from the atlas. When the atlas is rebuilt, the lease is
/// gone and the cache has to say so — once. A stale line that was not drawn again (a
/// tooltip whose pointer moved away) must not keep the answer true: that would turn every
/// later frame into a rebuild (I3).
#[test]
fn a_cache_should_report_a_stale_lease_once_and_then_drop_it() {
    let mut harness = WidgetTestApp::new();
    let style = label_style(10.0, 14.0, Color::WHITE);
    let mut cache = ChartTextCache::default();
    harness.with_cx(|cx| cache.line(cx.app_mut(), "12,345", &style));
    assert!(
        !cache.has_stale(),
        "a line shaped for the current atlas is fresh"
    );

    // An atlas reset under the same font stack: every handle shaped before it is leased
    // from an atlas that no longer exists, and nothing in the requested text changed.
    let stack = harness.app().font_stack().clone();
    harness.app_mut().set_font_stack(stack);

    assert!(cache.has_stale(), "a lease the atlas dropped is reported");
    assert!(
        !cache.has_stale(),
        "and the dropped line is not kept to say it again"
    );
}
