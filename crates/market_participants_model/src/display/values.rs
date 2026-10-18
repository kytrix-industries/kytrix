//! The typed display values of the instrument display contract.
//!
//! Every display string in this module is produced by the formatter. The
//! newtype keeps its text private, so no caller outside [`crate::display`] can
//! build a display value that skipped the normalization. There is no string
//! parsing and no identity guess in the other direction: a display value is never
//! turned back into an instrument identity.

use std::fmt;

use smol_str::SmolStr;

use crate::decimal::UnixMillis;

/// A published instrument display name.
///
/// The name is derived from its canonical terms inside one catalog generation;
/// it is not an identity encoder and carries no route or listing metadata.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstrumentDisplayString(SmolStr);

impl InstrumentDisplayString {
    /// Wraps the grammar text one product family rendered.
    ///
    /// The constructor is visible inside [`crate::display`] only, because a
    /// display name is only ever produced by the formatter: no caller outside the
    /// display layer can build a name that skipped the grammar and the pipeline.
    pub(in crate::display) fn new(value: SmolStr) -> Self {
        Self(value)
    }

    /// Returns the display name.
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

/// The typed rejection of one display formatting step.
///
/// The set is closed and holds only what the pure formatter can detect. A missing
/// asset code, a staged code two entries disagree on and a name that does not
/// equal its own re-rendering are publication errors of the connector
/// (`AssetDisplayCodeMissing`, `StagedCodeMismatch`, `InstrumentNameMismatch`) and
/// are deliberately not part of this enum.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DisplayFormatError {
    /// A Unix millisecond timestamp has no UTC display form.
    #[error("timestamp {millis} cannot be displayed as a UTC time")]
    InvalidTimestamp {
        /// The rejected timestamp in milliseconds.
        millis: UnixMillis,
    },
    /// A text value cannot be rendered as a display component.
    #[error("display text `{value}` cannot be rendered: {reason}")]
    InvalidText {
        /// The rejected text.
        value: SmolStr,
        /// Why the text was rejected.
        reason: InvalidTextReason,
    },
    /// A numeric value is not a canonical decimal display value.
    #[error("`{value}` is not a canonical decimal display value")]
    InvalidDecimal {
        /// The rejected rendering.
        value: SmolStr,
    },
}

/// Why a text value cannot be rendered as a display component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidTextReason {
    /// The value is empty.
    Empty,
    /// The value contains a control character.
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
