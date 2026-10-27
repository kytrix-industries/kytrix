//! Everything the chart says: the amounts it prints, the dates it names, and the
//! shaped text it reuses.

use time::{Date, Month};

use super::text::*;

fn date(year: i32, month: Month, day: u8) -> Date {
    Date::from_calendar_date(year, month, day).expect("fixture date should be valid")
}

#[test]
fn an_amount_should_be_read_in_the_unit_the_service_publishes() {
    assert_eq!(format_money(12.5), "$12.50M");
    assert_eq!(format_money(1_250.0), "$1.25B");
    assert_eq!(format_money(1_250_000.0), "$1.25T");
    assert_eq!(format_money(-1_250.0), "-$1.25B");
}

#[test]
fn a_signed_amount_should_keep_its_sign_in_front_of_the_currency() {
    assert_eq!(format_signed_money(1_250.0), "+$1.25B");
    assert_eq!(format_signed_money(-1_250.0), "-$1.25B");
    assert_eq!(format_signed_money(0.0), "$0.00M");
}

#[test]
fn an_amount_that_is_not_a_reading_should_be_named_rather_than_printed() {
    assert_eq!(format_money(f64::NAN), NOT_A_READING);
    assert_eq!(format_signed_money(f64::INFINITY), NOT_A_READING);
}

#[test]
fn an_axis_label_should_carry_the_precision_its_step_needs() {
    assert_eq!(format_axis_value(20_000.0, 10_000.0), "20000");
    assert_eq!(format_axis_value(0.0, 1.0), "0");
    assert_eq!(
        format_axis_value(-1_476.0, 500.0),
        "-1476",
        "a step of hundreds still prints whole numbers"
    );
    assert_eq!(format_axis_value(0.5, 0.1), "0.5");
}

#[test]
fn the_time_axis_should_name_the_day_without_its_year() {
    assert_eq!(format_date(date(2026, Month::June, 3)), "06-03");
}

#[test]
fn a_tooltip_should_name_the_whole_trading_day() {
    assert_eq!(format_header_date(date(2026, Month::June, 3)), "2026-06-03");
}
