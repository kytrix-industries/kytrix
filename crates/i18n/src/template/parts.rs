/// Opening delimiter of an interpolation placeholder.
pub(super) const OPEN: &str = "%{";
/// Closing delimiter of an interpolation placeholder.
pub(super) const CLOSE: char = '}';
/// Inline capacity of the resolved parts of a message. Interface messages almost
/// always interpolate only one or two values.
pub(super) const INLINE_PARTS: usize = 6;

/// One part of a message template.
#[doc(hidden)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TemplatePart {
    /// Literal text, such as `"Hello, "`.
    Text(Box<str>),
    /// A placeholder, holding the name written between `%{` and `}`.
    Placeholder(Box<str>),
}
