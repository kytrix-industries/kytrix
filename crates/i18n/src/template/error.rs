use std::fmt;

/// Failure reasons for parsing an interpolated message template.
///
/// Positions are byte offsets into the raw template text, so an error points at
/// the exact `%{` that could not be parsed.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TemplateError {
    /// A `%{` sequence is not closed by a `}`.
    UnclosedPlaceholder { at: usize },
    /// A `%{` sequence closes immediately, as in `%{}`.
    EmptyPlaceholder { at: usize },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnclosedPlaceholder { at } => {
                write!(formatter, "unclosed placeholder starting at byte {at}")
            }
            TemplateError::EmptyPlaceholder { at } => {
                write!(formatter, "empty placeholder at byte {at}")
            }
        }
    }
}

impl std::error::Error for TemplateError {}
