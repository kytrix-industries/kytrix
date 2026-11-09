//! The translator and the process-wide translator instance.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use ai_provider::{
    error::ProviderError,
    llama::{
        config::{EngineConfig, SamplingParams},
        engine::Engine,
    },
};
use app_config::translation::TranslationModel;
use smol_str::SmolStr;

use crate::{error::TranslationError, language::TargetLanguage, prompt};

/// Names the model file the ignored integration test loads.
pub const TEST_MODEL_ENV: &str = "KYTRIX_AI_TEST_MODEL";

/// How long unloading may wait for translations that already hold the model.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

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
/// The model is the selected model's file inside `model_directory`: it is loaded on first
/// use and reused afterwards; a failed load is not cached, so the next call tries again.
/// The complete model path is part of what is cached, which is what makes a changed
/// directory or model apply to the next translation. The language is parsed before any
/// model work, which is what keeps an unknown code from turning into a multi-second model
/// load.
///
/// Empty content is answered before a directory or model is needed, so a build with no
/// model configured can still prove its translation entry point is usable.
pub fn translate(
    model_directory: Option<&Path>,
    selected_model: Option<TranslationModel>,
    target_lang: &str,
    content: &str,
) -> Result<String, TranslationError> {
    let language = parse_language(target_lang)?;
    if content.is_empty() {
        return Ok(String::new());
    }
    let directory = model_directory.ok_or(TranslationError::ModelDirectoryUnset)?;
    let model = selected_model.ok_or(TranslationError::ModelNotSelected)?;
    let model_path = directory.join(model.file_name());

    shared_translator(&model_path)?.translate_parsed(language, content)
}

/// The process-wide translator slot.
enum CachedTranslator {
    /// No model is cached.
    Empty,
    /// The model at `path` is loaded and shared by translations in flight.
    Loaded {
        path: PathBuf,
        translator: Arc<Translator>,
    },
    /// The model at `path` is being dropped and must not be loaded again yet.
    Unloading { path: PathBuf },
}

/// The translator shared by every caller of [`translate`], with the model path it was
/// loaded from.
static TRANSLATOR: OnceLock<Mutex<CachedTranslator>> = OnceLock::new();

/// Returns the translator for `model_path`, loading it when the cached model came from
/// another path.
///
/// The lock is held while the model loads: callers clone the `Arc` and translate outside
/// it, so concurrent translations do not queue on the mutex (the engine serializes them
/// itself). A different model path replaces the cached translator, so a session that
/// changes the directory or the selected model gets the new file without a restart; a
/// failed load leaves the slot empty, so the next call tries again. A model that is being
/// unloaded is reported as busy instead of being loaded while its file is still dropped.
fn shared_translator(model_path: &Path) -> Result<Arc<Translator>, TranslationError> {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(CachedTranslator::Empty));
    let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
    match &*cached {
        CachedTranslator::Loaded { path, translator } if path.as_path() == model_path => {
            return Ok(Arc::clone(translator));
        }
        CachedTranslator::Unloading { path } if path.as_path() == model_path => {
            return Err(TranslationError::ModelBusy {
                path: model_path.to_path_buf(),
            });
        }
        _ => {}
    }

    match Translator::new(model_path) {
        Ok(translator) => {
            let translator = Arc::new(translator);
            *cached = CachedTranslator::Loaded {
                path: model_path.to_path_buf(),
                translator: Arc::clone(&translator),
            };
            Ok(translator)
        }
        Err(error) => {
            *cached = CachedTranslator::Empty;
            Err(error)
        }
    }
}

/// Drops the cached translator when it is the model at `model_path`.
///
/// The matching entry is marked unloading and its `Arc` is taken out of the cache before
/// the lock is released. Translations that already hold the translator keep it alive, so
/// this waits until they have all finished and the `Arc` is the last reference; only then
/// is the engine dropped and the model file free to be deleted. The wait is bounded: if
/// translations still hold the model after [`UNLOAD_TIMEOUT`], the cache keeps the model
/// and the caller is told the file cannot be removed yet. The waiting happens on the
/// caller's thread, which is a blocking worker because this is called through
/// `TranslationDownloads`' blocking delete.
pub fn unload_model(model_path: &Path) -> Result<(), TranslationError> {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(CachedTranslator::Empty));
    let removed = {
        let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
        match &*cached {
            CachedTranslator::Loaded { path, translator } if path.as_path() == model_path => {
                let path = path.clone();
                let translator = Arc::clone(translator);
                *cached = CachedTranslator::Unloading { path };
                Some(translator)
            }
            _ => None,
        }
    };
    let Some(translator) = removed else {
        return Ok(());
    };

    let deadline = Instant::now() + UNLOAD_TIMEOUT;
    while Arc::strong_count(&translator) > 1 {
        if Instant::now() >= deadline {
            let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let still_unloading = matches!(&*cached, CachedTranslator::Unloading { path } if path.as_path() == model_path);
            if still_unloading {
                *cached = CachedTranslator::Loaded {
                    path: model_path.to_path_buf(),
                    translator,
                };
            }
            return Err(TranslationError::UnloadTimeout {
                path: model_path.to_path_buf(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(translator);

    let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let CachedTranslator::Unloading { path } = &*cached
        && path.as_path() == model_path
    {
        *cached = CachedTranslator::Empty;
    }
    Ok(())
}

/// Replaces the process-wide translator slot, for tests that exercise its states without a
/// multi-gigabyte model.
#[cfg(test)]
fn set_cached_translator_for_test(cached: CachedTranslator) {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(CachedTranslator::Empty));
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = cached;
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
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use ai_provider::error::ProviderError;
    use app_config::translation::TranslationModel;

    use super::{CachedTranslator, sampling_params, set_cached_translator_for_test, translate};
    use crate::{error::TranslationError, language::TargetLanguage};

    /// The translator slot is process-wide, so tests that replace it take turns.
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        let error =
            translate(None, None, "xx", "Hello, world!").expect_err("xx is not a model code");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { code } if code == "xx"
        ));
    }

    #[test]
    fn an_unsupported_language_fails_even_for_empty_content() {
        let error = translate(None, None, "xx", "").expect_err("the language is checked first");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { .. }
        ));
    }

    #[test]
    fn empty_content_is_answered_without_a_model_directory() {
        assert_eq!(
            translate(None, None, "en", "").expect("empty content must not need a directory"),
            ""
        );
    }

    #[test]
    fn empty_content_is_answered_without_a_selected_model() {
        let directory = PathBuf::from("no-such-translation-model-directory");

        assert_eq!(
            translate(
                Some(&directory),
                Some(TranslationModel::HyMt218BQ4Km),
                "en",
                ""
            )
            .expect("empty content must not need a model"),
            ""
        );
    }

    #[test]
    fn content_needs_a_configured_model_directory() {
        let error = translate(None, Some(TranslationModel::HyMt218BQ4Km), "en", "Hello")
            .expect_err("no directory is configured");

        assert!(matches!(error, TranslationError::ModelDirectoryUnset));
    }

    #[test]
    fn content_needs_a_selected_model() {
        let directory = PathBuf::from("models");
        let error =
            translate(Some(&directory), None, "en", "Hello").expect_err("no model is selected");

        assert!(matches!(error, TranslationError::ModelNotSelected));
    }

    #[test]
    fn every_table_code_is_accepted_before_a_model_is_needed() {
        for language in TargetLanguage::all() {
            assert_eq!(
                translate(None, None, language.code(), "").expect("every table code is valid"),
                ""
            );
        }
    }

    #[test]
    fn language_codes_are_matched_case_insensitively() {
        assert_eq!(
            translate(None, None, "ZH-HANT", "").expect("codes are case-insensitive"),
            ""
        );
    }

    /// Unloading a model that is not cached is not a failure: there is nothing to release.
    #[test]
    fn unloading_an_uncached_model_should_succeed() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        set_cached_translator_for_test(CachedTranslator::Empty);

        super::unload_model(Path::new("no-such-cached-model.gguf"))
            .expect("an uncached model has nothing to release");
    }

    /// A model that is being unloaded is not loaded again while its file may still be held;
    /// the caller gets a typed busy error instead.
    #[test]
    fn a_model_that_is_being_unloaded_should_be_reported_busy() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        let directory = PathBuf::from("a-directory-whose-model-is-unloading");
        let path = directory.join(TranslationModel::HyMt218BQ4Km.file_name());
        set_cached_translator_for_test(CachedTranslator::Unloading { path: path.clone() });

        let error = translate(
            Some(&directory),
            Some(TranslationModel::HyMt218BQ4Km),
            "en",
            "Hello",
        )
        .expect_err("a model being unloaded must not be loaded again");

        assert!(matches!(
            error,
            TranslationError::ModelBusy { path: busy } if busy == path
        ));
        set_cached_translator_for_test(CachedTranslator::Empty);
    }

    /// The selected model decides the file name looked up inside the configured
    /// directory: no other location or fixed file name is consulted.
    #[test]
    fn the_selected_model_decides_which_file_is_looked_for() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        let directory = Path::new("no-such-translation-model-directory");
        let error = translate(
            Some(directory),
            Some(TranslationModel::HyMt27BQ8),
            "en",
            "Hello",
        )
        .expect_err("the model file is missing");

        match error {
            TranslationError::Provider(ProviderError::ModelFileMissing { path }) => {
                assert_eq!(
                    path,
                    directory.join(TranslationModel::HyMt27BQ8.file_name()),
                    "the 7B Q8_0 selection loads the 7B Q8_0 file, not another model's"
                );
            }
            other => panic!("expected a missing model file, got {other:?}"),
        }
        set_cached_translator_for_test(CachedTranslator::Empty);
    }
}
