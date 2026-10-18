use std::str::FromStr;

use market_participants_model::decimal::Tick;

use super::text::labels::*;

fn tick(value: &str) -> Tick {
    Tick::from_str(value).expect("test tick should parse")
}

#[test]
fn a_value_should_be_rounded_to_the_instrument_tick() {
    assert_eq!(format_quantity(12.345_678, tick("0.01")), "12.35");
    assert_eq!(format_price(12.345_678, tick("0.5")), "12.5");
}

#[test]
fn a_tick_should_keep_its_own_decimals() {
    assert_eq!(format_quantity(12.0, tick("0.001")), "12.000");
}

#[test]
fn a_value_that_is_not_a_reading_should_be_named_as_such() {
    assert_eq!(format_quantity(f64::NAN, tick("0.01")), "--");
    assert_eq!(format_price(-1.0, tick("0.01")), "--");
    assert_eq!(format_price(f64::INFINITY, tick("0.01")), "--");
}

#[test]
fn a_value_with_a_tick_should_use_the_tick_not_a_short_form() {
    // A tick is the instrument's own precision, so it wins over a short form:
    // this is the number the venue itself would have printed.
    assert_eq!(format_quantity(1_500_000.0, tick("0.01")), "1,500,000.00");
}

#[test]
fn an_integer_part_should_be_grouped_in_threes() {
    assert_eq!(group_digits("1234567.00"), "1,234,567.00");
    assert_eq!(group_digits("999"), "999");
    assert_eq!(group_digits("1000"), "1,000");
    assert_eq!(group_digits("-12345.5"), "-12,345.5");
    assert_eq!(group_digits("1.50e18"), "1.50e18");
    assert_eq!(group_digits("--"), "--");
    assert_eq!(group_digits(""), "");
}

#[test]
fn a_compact_label_should_use_a_readable_suffix() {
    assert_eq!(compact(999.0), "999");
    assert_eq!(compact(1_500.0), "2K");
    assert_eq!(compact(2_400_000.0), "2M");
    assert_eq!(compact(3_600_000_000.0), "4B");
    assert_eq!(compact(1.5e18), "1.50e18");
}

#[test]
fn neighbouring_compact_labels_should_stay_distinct() {
    // Two grid lines a step apart have to read as two different numbers.
    assert_ne!(
        format_compact(1_000.0, 200.0),
        format_compact(1_200.0, 200.0)
    );
    assert_ne!(
        format_compact(1_000_000.0, 250_000.0),
        format_compact(1_250_000.0, 250_000.0)
    );
}

#[test]
fn a_suffix_that_would_swallow_a_step_should_be_dropped() {
    // A thousand-wide step cannot be written under a "K": both ends of it would
    // print as "1K".
    assert_ne!(
        format_compact(1_000.0, 1_000.0),
        format_compact(2_000.0, 1_000.0)
    );
}

#[test]
fn a_ratio_should_be_written_with_three_decimals() {
    assert_eq!(format_ratio(1.234), "1.234");
    assert_eq!(format_ratio(2.0), "2.000");
    assert_ne!(
        format_ratio(1.234_1),
        format_ratio(1.234_6),
        "the third decimal is where a ratio's movement between two intervals lives"
    );
    assert!(format_ratio(f64::MAX).contains('e'));
}

#[test]
fn a_share_should_be_written_as_a_percentage() {
    assert_eq!(format_share(0.6026), "60.26%");
}

#[test]
fn the_axis_should_omit_the_year_and_the_tooltip_should_not() {
    assert_eq!(format_axis_time(0), "01-01 00:00");
    assert_eq!(format_tooltip_time(0), "1970-01-01 00:00 UTC");
}

#[test]
fn a_timestamp_outside_the_calendar_should_not_panic() {
    assert_eq!(format_axis_time(i64::MAX), "--");
    assert_eq!(format_tooltip_time(i64::MIN), "--");
}

#[test]
fn a_labelled_value_should_name_the_series_before_the_number() {
    assert_eq!(labelled_value("Amount", "12.35").as_str(), "Amount: 12.35");
    assert_eq!(label_with_unit("Amount", "BTC").as_str(), "Amount (BTC)");
}
