use std::fmt;

use smol_str::SmolStr;

use crate::decimal::UnixMillis;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstrumentDisplayString(SmolStr);

impl InstrumentDisplayString {
    pub(in crate::display) fn new(value: SmolStr) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for InstrumentDisplayString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for InstrumentDisplayString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DisplayFormatError {
    #[error("timestamp {millis} cannot be displayed as a UTC time")]
    InvalidTimestamp { millis: UnixMillis },
    #[error("display text `{value}` cannot be rendered: {reason}")]
    InvalidText {
        value: SmolStr,
        reason: InvalidTextReason,
    },
    #[error("`{value}` is not a canonical decimal display value")]
    InvalidDecimal { value: SmolStr },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidTextReason {
    Empty,
    ControlCharacter,
}

impl fmt::Display for InvalidTextReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "empty",
            Self::ControlCharacter => "control character",
        })
    }
}
