use std::fmt::{Display, Write};

use either::Either;
use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{
    error::TemplateError,
    parts::{CLOSE, INLINE_PARTS, OPEN, TemplatePart},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Template {
    Static(SmolStr),
    Interpolated { parts: Box<[TemplatePart]> },
}

impl Template {
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
            return Ok(Template::Static(SmolStr::from(text)));
        }
        push_text(&mut parts, rest);

        Ok(Template::Interpolated {
            parts: parts.into_vec().into_boxed_slice(),
        })
    }

    pub fn text(&self) -> Option<&SmolStr> {
        match self {
            Template::Static(text) => Some(text),
            Template::Interpolated { .. } => None,
        }
    }

    pub fn placeholder_names(&self) -> impl Iterator<Item = &str> {
        self.parts().filter_map(|part| match part {
            TemplatePart::Text(_) => None,
            TemplatePart::Placeholder(name) => Some(&**name),
        })
    }

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

    fn parts(&self) -> impl Iterator<Item = &TemplatePart> {
        match self {
            Template::Static(_) => Either::Left(std::iter::empty()),
            Template::Interpolated { parts } => Either::Right(parts.iter()),
        }
    }
}

fn push_text(parts: &mut SmallVec<[TemplatePart; INLINE_PARTS]>, text: &str) {
    if text.is_empty() {
        return;
    }

    parts.push(TemplatePart::Text(text.into()));
}

fn literal_len(parts: &[Either<&str, &dyn Display>]) -> usize {
    parts
        .iter()
        .map(|part| match part {
            Either::Left(text) => text.len(),
            Either::Right(_) => 0,
        })
        .sum()
}
