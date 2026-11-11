//! Batch translation: many short segments in one model request.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ai_provider::llama::cancel::CancelCheck;
use app_config::translation::TranslationModel;
use smol_str::SmolStr;

use crate::{
    error::TranslationError,
    prompt,
    translator::{parse_language, resolve_model_path, shared_translator},
};

/// One segment to translate, identified by a caller-assigned id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchSegment {
    /// A short caller-assigned id, e.g. `s00`.
    pub id: SmolStr,
    /// The text to translate.
    pub text: SmolStr,
}

/// The translation of one requested segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchTranslation {
    /// The id of the [`BatchSegment`] this text translates.
    pub id: SmolStr,
    /// The translated text, trimmed of the padding the model tends to emit.
    pub text: SmolStr,
}

/// Loads and caches the translation model without generating anything.
///
/// This is the same load [`translate_batch`] performs. It exists so a caller can
/// wait for the model to be ready and only then start its own deadline for the
/// generation that follows.
///
/// `cancel` is checked before the model path is resolved and loaded and again after
/// the load returns, so a caller that cancelled does not start a batch afterwards.
/// The load itself cannot be aborted: a cancellation that arrives while the model
/// loads is reported once the load has finished.
pub fn prepare(
    model_directory: Option<&Path>,
    selected_model: Option<TranslationModel>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), TranslationError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(TranslationError::Cancelled);
    }
    let model_path = resolve_model_path(model_directory, selected_model)?;
    shared_translator(&model_path)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(TranslationError::Cancelled);
    }

    Ok(())
}

/// Translates `segments` into `target_lang` in one model request.
///
/// The language is parsed before any model work, and an empty batch is answered
/// without loading a model. A repeated segment id is rejected because the model's
/// answer maps ids to texts and could not tell the duplicates apart. Otherwise the
/// model is given one JSON object that maps each id to its text, and its answer must
/// contain exactly the requested ids with non-empty values. Results are returned in
/// `segments` order.
///
/// `cancel` is polled before and after the model is loaded, and the engine polls it
/// while it generates; a cancelled request is reported as
/// [`TranslationError::Cancelled`]. The engine stays usable afterwards.
pub fn translate_batch(
    model_directory: Option<&Path>,
    selected_model: Option<TranslationModel>,
    target_lang: &str,
    segments: &[BatchSegment],
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<BatchTranslation>, TranslationError> {
    let language = parse_language(target_lang)?;
    if segments.is_empty() {
        return Ok(Vec::new());
    }

    let mut requested: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
    for segment in segments {
        let replaced = requested.insert(segment.id.clone(), segment.text.clone());
        if replaced.is_some() {
            return Err(TranslationError::BatchRequestDuplicateId {
                id: segment.id.clone(),
            });
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(TranslationError::Cancelled);
    }
    let model_path = resolve_model_path(model_directory, selected_model)?;
    let translator = shared_translator(&model_path)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(TranslationError::Cancelled);
    }

    let json = serde_json::to_string(&requested).expect("a string map encodes as JSON");
    let prompt = prompt::build_batch(language, &json);
    let cancel_flag = Arc::clone(cancel);
    let check: CancelCheck = Arc::new(move || cancel_flag.load(Ordering::Relaxed));
    let output = translator.generate(&prompt, Some(check))?;
    let translations = parse_response(&output, &requested)?;

    Ok(order_translations(segments, translations))
}

/// Parses the model's JSON answer and checks it against the requested ids.
///
/// The answer is located as the first `{` through the last `}`, so an optional
/// markdown fence and surrounding prose are tolerated. Every value is trimmed.
fn parse_response(
    output: &str,
    requested: &BTreeMap<SmolStr, SmolStr>,
) -> Result<BTreeMap<SmolStr, SmolStr>, TranslationError> {
    let (Some(start), Some(end)) = (output.find('{'), output.rfind('}')) else {
        return Err(TranslationError::BatchResponseNotJson);
    };
    let Some(json) = output.get(start..=end) else {
        return Err(TranslationError::BatchResponseNotJson);
    };
    let parsed: BTreeMap<SmolStr, SmolStr> =
        serde_json::from_str(json).map_err(|_| TranslationError::BatchResponseNotJson)?;

    let missing: Vec<SmolStr> = requested
        .keys()
        .filter(|id| !parsed.contains_key(*id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(TranslationError::BatchResponseMissing { ids: missing });
    }
    let extra: Vec<SmolStr> = parsed
        .keys()
        .filter(|id| !requested.contains_key(*id))
        .cloned()
        .collect();
    if !extra.is_empty() {
        return Err(TranslationError::BatchResponseExtra { ids: extra });
    }
    let empty: Vec<SmolStr> = parsed
        .iter()
        .filter(|(_, text)| text.trim().is_empty())
        .map(|(id, _)| id.clone())
        .collect();
    if !empty.is_empty() {
        return Err(TranslationError::BatchResponseEmpty { ids: empty });
    }

    Ok(parsed
        .into_iter()
        .map(|(id, text)| (id, SmolStr::new(text.trim())))
        .collect())
}

/// Returns the translations in the order the caller supplied the segments.
///
/// [`parse_response`] has already checked that the answer contains exactly the
/// requested ids, so every segment is guaranteed a translation.
fn order_translations(
    segments: &[BatchSegment],
    translations: BTreeMap<SmolStr, SmolStr>,
) -> Vec<BatchTranslation> {
    segments
        .iter()
        .map(|segment| BatchTranslation {
            id: segment.id.clone(),
            text: translations
                .get(&segment.id)
                .expect("the parsed response contains every requested id")
                .clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path, sync::Arc};

    use app_config::translation::TranslationModel;
    use smol_str::SmolStr;

    use super::{BatchSegment, order_translations, parse_response, prepare, translate_batch};
    use crate::{error::TranslationError, translator::CACHE_TEST_LOCK};

    /// The request shape the parser tests answer.
    fn requested() -> BTreeMap<SmolStr, SmolStr> {
        BTreeMap::from([
            (SmolStr::new_static("1"), SmolStr::new_static("Hello")),
            (SmolStr::new_static("2"), SmolStr::new_static("World")),
        ])
    }

    /// A batch with ids out of sorted order, so tests can tell order from sorted order.
    fn segments() -> Vec<BatchSegment> {
        vec![
            BatchSegment {
                id: SmolStr::new_static("2"),
                text: SmolStr::new_static("World"),
            },
            BatchSegment {
                id: SmolStr::new_static("1"),
                text: SmolStr::new_static("Hello"),
            },
        ]
    }

    fn cancelled() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(true))
    }

    #[test]
    fn a_plain_json_answer_is_parsed() {
        let parsed = parse_response(r#"{"1":"你好","2":"世界"}"#, &requested())
            .expect("a plain JSON object is a valid answer");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get("1").map(SmolStr::as_str), Some("你好"));
        assert_eq!(parsed.get("2").map(SmolStr::as_str), Some("世界"));
    }

    #[test]
    fn a_markdown_fenced_answer_is_parsed() {
        let parsed = parse_response(
            "```json\n{\"1\":\"你好\",\"2\":\"世界\"}\n```",
            &requested(),
        )
        .expect("a fenced answer is tolerated");

        assert_eq!(parsed.get("2").map(SmolStr::as_str), Some("世界"));
    }

    #[test]
    fn prose_around_the_json_is_ignored() {
        let parsed = parse_response(
            "Here is the translation:\n{\"1\":\"你好\",\"2\":\"世界\"}\nHope this helps.",
            &requested(),
        )
        .expect("an answer wrapped in prose is tolerated");

        assert_eq!(parsed.get("1").map(SmolStr::as_str), Some("你好"));
    }

    #[test]
    fn values_are_trimmed() {
        let parsed = parse_response("{\"1\":\" 你好 \",\"2\":\"世界\\n\"}", &requested())
            .expect("padded values are valid");

        assert_eq!(parsed.get("1").map(SmolStr::as_str), Some("你好"));
        assert_eq!(parsed.get("2").map(SmolStr::as_str), Some("世界"));
    }

    #[test]
    fn missing_ids_are_reported() {
        let error = parse_response(r#"{"1":"你好"}"#, &requested())
            .expect_err("the answer does not translate id 2");

        assert!(matches!(
            error,
            TranslationError::BatchResponseMissing { ids } if ids == vec![SmolStr::new_static("2")]
        ));
    }

    #[test]
    fn extra_ids_are_reported() {
        let error = parse_response(r#"{"1":"你好","2":"世界","3":"!"}"#, &requested())
            .expect_err("id 3 was not requested");

        assert!(matches!(
            error,
            TranslationError::BatchResponseExtra { ids } if ids == vec![SmolStr::new_static("3")]
        ));
    }

    #[test]
    fn empty_values_are_reported() {
        let error = parse_response(r#"{"1":"你好","2":"  "}"#, &requested())
            .expect_err("a whitespace-only translation is empty");

        assert!(matches!(
            error,
            TranslationError::BatchResponseEmpty { ids } if ids == vec![SmolStr::new_static("2")]
        ));
    }

    #[test]
    fn malformed_answers_are_not_json() {
        for answer in ["", "not json", "{\"1\":", "[]", "null"] {
            let error = parse_response(answer, &requested())
                .expect_err("a non-object answer must be rejected");

            assert!(
                matches!(error, TranslationError::BatchResponseNotJson),
                "`{answer}` must be reported as not JSON, got {error:?}"
            );
        }
    }

    #[test]
    fn translations_are_returned_in_input_order() {
        let translations = BTreeMap::from([
            (SmolStr::new_static("1"), SmolStr::new_static("你好")),
            (SmolStr::new_static("2"), SmolStr::new_static("世界")),
        ]);

        let ordered = order_translations(&segments(), translations);

        assert_eq!(
            ordered,
            vec![
                super::BatchTranslation {
                    id: SmolStr::new_static("2"),
                    text: SmolStr::new_static("世界"),
                },
                super::BatchTranslation {
                    id: SmolStr::new_static("1"),
                    text: SmolStr::new_static("你好"),
                },
            ]
        );
    }

    #[test]
    fn an_empty_batch_needs_no_model() {
        let translations = translate_batch(None, None, "en", &[], &Arc::default())
            .expect("an empty batch has nothing to load");

        assert!(translations.is_empty());
    }

    #[test]
    fn the_language_is_parsed_before_the_model_is_needed() {
        let error = translate_batch(None, None, "xx", &segments(), &Arc::default())
            .expect_err("xx is not a model code");

        assert!(matches!(
            error,
            TranslationError::UnsupportedLanguage { code } if code == "xx"
        ));
    }

    #[test]
    fn cancellation_is_checked_before_the_model_is_loaded() {
        let error = translate_batch(None, None, "en", &segments(), &cancelled())
            .expect_err("a cancelled request must stop before loading a model");

        assert!(matches!(error, TranslationError::Cancelled));
    }

    #[test]
    fn a_batch_may_not_repeat_a_segment_id() {
        let mut segments = segments();
        segments.push(BatchSegment {
            id: SmolStr::new_static("s00"),
            text: SmolStr::new_static("First"),
        });
        segments.push(BatchSegment {
            id: SmolStr::new_static("s00"),
            text: SmolStr::new_static("Second"),
        });

        let error = translate_batch(None, None, "en", &segments, &Arc::default())
            .expect_err("the answer could not be matched to two segments with one id");

        assert!(matches!(
            error,
            TranslationError::BatchRequestDuplicateId { id } if id == "s00"
        ));
    }

    #[test]
    fn a_batch_needs_a_configured_model_directory() {
        let error = translate_batch(
            None,
            Some(TranslationModel::HyMt218BQ4Km),
            "en",
            &segments(),
            &Arc::default(),
        )
        .expect_err("no directory is configured");

        assert!(matches!(error, TranslationError::ModelDirectoryUnset));
    }

    #[test]
    fn a_batch_needs_a_selected_model() {
        let directory = Path::new("batch-no-such-directory");
        let error = translate_batch(Some(directory), None, "en", &segments(), &Arc::default())
            .expect_err("no model is selected");

        assert!(matches!(error, TranslationError::ModelNotSelected));
    }

    #[test]
    fn a_batch_reports_a_missing_model_file() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        let directory = Path::new("batch-no-such-directory");
        let error = translate_batch(
            Some(directory),
            Some(TranslationModel::HyMt218BQ4Km),
            "en",
            &segments(),
            &Arc::default(),
        )
        .expect_err("the selected model file does not exist");

        assert!(matches!(
            error,
            TranslationError::Provider(ai_provider::error::ProviderError::ModelFileMissing { .. })
        ));
    }

    #[test]
    fn prepare_needs_a_configured_model_directory() {
        let error = prepare(None, Some(TranslationModel::HyMt218BQ4Km), &Arc::default())
            .expect_err("no directory is configured");

        assert!(matches!(error, TranslationError::ModelDirectoryUnset));
    }

    #[test]
    fn prepare_needs_a_selected_model() {
        let error = prepare(
            Some(Path::new("batch-no-such-directory")),
            None,
            &Arc::default(),
        )
        .expect_err("no model is selected");

        assert!(matches!(error, TranslationError::ModelNotSelected));
    }

    #[test]
    fn prepare_is_cancelled_before_the_model_is_resolved() {
        let error = prepare(None, None, &cancelled())
            .expect_err("a cancelled call must stop before resolving a model");

        assert!(matches!(error, TranslationError::Cancelled));
    }

    #[test]
    fn prepare_reports_a_missing_model_file() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .expect("the translator test lock is not poisoned");
        let directory = Path::new("batch-prepare-no-such-directory");
        let error = prepare(
            Some(directory),
            Some(TranslationModel::HyMt27BQ8),
            &Arc::default(),
        )
        .expect_err("the selected model file does not exist");

        assert!(matches!(
            error,
            TranslationError::Provider(ai_provider::error::ProviderError::ModelFileMissing { .. })
        ));
    }
}
