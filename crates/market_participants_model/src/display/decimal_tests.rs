//! Unit tests of the canonical decimal display rule.

use std::fmt;

use smol_str::SmolStr;

use crate::{
    decimal::{Fraction, Price, Quantity, Ratio},
    display::{decimal::decimal_display, values::DisplayFormatError},
};

/// A rendering that is not a canonical decimal, used to prove the guard.
struct NotADecimal(&'static str);

impl fmt::Display for NotADecimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// an integer drops the decimal point and trailing zeroes.
#[test]
fn decimal_integers_and_trailing_zeroes_are_canonical() {
    assert_eq!(
        decimal_display(Quantity::from(5_u32))
            .expect("a quantity renders")
            .as_str(),
        "5"
    );
    assert_eq!(
        decimal_display("1.500".parse::<Price>().expect("a price parses"))
            .expect("a price renders")
            .as_str(),
        "1.5"
    );
    assert_eq!(
        decimal_display("0.10".parse::<Quantity>().expect("a quantity parses"))
            .expect("a quantity renders")
            .as_str(),
        "0.1"
    );
    assert_eq!(
        decimal_display("0.001".parse::<Quantity>().expect("a quantity parses"))
            .expect("a quantity renders")
            .as_str(),
        "0.001"
    );
}

/// zero is written as `0`, never as `0.0` or `-0`.
#[test]
fn decimal_zero_is_written_as_zero() {
    assert_eq!(
        decimal_display(Price::ZERO)
            .expect("a price renders")
            .as_str(),
        "0"
    );
    assert_eq!(
        decimal_display(Quantity::from(0_u32))
            .expect("a quantity renders")
            .as_str(),
        "0"
    );
    assert_eq!(
        decimal_display(Fraction::ZERO)
            .expect("a fraction renders")
            .as_str(),
        "0"
    );
    assert_eq!(
        decimal_display("0.000".parse::<Ratio>().expect("a ratio parses"))
            .expect("a ratio renders")
            .as_str(),
        "0"
    );
}

/// every canonical numeric type renders through the one decimal
/// rule, and a name states the option strike through it.
#[test]
fn every_canonical_numeric_renders_through_one_rule() {
    // Strike and contract size.
    assert_eq!(
        decimal_display("60000".parse::<Price>().expect("a strike parses"))
            .expect("a strike renders")
            .as_str(),
        "60000"
    );
    assert_eq!(
        decimal_display("2.5".parse::<Quantity>().expect("a contract size parses"))
            .expect("a contract size renders")
            .as_str(),
        "2.5"
    );
    // Delivery quantity.
    assert_eq!(
        decimal_display("15000".parse::<Quantity>().expect("a quantity parses"))
            .expect("a quantity renders")
            .as_str(),
        "15000"
    );
    // Settle fraction.
    assert_eq!(
        decimal_display(
            Fraction::new("0.5".parse::<Ratio>().expect("a ratio parses"))
                .expect("a fraction is in range")
        )
        .expect("a fraction renders")
        .as_str(),
        "0.5"
    );
    assert_eq!(
        decimal_display(Fraction::ONE)
            .expect("a fraction renders")
            .as_str(),
        "1"
    );
    // Conversion observation.
    assert_eq!(
        decimal_display("1.25".parse::<Ratio>().expect("a ratio parses"))
            .expect("a ratio renders")
            .as_str(),
        "1.25"
    );
}

/// no float, grouping, plus sign, leading zero or exponent is accepted.
#[test]
fn non_canonical_renderings_are_rejected() {
    for value in [
        "1,234.50", "1e5", "1E5", "+3", "01", "007", "1.50", "3.0", "0.0", "-0", "-0.0", ".5",
        "1.", "", "1 ", " 1", "1_000",
    ] {
        assert_eq!(
            decimal_display(NotADecimal(value)).expect_err("a non-canonical rendering"),
            DisplayFormatError::InvalidDecimal {
                value: SmolStr::new(value),
            },
            "`{value}` must not be a canonical decimal"
        );
    }
}

/// every rendered decimal uses only digits, one optional sign and one
/// optional decimal point.
#[test]
fn canonical_decimals_use_no_grouping_sign_or_exponent() {
    let values = [
        decimal_display(
            "0.000000000000000001"
                .parse::<Price>()
                .expect("a price parses"),
        )
        .expect("a price renders"),
        decimal_display(
            "340282366920938463463.374607431768211455"
                .parse::<Price>()
                .expect("a price parses"),
        )
        .expect("a price renders"),
        decimal_display(
            "-2.5"
                .parse::<crate::decimal::SignedQuantity>()
                .expect("a value parses"),
        )
        .expect("a signed value renders"),
    ];
    for value in values {
        assert!(
            value
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, '-' | '.')),
            "`{value}` uses only decimal characters"
        );
        assert!(!value.contains(','));
        assert!(!value.contains('+'));
        assert!(!value.contains('e') && !value.contains('E'));
    }
}
