use std::fmt;

use smol_str::{SmolStr, format_smolstr};

use crate::display::values::DisplayFormatError;

pub fn decimal_display(value: impl fmt::Display) -> Result<SmolStr, DisplayFormatError> {
    let text = format_smolstr!("{value}");
    if is_canonical_decimal(&text) {
        Ok(text)
    } else {
        Err(DisplayFormatError::InvalidDecimal { value: text })
    }
}

fn is_canonical_decimal(value: &str) -> bool {
    let unsigned = match value.strip_prefix('-') {
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
