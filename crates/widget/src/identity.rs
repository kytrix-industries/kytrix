use std::{
    borrow::Borrow,
    error::Error,
    fmt::{self, Display},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use smol_str::SmolStr;

const MAX_WIDGET_ID_LEN: usize = 128;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WidgetId(SmolStr);

impl WidgetId {
    #[track_caller]
    pub const fn new_static(value: &'static str) -> Self {
        match validate_widget_id(value) {
            Ok(()) => Self(SmolStr::new_static(value)),
            Err(_) => panic!("invalid static widget id"),
        }
    }

    pub const fn try_new_static(value: &'static str) -> Result<Self, InvalidWidgetId> {
        match validate_widget_id(value) {
            Ok(()) => Ok(Self(SmolStr::new_static(value))),
            Err(error) => Err(error),
        }
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self, InvalidWidgetId> {
        let value = value.as_ref();
        validate_widget_id(value)?;
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for WidgetId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for WidgetId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Display for WidgetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WidgetId {
    type Err = InvalidWidgetId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for WidgetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WidgetId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidWidgetId {
    Empty,
    TooLong { length: usize, maximum: usize },
    InvalidSegmentStart { index: usize, byte: u8 },
    InvalidCharacter { index: usize, byte: u8 },
    EmptyTrailingSegment,
}

impl Display for InvalidWidgetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("widget id must not be empty"),
            Self::TooLong { length, maximum } => {
                write!(
                    formatter,
                    "widget id is {length} bytes; the maximum is {maximum}"
                )
            }
            Self::InvalidSegmentStart { index, byte } => write!(
                formatter,
                "widget id segment at byte {index} must start with a lowercase ASCII letter; found 0x{byte:02x}"
            ),
            Self::InvalidCharacter { index, byte } => write!(
                formatter,
                "widget id contains invalid byte 0x{byte:02x} at index {index}"
            ),
            Self::EmptyTrailingSegment => {
                formatter.write_str("widget id must not end with an empty namespace segment")
            }
        }
    }
}

impl Error for InvalidWidgetId {}

const fn validate_widget_id(value: &str) -> Result<(), InvalidWidgetId> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(InvalidWidgetId::Empty);
    }
    if bytes.len() > MAX_WIDGET_ID_LEN {
        return Err(InvalidWidgetId::TooLong {
            length: bytes.len(),
            maximum: MAX_WIDGET_ID_LEN,
        });
    }

    let mut index = 0;
    let mut at_segment_start = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if at_segment_start {
            if !byte.is_ascii_lowercase() {
                return Err(InvalidWidgetId::InvalidSegmentStart { index, byte });
            }
            at_segment_start = false;
        } else if byte == b'.' {
            at_segment_start = true;
        } else if !(byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'-'
            || byte == b'_')
        {
            return Err(InvalidWidgetId::InvalidCharacter { index, byte });
        }
        index += 1;
    }

    if at_segment_start {
        Err(InvalidWidgetId::EmptyTrailingSegment)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WidgetInstanceId(u64);

impl WidgetInstanceId {
    #[must_use]
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        match NEXT_ID.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        }) {
            Ok(id) => Self(id),
            Err(_) => panic!("widget instance id space exhausted"),
        }
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl Default for WidgetInstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for WidgetInstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[cfg(test)]
mod tests {
    use ahash::HashSet;

    use super::*;

    const STATIC_ID: WidgetId = WidgetId::new_static("kytrix.candlestick-chart");

    #[test]
    fn widget_id_should_support_const_static_construction() {
        assert_eq!(STATIC_ID.as_str(), "kytrix.candlestick-chart");
    }

    #[test]
    fn widget_id_should_reject_non_canonical_values() {
        assert_eq!(WidgetId::parse(""), Err(InvalidWidgetId::Empty));
        assert!(matches!(
            WidgetId::parse("Kytrix.chart"),
            Err(InvalidWidgetId::InvalidSegmentStart { index: 0, .. })
        ));
        assert!(matches!(
            WidgetId::parse("kytrix..chart"),
            Err(InvalidWidgetId::InvalidSegmentStart { index: 7, .. })
        ));
        assert_eq!(
            WidgetId::parse("kytrix.chart."),
            Err(InvalidWidgetId::EmptyTrailingSegment)
        );
    }

    #[test]
    fn widget_instance_id_should_be_unique() {
        let ids = (0..1_024)
            .map(|_| WidgetInstanceId::new())
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), 1_024);
    }
}
