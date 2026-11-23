use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use ai_provider::{
    error::ProviderError,
    llama::{
        cancel::CancelCheck,
        config::{EngineConfig, SamplingParams},
        engine::Engine,
        prompt::PromptSpan,
    },
};
use app_config::translation::TranslationModel;
use smol_str::SmolStr;

use crate::{error::TranslationError, language::TargetLanguage, prompt};

pub const TEST_MODEL_ENV: &str = "KYTRIX_AI_TEST_MODEL";

const UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Translator {
    engine: Engine,
    sampling: SamplingParams,
}

impl Translator {
    pub fn new(model_path: impl Into<PathBuf>) -> Result<Self, TranslationError> {
        let engine = Engine::load(EngineConfig::new(model_path))?;
        Ok(Self {
            engine,
            sampling: sampling_params(),
        })
    }

    pub fn translate(&self, target_lang: &str, content: &str) -> Result<String, TranslationError> {
        let language = parse_language(target_lang)?;

        self.translate_parsed(language, content)
    }

    fn translate_parsed(
        &self,
        language: TargetLanguage,
        content: &str,
    ) -> Result<String, TranslationError> {
        if content.is_empty() {
            return Ok(String::new());
        }

        let prompt = prompt::build(language, content);
        Ok(self.generate(&prompt, None)?.trim().to_owned())
    }

    pub(crate) fn generate(
        &self,
        prompt: &[PromptSpan],
        cancel: Option<CancelCheck>,
    ) -> Result<String, TranslationError> {
        self.engine
            .generate(prompt, &self.sampling, cancel)
            .map_err(map_provider_error)
    }
}

fn map_provider_error(error: ProviderError) -> TranslationError {
    match error {
        ProviderError::PromptTooLong { tokens, limit } => {
            TranslationError::ContentTooLong { tokens, limit }
        }
        ProviderError::Cancelled => TranslationError::Cancelled,
        error => TranslationError::Provider(error),
    }
}

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
    let model_path = resolve_model_path(model_directory, selected_model)?;

    shared_translator(&model_path)?.translate_parsed(language, content)
}

pub(crate) fn resolve_model_path(
    model_directory: Option<&Path>,
    selected_model: Option<TranslationModel>,
) -> Result<PathBuf, TranslationError> {
    let directory = model_directory.ok_or(TranslationError::ModelDirectoryUnset)?;
    let model = selected_model.ok_or(TranslationError::ModelNotSelected)?;

    Ok(directory.join(model.file_name()))
}

enum CachedTranslator {
    Empty,
    Loaded {
        path: PathBuf,
        translator: Arc<Translator>,
    },
    Unloading {
        path: PathBuf,
    },
}

static TRANSLATOR: OnceLock<Mutex<CachedTranslator>> = OnceLock::new();

#[cfg(test)]
pub(crate) static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn shared_translator(model_path: &Path) -> Result<Arc<Translator>, TranslationError> {
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

#[cfg(test)]
fn set_cached_translator_for_test(cached: CachedTranslator) {
    let slot = TRANSLATOR.get_or_init(|| Mutex::new(CachedTranslator::Empty));
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = cached;
}

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

pub(crate) fn parse_language(code: &str) -> Result<TargetLanguage, TranslationError> {
    TargetLanguage::parse(code).ok_or_else(|| TranslationError::UnsupportedLanguage {
        code: SmolStr::new(code),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ai_provider::error::ProviderError;
    use app_config::translation::TranslationModel;

    use super::{
        CACHE_TEST_LOCK, CachedTranslator, map_provider_error, sampling_params,
        set_cached_translator_for_test, translate,
    };
    use crate::{error::TranslationError, language::TargetLanguage};

    #[test]
    fn provider_failures_are_mapped_to_the_errors_callers_handle() {
        assert!(matches!(
            map_provider_error(ProviderError::Cancelled),
            TranslationError::Cancelled
        ));

        assert!(matches!(
            map_provider_error(ProviderError::PromptTooLong {
                tokens: 9000,
                limit: 4096,
            }),
            TranslationError::ContentTooLong {
                tokens: 9000,
                limit: 4096,
            }
        ));

        let missing = ProviderError::ModelFileMissing {
            path: PathBuf::from("models/model.gguf"),
        };
        assert!(matches!(
            map_provider_error(missing),
            TranslationError::Provider(ProviderError::ModelFileMissing { .. })
        ));
    }

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

    #[test]
    fn unloading_an_uncached_model_should_succeed() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        set_cached_translator_for_test(CachedTranslator::Empty);

        super::unload_model(Path::new("no-such-cached-model.gguf"))
            .expect("an uncached model has nothing to release");
    }

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
