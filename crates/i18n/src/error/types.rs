use std::{io, path::PathBuf};

use thiserror::Error;

use crate::{language::codes::LanguageCode, template::error::TemplateError};

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read locale file `{}`: {source}", path.display())]
    ReadLocaleFile {
        path: PathBuf,
        source: Box<io::Error>,
    },
    #[error("failed to parse locale file `{}`: {source}", path.display())]
    ParseLocaleFile {
        path: PathBuf,
        source: Box<serde_saphyr::Error>,
    },
    #[error(
        "`{code}` in `{}` is not a supported language code",
        path.display()
    )]
    UnsupportedLanguageCode { path: PathBuf, code: String },
    #[error(
        "message `{key}` in `{}` is not a valid template: {source}",
        path.display()
    )]
    InvalidTemplate {
        path: PathBuf,
        key: String,
        source: TemplateError,
    },
    #[error(
        "`{key}` in `{}` is not a message; locale entries must be strings or nested sections",
        path.display()
    )]
    NotAMessage { path: PathBuf, key: String },
    #[error("message `{key}` is defined more than once in `{}`", path.display())]
    DuplicateKey { path: PathBuf, key: String },
    #[error("locale file `{}` contains no message", path.display())]
    EmptyLocaleFile { path: PathBuf },
    #[error("failed to list locale directory `{}`: {source}", path.display())]
    ReadLocaleDirectory {
        path: PathBuf,
        source: Box<io::Error>,
    },
    #[error("the locale of `{}` was loaded more than once", language.as_str())]
    LanguageLoadedTwice { language: LanguageCode },
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
