//! UTC rendering of Unix millisecond timestamps.
//!
//! The UTC rule converts a `UnixMillis` to UTC first and then writes one of three
//! shapes: `YYYYMMDD` at midnight, `YYYYMMDDTHHMMSSZ` for a whole second and
//! `YYYYMMDDTHHMMSS.mmmZ` when the millisecond is non-zero. An expiry is the one
//! instant a name states, and it is rendered here: the main date of a future, of
//! an option and of an outcome group all come out of this one formatter, so two
//! names of one day can never disagree about how that day is written.
//!
//! The workspace `time` crate is used for both the civil conversion and the
//! rendering, which is why `market_participants_model` depends on
//! its `formatting` feature for. The three shapes are three fixed format
//! descriptions, built once as constants out of `BorrowedFormatItem` and
//! `Component` values: the descriptors are neither parsed nor re-created per
//! call, and each component is a fixed-width, zero-padded field, so the three
//! shapes are exactly the three the rule names.

use smol_str::SmolStr;
use time::{
    OffsetDateTime,
    format_description::{BorrowedFormatItem, Component, StaticFormatDescription, modifier},
};

use crate::{decimal::UnixMillis, display::values::DisplayFormatError};

/// `YYYYMMDD`: the whole date, every field zero padded.
const DATE: StaticFormatDescription = &[
    BorrowedFormatItem::Component(Component::CalendarYearFullStandardRange(
        modifier::CalendarYearFullStandardRange::default(),
    )),
    BorrowedFormatItem::Component(Component::MonthNumerical(
        modifier::MonthNumerical::default(),
    )),
    BorrowedFormatItem::Component(Component::Day(modifier::Day::default())),
];

/// `YYYYMMDDTHHMMSSZ`: the date and the whole second.
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

/// `YYYYMMDDTHHMMSS.mmmZ`: the date, the whole second and the millisecond.
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

/// Renders one Unix millisecond timestamp in the UTC display form.
///
/// The display domain is the non-negative millisecond range whose UTC year the
/// four-digit year field of every shape can state; a negative value and a value
/// no shape can render are rejected with `InvalidTimestamp`.
pub fn utc_display(timestamp: UnixMillis) -> Result<SmolStr, DisplayFormatError> {
    let unrepresentable = || DisplayFormatError::InvalidTimestamp { millis: timestamp };
    // A negative timestamp cannot come from validated terms: the `UnixMillis`
    // domain of the model is non-negative. It is still refused explicitly so an
    // unvalidated input is never silently rendered as a shape such as `-0001`.
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
    // The rendered text comes from the `time` formatter itself; no `format!`
    // intermediate exists on this path.
    Ok(SmolStr::new(rendered))
}
