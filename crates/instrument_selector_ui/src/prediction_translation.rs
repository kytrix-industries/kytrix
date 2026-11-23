//! The translation seam of the prediction-event dialog.
//!
//! The dialog translates the text of the cards it draws, but it cannot depend on the
//! local model engine: Windows ARM64 cannot build `llama.cpp`. The dialog therefore
//! depends only on [`LocalTranslator`], and the composition root hands the one
//! implementation a build has to [`SelectorHost`](crate::host::SelectorHost) with
//! [`SelectorHost::with_translator`](crate::host::SelectorHost::with_translator).
//! A build that installs nothing draws the page untranslated, which is why the host's
//! accessor answers `None` rather than a fallback.
//!
//! There is no process-wide installation point: the translator is a field of the host
//! the dialog is opened with, exactly like the translation context itself. The
//! the previous version of this module owned a toolkit-wide global (`install`/`translator`) that
//! the application root wrote at startup; the engine has no type-keyed global store
//! (design §11.7), so the aggregate the composition root builds is the replacement.
//!
//! The trait is deliberately narrow, and its methods are synchronous and blocking: the
//! dialog runs them on the application's blocking pool, and this crate never starts
//! model work of its own.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::{AHashMap, AHashSet};
use app_config::{settings::Language, translation::TranslationModel};
use market_participants_model::prediction::PredictionEventCard;
use smol_str::{SmolStr, format_smolstr};

/// The number of cards one translation request covers.
///
/// The dialog draws eight cards a page, so a page is translated in two requests: a
/// model request is bounded in content, and a card's title and options must stay in the
/// same request to be translated together.
const CARDS_PER_CHUNK: usize = 4;

/// Where the local translation model is stored and which model is selected.
///
/// The value mirrors the two translation settings the model engine needs. The dialog
/// reads them from the settings snapshot and hands them to the installed translator,
/// so the engine never reads settings itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationSetup {
    /// The directory the model files are stored in, when one is configured.
    pub directory: Option<PathBuf>,
    /// The selected model, or `None` until the user chooses one.
    pub model: Option<TranslationModel>,
}

/// One text to translate, named by the id its translation is matched back with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationSegment {
    /// The id the translated text is matched by; it is local to one request.
    pub id: SmolStr,
    /// The text to translate.
    pub text: SmolStr,
}

/// One translated text, named by the id of the [`TranslationSegment`] it translates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslatedSegment {
    /// The id of the requested segment this text translates.
    pub id: SmolStr,
    /// The translated text.
    pub text: SmolStr,
}

/// The cancellation shared by one translation and the caller that started it.
///
/// The caller cancels through [`Self::cancel`], the model engine polls the flag
/// [`Self::flag`] hands it, and a request that observes the cancellation stops and
/// answers [`TranslationFailure::Cancelled`]. The engine stays usable afterwards.
#[derive(Clone, Debug, Default)]
pub struct TranslationCancel(Arc<AtomicBool>);

impl TranslationCancel {
    /// Creates a cancellation nothing has cancelled yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the translation this value belongs to.
    ///
    /// Cancelling twice is not an error: a request that already stopped is cancelled
    /// either way.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Returns whether the translation this value belongs to was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Returns the flag the model engine polls while it generates.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

/// Why one translation could not be produced.
///
/// The dialog does not present these variants differently: a failure ends the run, is
/// logged with this value, and leaves the texts that did arrive translated. The variants
/// exist so the log says what went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranslationFailure {
    /// The request was cancelled before the model finished it.
    Cancelled,
    /// No directory is configured to load the model from.
    ModelDirectoryUnset,
    /// No model has been selected in the settings.
    ModelNotSelected,
    /// The model is being unloaded and cannot be loaded again yet.
    ModelBusy,
    /// The texts and the requested output do not fit in the model's context.
    ContentTooLong,
    /// The model answered with something that is not a usable translation.
    Response,
    /// The inference engine failed.
    Engine,
}

/// The local translation engine the prediction-event dialog draws its text from.
///
/// The dialog depends on this seam rather than on the engine crate, so a build without
/// the engine still compiles and simply installs no translator. The composition root
/// hands the implementation that owns the engine to the host it opens the dialog with.
///
/// The methods are synchronous and may block for as long as a model load or a
/// generation takes; callers run them on the application's blocking pool.
pub trait LocalTranslator: Send + Sync + 'static {
    /// Loads the model the setup names, without generating anything.
    ///
    /// The dialog prepares the model before it starts its own deadline, so a model that
    /// cannot be loaded is reported as the failure it is rather than as a timeout of the
    /// translation that follows.
    ///
    /// A model load cannot be aborted once it has started. The flag `cancel` carries is
    /// checked before the load begins and again when it returns, and the translation
    /// that follows polls the same flag while it generates. A caller must treat
    /// [`TranslationFailure::Cancelled`] as "stop": nothing was produced and nothing is
    /// wrong.
    ///
    /// # Errors
    ///
    /// Returns the failure the engine reported: an unset directory, a missing model
    /// selection, a model that is being unloaded, or an engine failure. Returns
    /// [`TranslationFailure::Cancelled`] when the load observed a cancelled flag.
    fn prepare(
        &self,
        setup: &TranslationSetup,
        cancel: TranslationCancel,
    ) -> Result<(), TranslationFailure>;

    /// Translates `segments` into `target` in one model request.
    ///
    /// The returned texts are matched to the request by [`TranslationSegment::id`].
    ///
    /// # Errors
    ///
    /// Returns [`TranslationFailure::Cancelled`] when the flag was cancelled before the
    /// model finished, [`TranslationFailure::ContentTooLong`] when the request does not
    /// fit in the context, [`TranslationFailure::Response`] when the model answered with
    /// something that is not a usable translation, and the remaining variants when the
    /// model could not be loaded or the engine failed.
    fn translate(
        &self,
        setup: &TranslationSetup,
        target: Language,
        segments: Vec<TranslationSegment>,
        cancel: TranslationCancel,
    ) -> Result<Vec<TranslatedSegment>, TranslationFailure>;
}

/// The texts of at most [`CARDS_PER_CHUNK`] consecutive cards, named by local ids.
///
/// The dialog plans a page with [`plan_translation`] and then translates every chunk
/// through [`LocalTranslator::translate`], so the segments reach the translator with the
/// ids the results are matched by.
pub struct TranslationChunk {
    segments: Vec<TranslationSegment>,
}

impl TranslationChunk {
    /// Returns the texts of this chunk, in display order.
    #[must_use]
    pub fn segments(&self) -> &[TranslationSegment] {
        &self.segments
    }
}

/// Plans the translation of the visible cards, in display order.
///
/// A card contributes its title and then every option label. Text that is empty, that
/// was already translated in this dialog, or that an earlier card already requested is
/// skipped, so a shared label such as `Yes` is requested once. Cards are grouped into
/// consecutive groups of at most [`CARDS_PER_CHUNK`], and a group whose cards contribute
/// no text is dropped.
///
/// The ids inside a chunk are local to it: `s00`, `s01`, ... in the order the
/// segments were collected. They are zero-padded so that a serialized map of the
/// segments keeps the display order even though `serde_json` sorts map keys.
#[must_use]
pub fn plan_translation(
    cards: &[PredictionEventCard],
    already: &AHashMap<SmolStr, SmolStr>,
) -> Vec<TranslationChunk> {
    let texts: usize = cards
        .iter()
        .map(|card| 1 + card.options.len())
        .sum::<usize>();
    let mut requested: AHashSet<SmolStr> = AHashSet::with_capacity(texts);
    let mut chunks: Vec<TranslationChunk> =
        Vec::with_capacity(cards.len().div_ceil(CARDS_PER_CHUNK));

    for group in cards.chunks(CARDS_PER_CHUNK) {
        let capacity: usize = group.iter().map(|card| 1 + card.options.len()).sum();
        let mut segments: Vec<TranslationSegment> = Vec::with_capacity(capacity);

        for card in group {
            let labels = card.options.iter().map(|option| &option.label);
            for text in std::iter::once(&card.title).chain(labels) {
                if text.trim().is_empty()
                    || already.contains_key(text)
                    || !requested.insert(text.clone())
                {
                    continue;
                }
                segments.push(TranslationSegment {
                    id: format_smolstr!("s{:02}", segments.len()),
                    text: text.clone(),
                });
            }
        }

        if !segments.is_empty() {
            chunks.push(TranslationChunk { segments });
        }
    }

    chunks
}

/// Whether any title or option label the cards draw is worth translating.
///
/// [`plan_translation`] skips text that is empty or whitespace-only, so a page whose
/// cards carry nothing else has nothing to request even though it has cards.
#[must_use]
pub fn page_has_translatable_text(cards: &[PredictionEventCard]) -> bool {
    cards.iter().any(|card| {
        !card.title.trim().is_empty()
            || card
                .options
                .iter()
                .any(|option| !option.label.trim().is_empty())
    })
}

#[cfg(test)]
mod tests;
