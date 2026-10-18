use std::fmt::{self, Display};

use gpui_kit::SharedString;

use crate::language::codes::LanguageCode;

/// Upper bound of cached rendered messages. Each entry holds one rendered text
/// per distinct `(language, key, arguments)` combination, so the cache only grows
/// with runtime-generated text, such as file names in a dialog.
const MAX_ENTRIES: usize = 64;

/// Cached rendering of one message.
#[derive(Debug)]
struct Entry {
    language: LanguageCode,
    key: Box<str>,
    values: Box<[Box<str>]>,
    text: SharedString,
}

/// Rendered messages, keyed by language, message key and argument values.
///
/// Returning a cached [`SharedString`] costs one clone, and a heap-backed
/// `SharedString` clones by bumping the reference count of its `Arc<str>`, so no
/// text is copied and no template is walked again.
///
/// The cache is a short vector rather than a map: interface messages carry few
/// arguments, so a scan costs a handful of string comparisons and avoids hashing
/// every argument on each lookup.
#[derive(Debug, Default)]
pub(crate) struct RenderedCache {
    entries: Vec<Entry>,
    /// Reused buffer for rendering argument values during a lookup.
    scratch: String,
}

impl RenderedCache {
    /// Returns the cached rendering of `language` + `key` + `arguments`.
    pub fn get(
        &mut self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> Option<SharedString> {
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

    /// Caches the rendering of `language` + `key` + `arguments`.
    ///
    /// When the cache is full the oldest entries are dropped, which costs one
    /// re-render each the next time they are displayed.
    pub fn insert(
        &mut self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
        text: SharedString,
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

/// Renders `value` into `output`, returning the rendered text.
fn write_value<'a>(output: &'a mut String, value: &dyn Display) -> Option<&'a str> {
    fmt::Write::write_fmt(output, format_args!("{value}")).ok()?;

    Some(output)
}
