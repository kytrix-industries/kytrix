//! Wiring for the local translation model.
//!
//! The model is loaded lazily on the first real translation. The directory it is loaded from is
//! read from the settings on every call, so a directory the user changes is used by the next
//! translation without a restart. Startup only proves that the translation crate is linked and
//! accepts the language codes the app uses; it must not pay for a multi-second model load
//! before the first window.
//!
//! [`AiTranslator`] is the engine the prediction-event dialog translates through. The dialog
//! owns the batching, the deadline and the cancellation; this module only adapts its seam to
//! the model crate and installs the result.

use std::sync::Arc;

use ai_translator::{
    batch::{BatchSegment, translate_batch},
    error::TranslationError,
};
use app_config::settings::Language;
use app_config_provider::manager::ConfigManager;
use gpui_kit::App;
use instrument_selector_ui::prediction_translation::{
    self, LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure,
    TranslationSegment, TranslationSetup,
};

/// The local model engine behind the prediction-event dialog's translation seam.
struct AiTranslator;

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

/// The model code for a display language.
///
/// The model rejects the settings locale `zh-CN` and translates Chinese from the code `zh`,
/// so the display language is mapped rather than handed over as its locale.
fn target_code(language: Language) -> &'static str {
    match language {
        Language::ZhCn => "zh",
        Language::En => "en",
    }
}

/// Maps an engine failure to the failure the dialog logs.
///
/// The dialog draws no error text and does not branch on the variants: it logs the failure,
/// stops, and keeps the texts that did arrive translated. The mapping keeps the log precise
/// where the engine is precise and folds together failures that mean the same thing to the
/// caller — an unload timeout is a busy model from the caller's side, and an unusable model
/// answer is a response failure rather than an engine one.
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

/// Translates `content` into `target_lang` through the process-wide model.
///
/// The directory comes from the settings rather than from this module, so changing it in the
/// settings page takes effect on the next call.
pub(crate) fn translate(
    cx: &App,
    target_lang: &str,
    content: &str,
) -> Result<String, TranslationError> {
    let config = cx.global::<ConfigManager>().read();
    let directory = config.translation.model_directory.as_deref();
    let selected_model = config.translation.selected_model;

    ai_translator::translator::translate(directory, selected_model, target_lang, content)
}

/// Proves the translation path is usable and publishes the engine the prediction-event
/// dialog translates through.
///
/// The empty probe keeps the whole translation path in the binary without loading the GGUF
/// at startup. The engine is then installed as the process-wide translator global the dialog
/// looks up when it opens; a failed probe installs nothing and returns the error to the
/// caller.
pub(crate) fn install(cx: &mut App) -> Result<(), TranslationError> {
    // Empty content is answered before the model is touched. The call keeps the whole
    // translation path in the binary without loading the GGUF at startup.
    let _ = translate(cx, "en", "")?;

    prediction_translation::install(cx, Arc::new(AiTranslator));

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ai_translator::{error::TranslationError, language::TargetLanguage};
    use app_config::settings::{Config, Language};
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::TestAppContext;
    use instrument_selector_ui::prediction_translation::{
        LocalTranslator, TranslationCancel, TranslationFailure, TranslationSegment,
        TranslationSetup,
    };
    use smol_str::SmolStr;

    use super::{AiTranslator, failure, target_code};

    #[gpui_kit::test]
    fn the_translation_entry_point_accepts_empty_content_without_a_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        let translated = cx.update(|cx| {
            super::translate(cx, "en", "").expect("empty content must not load the model")
        });

        assert_eq!(translated, "");
    }

    /// Startup publishes the translator the prediction-event dialog looks up, so a build
    /// that runs the composition root never opens the dialog without one.
    #[gpui_kit::test]
    fn install_should_publish_the_prediction_translator(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
            super::install(cx).expect("the empty probe and the install should succeed");
            assert!(
                instrument_selector_ui::prediction_translation::translator(cx).is_some(),
                "the dialog's translator must be installed at startup"
            );
        });
    }

    /// The dialog's display language reaches the model as one of its own codes, so the
    /// settings locale `zh-CN` — which the model rejects — never does.
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

    /// Every engine failure the dialog can see, and the failure it is handled as. No model
    /// is involved: these are the error values the engine answers with.
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

    /// A batch with no segments is answered before the model is touched, so the adapter
    /// needs no model files to be exercised end to end.
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

    /// The cancellation the dialog hands over is the flag the engine polls, and a
    /// request cancelled up front stops before it needs a model.
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

    /// Preparing without a configured directory reports it unset; the dialog only logs that
    /// failure and stops. No model is needed.
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
}
