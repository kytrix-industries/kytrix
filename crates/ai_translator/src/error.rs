//! Errors produced while resolving a model and translating content.

use std::path::PathBuf;

use ai_provider::error::ProviderError;
use smol_str::SmolStr;
use thiserror::Error;

/// Typed failures from the translator.
#[derive(Debug, Error)]
pub enum TranslationError {
    /// The requested target language is not one of the model's codes.
    #[error("target language `{code}` is not supported")]
    UnsupportedLanguage { code: SmolStr },
    /// The content and the requested output do not fit in the model's context.
    #[error("the request needs {tokens} prompt tokens and only {limit} are available for them")]
    ContentTooLong { tokens: usize, limit: usize },
    /// No model file was found in any of the searched locations.
    #[error("no model file found in any of {searched:?}")]
    ModelNotFound { searched: Vec<PathBuf> },
    /// The inference engine failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, path::PathBuf};

    use ai_provider::error::ProviderError;

    use super::TranslationError;

    #[test]
    fn failures_name_the_input_they_came_from() {
        let unsupported = TranslationError::UnsupportedLanguage { code: "xx".into() };
        assert_eq!(
            unsupported.to_string(),
            "target language `xx` is not supported"
        );
        assert!(unsupported.source().is_none());

        let missing = TranslationError::ModelNotFound {
            searched: vec![PathBuf::from("models/model.gguf")],
        };
        assert!(missing.to_string().contains("models/model.gguf"));
        assert!(missing.source().is_none());
    }

    #[test]
    fn provider_failures_stay_transparent() {
        let provider = ProviderError::ModelFileMissing {
            path: PathBuf::from("models/model.gguf"),
        };
        let error = TranslationError::from(provider);

        assert_eq!(
            error.to_string(),
            "model file `models/model.gguf` does not exist"
        );
        assert!(matches!(error, TranslationError::Provider(_)));
    }
}
