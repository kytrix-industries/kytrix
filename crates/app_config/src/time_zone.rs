use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use smol_str::{SmolStr, format_smolstr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataTimeZone {
    hours: i8,
}

impl DataTimeZone {
    pub const MIN_HOURS: i8 = -12;
    pub const MAX_HOURS: i8 = 14;
    pub const UTC: Self = Self { hours: 0 };
    pub const COUNT: usize = (Self::MAX_HOURS - Self::MIN_HOURS + 1) as usize;

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

    pub const fn hours(self) -> i8 {
        self.hours
    }

    pub const fn from_hours(hours: i8) -> Option<Self> {
        if hours < Self::MIN_HOURS || hours > Self::MAX_HOURS {
            return None;
        }
        Some(Self { hours })
    }

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
