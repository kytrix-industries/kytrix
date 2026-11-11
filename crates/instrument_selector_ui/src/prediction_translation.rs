//! The translation seam of the prediction-event dialog.
//!
//! The dialog translates the text of the cards it draws, but it cannot depend on the
//! local model engine: Windows ARM64 cannot build `llama.cpp`. The dialog therefore
//! depends only on [`LocalTranslator`], and the composition root installs the one
//! implementation a build has. A build that installs nothing draws the page
//! untranslated, which is why [`translator`] answers `None` rather than a fallback.
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
use gpui_kit::{App, Global};
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
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Returns the flag the model engine polls while it generates.
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
/// installs the implementation that owns the engine.
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

/// Installs the local translator the prediction-event dialog uses.
///
/// The composition root installs the one implementation a build has; installing another
/// one replaces it. A build that installs none draws the page without translation.
pub fn install(cx: &mut App, translator: Arc<dyn LocalTranslator>) {
    cx.set_global(LocalTranslatorGlobal(translator));
}

/// Returns the installed local translator, or `None` when this build has none.
pub fn translator(cx: &App) -> Option<Arc<dyn LocalTranslator>> {
    cx.try_global::<LocalTranslatorGlobal>()
        .map(|installed| Arc::clone(&installed.0))
}

/// The installed translator, in the application's global state.
struct LocalTranslatorGlobal(Arc<dyn LocalTranslator>);

impl Global for LocalTranslatorGlobal {}

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
mod tests {
    use std::sync::Arc;

    use ahash::AHashMap;
    use gpui_kit::TestAppContext;
    use market_participants_model::{
        identifier::{IdentifierValue, VenueId, VenueScopeId},
        listing::route::VenueRoute,
        outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
        prediction::{
            PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionMarketRef,
            PredictionOptionCard,
        },
    };
    use smol_str::{SmolStr, format_smolstr};

    use super::{
        LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure,
        TranslationSegment, TranslationSetup, install, page_has_translatable_text,
        plan_translation, translator,
    };

    /// One canonical identity value, spelled the way the model publishes it.
    fn identity(digit: char) -> String {
        format!("v1/{}", digit.to_string().repeat(64))
    }

    fn leg() -> OutcomeLegRef {
        OutcomeLegRef {
            group: OutcomeGroupId::from_identity_value(&identity('1'))
                .expect("the fixture group identity should be valid"),
            member: OutcomeMemberId::from_identity_value(&identity('2'))
                .expect("the fixture member identity should be valid"),
            side: OutcomeSideId::from_identity_value(&identity('3'))
                .expect("the fixture side identity should be valid"),
        }
    }

    fn option(event: &PredictionEventRef, label: &str) -> PredictionOptionCard {
        PredictionOptionCard {
            market: PredictionMarketRef {
                event: event.clone(),
                native_market: IdentifierValue::new("market")
                    .expect("the fixture market identity should be valid"),
            },
            leg: leg(),
            label: SmolStr::new(label),
            probability: None,
        }
    }

    /// One card with `title` and the option labels it draws, in order.
    fn card(index: usize, title: impl Into<SmolStr>, labels: &[&str]) -> PredictionEventCard {
        let event = PredictionEventRef {
            route: VenueRoute::new(
                VenueId::Polymarket,
                VenueScopeId::new("predict").expect("the fixture scope should be valid"),
            ),
            native_event: IdentifierValue::new(format_smolstr!("event-{index}"))
                .expect("the fixture event identity should be valid"),
        };
        PredictionEventCard {
            event: event.clone(),
            title: title.into(),
            category: PredictionCategory::new("politics", "Politics"),
            ends_at: None,
            volume: None,
            liquidity: None,
            options: labels.iter().map(|label| option(&event, label)).collect(),
            remaining_options: 0,
            market_count: 1,
            remaining_markets: 0,
        }
    }

    fn texts(chunk: &super::TranslationChunk) -> Vec<&str> {
        chunk
            .segments()
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    fn ids(chunk: &super::TranslationChunk) -> Vec<&str> {
        chunk
            .segments()
            .iter()
            .map(|segment| segment.id.as_str())
            .collect()
    }

    /// A full page is eight cards, which is two model requests of four cards each; the
    /// ids restart per request and a card's title comes before its options.
    #[test]
    fn a_full_page_should_split_into_two_chunks_with_local_sequential_ids() {
        let cards: Vec<PredictionEventCard> = (0..8)
            .map(|index| card(index, format_smolstr!("Event {index}"), &["Yes", "No"]))
            .collect();

        let chunks = plan_translation(&cards, &AHashMap::new());

        assert_eq!(chunks.len(), 2, "eight cards should plan two requests");
        assert_eq!(
            ids(&chunks[0]),
            ["s00", "s01", "s02", "s03", "s04", "s05"],
            "the first request should name its segments locally and in order"
        );
        assert_eq!(
            texts(&chunks[0]),
            ["Event 0", "Yes", "No", "Event 1", "Event 2", "Event 3"],
            "a title should come before its options, and a shared label should not repeat"
        );
        assert_eq!(
            ids(&chunks[1]),
            ["s00", "s01", "s02", "s03"],
            "the second request should start its ids again"
        );
        assert_eq!(
            texts(&chunks[1]),
            ["Event 4", "Event 5", "Event 6", "Event 7"],
            "the second request should only carry text the first one did not"
        );
    }

    /// A label shared by cards in both requests belongs to the first request that needs
    /// it, so the model is never asked for the same text twice.
    #[test]
    fn shared_labels_should_appear_only_in_the_first_chunk_that_needs_them() {
        let cards: Vec<PredictionEventCard> = (0..8)
            .map(|index| {
                card(
                    index,
                    format_smolstr!("Event {index}"),
                    &["Yes", "No", "Other"],
                )
            })
            .collect();

        let chunks = plan_translation(&cards, &AHashMap::new());

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            texts(&chunks[0]),
            [
                "Event 0", "Yes", "No", "Other", "Event 1", "Event 2", "Event 3"
            ]
        );
        for shared in ["Yes", "No", "Other"] {
            assert!(
                !texts(&chunks[1]).contains(&shared),
                "{shared} should not be requested again by the second chunk"
            );
        }
    }

    /// Text translated earlier in the dialog is not requested again, and a request whose
    /// cards contribute nothing but such text is dropped rather than sent empty.
    #[test]
    fn already_translated_text_should_be_skipped_and_a_newly_empty_chunk_dropped() {
        let cards: Vec<PredictionEventCard> = (0..8)
            .map(|index| card(index, format_smolstr!("Event {index}"), &[]))
            .collect();
        let mut already = AHashMap::new();
        for index in 0..4 {
            already.insert(
                format_smolstr!("Event {index}"),
                SmolStr::new_static("Translated"),
            );
        }

        let chunks = plan_translation(&cards, &already);

        assert_eq!(
            chunks.len(),
            1,
            "the all-translated chunk should be dropped"
        );
        assert_eq!(
            texts(&chunks[0]),
            ["Event 4", "Event 5", "Event 6", "Event 7"]
        );
        assert_eq!(
            ids(&chunks[0]),
            ["s00", "s01", "s02", "s03"],
            "the remaining chunk should renumber its ids from zero"
        );
    }

    /// Empty and whitespace-only text is not a translation request.
    #[test]
    fn empty_and_whitespace_only_text_should_be_skipped() {
        let cards = [card(0, "  ", &["", "\t", "Yes", " \n "])];

        let chunks = plan_translation(&cards, &AHashMap::new());

        assert_eq!(chunks.len(), 1);
        assert_eq!(texts(&chunks[0]), ["Yes"]);
        assert_eq!(ids(&chunks[0]), ["s00"]);
    }

    /// A page whose cards carry only blank text has nothing the button could ask
    /// for, while one non-blank title or label makes the page translatable.
    #[test]
    fn a_page_should_be_translatable_only_while_a_title_or_label_is_not_blank() {
        assert!(!page_has_translatable_text(&[]));
        assert!(!page_has_translatable_text(&[card(
            0,
            "  ",
            &["", "\t", " \n "]
        )]));
        assert!(page_has_translatable_text(&[card(0, "  ", &[" ", "Yes"])]));
        assert!(page_has_translatable_text(&[card(0, "Event", &[])]));
    }

    /// A page that is not a multiple of the group size keeps a short last request.
    #[test]
    fn a_page_of_five_cards_should_yield_a_four_card_and_a_one_card_chunk() {
        let cards: Vec<PredictionEventCard> = (0..5)
            .map(|index| card(index, format_smolstr!("Event {index}"), &[]))
            .collect();

        let chunks = plan_translation(&cards, &AHashMap::new());

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            texts(&chunks[0]),
            ["Event 0", "Event 1", "Event 2", "Event 3"]
        );
        assert_eq!(texts(&chunks[1]), ["Event 4"]);
        assert_eq!(ids(&chunks[1]), ["s00"]);
    }

    #[test]
    fn no_cards_should_yield_no_chunks() {
        let chunks = plan_translation(&[], &AHashMap::new());

        assert!(chunks.is_empty());
    }

    /// Cancellation starts clear, and the flag the engine polls observes it.
    #[test]
    fn a_cancellation_should_start_clear_and_be_observable_through_its_flag() {
        use std::sync::atomic::Ordering;

        let cancel = TranslationCancel::new();
        assert!(!cancel.is_cancelled());
        assert!(!TranslationCancel::default().is_cancelled());
        assert!(!cancel.flag().load(Ordering::Acquire));

        cancel.cancel();

        assert!(cancel.is_cancelled());
        assert!(cancel.flag().load(Ordering::Acquire));
    }

    /// A translator installed on the application is what the dialog reads, and this
    /// build answers `None` until the composition root installs one.
    struct FakeTranslator;

    impl LocalTranslator for FakeTranslator {
        fn prepare(
            &self,
            _setup: &TranslationSetup,
            _cancel: TranslationCancel,
        ) -> Result<(), TranslationFailure> {
            Ok(())
        }

        fn translate(
            &self,
            _setup: &TranslationSetup,
            _target: app_config::settings::Language,
            segments: Vec<TranslationSegment>,
            _cancel: TranslationCancel,
        ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
            Ok(segments
                .into_iter()
                .map(|segment| TranslatedSegment {
                    id: segment.id,
                    text: segment.text,
                })
                .collect())
        }
    }

    #[test]
    fn the_installed_translator_should_be_readable_and_replaceable() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            assert!(
                translator(cx).is_none(),
                "a build that installs no translator should answer none"
            );

            let first: Arc<dyn LocalTranslator> = Arc::new(FakeTranslator);
            install(cx, Arc::clone(&first));
            let installed = translator(cx).expect("an installed translator should be readable");
            assert!(Arc::ptr_eq(&installed, &first));

            let second: Arc<dyn LocalTranslator> = Arc::new(FakeTranslator);
            install(cx, Arc::clone(&second));
            let replaced = translator(cx).expect("a replaced translator should be readable");
            assert!(Arc::ptr_eq(&replaced, &second));
            assert!(!Arc::ptr_eq(&replaced, &first));
        });
    }
}
