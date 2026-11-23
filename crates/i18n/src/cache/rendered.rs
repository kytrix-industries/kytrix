use std::fmt::{self, Display};

use smol_str::SmolStr;

use crate::language::codes::LanguageCode;

const MAX_ENTRIES: usize = 64;

#[derive(Debug)]
struct Entry {
    language: LanguageCode,
    key: Box<str>,
    values: Box<[Box<str>]>,
    text: SmolStr,
}

#[derive(Debug, Default)]
pub(crate) struct RenderedCache {
    entries: Vec<Entry>,
    scratch: String,
}

impl RenderedCache {
    pub fn get(
        &mut self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> Option<SmolStr> {
        for entry in &self.entries {
            if entry.language != language
                || &*entry.key != key
                || entry.values.len() != arguments.len()
            {
                continue;
            }

            let matches = arguments
                .iter()
                .zip(entry.values.iter())
                .all(|((_, value), stored)| {
                    self.scratch.clear();
                    write_value(&mut self.scratch, *value).is_some_and(|text| text == &**stored)
                });

            if matches {
                return Some(entry.text.clone());
            }
        }

        None
    }

    pub fn insert(
        &mut self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
        text: SmolStr,
    ) {
        self.entries.push(Entry {
            language,
            key: key.into(),
            values: arguments
                .iter()
                .map(|(_, value)| value.to_string().into_boxed_str())
                .collect(),
            text,
        });

        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() / 2);
        }
    }
}

fn write_value<'a>(output: &'a mut String, value: &dyn Display) -> Option<&'a str> {
    fmt::Write::write_fmt(output, format_args!("{value}")).ok()?;

    Some(output)
}
