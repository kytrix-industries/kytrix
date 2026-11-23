use std::fmt;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TemplateError {
    UnclosedPlaceholder { at: usize },
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
