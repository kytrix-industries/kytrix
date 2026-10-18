use std::{io, path::PathBuf};

use thiserror::Error;

use crate::{language::codes::LanguageCode, template::error::TemplateError};

/// Failure reasons for loading locale files and building an
/// [`I18n`](crate::translator::types::I18n) instance.
///
/// The variants carry the locale file they came from, so a startup failure can
/// point at the file to fix. Sources are boxed to keep the error small enough to
/// travel through `Result` without copying.
#[derive(Debug, Error)]
pub enum Error {
    /// A locale file could not be read from disk.
    #[error("failed to read locale file `{}`: {source}", path.display())]
    ReadLocaleFile {
        path: PathBuf,
        source: Box<io::Error>,
    },
    /// A locale file is not valid YAML.
    #[error("failed to parse locale file `{}`: {source}", path.display())]
    ParseLocaleFile {
        path: PathBuf,
        source: Box<serde_saphyr::Error>,
    },
    /// A locale file name is not one of the language codes of
    /// <https://developers.google.com/workspace/admin/directory/v1/languages>.
    #[error(
        "`{code}` in `{}` is not a supported language code",
        path.display()
    )]
    UnsupportedLanguageCode { path: PathBuf, code: String },
    /// A message in a locale file is not a valid template.
    #[error(
        "message `{key}` in `{}` is not a valid template: {source}",
        path.display()
    )]
    InvalidTemplate {
        path: PathBuf,
        key: String,
        source: TemplateError,
    },
    /// A locale file entry holds a number, a boolean, a list or nothing instead
    /// of a message.
    #[error(
        "`{key}` in `{}` is not a message; locale entries must be strings or nested sections",
        path.display()
    )]
    NotAMessage { path: PathBuf, key: String },
    /// A message key appears more than once in the same locale file.
    #[error("message `{key}` is defined more than once in `{}`", path.display())]
    DuplicateKey { path: PathBuf, key: String },
    /// A locale file contains no message.
    #[error("locale file `{}` contains no message", path.display())]
    EmptyLocaleFile { path: PathBuf },
    /// The locale directory could not be listed.
    #[error("failed to list locale directory `{}`: {source}", path.display())]
    ReadLocaleDirectory {
        path: PathBuf,
        source: Box<io::Error>,
    },
    /// The same language was loaded more than once.
    #[error("the locale of `{}` was loaded more than once", language.as_str())]
    LanguageLoadedTwice { language: LanguageCode },
    /// The selected or fallback language has no loaded locale.
    #[error("no locale was loaded for `{}`", language.as_str())]
    UnknownLanguage { language: LanguageCode },
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, io, path::PathBuf};

    use super::Error;
    use crate::language::codes::LanguageCode;

    #[test]
    fn io_failures_expose_the_underlying_error_and_name_the_file() {
        for error in [
            Error::ReadLocaleFile {
                path: PathBuf::from("locales/en.yaml"),
                source: Box::new(io::Error::other("disk is gone")),
            },
            Error::ReadLocaleDirectory {
                path: PathBuf::from("locales"),
                source: Box::new(io::Error::other("disk is gone")),
            },
        ] {
            let message = error.to_string();

            assert_eq!(
                error.source().map(ToString::to_string).as_deref(),
                Some("disk is gone")
            );
            assert!(message.contains("disk is gone"), "{message}");
            assert!(message.contains("locales"), "{message}");
        }
    }

    #[test]
    fn vocabulary_failures_have_no_source_and_name_the_language() {
        let error = Error::LanguageLoadedTwice {
            language: LanguageCode::ZhCn,
        };

        assert!(error.source().is_none());
        assert!(error.to_string().contains("zh-CN"), "{error}");
    }

    #[test]
    fn file_failures_without_a_cause_still_name_the_file() {
        let error = Error::EmptyLocaleFile {
            path: PathBuf::from("locales/en.yaml"),
        };

        assert!(error.source().is_none());
        assert!(error.to_string().contains("locales/en.yaml"), "{error}");
    }
}
