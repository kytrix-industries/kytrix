use crate::display::{timestamp::utc_display, values::DisplayFormatError};

#[test]
fn utc_midnight_is_a_date_only() {
    assert_eq!(
        utc_display(0).expect("the epoch renders").as_str(),
        "19700101"
    );
    assert_eq!(
        utc_display(1_734_566_400_000)
            .expect("a midnight renders")
            .as_str(),
        "20241219"
    );
    assert_eq!(
        utc_display(1_735_171_200_000)
            .expect("a midnight renders")
            .as_str(),
        "20241226"
    );
    assert_eq!(
        utc_display(1_735_689_600_000)
            .expect("a midnight renders")
            .as_str(),
        "20250101"
    );
    assert_eq!(
        utc_display(1_767_139_200_000)
            .expect("a midnight renders")
            .as_str(),
        "20251231"
    );
}

#[test]
fn utc_non_midnight_keeps_the_time() {
    assert_eq!(
        utc_display(1_709_210_096_000)
            .expect("a daytime renders")
            .as_str(),
        "20240229T123456Z"
    );
    assert_eq!(
        utc_display(946_684_799_000)
            .expect("a year end renders")
            .as_str(),
        "19991231T235959Z"
    );
    assert_eq!(
        utc_display(1_735_689_600_001)
            .expect("a millisecond renders")
            .as_str(),
        "20250101T000000.001Z"
    );
}

#[test]
fn utc_millisecond_uses_three_digits_and_a_literal_dot() {
    let rendered = utc_display(1_735_689_599_999).expect("a millisecond renders");
    assert_eq!(rendered.as_str(), "20241231T235959.999Z");
    assert!(rendered.as_str().contains('.'));
    assert_eq!(rendered.as_str().len(), 20);
    let rendered = utc_display(1_735_689_600_010).expect("a millisecond renders");
    assert_eq!(rendered.as_str(), "20250101T000000.010Z");
    let rendered = utc_display(1_709_210_096_007).expect("a millisecond renders");
    assert_eq!(rendered.as_str(), "20240229T123456.007Z");
}

#[test]
fn utc_leap_years_are_converted() {
    assert_eq!(
        utc_display(1_709_164_800_000)
            .expect("a leap day renders")
            .as_str(),
        "20240229"
    );
    assert_eq!(
        utc_display(951_782_400_000)
            .expect("a century leap day renders")
            .as_str(),
        "20000229"
    );
}

#[test]
fn utc_negative_and_unrepresentable_values_are_rejected() {
    for timestamp in [-1, -1_000, i64::MIN, i64::MAX, 253_402_300_800_000] {
        assert_eq!(
            utc_display(timestamp).expect_err("an unrepresentable timestamp"),
            DisplayFormatError::InvalidTimestamp { millis: timestamp },
            "`{timestamp}` must not be representable"
        );
    }
}
