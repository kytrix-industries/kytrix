//! Errors produced while loading a model and translating content.

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
    /// No directory has been configured to load the model from.
    #[error("no translation model directory is configured")]
    ModelDirectoryUnset,
    /// No model has been selected in the settings.
    #[error("no translation model is selected")]
    ModelNotSelected,
    /// The model is being unloaded and cannot be loaded again yet.
    #[error("translation model `{path}` is being unloaded")]
    ModelBusy { path: PathBuf },
    /// Other translations kept the model in use for longer than an unload may wait.
    #[error("translation model `{path}` did not finish its translations within 30 seconds")]
    UnloadTimeout { path: PathBuf },
    /// The caller cancelled the request before the model finished it.
    #[error("the translation request was cancelled")]
    Cancelled,
    /// The batch request repeats a segment id, so its answer could not be matched back.
    #[error("the batch request repeats the segment id `{id}`")]
    BatchRequestDuplicateId { id: SmolStr },
    /// The model did not answer a batch request with a JSON object.
    #[error("the model response is not a JSON object")]
    BatchResponseNotJson,
    /// The model response did not translate every requested segment.
    #[error("the model response is missing translations for ids {ids:?}")]
    BatchResponseMissing { ids: Vec<SmolStr> },
    /// The model response invented segment ids that were not requested.
    #[error("the model response contains translations for unrequested ids {ids:?}")]
    BatchResponseExtra { ids: Vec<SmolStr> },
    /// The model returned an empty translation for a requested segment.
    #[error("the model response left translations empty for ids {ids:?}")]
    BatchResponseEmpty { ids: Vec<SmolStr> },
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

        let unset = TranslationError::ModelDirectoryUnset;
        assert_eq!(
            unset.to_string(),
            "no translation model directory is configured"
        );
        assert!(unset.source().is_none());

        let not_selected = TranslationError::ModelNotSelected;
        assert_eq!(not_selected.to_string(), "no translation model is selected");
        assert!(not_selected.source().is_none());

        let busy = TranslationError::ModelBusy {
            path: PathBuf::from("models/model.gguf"),
        };
        assert_eq!(
            busy.to_string(),
            "translation model `models/model.gguf` is being unloaded"
        );
        assert!(busy.source().is_none());

        let timeout = TranslationError::UnloadTimeout {
            path: PathBuf::from("models/model.gguf"),
        };
        assert_eq!(
            timeout.to_string(),
            "translation model `models/model.gguf` did not finish its translations within 30 seconds"
        );
        assert!(timeout.source().is_none());
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

    #[test]
    fn cancellation_has_its_own_message() {
        let cancelled = TranslationError::Cancelled;

        assert_eq!(
            cancelled.to_string(),
            "the translation request was cancelled"
        );
        assert!(cancelled.source().is_none());
    }

    #[test]
    fn batch_failures_name_the_affected_ids() {
        let duplicate = TranslationError::BatchRequestDuplicateId { id: "s00".into() };
        assert_eq!(
            duplicate.to_string(),
            "the batch request repeats the segment id `s00`"
        );
        assert!(duplicate.source().is_none());

        let not_json = TranslationError::BatchResponseNotJson;
        assert_eq!(
            not_json.to_string(),
            "the model response is not a JSON object"
        );
        assert!(not_json.source().is_none());

        let missing = TranslationError::BatchResponseMissing {
            ids: vec!["1".into(), "3".into()],
        };
        assert_eq!(
            missing.to_string(),
            "the model response is missing translations for ids [\"1\", \"3\"]"
        );

        let extra = TranslationError::BatchResponseExtra {
            ids: vec!["9".into()],
        };
        assert_eq!(
            extra.to_string(),
            "the model response contains translations for unrequested ids [\"9\"]"
        );

        let empty = TranslationError::BatchResponseEmpty {
            ids: vec!["2".into()],
        };
        assert_eq!(
            empty.to_string(),
            "the model response left translations empty for ids [\"2\"]"
        );
    }
}
