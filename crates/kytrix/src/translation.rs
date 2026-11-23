use std::sync::Arc;

use ai_translator::{
    batch::{BatchSegment, translate_batch},
    error::TranslationError,
};
use app_config::settings::{Config, Language};
use instrument_selector_ui::prediction_translation::{
    LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure, TranslationSegment,
    TranslationSetup,
};

pub(crate) struct AiTranslator;

impl LocalTranslator for AiTranslator {
    fn prepare(
        &self,
        setup: &TranslationSetup,
        cancel: TranslationCancel,
    ) -> Result<(), TranslationFailure> {
        ai_translator::batch::prepare(setup.directory.as_deref(), setup.model, &cancel.flag())
            .map_err(failure)
    }

    fn translate(
        &self,
        setup: &TranslationSetup,
        target: Language,
        segments: Vec<TranslationSegment>,
        cancel: TranslationCancel,
    ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
        let segments: Vec<BatchSegment> = segments
            .into_iter()
            .map(|segment| BatchSegment {
                id: segment.id,
                text: segment.text,
            })
            .collect();
        let flag = cancel.flag();

        translate_batch(
            setup.directory.as_deref(),
            setup.model,
            target_code(target),
            &segments,
            &flag,
        )
        .map(|translations| {
            translations
                .into_iter()
                .map(|translation| TranslatedSegment {
                    id: translation.id,
                    text: translation.text,
                })
                .collect()
        })
        .map_err(failure)
    }
}

pub(crate) fn production() -> Arc<dyn LocalTranslator> {
    Arc::new(AiTranslator)
}

fn target_code(language: Language) -> &'static str {
    match language {
        Language::ZhCn => "zh",
        Language::En => "en",
    }
}

fn failure(error: TranslationError) -> TranslationFailure {
    match error {
        TranslationError::Cancelled => TranslationFailure::Cancelled,
        TranslationError::ModelDirectoryUnset => TranslationFailure::ModelDirectoryUnset,
        TranslationError::ModelNotSelected => TranslationFailure::ModelNotSelected,
        TranslationError::ModelBusy { .. } | TranslationError::UnloadTimeout { .. } => {
            TranslationFailure::ModelBusy
        }
        TranslationError::ContentTooLong { .. } => TranslationFailure::ContentTooLong,
        TranslationError::BatchRequestDuplicateId { .. }
        | TranslationError::BatchResponseNotJson
        | TranslationError::BatchResponseMissing { .. }
        | TranslationError::BatchResponseExtra { .. }
        | TranslationError::BatchResponseEmpty { .. } => TranslationFailure::Response,
        TranslationError::Provider(_) | TranslationError::UnsupportedLanguage { .. } => {
            TranslationFailure::Engine
        }
    }
}

pub(crate) fn probe(config: &Config) -> Result<(), TranslationError> {
    let directory = config.translation.model_directory.as_deref();
    let selected_model = config.translation.selected_model;

    ai_translator::translator::translate(directory, selected_model, "en", "").map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ai_translator::{error::TranslationError, language::TargetLanguage};
    use app_config::settings::{Config, Language};
    use instrument_selector_ui::prediction_translation::{
        LocalTranslator, TranslationCancel, TranslationFailure, TranslationSegment,
        TranslationSetup,
    };
    use smol_str::SmolStr;

    use super::{AiTranslator, failure, probe, target_code};

    #[test]
    fn the_dialog_languages_map_to_codes_the_model_accepts() {
        assert_eq!(target_code(Language::ZhCn), "zh");
        assert_eq!(target_code(Language::En), "en");
        for language in Language::ALL {
            assert!(
                TargetLanguage::parse(target_code(language)).is_some(),
                "the model must accept the code for {}",
                language.locale()
            );
        }
    }

    #[test]
    fn engine_failures_map_to_the_dialog_failures() {
        let cases = [
            (TranslationError::Cancelled, TranslationFailure::Cancelled),
            (
                TranslationError::ModelDirectoryUnset,
                TranslationFailure::ModelDirectoryUnset,
            ),
            (
                TranslationError::ModelNotSelected,
                TranslationFailure::ModelNotSelected,
            ),
            (
                TranslationError::ModelBusy {
                    path: PathBuf::from("model.gguf"),
                },
                TranslationFailure::ModelBusy,
            ),
            (
                TranslationError::UnloadTimeout {
                    path: PathBuf::from("model.gguf"),
                },
                TranslationFailure::ModelBusy,
            ),
            (
                TranslationError::ContentTooLong {
                    tokens: 9000,
                    limit: 8192,
                },
                TranslationFailure::ContentTooLong,
            ),
            (
                TranslationError::BatchRequestDuplicateId {
                    id: SmolStr::new_static("s0"),
                },
                TranslationFailure::Response,
            ),
            (
                TranslationError::BatchResponseNotJson,
                TranslationFailure::Response,
            ),
            (
                TranslationError::BatchResponseMissing {
                    ids: vec![SmolStr::new_static("s0")],
                },
                TranslationFailure::Response,
            ),
            (
                TranslationError::BatchResponseExtra {
                    ids: vec![SmolStr::new_static("s9")],
                },
                TranslationFailure::Response,
            ),
            (
                TranslationError::BatchResponseEmpty {
                    ids: vec![SmolStr::new_static("s1")],
                },
                TranslationFailure::Response,
            ),
            (
                TranslationError::UnsupportedLanguage {
                    code: SmolStr::new_static("zh-CN"),
                },
                TranslationFailure::Engine,
            ),
        ];

        for (error, expected) in cases {
            let described = error.to_string();
            assert_eq!(failure(error), expected, "`{described}` mapped wrongly");
        }
    }

    #[test]
    fn an_empty_batch_translates_without_a_model() {
        let setup = TranslationSetup {
            directory: None,
            model: None,
        };

        let translated = AiTranslator
            .translate(&setup, Language::ZhCn, Vec::new(), TranslationCancel::new())
            .expect("an empty batch must not load the model");

        assert!(translated.is_empty());
    }

    #[test]
    fn a_cancelled_batch_is_reported_as_cancelled() {
        let setup = TranslationSetup {
            directory: None,
            model: None,
        };
        let cancel = TranslationCancel::new();
        cancel.cancel();

        let error = AiTranslator
            .translate(
                &setup,
                Language::En,
                vec![TranslationSegment {
                    id: SmolStr::new_static("s0"),
                    text: SmolStr::new_static("Hello"),
                }],
                cancel,
            )
            .expect_err("a cancelled request must not translate");

        assert_eq!(error, TranslationFailure::Cancelled);
    }

    #[test]
    fn preparing_without_a_directory_reports_it_unset() {
        let setup = TranslationSetup {
            directory: None,
            model: None,
        };

        let error = AiTranslator
            .prepare(&setup, TranslationCancel::new())
            .expect_err("no directory is configured");

        assert_eq!(error, TranslationFailure::ModelDirectoryUnset);
    }

    #[test]
    fn the_startup_probe_succeeds_on_a_fresh_installation() {
        probe(&Config::default()).expect("an empty probe must not need a model");
    }
}
