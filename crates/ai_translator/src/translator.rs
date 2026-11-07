//! The translator and the process-wide translator instance.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use ai_provider::{
    error::ProviderError,
    llama::{
        config::{EngineConfig, SamplingParams},
        engine::Engine,
    },
};
use smol_str::SmolStr;

use crate::{error::TranslationError, language::TargetLanguage, prompt};

/// The model file the translator loads from the configured directory.
pub const MODEL_FILE_NAME: &str = "Hy-MT2-1.8B-Q4_K_M.gguf";

/// Names the model file the ignored integration test loads.
pub const TEST_MODEL_ENV: &str = "KYTRIX_AI_TEST_MODEL";

/// A loaded Hy-MT2 model and the sampling parameters its model card specifies.
pub struct Translator {
    engine: Engine,
    sampling: SamplingParams,
}

impl Translator {
    /// Loads the model at `model_path` and blocks until it can translate.
    pub fn new(model_path: impl Into<PathBuf>) -> Result<Self, TranslationError> {
        let engine = Engine::load(EngineConfig::new(model_path))?;
        Ok(Self {
            engine,
            sampling: sampling_params(),
        })
    }

    /// Translates `content` into `target_lang`.
    ///
    /// `target_lang` is a model code such as `"en"` or `"zh-Hant"`. Empty content is
    /// answered without touching the model, and the result is trimmed of the padding the
    /// model tends to emit.
    pub fn translate(&self, target_lang: &str, content: &str) -> Result<String, TranslationError> {
        let language = parse_language(target_lang)?;

        self.translate_parsed(language, content)
    }

    /// Translates content whose language has already been parsed.
    fn translate_parsed(
        &self,
        language: TargetLanguage,
        content: &str,
    ) -> Result<String, TranslationError> {
        if content.is_empty() {
            return Ok(String::new());
        }

        let prompt = prompt::build(language, content);
        match self.engine.generate(&prompt, &self.sampling) {
            Ok(text) => Ok(text.trim().to_owned()),
            Err(ProviderError::PromptTooLong { tokens, limit }) => {
                Err(TranslationError::ContentTooLong { tokens, limit })
            }
            Err(error) => Err(TranslationError::Provider(error)),
        }
    }
}

/// Translates `content` into `target_lang` through the process-wide translator.
///
/// The model is loaded from `model_directory` on first use and reused afterwards; a failed
/// load is not cached, so the next call tries again. The directory is part of what is cached,
/// which is what makes a directory the user changed apply to the next translation. The
/// language is parsed before any model work, which is what keeps an unknown code from
/// turning into a multi-second model load.
///
/// Empty content is answered before a directory is needed, so a build with no model
/// configured can still prove its translation entry point is usable.
pub fn translate(
    model_directory: Option<&Path>,
    target_lang: &str,
    content: &str,
) -> Result<String, TranslationError> {
    let language = parse_language(target_lang)?;
    if content.is_empty() {
        return Ok(String::new());
    }
    let directory = model_directory.ok_or(TranslationError::ModelDirectoryUnset)?;

    shared_translator(directory)?.translate_parsed(language, content)
}

/// The translator shared by every caller of [`translate`], with the directory it was loaded
/// from.
static TRANSLATOR: OnceLock<Mutex<Option<(PathBuf, Arc<Translator>)>>> = OnceLock::new();

/// Returns the translator for `directory`, loading it when the cached model came from another
/// directory.
///
/// The lock is held while the model loads, as it was when there was one model: callers clone
/// the `Arc` and translate outside it, so concurrent translations do not queue on the mutex
/// (the engine serializes them itself). The directory is part of what is cached, so a session
/// that changes it gets the model from the new directory without a restart; a failed load
/// leaves the slot empty, so the next call tries again.
fn shared_translator(directory: &Path) -> Result<Arc<Translator>, TranslationError> {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(None));
    let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((cached_directory, translator)) = cached.as_ref()
        && cached_directory.as_path() == directory
    {
        return Ok(Arc::clone(translator));
    }

    match Translator::new(directory.join(MODEL_FILE_NAME)) {
        Ok(translator) => {
            let translator = Arc::new(translator);
            *cached = Some((directory.to_path_buf(), Arc::clone(&translator)));
            Ok(translator)
        }
        Err(error) => {
            *cached = None;
            Err(error)
        }
    }
}

/// The sampling parameters the model card specifies for Hy-MT2-1.8B.
///
/// `120_020` is the GGUF's EOS. `120_008`, `120_026` and `120_001` are the
/// model's EOT, conversation-end and end-of-sentence control tokens; stopping on
/// all four only cuts control markers, so it is safe and bounds a model that
/// would otherwise run to `max_tokens`.
fn sampling_params() -> SamplingParams {
    SamplingParams {
        max_tokens: 4096,
        temperature: 0.7,
        top_p: 0.6,
        top_k: 20,
        repetition_penalty: 1.05,
        seed: 42,
        stop_token_ids: vec![120_001, 120_020, 120_026, 120_008],
        stop_strings: Vec::new(),
    }
}

/// Parses a target language code into the language the prompt names.
fn parse_language(code: &str) -> Result<TargetLanguage, TranslationError> {
    TargetLanguage::parse(code).ok_or_else(|| TranslationError::UnsupportedLanguage {
        code: SmolStr::new(code),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ai_provider::error::ProviderError;

    use super::{MODEL_FILE_NAME, sampling_params, translate};
    use crate::{error::TranslationError, language::TargetLanguage};

    #[test]
    fn the_sampling_contract_matches_the_model_card() {
        let params = sampling_params();

        assert_eq!(params.max_tokens, 4096);
        assert_eq!(params.temperature, 0.7);
        assert_eq!(params.top_p, 0.6);
        assert_eq!(params.top_k, 20);
        assert_eq!(params.repetition_penalty, 1.05);
        assert_eq!(params.seed, 42);
        assert_eq!(
            params.stop_token_ids,
            vec![120_001, 120_020, 120_026, 120_008]
        );
        assert!(params.stop_strings.is_empty());
    }

    #[test]
    fn an_unsupported_language_fails_before_any_model_work() {
        let error = translate(None, "xx", "Hello, world!").expect_err("xx is not a model code");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { code } if code == "xx"
        ));
    }

    #[test]
    fn an_unsupported_language_fails_even_for_empty_content() {
        let error = translate(None, "xx", "").expect_err("the language is checked first");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { .. }
        ));
    }

    #[test]
    fn empty_content_is_answered_without_a_model_directory() {
        assert_eq!(
            translate(None, "en", "").expect("empty content must not need a directory"),
            ""
        );
    }

    #[test]
    fn content_needs_a_configured_model_directory() {
        let error = translate(None, "en", "Hello").expect_err("no directory is configured");

        assert!(matches!(error, TranslationError::ModelDirectoryUnset));
    }

    #[test]
    fn every_table_code_is_accepted_before_a_model_is_needed() {
        for language in TargetLanguage::all() {
            assert_eq!(
                translate(None, language.code(), "").expect("every table code is valid"),
                ""
            );
        }
    }

    #[test]
    fn language_codes_are_matched_case_insensitively() {
        assert_eq!(
            translate(None, "ZH-HANT", "").expect("codes are case-insensitive"),
            ""
        );
    }

    /// The directory the caller names is the whole of where the model is looked for: the file
    /// is the one the settings page's models list is loaded from, and no other location is
    /// consulted.
    #[test]
    fn a_configured_directory_is_where_the_model_file_is_looked_for() {
        let directory = PathBuf::from("no-such-translation-model-directory");
        let error =
            translate(Some(&directory), "en", "Hello").expect_err("the model file is missing");

        match error {
            TranslationError::Provider(ProviderError::ModelFileMissing { path }) => {
                assert_eq!(path, directory.join(MODEL_FILE_NAME));
            }
            other => panic!("expected a missing model file, got {other:?}"),
        }
    }
}
