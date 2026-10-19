//! The clock data timestamps are displayed in.
//!
//! Every timestamp a market participant or a Kytrix service publishes is a UTC
//! instant. What a user reads on screen is that instant converted by one offset,
//! and this module owns the setting that names it: the whole-hour offset from UTC,
//! constrained to the range real zones live in.
//!
//! The offset is stored as hours rather than as a zone identity because that is
//! the whole of what a display conversion needs: there is no daylight-saving rule
//! to apply, no zone database to consult, and no second answer a reader could
//! arrive at. The bytes on disk are the number of hours, so a file stays readable
//! whatever clock library a future reader uses.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use smol_str::{SmolStr, format_smolstr};

/// The time zone data timestamps are displayed in.
///
/// The value is the whole-hour offset from UTC, in `-12..=14`. Every widget that
/// draws a timestamp the service published in UTC converts it by this offset
/// before it reaches the screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataTimeZone {
    hours: i8,
}

impl DataTimeZone {
    /// The westernmost offset the setting offers.
    pub const MIN_HOURS: i8 = -12;
    /// The easternmost offset the setting offers.
    pub const MAX_HOURS: i8 = 14;
    /// The offset the setting starts at.
    pub const UTC: Self = Self { hours: 0 };
    /// How many offsets the setting offers.
    pub const COUNT: usize = (Self::MAX_HOURS - Self::MIN_HOURS + 1) as usize;

    /// Every offset the setting offers, west to east.
    pub const ALL: [Self; Self::COUNT] = {
        let mut all = [Self::UTC; Self::COUNT];
        let mut index = 0;
        while index < Self::COUNT {
            all[index] = Self {
                hours: Self::MIN_HOURS + index as i8,
            };
            index += 1;
        }
        all
    };

    /// The offset this zone adds to UTC, in whole hours.
    pub const fn hours(self) -> i8 {
        self.hours
    }

    /// The zone `hours` east of UTC, or nothing when no real zone is that far.
    pub const fn from_hours(hours: i8) -> Option<Self> {
        if hours < Self::MIN_HOURS || hours > Self::MAX_HOURS {
            return None;
        }
        Some(Self { hours })
    }

    /// The name the setting is drawn under, such as `UTC+8` or `UTC-12`.
    pub fn label(self) -> SmolStr {
        format_smolstr!("UTC{:+}", self.hours)
    }
}

impl fmt::Display for DataTimeZone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "UTC{:+}", self.hours)
    }
}

impl Serialize for DataTimeZone {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i8(self.hours)
    }
}

impl<'de> Deserialize<'de> for DataTimeZone {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hours = i8::deserialize(deserializer)?;
        Self::from_hours(hours).ok_or_else(|| {
            D::Error::custom(format_args!(
                "data time zone offset {hours} is outside UTC{:+}..=UTC{:+}",
                Self::MIN_HOURS,
                Self::MAX_HOURS
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every offset the setting offers is one the file can hold, and the two agree
    /// on what it is called.
    #[test]
    fn every_offered_offset_should_round_trip_through_the_file() {
        for zone in DataTimeZone::ALL {
            let serialized = serde_json::to_value(zone).expect("a zone should serialize");

            assert_eq!(
                serialized,
                serde_json::json!(zone.hours()),
                "the file stores the number of hours"
            );
            assert_eq!(
                serde_json::from_value::<DataTimeZone>(serialized)
                    .expect("a stored offset should decode"),
                zone
            );
        }
    }

    /// The offered offsets are every whole hour of the range exactly once, in order:
    /// a gap would be a setting a user cannot choose, and a repeat two choices that
    /// store the same value.
    #[test]
    fn the_offered_offsets_should_be_the_whole_range() {
        let hours: Vec<i8> = DataTimeZone::ALL.iter().map(|zone| zone.hours()).collect();

        assert_eq!(hours.len(), 27);
        assert_eq!(
            hours,
            Vec::from_iter(DataTimeZone::MIN_HOURS..=DataTimeZone::MAX_HOURS)
        );
    }

    #[test]
    fn an_offset_outside_the_range_should_be_refused() {
        for hours in [i8::MIN, -13, 15, i8::MAX] {
            assert_eq!(DataTimeZone::from_hours(hours), None);
            assert!(
                serde_json::from_value::<DataTimeZone>(serde_json::json!(hours)).is_err(),
                "{hours} is not an offset this setting offers"
            );
        }
    }

    #[test]
    fn the_default_should_be_utc() {
        assert_eq!(DataTimeZone::default(), DataTimeZone::UTC);
        assert_eq!(DataTimeZone::default().hours(), 0);
    }

    #[test]
    fn an_offset_should_be_named_by_the_hours_it_adds() {
        assert_eq!(DataTimeZone::from_hours(-12).unwrap().label(), "UTC-12");
        assert_eq!(DataTimeZone::UTC.label(), "UTC+0");
        assert_eq!(DataTimeZone::from_hours(8).unwrap().label(), "UTC+8");
        assert_eq!(DataTimeZone::from_hours(14).unwrap().to_string(), "UTC+14");
    }
}
