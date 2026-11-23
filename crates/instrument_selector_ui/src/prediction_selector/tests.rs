//! The prediction dialog's state machine.
//!
//! The old suite drove the dialog through a real window and the old toolkit's executor. The engine
//! has no executor to drive from a test host, so the read and translation supervisors (which
//! are tokio tasks) are out of reach here; what is asserted instead is everything the
//! supervisors report back into: the affordance rules, the chunk application, the
//! cancellation paths, paging and confirmation. The supervisor functions themselves are
//! exercised by the real application, and the ledger records which old tests became which.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use ahash::AHashSet;
use app_config::{settings::Language, translation::TranslationModel};
use i18n::language::codes::LanguageCode;
use market_participants_model::{
    identifier::{IdentifierValue, VenueId, VenueScopeId},
    listing::route::VenueRoute,
    outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
    prediction::{
        PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionEventsPage,
        PredictionMarketRef, PredictionOptionCard,
    },
};
use smol_str::{SmolStr, format_smolstr};

use super::*;
use crate::test_support::{EchoTranslator, Harness};

/// One source that answers every read with the same page.
#[derive(Clone)]
struct FakeSource {
    page: Arc<PredictionEventsPage>,
}

impl PredictionEventSource for FakeSource {
    async fn page(
        &self,
        _request: PredictionEventsRequest,
    ) -> Result<PredictionEventsPage, MarketError> {
        Ok((*self.page).clone())
    }
}

fn identity(digit: char) -> String {
    format!("v1/{}", digit.to_string().repeat(64))
}

fn leg() -> OutcomeLegRef {
    OutcomeLegRef {
        group: OutcomeGroupId::from_identity_value(&identity('1'))
            .expect("the fixture group identity is valid"),
        member: OutcomeMemberId::from_identity_value(&identity('2'))
            .expect("the fixture member identity is valid"),
        side: OutcomeSideId::from_identity_value(&identity('3'))
            .expect("the fixture side identity is valid"),
    }
}

fn event(index: usize) -> PredictionEventRef {
    PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the fixture scope is valid"),
        ),
        native_event: IdentifierValue::new(format_smolstr!("event-{index}"))
            .expect("the fixture event identity is valid"),
    }
}

fn option(event: &PredictionEventRef, label: &str) -> PredictionOptionCard {
    PredictionOptionCard {
        market: PredictionMarketRef {
            event: event.clone(),
            native_market: IdentifierValue::new("market")
                .expect("the fixture market identity is valid"),
        },
        leg: leg(),
        label: SmolStr::new(label),
        probability: None,
    }
}

fn card(index: usize, title: impl Into<SmolStr>, labels: &[&str]) -> PredictionEventCard {
    let event = event(index);
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

fn page(cards: Vec<PredictionEventCard>) -> PredictionEventsPage {
    PredictionEventsPage {
        events: cards,
        categories: vec![PredictionCategory::new("politics", "Politics")],
        next_cursor: None,
        total: None,
    }
}

fn translatable_page() -> PredictionEventsPage {
    page(vec![
        card(0, "Event 0", &["Yes", "No"]),
        card(1, "Event 1", &["Yes"]),
    ])
}

/// A host whose translation engine is present (`translator`) and whose settings name a
/// model and its directory, as a user who configured translation has.
fn translation_host(translator: bool) -> Harness {
    let harness = Harness::new();

    if translator {
        harness.with_translator(Arc::new(EchoTranslator))
    } else {
        harness
    }
}

/// The same host with the model configured.
fn configured(translator: bool) -> Harness {
    let mut harness = translation_host(translator);
    harness.update_config(|config| {
        config.language = Language::ZhCn;
        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
        config.translation.model_directory = Some(PathBuf::from("C:/models"));
    });
    harness
}

/// A state over `harness` with `page` loaded, as a completed read leaves it.
fn loaded_state(
    harness: &mut Harness,
    page: PredictionEventsPage,
    translator: bool,
) -> Box<SelectorState<FakeSource>> {
    let host = harness.host().clone();
    let window = harness.window();
    let source = FakeSource {
        page: Arc::new(page),
    };
    let loaded = (*source.page).clone();
    let mut state = Box::new(SelectorState::new(
        Weak::new(),
        &host,
        window,
        source,
        vec![VenueId::Polymarket],
        None,
        None,
        Rc::new(|_, _, _, _| {}),
        harness.app_mut(),
    ));
    state.loading = false;
    state.page = Some(loaded);
    let _ = translator;
    state
}

fn run(generation: u64, pending: &[&str]) -> TranslationRun {
    TranslationRun {
        generation,
        pending: pending.iter().map(|text| SmolStr::new(*text)).collect(),
        cancel: TranslationCancel::new(),
    }
}

#[test]
fn an_english_interface_hides_the_translate_button() {
    let mut harness = configured(true);
    harness.update_config(|config| config.language = Language::En);
    let state = loaded_state(&mut harness, translatable_page(), true);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Hidden
    );
}

/// An English interface does not draw the control at all: the old dialog returned an empty
/// element instead of a button, and a disabled button would be a control that can never do
/// anything. The container the button lives in is what is hidden, so it leaves the layout and
/// the pointer never reaches it.
#[test]
fn an_english_interface_hides_the_control_not_only_the_affordance() {
    let mut harness = configured(true); // Chinese: translation is offered
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    let body = harness.root();
    let window = harness.window();
    state
        .build_content(harness.app_mut(), window, body)
        .expect("the dialog content builds");
    state.update_controls(harness.app_mut());
    harness.drain();
    let holder = harness
        .find(&format_smolstr!("{PANEL_ID}:translate"))
        .expect("the translation control is built");
    assert_eq!(
        harness.style_patch(holder).and_then(|patch| patch.visible),
        Some(true),
        "a language that has something to translate draws the control"
    );

    harness.update_config(|config| config.language = Language::En);
    state.update_controls(harness.app_mut());
    harness.drain();

    assert_eq!(
        harness.style_patch(holder).and_then(|patch| patch.visible),
        Some(false),
        "an English interface hides the control instead of showing a button that never works"
    );
}

/// Waiting text is dimmed while its translation is in flight; `reduce_motion` is what skips
/// the dimming and lands on the final value, and a title whose translation arrived is drawn
/// at full strength either way.
///
/// The assertion is on the opacity the text node's `bind_style` computed, so an
/// implementation that ignores `reduce_motion` (dimming always) fails here.
#[test]
fn a_pending_title_is_dimmed_unless_the_application_reduces_motion() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(1, &["Event 0"]));
    state.translations.insert(
        SmolStr::new_static("Event 1"),
        SmolStr::new_static("事件 1"),
    );
    let text_size = harness.host().theme().tokens().small.font_size;
    let root = harness.root();

    state.build_card(
        harness.app_mut(),
        root,
        0,
        &translatable_page().events[0],
        text_size,
    );
    state.build_card(
        harness.app_mut(),
        root,
        1,
        &translatable_page().events[1],
        text_size,
    );
    harness.drain();

    let pending = harness
        .first_child(
            harness
                .find(&format_smolstr!("{PANEL_ID}:card:0"))
                .expect("the first card is built"),
        )
        .expect("a card draws its title first");
    assert_eq!(harness.node_text(pending), "Event 0");
    assert_eq!(
        harness.style_patch(pending).and_then(|patch| patch.opacity),
        Some(0.55),
        "text waiting for its translation is dimmed"
    );
    let arrived = harness
        .first_child(
            harness
                .find(&format_smolstr!("{PANEL_ID}:card:1"))
                .expect("the second card is built"),
        )
        .expect("a card draws its title first");
    assert_eq!(harness.node_text(arrived), "事件 1");
    assert_eq!(
        harness.style_patch(arrived).and_then(|patch| patch.opacity),
        None,
        "a title whose translation arrived is drawn at full strength"
    );

    let reduce_motion = harness.services().reduce_motion;
    let _ = harness.app_mut().write(&reduce_motion, true);
    state.build_card(
        harness.app_mut(),
        root,
        2,
        &translatable_page().events[0],
        text_size,
    );
    harness.drain();
    let undimmed = harness
        .first_child(
            harness
                .find(&format_smolstr!("{PANEL_ID}:card:2"))
                .expect("the third card is built"),
        )
        .expect("a card draws its title first");
    assert_eq!(harness.node_text(undimmed), "Event 0");
    assert_eq!(
        harness
            .style_patch(undimmed)
            .and_then(|patch| patch.opacity),
        None,
        "reduced motion draws waiting text at its final opacity"
    );
}

/// A chunk's texts are matched back by the id the translator echoes, not by their position in
/// the answer: a translator that returns its segments reordered must still land every text on
/// the source it translates.
#[test]
fn a_chunk_is_matched_by_segment_id_not_by_position() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(9, &["Event 0", "Yes"]));
    let chunk = plan_translation(&[card(0, "Event 0", &["Yes"])], &AHashMap::new())
        .into_iter()
        .next()
        .expect("the plan has one chunk");

    state.apply_translation_event(
        TranslationEvent::Chunk {
            generation: 9,
            chunk,
            // The ids the request carried, answered in the opposite order.
            outcome: Ok(vec![
                TranslatedSegment {
                    id: SmolStr::new_static("s01"),
                    text: SmolStr::new_static("是"),
                },
                TranslatedSegment {
                    id: SmolStr::new_static("s00"),
                    text: SmolStr::new_static("事件 0"),
                },
            ]),
        },
        harness.app_mut(),
    );

    assert_eq!(
        state.translations.get("Event 0").map(SmolStr::as_str),
        Some("事件 0"),
        "the title got the translation of the id it was requested under"
    );
    assert_eq!(
        state.translations.get("Yes").map(SmolStr::as_str),
        Some("是"),
        "the label got its own translation, not the one at the same position"
    );
    assert!(
        state
            .translation
            .as_ref()
            .expect("the run continues")
            .pending
            .is_empty(),
        "both requested texts were answered"
    );
}

/// The dialog's signals live in a scope of its own: finishing releases it, and releasing it
/// again reclaims nothing.
#[test]
fn the_dialog_releases_its_signal_scope_when_it_finishes() {
    let mut harness = Harness::new();
    let mut state = loaded_state(&mut harness, translatable_page(), false);
    let scope = state.scope;

    state.finish(harness.app_mut());

    assert_eq!(
        harness.app_mut().release_scope(scope),
        0,
        "finish already released the dialog's scope, so a second release reclaims nothing"
    );
}

#[test]
fn a_build_without_a_translator_disables_the_translation() {
    let mut harness = configured(false);
    let state = loaded_state(&mut harness, translatable_page(), false);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_unavailable")
    );
}

#[test]
fn a_model_without_a_directory_disables_the_translation() {
    let mut harness = configured(true);
    harness.update_config(|config| config.translation.model_directory = None);
    let state = loaded_state(&mut harness, translatable_page(), true);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
    );

    harness.update_config(|config| {
        config.translation.model_directory = Some(PathBuf::from("C:/models"));
        config.translation.selected_model = None;
    });
    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset"),
        "a model that was never selected is as unusable as no directory"
    );
}

#[test]
fn a_loading_page_disables_the_translation() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.loading = true;

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_page_loading")
    );
}

#[test]
fn a_page_without_translatable_text_disables_the_translation() {
    let mut harness = configured(true);
    let state = loaded_state(&mut harness, page(vec![card(0, "  ", &["", " \t "])]), true);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_no_content")
    );
}

#[test]
fn an_error_page_disables_the_translation() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.error = Some(PredictionSelectorError::Interrupted);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_no_content")
    );
}

#[test]
fn a_ready_page_with_a_model_offers_the_translation() {
    let mut harness = configured(true);
    let state = loaded_state(&mut harness, translatable_page(), true);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Ready
    );
}

#[test]
fn a_running_translation_is_busy() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(1, &["Event 0"]));

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Busy
    );
}

#[test]
fn changing_the_model_while_open_changes_the_affordance() {
    let mut harness = configured(true);
    let state = loaded_state(&mut harness, translatable_page(), true);
    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Ready
    );

    harness.update_config(|config| config.translation.selected_model = None);

    assert_eq!(
        state.translate_affordance(harness.app()),
        TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset"),
        "the settings are read in the frame the button is drawn in"
    );
}

#[test]
fn starting_a_translation_tracks_every_pending_text() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);

    state.start_translation(harness.app_mut());

    let translation = state.translation.as_ref().expect("the run started");
    let mut pending: Vec<&str> = translation.pending.iter().map(SmolStr::as_str).collect();
    pending.sort_unstable();
    assert_eq!(
        pending,
        ["Event 0", "Event 1", "No", "Yes"],
        "every text the page draws and no translation has is pending"
    );
    assert!(state.translations.is_empty());
    assert!(
        state.translation_rx.is_some(),
        "the supervisor reports into a channel"
    );
}

#[test]
fn a_chunk_merges_its_translations_and_clears_them_from_pending() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(7, &["Event 0", "Yes", "No"]));
    let revision = state.revision;
    let chunk = plan_translation(&[card(0, "Event 0", &["Yes", "No"])], &AHashMap::new())
        .into_iter()
        .next()
        .expect("the plan has one chunk");

    state.apply_translation_event(
        TranslationEvent::Chunk {
            generation: 7,
            chunk,
            outcome: Ok(vec![
                TranslatedSegment {
                    id: SmolStr::new_static("s00"),
                    text: SmolStr::new_static("事件 0"),
                },
                TranslatedSegment {
                    id: SmolStr::new_static("s01"),
                    text: SmolStr::new_static("是"),
                },
            ]),
        },
        harness.app_mut(),
    );

    assert_eq!(
        state.translations.get("Event 0").map(SmolStr::as_str),
        Some("事件 0")
    );
    assert_eq!(
        state.translations.get("Yes").map(SmolStr::as_str),
        Some("是")
    );
    let pending: Vec<&str> = state
        .translation
        .as_ref()
        .expect("the run continues")
        .pending
        .iter()
        .map(SmolStr::as_str)
        .collect();
    assert_eq!(pending, ["No"], "the answered texts leave the pending set");
    assert!(
        state.revision > revision,
        "the cards are rebuilt with the new text"
    );
}

#[test]
fn a_failed_chunk_keeps_the_translations_that_arrived() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(3, &["Event 0"]));
    state
        .translations
        .insert(SmolStr::new_static("Yes"), SmolStr::new_static("是"));
    let chunk = plan_translation(&[card(0, "Event 0", &["No"])], &AHashMap::new())
        .into_iter()
        .next()
        .expect("the plan has one chunk");

    state.apply_translation_event(
        TranslationEvent::Chunk {
            generation: 3,
            chunk,
            outcome: Err(TranslationFailure::Engine),
        },
        harness.app_mut(),
    );

    assert!(
        state.translation.is_none(),
        "a failed chunk ends the run rather than retrying"
    );
    assert_eq!(
        state.translations.get("Yes").map(SmolStr::as_str),
        Some("是"),
        "the texts that did arrive stay translated"
    );
}

#[test]
fn a_timed_out_run_ends_without_losing_finished_texts() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(4, &["Event 0"]));
    state
        .translations
        .insert(SmolStr::new_static("Yes"), SmolStr::new_static("是"));

    state.apply_translation_event(
        TranslationEvent::TimedOut { generation: 4 },
        harness.app_mut(),
    );

    assert!(state.translation.is_none());
    assert_eq!(
        state.translations.get("Yes").map(SmolStr::as_str),
        Some("是")
    );
}

#[test]
fn changing_the_page_cancels_a_running_translation() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    let running = run(5, &["Event 0"]);
    let cancel = running.cancel.clone();
    state.translation = Some(running);
    state
        .translations
        .insert(SmolStr::new_static("Yes"), SmolStr::new_static("是"));

    state.reload(harness.app_mut());

    assert!(
        cancel.is_cancelled(),
        "the model work observes the cancellation"
    );
    assert!(state.translation.is_none());
    assert!(
        state.translations.is_empty(),
        "a replaced page keeps no old translation"
    );
    assert!(state.loading, "the replacement read is in flight");
}

#[test]
fn switching_language_cancels_a_running_translation() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    let running = run(6, &["Event 0"]);
    let cancel = running.cancel.clone();
    state.translation = Some(running);

    let language = harness.services().language;
    let _ = harness.app_mut().write(&language, LanguageCode::ZhCn);
    state.follow_language(harness.app_mut());

    assert!(cancel.is_cancelled());
    assert!(state.translation.is_none());
}

#[test]
fn confirming_cancels_the_run_and_reports_the_original_title() {
    let mut harness = configured(true);
    let confirmed: Rc<RefCell<Option<(PredictionEventRef, SmolStr)>>> = Rc::new(RefCell::new(None));
    let callback = Rc::clone(&confirmed);
    let host = harness.host().clone();
    let window = harness.window();
    let page = translatable_page();
    let selected = page.events[0].event.clone();
    let source = FakeSource {
        page: Arc::new(page),
    };
    let mut state = SelectorState::new(
        Weak::new(),
        &host,
        window,
        source,
        vec![VenueId::Polymarket],
        Some(selected.clone()),
        Some(SmolStr::new_static("Event 0")),
        Rc::new(move |event, title, _, _| {
            *callback.borrow_mut() = Some((event, title));
        }),
        harness.app_mut(),
    );
    let running = run(8, &["Event 0"]);
    let cancel = running.cancel.clone();
    state.translation = Some(running);

    state.confirm(harness.app_mut());

    assert!(cancel.is_cancelled(), "confirming stops the model work");
    assert_eq!(
        confirmed.borrow().as_ref(),
        Some(&(selected, SmolStr::new_static("Event 0"))),
        "the caller gets the event and the title its card showed"
    );
}

#[test]
fn a_venue_switch_clears_the_categories_and_the_search() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.category = Some(SmolStr::new_static("politics"));
    state.search = String::from("btc");
    state.cursor = Some(SmolStr::new_static("cursor"));
    let generation = state.generation;

    state.select_venue(harness.app_mut(), VenueId::Polymarket);
    assert_eq!(state.generation, generation, "the venue did not move");

    state.select_venue(harness.app_mut(), VenueId::Hyperliquid);
    assert_eq!(state.venue, VenueId::Hyperliquid);
    assert!(state.categories.is_empty());
    assert!(state.category.is_none());
    assert!(state.search.is_empty());
    assert!(state.cursor.is_none());
    assert!(state.page.is_none());
    assert_eq!(state.page_number, 1);
    assert!(
        state.generation > generation,
        "the new venue reads its own page"
    );
}

#[test]
fn paging_keeps_a_cursor_stack_and_restores_it_when_the_read_fails() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.page = Some(PredictionEventsPage {
        events: translatable_page().events,
        categories: Vec::new(),
        next_cursor: Some(SmolStr::new_static("cursor-2")),
        total: None,
    });
    let cursor = state.cursor.clone();
    let page_number = state.page_number;

    state.next_page(harness.app_mut());
    assert_eq!(state.cursor.as_deref(), Some("cursor-2"));
    assert_eq!(state.page_number, 2);
    assert_eq!(state.cursor_stack, vec![cursor.clone()]);
    let generation = state.generation;

    state.apply_load(
        LoadOutcome {
            generation,
            result: Err(PredictionSelectorError::Interrupted),
        },
        harness.app_mut(),
    );

    assert_eq!(
        state.cursor, cursor,
        "a page that failed to read is not the page the reader is on"
    );
    assert_eq!(state.page_number, page_number);
    assert!(state.cursor_stack.is_empty());
    assert_eq!(state.error, Some(PredictionSelectorError::Interrupted));
}

#[test]
fn a_stale_read_result_is_ignored() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.generation = 9;
    let page = state.page.clone();

    state.apply_load(
        LoadOutcome {
            generation: 8,
            result: Ok(page_without_events()),
        },
        harness.app_mut(),
    );

    assert_eq!(
        state.page, page,
        "a read a newer generation replaced cannot install its page"
    );
}

fn page_without_events() -> PredictionEventsPage {
    PredictionEventsPage {
        events: Vec::new(),
        categories: Vec::new(),
        next_cursor: None,
        total: None,
    }
}

#[test]
fn a_translation_plan_skips_text_that_already_arrived() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translations.insert(
        SmolStr::new_static("Event 0"),
        SmolStr::new_static("事件 0"),
    );

    let plan = state.page_translation_plan();
    let requested: Vec<&str> = plan
        .iter()
        .flat_map(|chunk| chunk.segments().iter().map(|segment| segment.text.as_str()))
        .collect();

    assert!(
        !requested.contains(&"Event 0"),
        "a text already translated is not asked for again: {requested:?}"
    );
    assert!(
        requested.contains(&"Yes"),
        "the rest is still requested: {requested:?}"
    );
}

#[test]
fn error_messages_resolve_in_the_language_the_host_renders() {
    let translate = crate::test_support::translate();
    let interrupted = PredictionSelectorError::Interrupted.translated_message(&translate);
    assert!(!interrupted.is_empty());
    assert!(
        !interrupted.contains("error."),
        "a failure is a sentence, not a key: {interrupted}"
    );
}

/// A page's categories accumulate across pages, so a category label that a later
/// generation renames is updated rather than duplicated.
#[test]
fn a_renamed_category_updates_the_known_label() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);

    let mut renamed = page_without_events();
    renamed.categories = vec![PredictionCategory::new("politics", "Politics (renamed)")];
    let generation = state.generation;
    state.apply_load(
        LoadOutcome {
            generation,
            result: Ok(renamed),
        },
        harness.app_mut(),
    );

    assert_eq!(state.categories.len(), 1);
    assert_eq!(state.categories[0].label, "Politics (renamed)");
}

/// The pending set is a set, not a list: a text two cards share is tracked once.
#[test]
fn a_shared_label_is_pending_once() {
    let mut harness = configured(true);
    let mut state = loaded_state(
        &mut harness,
        page(vec![
            card(0, "Event 0", &["Yes"]),
            card(1, "Event 1", &["Yes"]),
        ]),
        true,
    );

    state.start_translation(harness.app_mut());

    let pending = &state.translation.as_ref().expect("the run started").pending;
    assert_eq!(
        pending.len(),
        3,
        "two titles and one shared label: {pending:?}"
    );
    let expected: AHashSet<SmolStr> = ["Event 0", "Event 1", "Yes"]
        .into_iter()
        .map(SmolStr::new_static)
        .collect();
    assert_eq!(pending, &expected);
}

/// A second click while a run is in flight starts nothing.
#[test]
fn a_second_click_while_busy_starts_nothing() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, translatable_page(), true);
    state.translation = Some(run(1, &["Event 0"]));
    let generation = state.translation_generation;

    state.start_translation(harness.app_mut());

    assert_eq!(
        state.translation_generation, generation,
        "a busy dialog does not supersede its own run"
    );
    assert_eq!(
        state.translation.as_ref().map(|run| run.generation),
        Some(1)
    );
}

/// Nothing to translate means nothing to start: an all-translated page leaves the
/// finished translation in place.
#[test]
fn clicking_a_fully_translated_page_starts_nothing() {
    let mut harness = configured(true);
    let mut state = loaded_state(&mut harness, page(vec![card(0, "Event 0", &[])]), true);
    state.translations.insert(
        SmolStr::new_static("Event 0"),
        SmolStr::new_static("事件 0"),
    );
    let generation = state.translation_generation;

    state.start_translation(harness.app_mut());

    assert!(state.translation.is_none());
    assert_eq!(state.translation_generation, generation);
}

/// The timeout is a per-run value the dialog owns, which is what lets the supervisor
/// take it as an argument (and a test assemble a run without a clock).
#[test]
fn the_run_timeout_is_the_dialog_s_own_value() {
    let mut harness = Harness::new();
    let state = loaded_state(&mut harness, translatable_page(), false);

    assert_eq!(state.translation_timeout, Duration::from_secs(20));
}
