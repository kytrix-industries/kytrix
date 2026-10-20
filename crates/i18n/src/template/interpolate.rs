use std::fmt::{Display, Write};

use either::Either;
use gpui_kit::SharedString;
use smallvec::SmallVec;

use super::{
    error::TemplateError,
    parts::{CLOSE, INLINE_PARTS, OPEN, TemplatePart},
};

/// A message template parsed from a locale file.
///
/// Templates interpolate named placeholders written as `%{name}`, the same
/// syntax `rust-i18n` uses:
///
/// ```text
/// greeting: "Hello, %{name}! You have %{count} new messages."
/// ```
///
/// Parsing happens once, while loading a locale file. A template without
/// placeholders holds the exact text it hands to callers, so rendering it costs
/// one clone and no allocation.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Template {
    /// A message without placeholders, stored as the text handed to callers.
    ///
    /// The text is built once while loading the locale, so rendering the message
    /// only costs a clone of an immutable string: messages up to the inline
    /// capacity of a short string copy an inline buffer, and longer ones bump a
    /// reference count. Neither path allocates.
    Static(SharedString),
    /// A message with at least one placeholder, split into literal and
    /// placeholder parts in source order.
    Interpolated { parts: Box<[TemplatePart]> },
}

impl Template {
    /// Parses `text` into a template.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError`] when a `%{` placeholder is not closed by `}`, or
    /// when a placeholder has no name, as in `%{}`.
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let mut parts: SmallVec<[TemplatePart; INLINE_PARTS]> = SmallVec::new();
        let mut rest = text;

        while let Some(start) = rest.find(OPEN) {
            push_text(&mut parts, &rest[..start]);

            let placeholder = &rest[start + OPEN.len()..];
            let Some(end) = placeholder.find(CLOSE) else {
                return Err(TemplateError::UnclosedPlaceholder {
                    at: text.len() - rest.len() + start,
                });
            };

            let name = &placeholder[..end];
            if name.is_empty() {
                return Err(TemplateError::EmptyPlaceholder {
                    at: text.len() - rest.len() + start,
                });
            }

            parts.push(TemplatePart::Placeholder(name.into()));
            rest = &placeholder[end + CLOSE.len_utf8()..];
        }

        if parts.is_empty() {
            return Ok(Template::Static(SharedString::from(text)));
        }
        push_text(&mut parts, rest);

        Ok(Template::Interpolated {
            parts: parts.into_vec().into_boxed_slice(),
        })
    }

    /// Returns the whole text of a message that contains no placeholder.
    ///
    /// This is the fast path of rendering: a message without placeholders is one
    /// string, and cloning it neither walks a template nor allocates.
    pub fn text(&self) -> Option<&SharedString> {
        match self {
            Template::Static(text) => Some(text),
            Template::Interpolated { .. } => None,
        }
    }

    /// Returns the names of the placeholders in source order, including repeats.
    pub fn placeholder_names(&self) -> impl Iterator<Item = &str> {
        self.parts().filter_map(|part| match part {
            TemplatePart::Text(_) => None,
            TemplatePart::Placeholder(name) => Some(&**name),
        })
    }

    /// Writes the template to `output`, replacing every placeholder with the
    /// argument of the same name.
    ///
    /// Placeholders without a matching argument render as the empty string.
    pub fn write_with(&self, arguments: &[(&str, &dyn Display)], output: &mut String) {
        if let Template::Static(text) = self {
            output.push_str(text);
            return;
        }

        let resolved: SmallVec<[Either<&str, &dyn Display>; INLINE_PARTS]> = self
            .parts()
            .map(|part| match part {
                TemplatePart::Text(text) => Either::Left(&**text),
                TemplatePart::Placeholder(name) => Either::Right(
                    arguments
                        .iter()
                        .find_map(|(candidate, value)| (*candidate == &**name).then_some(*value))
                        .unwrap_or(&""),
                ),
            })
            .collect();

        output.reserve(literal_len(&resolved));

        for part in &resolved {
            match part {
                Either::Left(text) => output.push_str(text),
                Either::Right(value) => {
                    if write!(output, "{value}").is_err() {
                        log::error!("failed to write an interpolation argument");
                    }
                }
            }
        }
    }

    /// Returns every part in source order, or nothing for a template without
    /// placeholders.
    fn parts(&self) -> impl Iterator<Item = &TemplatePart> {
        match self {
            Template::Static(_) => Either::Left(std::iter::empty()),
            Template::Interpolated { parts } => Either::Right(parts.iter()),
        }
    }
}

/// Pushes literal text, leaving out the empty literals that start or end a
/// template.
fn push_text(parts: &mut SmallVec<[TemplatePart; INLINE_PARTS]>, text: &str) {
    if text.is_empty() {
        return;
    }

    parts.push(TemplatePart::Text(text.into()));
}

/// Upper bound of the rendered length, used to reserve output capacity once.
fn literal_len(parts: &[Either<&str, &dyn Display>]) -> usize {
    parts
        .iter()
        .map(|part| match part {
            Either::Left(text) => text.len(),
            Either::Right(_) => 0,
        })
        .sum()
}
