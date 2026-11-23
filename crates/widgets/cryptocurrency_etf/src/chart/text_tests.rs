//! Everything that turns chart values into text: the money formats, the two date
//! formats, and the precision an axis label is written at.

use smol_str::SmolStr;
use time::Month;

use super::text::{
    NOT_A_READING, format_axis_value, format_date, format_header_date, format_money,
    format_signed_money,
};

fn date(year: i32, month: Month, day: u8) -> time::Date {
    time::Date::from_calendar_date(year, month, day).expect("fixture date should be valid")
}

#[test]
fn an_amount_should_be_read_in_the_unit_the_service_publishes() {
    assert_eq!(format_money(1_000.0), SmolStr::new_static("$1.00B"));
    assert_eq!(format_money(-1_000.0), SmolStr::new_static("-$1.00B"));
    assert_eq!(
        format_money(1_500_000.0),
        SmolStr::new_static("$1.50T"),
        "a million millions is read in trillions"
    );
    assert_eq!(
        format_money(12.5),
        SmolStr::new_static("$12.50M"),
        "the sheet's unit is millions"
    );
}

#[test]
fn a_signed_amount_should_keep_its_sign_in_front_of_the_currency() {
    assert_eq!(format_signed_money(2_000.0), SmolStr::new_static("+$2.00B"));
    assert_eq!(
        format_signed_money(-500.0),
        SmolStr::new_static("-$500.00M")
    );
    assert_eq!(
        format_signed_money(0.0),
        SmolStr::new_static("$0.00M"),
        "a day that moved nothing has no sign to show"
    );
}

#[test]
fn an_amount_that_is_not_a_reading_should_be_named_rather_than_printed() {
    assert_eq!(format_money(f64::NAN), NOT_A_READING);
    assert_eq!(format_signed_money(f64::INFINITY), NOT_A_READING);
    assert_eq!(NOT_A_READING, SmolStr::new_static("--"));
}

#[test]
fn an_axis_label_should_carry_the_precision_its_step_needs() {
    assert_eq!(
        format_axis_value(12_345.0, 5_000.0),
        SmolStr::new_static("12345")
    );
    assert_eq!(format_axis_value(1.25, 0.01), SmolStr::new_static("1.25"));
    assert_eq!(
        format_axis_value(0.0, 1.0),
        SmolStr::new_static("0"),
        "a tick of a whole-number axis is written without decimals"
    );
}

#[test]
fn the_time_axis_should_name_the_day_without_its_year() {
    assert_eq!(
        format_date(date(2024, Month::March, 7)),
        SmolStr::new_static("03-07")
    );
}

#[test]
fn a_tooltip_should_name_the_whole_trading_day() {
    assert_eq!(
        format_header_date(date(2024, Month::March, 7)),
        SmolStr::new_static("2024-03-07")
    );
}
