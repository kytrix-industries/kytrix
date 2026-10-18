//! Canonical decimal rendering.
//!
//! One decimal rule covers the numeric field a name states, which
//! is the strike of an option: it renders through the canonical `Display` of
//! `Price`, which already prints an integer without a decimal point, drops
//! trailing fractional zeroes and writes zero as `0`. This module re-checks the
//! contract, so a float, a thousands separator, a leading plus sign or a
//! scientific notation can never reach a display string.

use std::fmt;

use smol_str::{SmolStr, format_smolstr};

use crate::display::values::DisplayFormatError;

/// Renders one numeric value in the canonical decimal form.
pub fn decimal_display(value: impl fmt::Display) -> Result<SmolStr, DisplayFormatError> {
    let text = format_smolstr!("{value}");
    if is_canonical_decimal(&text) {
        Ok(text)
    } else {
        Err(DisplayFormatError::InvalidDecimal { value: text })
    }
}

/// Returns `true` when a text is exactly the canonical decimal spelling.
fn is_canonical_decimal(value: &str) -> bool {
    let unsigned = match value.strip_prefix('-') {
        // A negative zero is not a canonical decimal.
        Some(rest) if rest.is_empty() || rest == "0" => return false,
        Some(rest) => rest,
        None => value,
    };
    let (integer, fraction) = match unsigned.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (unsigned, None),
    };
    let integer_is_canonical = integer == "0"
        || (!integer.is_empty()
            && !integer.starts_with('0')
            && integer.bytes().all(|byte| byte.is_ascii_digit()));
    let fraction_is_canonical = fraction.is_none_or(|fraction| {
        !fraction.is_empty()
            && !fraction.ends_with('0')
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    });
    integer_is_canonical && fraction_is_canonical
}
