use smol_str::SmolStr;
use time::{
    OffsetDateTime,
    format_description::{BorrowedFormatItem, Component, StaticFormatDescription, modifier},
};

use crate::{decimal::UnixMillis, display::values::DisplayFormatError};

const DATE: StaticFormatDescription = &[
    BorrowedFormatItem::Component(Component::CalendarYearFullStandardRange(
        modifier::CalendarYearFullStandardRange::default(),
    )),
    BorrowedFormatItem::Component(Component::MonthNumerical(
        modifier::MonthNumerical::default(),
    )),
    BorrowedFormatItem::Component(Component::Day(modifier::Day::default())),
];

const DATE_AND_SECOND: StaticFormatDescription = &[
    BorrowedFormatItem::Component(Component::CalendarYearFullStandardRange(
        modifier::CalendarYearFullStandardRange::default(),
    )),
    BorrowedFormatItem::Component(Component::MonthNumerical(
        modifier::MonthNumerical::default(),
    )),
    BorrowedFormatItem::Component(Component::Day(modifier::Day::default())),
    BorrowedFormatItem::StringLiteral("T"),
    BorrowedFormatItem::Component(Component::Hour24(modifier::Hour24::default())),
    BorrowedFormatItem::Component(Component::Minute(modifier::Minute::default())),
    BorrowedFormatItem::Component(Component::Second(modifier::Second::default())),
    BorrowedFormatItem::StringLiteral("Z"),
];

const DATE_AND_MILLISECOND: StaticFormatDescription = &[
    BorrowedFormatItem::Component(Component::CalendarYearFullStandardRange(
        modifier::CalendarYearFullStandardRange::default(),
    )),
    BorrowedFormatItem::Component(Component::MonthNumerical(
        modifier::MonthNumerical::default(),
    )),
    BorrowedFormatItem::Component(Component::Day(modifier::Day::default())),
    BorrowedFormatItem::StringLiteral("T"),
    BorrowedFormatItem::Component(Component::Hour24(modifier::Hour24::default())),
    BorrowedFormatItem::Component(Component::Minute(modifier::Minute::default())),
    BorrowedFormatItem::Component(Component::Second(modifier::Second::default())),
    BorrowedFormatItem::StringLiteral("."),
    BorrowedFormatItem::Component(Component::Subsecond(
        modifier::Subsecond::default().with_digits(modifier::SubsecondDigits::Three),
    )),
    BorrowedFormatItem::StringLiteral("Z"),
];

pub fn utc_display(timestamp: UnixMillis) -> Result<SmolStr, DisplayFormatError> {
    let unrepresentable = || DisplayFormatError::InvalidTimestamp { millis: timestamp };
    if timestamp < 0 {
        return Err(unrepresentable());
    }
    let nanos = i128::from(timestamp) * 1_000_000;
    let moment = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|_| unrepresentable())?;
    let description = if moment.millisecond() != 0 {
        DATE_AND_MILLISECOND
    } else if moment.hour() != 0 || moment.minute() != 0 || moment.second() != 0 {
        DATE_AND_SECOND
    } else {
        DATE
    };
    let rendered = moment.format(description).map_err(|_| unrepresentable())?;
    Ok(SmolStr::new(rendered))
}
