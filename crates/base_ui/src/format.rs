//! Number formatting shared by more than one widget.
//!
//! A number is read by its magnitude first: `1234567.00` has to be counted, and
//! `1,234,567.00` is seen. Everything here is pure text: no theme, no window.

use smol_str::SmolStr;

/// The integer part of a formatted number, grouped in threes.
///
/// The input is a number as `market_participants_model` writes one — digits, an
/// optional sign, an optional fractional part, and `e` notation for values beyond
/// the model's own scale. Everything except the run of digits before the point is
/// carried over untouched, so this never changes what a number says.
pub fn group_digits(text: &str) -> SmolStr {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, tail) = match rest.find(['.', 'e', 'E']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    if integer.len() <= 3 || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return SmolStr::new(text);
    }
    let mut grouped = String::with_capacity(text.len() + integer.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in integer.bytes().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.push_str(tail);
    SmolStr::new(grouped)
}

#[cfg(test)]
mod tests {
    use super::group_digits;

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
}
