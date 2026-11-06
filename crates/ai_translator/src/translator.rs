//! The translator and the process-wide translator instance.

use std::{
    env,
    path::PathBuf,
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

/// The model file the translator looks for when no path is configured.
pub const MODEL_FILE_NAME: &str = "Hy-MT2-1.8B-Q4_K_M.gguf";

/// Names the model file to use; when set, it is used exactly as given.
pub const MODEL_PATH_ENV: &str = "KYTRIX_AI_MODEL_PATH";

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
/// The model is loaded on first use and reused afterwards; a failed load is not cached,
/// so the next call tries again. The language is parsed before any model work, which is
/// what keeps an unknown code from turning into a multi-second model load.
pub fn translate(target_lang: &str, content: &str) -> Result<String, TranslationError> {
    let language = parse_language(target_lang)?;
    if content.is_empty() {
        return Ok(String::new());
    }

    shared_translator()?.translate_parsed(language, content)
}

/// The translator shared by every caller of [`translate`].
static TRANSLATOR: OnceLock<Mutex<Option<Arc<Translator>>>> = OnceLock::new();

/// Returns the shared translator, loading it on first use.
///
/// The lock is held only for the one-time initialization: callers clone the `Arc` and
/// translate outside it, so concurrent translations do not queue on the mutex (the engine
/// serializes them itself). A failed initialization leaves the slot empty.
fn shared_translator() -> Result<Arc<Translator>, TranslationError> {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(None));
    let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(translator) = cached.as_ref() {
        return Ok(Arc::clone(translator));
    }

    let translator = Arc::new(Translator::new(resolve_model_path()?)?);
    *cached = Some(Arc::clone(&translator));
    Ok(translator)
}

/// Finds the model file to load.
///
/// `KYTRIX_AI_MODEL_PATH` wins outright and is used exactly as given. Otherwise the
/// default location under the working directory and next to the executable are tried, in
/// that order, and the first one that is a regular file wins.
fn resolve_model_path() -> Result<PathBuf, TranslationError> {
    if let Some(path) = env::var_os(MODEL_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let mut searched = Vec::with_capacity(2);
    if let Ok(directory) = env::current_dir() {
        searched.push(directory.join("models").join(MODEL_FILE_NAME));
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        searched.push(directory.join("models").join(MODEL_FILE_NAME));
    }

    match searched.iter().find(|path| path.is_file()) {
        Some(path) => Ok(path.clone()),
        None => Err(TranslationError::ModelNotFound { searched }),
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
    use super::{sampling_params, translate};
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
        let error = translate("xx", "Hello, world!").expect_err("xx is not a model code");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { code } if code == "xx"
        ));
    }

    #[test]
    fn an_unsupported_language_fails_even_for_empty_content() {
        let error = translate("xx", "").expect_err("the language is checked first");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { .. }
        ));
    }

    #[test]
    fn empty_content_is_answered_without_a_model() {
        assert_eq!(
            translate("en", "").expect("empty content must not need a model"),
            ""
        );
    }

    #[test]
    fn every_table_code_is_accepted_before_a_model_is_needed() {
        for language in TargetLanguage::all() {
            assert_eq!(
                translate(language.code(), "").expect("every table code is valid"),
                ""
            );
        }
    }

    #[test]
    fn language_codes_are_matched_case_insensitively() {
        assert_eq!(
            translate("ZH-HANT", "").expect("codes are case-insensitive"),
            ""
        );
    }
}
