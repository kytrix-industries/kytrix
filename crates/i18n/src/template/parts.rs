pub(super) const OPEN: &str = "%{";
pub(super) const CLOSE: char = '}';
pub(super) const INLINE_PARTS: usize = 6;

#[doc(hidden)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TemplatePart {
    Text(Box<str>),
    Placeholder(Box<str>),
}
