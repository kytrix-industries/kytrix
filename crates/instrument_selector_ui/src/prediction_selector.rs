//! The prediction-event selector.
//!
//! A prediction venue publishes events rather than a catalog, so this dialog is
//! not an instrument selector with another filter: it asks the venue for a page of
//! event cards, draws them as cards with their option probabilities, and reports
//! the event the user confirmed. The dialog is one venue at a time: switching the
//! venue resets the category, the search and the page.

use std::{
    future::Future,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ahash::{AHashMap, AHashSet};
use app_config::settings::Language;
use app_config_provider::{events::LanguageChanged, manager::ConfigManager};
use base_ui::{
    badge::Badge,
    button::Button,
    empty_state::EmptyState,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    spinner::Spinner,
    style::theme::ThemeExt,
    tab_bar::TabBar,
    tooltip::TooltipExt,
};
use gpui_kit::{
    Animation, AnimationExt, AnyElement, App, AppContext, Context, DismissEvent, Div, ElementId,
    Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, base::Disableable, bounce, div,
    ease_in_out, prelude::FluentBuilder, px,
};
use i18n::t;
use market_participants_connector::{
    connector::MarketParticipantsConnector,
    market::{
        error::MarketError,
        prediction::{DEFAULT_PREDICTION_PAGE_SIZE, PredictionEventsRequest},
        provider::MarketCapability,
    },
    venue::loaded::LoadedVenues,
};
use market_participants_model::{
    identifier::VenueId,
    prediction::{
        PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionEventsPage,
        PredictionOptionCard,
    },
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};
use widget::{error::LocalizedError, time_zone::data_time_zone, venue::VenueDisplay};
use widget_tokio_runtime::{global, runtime::Runtime};

use crate::prediction_translation::{
    self, LocalTranslator, TranslatedSegment, TranslationCancel, TranslationChunk,
    TranslationFailure, TranslationSetup, page_has_translatable_text, plan_translation,
};

/// The number of cards one page draws.
const PAGE_SIZE: u16 = DEFAULT_PREDICTION_PAGE_SIZE;
/// The number of columns and rows the card grid draws.
const GRID_COLUMNS: u16 = 4;
const GRID_ROWS: u16 = 2;
/// How long the search box waits after a keystroke.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// The width of a probability bar at 100%.
const BAR_WIDTH: f32 = 96.0;
/// The size of the dialog content.
const DIALOG_WIDTH: f32 = 960.0;
const DIALOG_HEIGHT: f32 = 680.0;
/// How long text waiting for its translation takes to dim and brighten again.
const TRANSLATION_PULSE: Duration = Duration::from_millis(600);
/// How often the dialog looks for a chunk that finished on the blocking pool.
const TRANSLATION_POLL: Duration = Duration::from_millis(10);
/// The deadline one translation gets after its model finished loading.
const TRANSLATION_TIMEOUT: Duration = Duration::from_secs(20);

/// One chunk's translation as the blocking pool returns it.
type TranslationOutcome = Result<Vec<TranslatedSegment>, TranslationFailure>;

/// One source of prediction-event pages.
///
/// It is a private seam rather than a public extension point: production answers
/// through [`MarketCapability`] and a test answers with a fake page.
pub trait PredictionEventSource: Clone + Send + Sync + 'static {
    /// Reads one page.
    fn page(
        &self,
        request: PredictionEventsRequest,
    ) -> impl Future<Output = Result<PredictionEventsPage, MarketError>> + Send;
}

impl PredictionEventSource for MarketCapability {
    async fn page(
        &self,
        request: PredictionEventsRequest,
    ) -> Result<PredictionEventsPage, MarketError> {
        self.prediction_events(request).fetch().await
    }
}

/// What happens when the user confirms an event, with the title its card showed.
type ConfirmHandler = Rc<dyn Fn(PredictionEventRef, SmolStr, &mut Window, &mut App) + 'static>;

/// Why one page could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PredictionSelectorError {
    Market(MarketError),
    Interrupted,
}

impl LocalizedError for PredictionSelectorError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::Interrupted => t!(cx, "error.prediction_selector.interrupted"),
        }
    }
}

/// What the translate button offers in the frame it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranslateAffordance {
    /// The interface is English, so there is nothing to translate.
    Hidden,
    /// The button cannot start a translation, and its tooltip is this message key.
    Disabled(&'static str),
    /// A translation is running.
    Busy,
    /// The button starts a translation of the visible page.
    Ready,
}

/// One translation the dialog is waiting for.
struct TranslationRun {
    /// The run the supervisor task belongs to; a newer generation supersedes it.
    generation: u64,
    /// The source texts whose translation has not arrived yet.
    pending: AHashSet<SmolStr>,
    /// The cancellation shared with the translator, so a run that is no longer
    /// wanted stops in the model rather than after its next chunk.
    cancel: TranslationCancel,
}

/// The event selector's content area.
pub struct PredictionEventSelector<S: PredictionEventSource> {
    source: S,
    venues: Vec<VenueId>,
    venue: VenueId,
    categories: Vec<PredictionCategory>,
    category: Option<SmolStr>,
    search: String,
    page: Option<PredictionEventsPage>,
    loading: bool,
    error: Option<PredictionSelectorError>,
    selected: Option<PredictionEventRef>,
    selected_title: Option<SmolStr>,
    cursor: Option<SmolStr>,
    cursor_stack: Vec<Option<SmolStr>>,
    page_number: u32,
    navigation_backup: Option<(Option<SmolStr>, Vec<Option<SmolStr>>, u32)>,
    generation: u64,
    search_generation: u64,
    search_input: Entity<InputState>,
    _search_subscription: Subscription,
    /// Keeps the dialog listening for the interface language changing, so a run
    /// started for one language stops when the interface switches to another.
    _language: Option<Subscription>,
    search_task: Option<Task<()>>,
    translations: AHashMap<SmolStr, SmolStr>,
    translation: Option<TranslationRun>,
    translation_task: Option<Task<()>>,
    translation_generation: u64,
    translation_timeout: Duration,
    on_confirm: ConfirmHandler,
}

impl PredictionEventSelector<MarketCapability> {
    /// Presents the selector against the production market capability.
    ///
    /// Returns whether a dialog was presented: with no loaded venue that serves
    /// prediction events there is nothing to select from.
    pub fn show(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let source = cx.global::<MarketParticipantsConnector>().market();
        Self::show_with_source(current, current_title, source, on_confirm, window, cx)
    }
}

impl<S: PredictionEventSource> PredictionEventSelector<S> {
    /// Presents the selector against an explicit source.
    pub fn show_with_source(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        source: S,
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let venues = loaded_prediction_venues(cx.global::<MarketParticipantsConnector>().venues());
        if venues.is_empty() {
            return false;
        }
        let search_input = cx.new(|cx| {
            InputStateBuilder::new("prediction-event-search")
                .placeholder(t!(cx, "prediction_selector.search_placeholder"))
                .build(window, cx)
        });
        let selector = cx.new(|cx| {
            Self::new(
                source,
                venues,
                current,
                current_title,
                search_input,
                on_confirm,
                cx,
            )
        });
        window.dialog(cx, selector.clone());
        selector.update(cx, |selector, cx| selector.reload(cx));
        true
    }

    fn new(
        source: S,
        venues: Vec<VenueId>,
        selected: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        search_input: Entity<InputState>,
        on_confirm: ConfirmHandler,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = selected.filter(|event| venues.contains(&event.route.venue));
        let selected_title = selected.as_ref().and(current_title);
        let venue = selected
            .as_ref()
            .map(|event| event.route.venue)
            .unwrap_or(venues[0]);
        let subscription = cx.subscribe(&search_input, |this, _, event: &InputStateEvent, cx| {
            if let InputStateEvent::Change(query) = event {
                this.search_changed(query.as_ref(), cx);
            }
        });
        let language = cx
            .try_global::<ConfigManager>()
            .map(|manager| manager.events().clone())
            .map(|events| {
                cx.subscribe(&events, |this, _, _: &LanguageChanged, cx| {
                    this.cancel_translation(cx);
                })
            });
        Self {
            source,
            venues,
            venue,
            categories: Vec::new(),
            category: None,
            search: String::new(),
            page: None,
            loading: false,
            error: None,
            selected,
            selected_title,
            cursor: None,
            cursor_stack: Vec::new(),
            page_number: 1,
            navigation_backup: None,
            generation: 0,
            search_generation: 0,
            search_input,
            _search_subscription: subscription,
            _language: language,
            search_task: None,
            translations: AHashMap::with_capacity(48),
            translation: None,
            translation_task: None,
            translation_generation: 0,
            translation_timeout: TRANSLATION_TIMEOUT,
            on_confirm,
        }
    }

    fn request(&self) -> PredictionEventsRequest {
        PredictionEventsRequest::first(self.venue)
            .category(self.category.clone())
            .search((!self.search.trim().is_empty()).then(|| SmolStr::new(self.search.trim())))
            .cursor(self.cursor.clone())
            .page_size(PAGE_SIZE)
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        // A page that is being replaced cannot translate text it no longer shows.
        self.cancel_translation(cx);
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        let source = self.source.clone();
        let request = self.request();
        // The market clients use Tokio's timers and I/O, so the read must run on
        // the application Tokio runtime rather than on GPUI's executor.
        let load = global::handle(cx).run(async move { source.page(request).await });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(result) => result.map_err(PredictionSelectorError::Market),
                Err(error) if error.is_cancelled() => return,
                Err(_) => Err(PredictionSelectorError::Interrupted),
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.apply(result, cx);
                }
            });
        })
        .detach();
    }

    fn apply(
        &mut self,
        result: Result<PredictionEventsPage, PredictionSelectorError>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(page) => {
                for category in &page.categories {
                    if let Some(known) = self
                        .categories
                        .iter_mut()
                        .find(|known| known.key == category.key)
                    {
                        known.label = category.label.clone();
                    } else {
                        self.categories.push(category.clone());
                    }
                }
                self.navigation_backup = None;
                self.page = Some(page);
                self.error = None;
            }
            Err(error) => {
                if let Some((cursor, cursor_stack, page_number)) = self.navigation_backup.take() {
                    self.cursor = cursor;
                    self.cursor_stack = cursor_stack;
                    self.page_number = page_number;
                }
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn search_changed(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.search == query {
            return;
        }
        // The page the translation belongs to is about to be replaced by the query.
        self.cancel_translation(cx);
        self.search = query.to_owned();
        self.search_generation = self.search_generation.wrapping_add(1);
        self.navigation_backup = None;
        let generation = self.search_generation;
        // The page is stale from the first keystroke: show the loading state while
        // the debounce runs and invalidate any read that is still in flight.
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        cx.notify();
        // Dropping the previous debounce task cancels the search it belonged to.
        self.search_task.take();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.reload(cx);
                }
            });
        });
        self.search_task = Some(task);
    }

    fn select_venue(&mut self, venue: VenueId, window: &mut Window, cx: &mut Context<Self>) {
        if self.venue == venue {
            return;
        }
        self.venue = venue;
        self.selected = None;
        self.selected_title = None;
        self.categories.clear();
        self.category = None;
        self.search.clear();
        self.search_input
            .update(cx, |input, cx| input.update_value("", window, cx));
        // A debounce armed for the old venue must not reload the new one.
        self.cancel_search_debounce();
        self.navigation_backup = None;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.page = None;
        self.reload(cx);
    }

    fn select_category(&mut self, index: usize, cx: &mut Context<Self>) {
        let next = index
            .checked_sub(1)
            .and_then(|index| self.categories.get(index))
            .map(|category| category.key.clone());
        if self.category == next {
            return;
        }
        self.cancel_search_debounce();
        self.navigation_backup = None;
        self.category = next;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.reload(cx);
    }

    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(next) = self.page.as_ref().and_then(|page| page.next_cursor.clone()) else {
            return;
        };
        self.cancel_search_debounce();
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.push(self.cursor.clone());
        self.cursor = Some(next);
        self.page_number = self.page_number.saturating_add(1);
        self.reload(cx);
    }

    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(previous) = self.cursor_stack.last().cloned() else {
            return;
        };
        self.cancel_search_debounce();
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.pop();
        self.cursor = previous;
        self.page_number = self.page_number.saturating_sub(1).max(1);
        self.reload(cx);
    }

    fn cancel_search_debounce(&mut self) {
        self.search_task.take();
        self.search_generation = self.search_generation.wrapping_add(1);
    }

    /// What the translate button offers right now.
    ///
    /// The settings are read per frame, so a model the user selects while the dialog
    /// is open takes effect on the next frame without a reload.
    fn translate_affordance(&self, cx: &App) -> TranslateAffordance {
        let Some(manager) = cx.try_global::<ConfigManager>() else {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_model_unset",
            );
        };
        let config = manager.read();
        if config.language == Language::En {
            return TranslateAffordance::Hidden;
        }
        if prediction_translation::translator(cx).is_none() {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_unavailable",
            );
        }
        if self.translation.is_some() {
            return TranslateAffordance::Busy;
        }
        // The engine loads the model from the directory the settings name, so a
        // selected model without one is as unusable as no selection at all.
        if config.translation.selected_model.is_none()
            || config.translation.model_directory.is_none()
        {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_model_unset",
            );
        }
        if self.error.is_none() && (self.loading || self.page.is_none()) {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_page_loading",
            );
        }
        if self.error.is_some()
            || self.page.as_ref().is_none_or(|page| {
                !page_has_translatable_text(
                    &page.events[..page.events.len().min(usize::from(PAGE_SIZE))],
                )
            })
        {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_no_content",
            );
        }
        TranslateAffordance::Ready
    }

    /// The translate button in the state the frame found it in.
    ///
    /// A disabled button carries no click handler: `Button::disabled` only paints the
    /// state, so the handler is what would otherwise still start a translation.
    fn translate_button(affordance: TranslateAffordance, cx: &mut Context<Self>) -> AnyElement {
        let label = match affordance {
            TranslateAffordance::Hidden => return div().into_any_element(),
            TranslateAffordance::Busy => t!(cx, "prediction_selector.translating"),
            TranslateAffordance::Disabled(_) | TranslateAffordance::Ready => {
                t!(cx, "prediction_selector.translate")
            }
        };
        let mut button = Button::new(ElementId::Name(SharedString::new_static(
            "PredictionTranslate",
        )))
        .small()
        .tonal()
        .flex_none()
        .debug_selector(|| "PREDICTION_TRANSLATE".into());
        button = match affordance {
            TranslateAffordance::Busy => button.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .debug_selector(|| "PREDICTION_TRANSLATE_SPINNER".into())
                            .size_3()
                            .child(Spinner::new("PredictionTranslateSpinner")),
                    )
                    .child(label),
            ),
            _ => button.child(label),
        };
        match affordance {
            TranslateAffordance::Disabled(key) => {
                button = button.disabled(true).with_tooltip(t!(cx, key));
            }
            TranslateAffordance::Busy => {
                button = button.disabled(true);
            }
            TranslateAffordance::Ready => {
                button = button.on_click(cx.listener(|this, _, _, cx| this.start_translation(cx)));
            }
            TranslateAffordance::Hidden => {}
        }
        button.into_any_element()
    }

    /// Starts translating the page the user is looking at.
    fn start_translation(&mut self, cx: &mut Context<Self>) {
        // The state was drawn from the settings of the last frame; the click re-reads
        // them so a change that arrived in between cannot start the wrong work.
        if !matches!(self.translate_affordance(cx), TranslateAffordance::Ready) {
            return;
        }
        let Some(translator) = prediction_translation::translator(cx) else {
            return;
        };
        let Some(manager) = cx.try_global::<ConfigManager>() else {
            return;
        };
        let config = manager.read();
        let plan = self.page_translation_plan();
        if plan.is_empty() {
            // A page whose text is already translated has nothing to ask for: a
            // second click leaves the finished translation in place.
            return;
        }

        let texts: usize = plan.iter().map(|chunk| chunk.segments().len()).sum();
        let mut pending = AHashSet::with_capacity(texts);
        pending.extend(
            plan.iter()
                .flat_map(|chunk| chunk.segments().iter().map(|segment| segment.text.clone())),
        );
        self.translation_generation = self.translation_generation.wrapping_add(1);
        let generation = self.translation_generation;
        let cancel = TranslationCancel::new();
        self.translation = Some(TranslationRun {
            generation,
            pending,
            cancel: cancel.clone(),
        });
        cx.notify();

        let Some(runtime) = global::try_handle(cx) else {
            self.finish_translation(generation, cx);
            return;
        };
        let setup = TranslationSetup {
            directory: config.translation.model_directory.clone(),
            model: config.translation.selected_model,
        };
        let timeout = self.translation_timeout;
        self.translation_task = Some(Self::spawn_translation(
            cx,
            runtime,
            translator,
            setup,
            config.language,
            plan,
            generation,
            cancel,
            timeout,
        ));
    }

    /// The pending translation of the cards this page draws.
    fn page_translation_plan(&self) -> Vec<TranslationChunk> {
        let Some(page) = &self.page else {
            return Vec::new();
        };
        let cards = &page.events[..page.events.len().min(usize::from(PAGE_SIZE))];
        plan_translation(cards, &self.translations)
    }

    /// Supervises one translation: model load, then the chunks one after another.
    ///
    /// The supervisor only holds a weak handle to the dialog, so it cannot keep the
    /// dialog alive; a run whose dialog is gone ends when its next update fails.
    fn spawn_translation(
        cx: &mut Context<Self>,
        runtime: Runtime,
        translator: Arc<dyn LocalTranslator>,
        setup: TranslationSetup,
        target: Language,
        plan: Vec<TranslationChunk>,
        generation: u64,
        cancel: TranslationCancel,
        timeout: Duration,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            if cancel.is_cancelled() {
                let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                return;
            }
            let prepared = runtime
                .blocking({
                    let translator = Arc::clone(&translator);
                    let setup = setup.clone();
                    let cancel = cancel.clone();
                    move || translator.prepare(&setup, cancel)
                })
                .await;
            // The page, the language or the settings may have changed while the model
            // was loading. A model load cannot be aborted, so the run instead checks
            // the flag it was cancelled with and its generation here, before the
            // translation's deadline starts.
            let current = this
                .update(cx, |this, _| {
                    this.translation
                        .as_ref()
                        .is_some_and(|run| run.generation == generation)
                })
                .unwrap_or(false);
            if cancel.is_cancelled() || !current {
                cancel.cancel();
                let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                return;
            }
            match prepared {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => {
                    log::warn!("preparing the translation model failed: {failure:?}");
                    cancel.cancel();
                    let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                    return;
                }
                Err(error) => {
                    log::warn!("the translation model load task failed: {error}");
                    cancel.cancel();
                    let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                    return;
                }
            }
            // The user decision is that the model load does not consume the
            // translation's deadline: a cold model would otherwise time out the very
            // translation it was loading for.
            let deadline = cx.background_executor().now() + timeout;
            for chunk in plan {
                if cancel.is_cancelled() || cx.background_executor().now() >= deadline {
                    cancel.cancel();
                    let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                    return;
                }
                let slot: Arc<Mutex<Option<TranslationOutcome>>> = Arc::new(Mutex::new(None));
                // A GPUI task has no Tokio timer context, so the chunk is polled for
                // its answer instead of racing the translation with a Tokio timeout.
                let handle = runtime.blocking({
                    let translator = Arc::clone(&translator);
                    let setup = setup.clone();
                    let cancel = cancel.clone();
                    let slot = Arc::clone(&slot);
                    let segments = chunk.segments().to_vec();
                    move || {
                        let result = translator.translate(&setup, target, segments, cancel);
                        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                    }
                });
                // The blocking closure writes the slot before it returns, so a
                // finished handle with an empty slot means the closure panicked and
                // no answer is coming. Completion is given one poll to land first,
                // because the write and the completion are two steps this poll can
                // land between; a second empty poll is a failed run.
                let mut saw_finished = false;
                loop {
                    let finished = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(result) = finished {
                        let applied = this.update(cx, |this, cx| {
                            this.apply_translation_chunk(generation, &chunk, result, cx)
                        });
                        if !matches!(applied, Ok(true)) {
                            return;
                        }
                        break;
                    }
                    let finished = handle.is_finished();
                    if finished && saw_finished {
                        log::warn!(
                            "the translation worker for {} texts ended without an answer",
                            chunk.segments().len()
                        );
                        cancel.cancel();
                        let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                        return;
                    }
                    saw_finished = finished;
                    if cancel.is_cancelled() || cx.background_executor().now() >= deadline {
                        cancel.cancel();
                        let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
                        return;
                    }
                    cx.background_executor().timer(TRANSLATION_POLL).await;
                }
            }
            let _ = this.update(cx, |this, cx| this.finish_translation(generation, cx));
        })
    }

    /// Applies one chunk's translations, or ends the run when the chunk failed.
    ///
    /// Answers whether the run is still current and can start its next chunk.
    fn apply_translation_chunk(
        &mut self,
        generation: u64,
        chunk: &TranslationChunk,
        result: TranslationOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .translation
            .as_ref()
            .is_none_or(|run| run.generation != generation)
        {
            return false;
        }
        if let Err(failure) = &result {
            log::warn!(
                "translating {} texts failed: {failure:?}",
                chunk.segments().len()
            );
        }
        let Ok(translated) = result else {
            // The status quo is kept: the texts that did arrive stay, and the rest
            // keep showing their source. The failure is only logged.
            if let Some(run) = self.translation.take() {
                run.cancel.cancel();
            }
            cx.notify();
            return false;
        };
        let mut by_id: AHashMap<SmolStr, SmolStr> = AHashMap::with_capacity(translated.len());
        for segment in translated {
            by_id.insert(segment.id, segment.text);
        }
        let Some(run) = self.translation.as_mut() else {
            return false;
        };
        for segment in chunk.segments() {
            if let Some(text) = by_id.get(&segment.id) {
                self.translations.insert(segment.text.clone(), text.clone());
                run.pending.remove(&segment.text);
            }
        }
        cx.notify();
        true
    }

    /// Ends the run `generation` names, if it is still the current one.
    ///
    /// Answers whether anything was ended. A run that a newer generation superseded
    /// is left alone.
    fn finish_translation(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if self
            .translation
            .as_ref()
            .is_none_or(|run| run.generation != generation)
        {
            return false;
        }
        self.translation = None;
        cx.notify();
        true
    }

    /// Stops the run and drops everything it produced, for a page that changed.
    fn cancel_translation(&mut self, cx: &mut Context<Self>) {
        self.translation_generation = self.translation_generation.wrapping_add(1);
        if let Some(run) = self.translation.take() {
            run.cancel.cancel();
        }
        // The task is cancelled by the drop; the model work it started is stopped by
        // the shared cancellation, because a blocking call cannot be aborted.
        self.translation_task.take();
        self.translations.clear();
        cx.notify();
    }

    /// The text to draw for `source`, and whether it is still being translated.
    fn display_text(&self, source: &SmolStr) -> (SharedString, bool) {
        let text = self.translations.get(source).map_or_else(
            || SharedString::new(source.clone()),
            |translated| SharedString::new(translated.clone()),
        );
        let pulsing = self
            .translation
            .as_ref()
            .is_some_and(|run| run.pending.contains(source));
        (text, pulsing)
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_translation(cx);
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let Some(title) = self.selected_title.clone() else {
            return;
        };
        let on_confirm = self.on_confirm.clone();
        on_confirm(selected, title, window, cx);
        cx.emit(DismissEvent);
    }

    fn venue_select(&self, cx: &Context<Self>) -> Select {
        let select = self.venues.iter().fold(
            Select::new(ElementId::Name(SharedString::new_static("PredictionVenue")))
                .placeholder(SharedString::new(self.venue.display_string(cx)))
                .selected_item(Some(SharedString::new(self.venue.as_str())))
                .medium(),
            |select, venue| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(venue.as_str()),
                    SharedString::new(venue.display_string(cx)),
                )))
            },
        );
        select.on_change({
            let selector = cx.entity();
            let venues = self.venues.clone();
            move |value, window, app| {
                let Some(value) = value.as_deref() else {
                    return;
                };
                let venue = VenueId::from_name(value).ok().or_else(|| {
                    venues
                        .iter()
                        .copied()
                        .find(|venue| venue.display_string(app).as_ref() == value)
                });
                let Some(venue) = venue else {
                    return;
                };
                selector.update(app, |this, cx| this.select_venue(venue, window, cx));
            }
        })
    }

    fn category_tabs(&self, cx: &Context<Self>) -> TabBar {
        let selected = self
            .category
            .as_ref()
            .and_then(|category| {
                self.categories
                    .iter()
                    .position(|known| &known.key == category)
            })
            .map_or(0, |index| index + 1);
        let tabs = self.categories.iter().fold(
            TabBar::new("PredictionCategories").small().child(
                div()
                    .child(t!(cx, "prediction_selector.all"))
                    .into_any_element(),
            ),
            |tabs, category| {
                tabs.child(
                    div()
                        .child(SharedString::new(category.label.clone()))
                        .into_any_element(),
                )
            },
        );
        tabs.selected_index(selected).on_change({
            let selector = cx.entity();
            move |index, _, app| {
                selector.update(app, |this, cx| this.select_category(*index, cx));
            }
        })
    }

    fn render_card(
        &self,
        index: usize,
        card: &PredictionEventCard,
        reduce_motion: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_ref() == Some(&card.event);
        let selector = cx.entity().downgrade();
        let event = card.event.clone();
        let title = card.title.clone();
        let theme = cx.theme();
        let (title_text, title_pulsing) = self.display_text(&card.title);
        let mut options = div().flex().flex_col().gap_1();
        for (option_index, option) in card.options.iter().enumerate() {
            let (text, pulsing) = self.display_text(&option.label);
            let pulse_id = ElementId::NamedInteger(
                SharedString::new_static("PredictionOptionPulse"),
                ((index as u64) << 32) | (option_index as u64),
            );
            options = options.child(option_row(
                option,
                text,
                pulsing,
                pulse_id,
                reduce_motion,
                cx,
            ));
        }
        let mut body = div()
            .id((
                ElementId::Name(SharedString::new_static("PredictionCard")),
                SharedString::new(card.event.native_event.as_str()),
            ))
            .debug_selector(move || format!("PREDICTION_EVENT_CARD_{index}"))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if selected {
                theme.roles.brand.base.hsla
            } else {
                theme.roles.line.divider.hsla
            })
            .bg(theme.roles.surface.raised.hsla)
            .cursor_pointer()
            .on_click(move |_, _, app| {
                let _ = selector.update(app, |this, cx| {
                    this.selected = Some(event.clone());
                    this.selected_title = Some(title.clone());
                    cx.notify();
                });
            })
            .child(pulse_text(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.roles.fg.default.hsla)
                    .line_height(gpui_kit::px(18.0))
                    .max_h(px(36.0))
                    .overflow_hidden()
                    .child(title_text),
                ElementId::NamedInteger(
                    SharedString::new_static("PredictionTitlePulse"),
                    index as u64,
                ),
                title_pulsing,
                reduce_motion,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Badge::new()
                            .info()
                            .small()
                            .child(SharedString::new(card.category.label.clone())),
                    )
                    .children(card.ends_at.and_then(|end| format_end(end, cx)).map(|end| {
                        div()
                            .text_xs()
                            .text_color(theme.roles.fg.muted.hsla)
                            .child(SharedString::new(end))
                    })),
            )
            .child(options);
        if card.remaining_markets > 0 {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "prediction_selector.more_markets",
                        count = card.remaining_markets
                    )),
            );
        } else if card.remaining_options > 0 {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "prediction_selector.more_options",
                        count = card.remaining_options
                    )),
            );
        }
        body.into_any_element()
    }

    fn render_body(&self, cx: &Context<Self>) -> AnyElement {
        if self.loading {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .debug_selector(|| "PREDICTION_EVENT_SPINNER".into())
                        .size_5()
                        .child(Spinner::new("PredictionSpinner")),
                )
                .into_any_element();
        }
        if let Some(error) = &self.error {
            return div()
                .debug_selector(|| "PREDICTION_ERROR".into())
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(EmptyState::new(
                    "PredictionError",
                    error.localized_message(cx),
                ))
                .child(
                    Button::new("PredictionRetry")
                        .debug_selector(|| "PREDICTION_RETRY".into())
                        .tonal()
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                        .child(t!(cx, "common.retry")),
                )
                .into_any_element();
        }
        let Some(page) = &self.page else {
            return div().flex_1().into_any_element();
        };
        if page.events.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "PredictionEmpty",
                    t!(cx, "prediction_selector.no_matching_events"),
                ))
                .into_any_element();
        }
        let reduce_motion = cx.reduce_motion();
        let grid = div()
            .debug_selector(|| "PREDICTION_EVENTS_GRID".into())
            .size_full()
            .grid()
            .grid_cols(GRID_COLUMNS)
            .grid_rows(GRID_ROWS)
            .gap_2()
            .children(
                page.events
                    .iter()
                    .take(usize::from(PAGE_SIZE))
                    .enumerate()
                    .map(|(index, card)| self.render_card(index, card, reduce_motion, cx)),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(grid)
            .into_any_element()
    }

    fn footer(&self, cx: &Context<Self>) -> AnyElement {
        let previous = self.cursor_stack.is_empty() || self.loading;
        let next = self
            .page
            .as_ref()
            .is_none_or(|page| page.next_cursor.is_none() || self.loading);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("PredictionPrevious")
                    .ghost()
                    .small()
                    .disabled(previous)
                    .when(!previous, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.previous_page(cx)))
                    })
                    .child(t!(cx, "prediction_selector.previous")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().roles.fg.muted.hsla)
                    .child(t!(cx, "prediction_selector.page", page = self.page_number)),
            )
            .child(
                Button::new("PredictionNext")
                    .ghost()
                    .small()
                    .disabled(next)
                    .when(!next, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.next_page(cx)))
                    })
                    .child(t!(cx, "prediction_selector.next")),
            )
            .child(div().flex_1())
            .child(
                Button::new("PredictionCancel")
                    .debug_selector(|| "PREDICTION_CANCEL".into())
                    .tonal()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cancel_translation(cx);
                        cx.emit(DismissEvent);
                    }))
                    .child(t!(cx, "common.cancel")),
            )
            .child(
                Button::new("PredictionConfirm")
                    .debug_selector(|| "PREDICTION_CONFIRM".into())
                    .filled()
                    .disabled(self.selected.is_none() || self.selected_title.is_none())
                    .when(
                        self.selected.is_some() && self.selected_title.is_some(),
                        |button| {
                            button.on_click(
                                cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                            )
                        },
                    )
                    .child(t!(cx, "common.confirm")),
            )
            .into_any_element()
    }
}

impl<S: PredictionEventSource> EventEmitter<DismissEvent> for PredictionEventSelector<S> {}

/// The dialog is dropped after its exit transition, so the explicit cancellation in
/// `confirm`, the Cancel button and `reload` matters; this is the safety net for a
/// dialog that is dropped any other way.
impl<S: PredictionEventSource> Drop for PredictionEventSelector<S> {
    fn drop(&mut self) {
        if let Some(run) = self.translation.take() {
            run.cancel.cancel();
        }
        self.translation_task.take();
    }
}

impl<S: PredictionEventSource> Render for PredictionEventSelector<S> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let affordance = self.translate_affordance(cx);
        let translate = (!matches!(affordance, TranslateAffordance::Hidden))
            .then(|| Self::translate_button(affordance, cx));
        let theme = cx.theme();
        div()
            .debug_selector(|| "PREDICTION_EVENT_SELECTOR".into())
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.venue_select(cx))
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.search_input).clearable(true)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.category_tabs(cx)))
                    .when_some(translate, |row, translate| row.child(translate)),
            )
            .child(
                div()
                    .debug_selector(|| "PREDICTION_EVENTS_BODY".into())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .bg(theme.roles.surface.sunken.hsla)
                    .p_2()
                    .child(self.render_body(cx)),
            )
            .child(self.footer(cx))
    }
}

/// Draws one text leaf, pulsing while its translation is still on its way.
///
/// The animation belongs to the text alone: the card, its badge and its probability
/// bar must stay perfectly readable while the text that is still being translated
/// fades in and out.
fn pulse_text(text: Div, id: ElementId, pulsing: bool, reduce_motion: bool) -> AnyElement {
    if pulsing && !reduce_motion {
        text.with_animation(
            id,
            Animation::new(TRANSLATION_PULSE)
                .repeat_synced()
                .with_easing(bounce(ease_in_out)),
            |this, eased| this.opacity(0.3 + 0.7 * eased),
        )
        .into_any_element()
    } else {
        text.into_any_element()
    }
}

fn option_row(
    option: &PredictionOptionCard,
    text: SharedString,
    pulsing: bool,
    pulse_id: ElementId,
    reduce_motion: bool,
    cx: &App,
) -> AnyElement {
    let percentage = option
        .probability
        .map(|probability| (probability.ratio().inner() as f64 / 1e16).clamp(0.0, 100.0))
        .unwrap_or(0.0);
    let color = cx.theme().roles.chart.series[0].hsla;
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(pulse_text(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(cx.theme().roles.fg.default.hsla)
                .child(text),
            pulse_id,
            pulsing,
            reduce_motion,
        ))
        .child(
            div()
                .flex_none()
                .w(px(BAR_WIDTH))
                .h(px(4.0))
                .rounded_full()
                .bg(cx.theme().roles.surface.overlay.hsla)
                .child(
                    div()
                        .h_full()
                        .w(px(BAR_WIDTH * (percentage / 100.0) as f32))
                        .rounded_full()
                        .bg(color),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(34.0))
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(SharedString::new(format_smolstr!("{percentage:.0}%"))),
        )
        .into_any_element()
}

fn loaded_prediction_venues(venues: &LoadedVenues) -> Vec<VenueId> {
    let mut loaded: Vec<VenueId> = VenueId::ALL
        .into_iter()
        .filter(|venue| {
            venues.entries().iter().any(|(route, entry)| {
                route.venue == *venue
                    && entry
                        .capabilities
                        .iter()
                        .any(|support| support.capability == CapabilityId::PredictionEvents)
            })
        })
        .collect();
    loaded.sort();
    loaded
}

fn format_end(millis: i64, cx: &App) -> Option<String> {
    let seconds = millis.checked_div(1_000)?;
    let date = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let date = date.to_offset(data_time_zone(cx));
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        path::PathBuf,
        rc::Rc,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::{
        settings::{Config, Language},
        translation::TranslationModel,
    };
    use app_config_provider::manager::ConfigManager;
    use base_ui::{
        animation::ANIMATION_TICK,
        input::InputStateBuilder,
        layer::{Layer, LayerExt},
    };
    use gpui_kit::{
        App, AppContext as _, BorrowAppContext, Context, ElementId, Entity, IntoElement, Modifiers,
        ParentElement, Render, SharedString, Styled, TestAppContext, VisualTestContext, Window,
        div,
    };
    use i18n::{global::embedded, t};
    use market_participants_connector::market::error::MarketError;
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
    use widget_tokio_runtime::global;

    use super::{
        PredictionEventSelector, PredictionEventSource, TRANSLATION_POLL, TranslateAffordance,
        pulse_text,
    };
    use crate::prediction_translation::{
        self, LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure,
        TranslationSegment, TranslationSetup,
    };

    /// A page source that needs Tokio's timer, exactly like the HTTP clients do.
    #[derive(Clone, Default)]
    struct SlowSource {
        reads: Arc<AtomicUsize>,
        events: Arc<Vec<PredictionEventCard>>,
        next_cursor: Option<SmolStr>,
        /// Returned by the next read and then cleared, so a retry succeeds.
        fail_next_read: Arc<Mutex<Option<MarketError>>>,
    }

    impl PredictionEventSource for SlowSource {
        async fn page(
            &self,
            _request: market_participants_connector::market::prediction::PredictionEventsRequest,
        ) -> Result<PredictionEventsPage, MarketError> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self
                .fail_next_read
                .lock()
                .expect("the failure plan is never poisoned")
                .take()
            {
                return Err(error);
            }
            Ok(PredictionEventsPage {
                events: self.events.as_ref().clone(),
                categories: Vec::new(),
                next_cursor: self.next_cursor.clone(),
                total: Some(self.events.len() as u32),
            })
        }
    }

    fn card(index: usize) -> PredictionEventCard {
        PredictionEventCard {
            event: PredictionEventRef {
                route: VenueRoute::new(
                    VenueId::Polymarket,
                    VenueScopeId::new("predict").expect("the test scope should be valid"),
                ),
                native_event: IdentifierValue::new(format!("event-{index}"))
                    .expect("the test event should be valid"),
            },
            title: SmolStr::new(format!("Event {index}")),
            category: PredictionCategory::new("politics", "Politics"),
            ends_at: None,
            volume: None,
            liquidity: None,
            options: Vec::new(),
            remaining_options: 0,
            market_count: 1,
            remaining_markets: 0,
        }
    }

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn install(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            base_ui::style::theme::install(cx);
            embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });
    }

    fn selector(
        window: &mut VisualTestContext,
        source: SlowSource,
    ) -> Entity<PredictionEventSelector<SlowSource>> {
        window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    source,
                    vec![VenueId::Polymarket],
                    None,
                    None,
                    search,
                    Rc::new(|_: PredictionEventRef, _: SmolStr, _: &mut Window, _: &mut App| {}),
                    cx,
                )
            })
        })
    }

    /// Installs a translator the test can observe and gate.
    fn install_translator(cx: &mut TestAppContext, translator: &Arc<FakeTranslator>) {
        let installed: Arc<dyn LocalTranslator> = translator.clone();
        cx.update(|cx| prediction_translation::install(cx, installed));
    }

    /// Selects the smallest local model, so a translation is ready to start.
    fn select_translation_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.translation.model_directory = Some(PathBuf::from("models"));
                        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
                    },
                    cx,
                );
            });
        });
    }

    /// Removes the model selection, so the translate button has no model to load.
    fn clear_translation_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.translation.model_directory = None;
                        config.translation.selected_model = None;
                    },
                    cx,
                );
            });
        });
    }

    fn set_language(cx: &mut TestAppContext, language: Language) {
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.language = language, cx);
            });
        });
    }

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

    /// One card with a title and the two labels every event in the fixture shares.
    fn translatable_card(index: usize) -> PredictionEventCard {
        let mut card = card(index);
        let event = card.event.clone();
        card.options = ["Yes", "No"]
            .iter()
            .map(|label| option(&event, label))
            .collect();
        card
    }

    fn translatable_source() -> SlowSource {
        SlowSource {
            events: Arc::new((0..8).map(translatable_card).collect()),
            ..Default::default()
        }
    }

    /// Opens the dialog on the source and waits for its page.
    fn open_page(
        window: &mut VisualTestContext,
        selector: &Entity<PredictionEventSelector<SlowSource>>,
    ) {
        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the page to arrive", |cx| {
            !selector.read(cx).loading
        });
    }

    fn affordance(
        window: &mut VisualTestContext,
        selector: &Entity<PredictionEventSelector<SlowSource>>,
    ) -> TranslateAffordance {
        window.update(|_, cx| selector.read(cx).translate_affordance(cx))
    }

    /// Draws one frame and lets the notifications it queued settle.
    fn refresh(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// Clicks the center of a drawn element, exactly like a user would.
    fn click(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the dialog should draw {selector}"))
            .center();
        window.simulate_click(target, Modifiers::none());
        window.run_until_parked();
    }

    /// Hovers a drawn element and lets a tooltip's fade pass.
    fn hover(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the dialog should draw {selector}"))
            .center();
        window.simulate_mouse_move(target, None, Modifiers::none());
        window.run_until_parked();
        for _ in 0..10 {
            window
                .executor()
                .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
            window.update(|window, _| window.refresh());
            window.run_until_parked();
        }
    }

    /// Runs the dialog until `ready` holds, moving the GPUI clock the translation
    /// poll loop waits on.
    fn settle_translation(
        window: &mut VisualTestContext,
        what: &str,
        ready: impl Fn(&App) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            window.run_until_parked();
            if window.update(|_, cx| ready(cx)) {
                return;
            }
            window.executor().advance_clock(TRANSLATION_POLL);
            std::thread::sleep(Duration::from_millis(2));
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
        }
    }

    /// A one-way gate a fake blocking call waits behind.
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn open() -> Self {
            Self {
                open: Mutex::new(true),
                changed: Condvar::new(),
            }
        }

        fn closed() -> Self {
            Self {
                open: Mutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn release(&self) {
            *self.open.lock().expect("the gate is never poisoned") = true;
            self.changed.notify_all();
        }

        /// Waits for the gate unless the translation is cancelled first.
        fn pass_unless(&self, cancel: &TranslationCancel) -> bool {
            let mut open = self.open.lock().expect("the gate is never poisoned");
            while !*open {
                if cancel.is_cancelled() {
                    return false;
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(open, Duration::from_millis(5))
                    .expect("the gate is never poisoned");
                open = guard;
            }
            true
        }
    }

    /// A translator that records every request and can be held open by a test.
    struct FakeTranslator {
        prepares: AtomicUsize,
        translates: AtomicUsize,
        cancelled: AtomicUsize,
        requests: Mutex<Vec<Vec<SmolStr>>>,
        fail_on_call: Mutex<Option<usize>>,
        prepare_gate: Arc<Gate>,
        translate_gate: Arc<Gate>,
    }

    impl FakeTranslator {
        fn open() -> Self {
            Self {
                prepares: AtomicUsize::new(0),
                translates: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
                fail_on_call: Mutex::new(None),
                prepare_gate: Arc::new(Gate::open()),
                translate_gate: Arc::new(Gate::open()),
            }
        }

        /// A translator whose model load and translation both wait for the test.
        fn blocked() -> Self {
            let mut translator = Self::open();
            translator.prepare_gate = Arc::new(Gate::closed());
            translator.translate_gate = Arc::new(Gate::closed());
            translator
        }

        /// A translator that loads the model immediately but holds every translation.
        fn suspended() -> Self {
            let mut translator = Self::open();
            translator.translate_gate = Arc::new(Gate::closed());
            translator
        }
    }

    impl LocalTranslator for FakeTranslator {
        fn prepare(
            &self,
            _setup: &TranslationSetup,
            cancel: TranslationCancel,
        ) -> Result<(), TranslationFailure> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            if !self.prepare_gate.pass_unless(&cancel) {
                return Err(TranslationFailure::Cancelled);
            }
            Ok(())
        }

        fn translate(
            &self,
            _setup: &TranslationSetup,
            _target: Language,
            segments: Vec<TranslationSegment>,
            cancel: TranslationCancel,
        ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
            let call = self.translates.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .expect("the request log is never poisoned")
                .push(
                    segments
                        .iter()
                        .map(|segment| segment.text.clone())
                        .collect(),
                );
            if !self.translate_gate.pass_unless(&cancel) {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                return Err(TranslationFailure::Cancelled);
            }
            if *self
                .fail_on_call
                .lock()
                .expect("the failure plan is never poisoned")
                == Some(call)
            {
                return Err(TranslationFailure::Response);
            }
            Ok(segments
                .into_iter()
                .map(|segment| TranslatedSegment {
                    id: segment.id,
                    text: format_smolstr!("译:{}", segment.text),
                })
                .collect())
        }
    }

    /// A translator whose chunk panics on the blocking pool, the way a broken
    /// engine would.
    struct PanickingTranslator;

    impl LocalTranslator for PanickingTranslator {
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
            _target: Language,
            _segments: Vec<TranslationSegment>,
            _cancel: TranslationCancel,
        ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
            panic!("the fixture translator panicked");
        }
    }

    /// The event button names an event by its title, so the dialog hands the
    /// title back with the identity it confirmed.
    #[gpui_kit::test]
    fn confirming_an_event_should_pass_the_title(cx: &mut TestAppContext) {
        install(cx);
        let confirmed = Rc::new(RefCell::new(None));
        let captured = confirmed.clone();
        let source = SlowSource::default();
        let event = card(0).event;
        let title = card(0).title;
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    source,
                    vec![VenueId::Polymarket],
                    Some(event.clone()),
                    Some(title.clone()),
                    search,
                    Rc::new(
                        move |event: PredictionEventRef,
                              title: SmolStr,
                              _: &mut Window,
                              _: &mut App| {
                            *captured.borrow_mut() = Some((event, title));
                        },
                    ),
                    cx,
                )
            })
        });

        window.update(|window, cx| {
            selector.update(cx, |selector, cx| selector.confirm(window, cx));
        });

        assert_eq!(confirmed.borrow().clone(), Some((event, title)));
    }

    fn settle_until(window: &mut VisualTestContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            window.run_until_parked();
            let handle = window.update(|_, cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if window.update(|_, cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The read is started from a GPUI event, but the market clients use Tokio's
    /// reactor. A read that runs on GPUI's executor panics before the dialog can draw.
    #[gpui_kit::test]
    fn reading_a_page_should_run_on_the_application_tokio_runtime(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource::default();
        let window = cx.add_empty_window();
        let selector = selector(window, source.clone());

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.reload(cx)));
        settle_until(window, "the page to arrive", |cx| {
            !selector.read(cx).loading
        });

        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        assert!(window.update(|_, cx| selector.read(cx).error.is_none()));
        assert_eq!(
            window.update(|_, cx| { selector.read(cx).page.as_ref().and_then(|page| page.total) }),
            Some(0)
        );
    }

    /// The selector must be drawable in the layer that opens it, with the page it
    /// read; this is the path that was invisible while the read panicked.
    #[gpui_kit::test]
    fn the_open_selector_should_draw_its_empty_page(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource::default();
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the empty page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_EVENT_SELECTOR").is_some(),
            "the selector dialog should be drawn with its empty page"
        );
    }

    /// A full page is two rows of four cards, and every space around them is the
    /// same width: the grid's padding matches the gap between cards.
    #[gpui_kit::test]
    fn a_full_page_should_draw_eight_cards_with_even_gaps(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource {
            events: Arc::new((0..8).map(card).collect()),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the full page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        let body = window
            .debug_bounds("PREDICTION_EVENTS_BODY")
            .expect("the grid's container should be drawn");
        let cards: Vec<_> = (0..8)
            .map(|index| {
                let selector = Box::leak(format!("PREDICTION_EVENT_CARD_{index}").into_boxed_str());
                window
                    .debug_bounds(selector)
                    .expect("every card of a full page should be drawn")
            })
            .collect();

        let gap_x = cards[1].left().as_f32() - cards[0].right().as_f32();
        let gap_y = cards[4].top().as_f32() - cards[0].bottom().as_f32();
        let spaces = [
            gap_x,
            gap_y,
            cards[0].left().as_f32() - body.left().as_f32() - 1.0,
            body.right().as_f32() - cards[3].right().as_f32() - 1.0,
            cards[0].top().as_f32() - body.top().as_f32() - 1.0,
            body.bottom().as_f32() - cards[7].bottom().as_f32() - 1.0,
        ];
        assert!(gap_x > 0.0, "the cards should not touch");
        for space in spaces {
            assert!(
                (space - gap_x).abs() <= 1.0,
                "every space around the cards should equal the gap: {spaces:?}"
            );
        }
    }

    /// The category tabs belong to the venue being shown. Switching venues must
    /// drop the old venue's tabs before the new venue's first page arrives.
    #[gpui_kit::test]
    fn switching_venue_should_clear_the_event_categories(cx: &mut TestAppContext) {
        install(cx);
        let window = cx.add_empty_window();
        let selector = selector(window, SlowSource::default());

        window.update(|window, cx| {
            selector.update(cx, |selector, _| {
                selector
                    .categories
                    .push(PredictionCategory::new("politics", "Politics"));
            });
            selector.update(cx, |selector, cx| {
                selector.select_venue(VenueId::Hyperliquid, window, cx)
            });
        });

        window.update(|_, cx| {
            assert!(selector.read(cx).categories.is_empty());
            assert!(selector.read(cx).category.is_none());
        });
    }

    /// Moving to another page keeps the old page in memory for the footer, but the
    /// body shows the spinner while the new page is being read.
    #[gpui_kit::test]
    fn paging_should_show_the_spinner_in_the_middle_of_the_container(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource {
            events: Arc::new((0..8).map(card).collect()),
            next_cursor: Some(SmolStr::new("next")),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the first page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_EVENTS_GRID").is_some(),
            "the first page should be drawn before paging"
        );

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.next_page(cx)));
        window.run_until_parked();
        let spinner = window
            .debug_bounds("PREDICTION_EVENT_SPINNER")
            .expect("the loading spinner should be drawn while the next page is read");
        let body = window
            .debug_bounds("PREDICTION_EVENTS_BODY")
            .expect("the grid's container should be drawn");
        assert!(
            (spinner.center().x.as_f32() - body.center().x.as_f32()).abs() <= 1.0
                && (spinner.center().y.as_f32() - body.center().y.as_f32()).abs() <= 1.0,
            "the spinner should sit in the middle of the container"
        );

        settle_until(window, "the next page to arrive", |cx| {
            !selector.read(cx).loading
        });
    }

    /// An English interface has nothing to translate, so the control is not drawn at all.
    #[gpui_kit::test]
    fn english_should_hide_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        set_language(cx, Language::En);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, SlowSource::default());
        open_page(window, &selector);

        assert_eq!(affordance(window, &selector), TranslateAffordance::Hidden);
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_TRANSLATE").is_none(),
            "an English interface must not draw the translate button"
        );

        set_language(window, Language::ZhCn);
        window.update(|window, cx| {
            selector.update(cx, |_, cx| cx.notify());
            window.refresh();
        });
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_TRANSLATE").is_some(),
            "the button is drawn again when the interface is not English"
        );
    }

    /// Without a selected model there is nothing the translator could load.
    #[gpui_kit::test]
    fn an_unset_model_should_disable_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, SlowSource::default());
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
        );
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_TRANSLATE").is_some(),
            "a disabled button states its reason through its tooltip, not by disappearing"
        );
    }

    /// The page has not arrived yet, so there is no text to translate.
    #[gpui_kit::test]
    fn a_loading_page_should_disable_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let window = cx.add_empty_window();
        let selector = selector(window, translatable_source());

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.reload(cx)));

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_page_loading")
        );
    }

    /// A page with no events is a page with nothing to translate.
    #[gpui_kit::test]
    fn an_empty_page_should_disable_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, SlowSource::default());
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_no_content")
        );
    }

    /// A build that installs no translator (Windows ARM64, or no installed service)
    /// says so instead of offering a button that cannot work.
    #[gpui_kit::test]
    fn a_build_without_a_translator_should_disable_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        select_translation_model(cx);
        let window = cx.add_empty_window();
        let selector = selector(window, SlowSource::default());

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_unavailable")
        );
    }

    /// A full page is two model requests of four cards each, and every text of both
    /// requests is filled in by the time the run ends.
    #[gpui_kit::test]
    fn a_ready_button_should_translate_the_page_in_two_chunks(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);

        assert_eq!(affordance(window, &selector), TranslateAffordance::Ready);
        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the translation to finish", |cx| {
            selector.read(cx).translation.is_none()
        });

        assert_eq!(translator.translates.load(Ordering::SeqCst), 2);
        let requests = translator
            .requests
            .lock()
            .expect("the request log is never poisoned");
        assert_eq!(requests.len(), 2, "eight cards should be two requests");
        assert_eq!(requests[0].len(), 6, "the first chunk carries six texts");
        assert_eq!(requests[1].len(), 4, "the second chunk carries four texts");
        drop(requests);

        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert_eq!(
                selector.translations.len(),
                10,
                "eight titles and the two shared option labels"
            );
            assert_eq!(
                selector.translations.get("Event 0").map(SmolStr::as_str),
                Some("译:Event 0")
            );
            assert_eq!(
                selector.translations.get("Yes").map(SmolStr::as_str),
                Some("译:Yes")
            );
            assert!(selector.error.is_none(), "a translation is not an error");
            assert!(selector.translation.is_none(), "the run should be over");
        });

        // A second click on an already translated page has nothing to ask for: the
        // finished translation stays where it is.
        let translates = translator.translates.load(Ordering::SeqCst);
        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        window.run_until_parked();
        assert_eq!(
            translator.translates.load(Ordering::SeqCst),
            translates,
            "the second click should not translate anything again"
        );
        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert_eq!(
                selector.translations.len(),
                10,
                "the second click should keep every finished translation"
            );
            assert!(
                selector.translation.is_none(),
                "the second click should not start a run"
            );
        });
    }

    /// A failed chunk keeps the translations of the chunks before it and leaves the
    /// rest as their source text, silently.
    #[gpui_kit::test]
    fn a_failed_chunk_should_keep_the_translations_of_the_earlier_ones(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        *translator
            .fail_on_call
            .lock()
            .expect("the failure plan is never poisoned") = Some(1);
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the failed translation to end", |cx| {
            selector.read(cx).translation.is_none()
        });

        assert_eq!(translator.translates.load(Ordering::SeqCst), 2);
        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert_eq!(
                selector.translations.get("Event 0").map(SmolStr::as_str),
                Some("译:Event 0"),
                "the first chunk's translations stay"
            );
            assert!(
                selector.translations.contains_key("Yes"),
                "the shared option label from the first chunk stays"
            );
            assert!(
                !selector.translations.contains_key("Event 4"),
                "the failed chunk's texts stay untranslated"
            );
            assert_eq!(
                selector.display_text(&SmolStr::new_static("Event 4")),
                (gpui_kit::SharedString::new_static("Event 4"), false),
                "text that never arrived keeps its source and does not pulse"
            );
            assert!(selector.error.is_none(), "a failure is not an error state");
        });
    }

    /// The model load is not part of the translation's deadline: the deadline starts
    /// only after `prepare` returns.
    #[gpui_kit::test]
    fn the_deadline_should_start_after_the_model_load(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::blocked());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_millis(30);
            });
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        window.run_until_parked();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while translator.prepares.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the model load should have started"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(translator.translates.load(Ordering::SeqCst), 0);

        window.executor().advance_clock(Duration::from_millis(200));
        window.run_until_parked();
        assert!(
            window.update(|_, cx| selector.read(cx).translation.is_some()),
            "a slow model load must not consume the translation deadline"
        );

        translator.prepare_gate.release();
        // Wait for the first chunk to actually start without moving the clock: the
        // deadline begins when `prepare` returns, and only advancing past it after
        // the chunk is running proves the chunk was given the whole timeout.
        let start = std::time::Instant::now();
        while translator.translates.load(Ordering::SeqCst) == 0 {
            window.run_until_parked();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the first chunk should start after the model load"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        window.executor().advance_clock(Duration::from_millis(200));
        settle_translation(window, "the blocked translation to time out", |cx| {
            selector.read(cx).translation.is_none()
        });
        settle_translation(window, "the cancelled translation to stop", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.error.is_none(), "a deadline is not an error state");
            assert!(
                selector.translations.is_empty(),
                "no chunk finished before the deadline"
            );
        });
    }

    /// A translation that blocks past the injected deadline ends, keeps whatever it
    /// already applied (nothing here), and cancels the model work.
    #[gpui_kit::test]
    fn a_blocked_translation_should_stop_at_the_deadline(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_millis(30);
            });
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the blocked translation to time out", |cx| {
            selector.read(cx).translation.is_none()
        });
        settle_translation(window, "the cancelled translation to stop", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });

        assert_eq!(translator.translates.load(Ordering::SeqCst), 1);
        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.translations.is_empty());
            assert!(selector.error.is_none(), "a deadline is not an error state");
        });
    }

    /// A page that changes while its translation runs drops the translation.
    #[gpui_kit::test]
    fn changing_the_page_should_cancel_a_running_translation(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
            });
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.reload(cx)));

        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(
                selector.translation.is_none(),
                "the run should be cancelled"
            );
            assert!(
                selector.translations.is_empty(),
                "the old page's translations should be gone"
            );
        });
        settle_translation(window, "the cancelled translation to stop", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
        settle_until(window, "the reloaded page to arrive", |cx| {
            !selector.read(cx).loading
        });
    }

    /// Confirming or cancelling closes the dialog, so the run is stopped there too;
    /// the title handed back is the original one, never its translation.
    #[gpui_kit::test]
    fn confirm_should_cancel_the_translation_and_pass_the_original_title(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let confirmed = Rc::new(RefCell::new(None));
        let captured = confirmed.clone();
        let event = card(0).event;
        let title = card(0).title;
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    translatable_source(),
                    vec![VenueId::Polymarket],
                    Some(event.clone()),
                    Some(title.clone()),
                    search,
                    Rc::new(
                        move |event: PredictionEventRef,
                              title: SmolStr,
                              _: &mut Window,
                              _: &mut App| {
                            *captured.borrow_mut() = Some((event, title));
                        },
                    ),
                    cx,
                )
            })
        });
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
                selector
                    .translations
                    .insert(title.clone(), SmolStr::new_static("译:the original title"));
            });
        });
        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });
        window.update(|_, cx| {
            assert_eq!(
                selector.read(cx).display_text(&title).0.as_ref(),
                "译:the original title",
                "the card should show the translation"
            );
        });

        window.update(|window, cx| {
            selector.update(cx, |selector, cx| selector.confirm(window, cx));
        });

        assert_eq!(
            confirmed.borrow().clone(),
            Some((event, title.clone())),
            "the dialog confirms the original title, not the one it displays"
        );
        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.translation.is_none());
            assert!(selector.translations.is_empty());
        });
        settle_translation(window, "the cancelled translation to stop", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
    }

    /// Text whose translation is still on its way pulses; once its chunk arrives the
    /// translated text is drawn at full strength.
    #[gpui_kit::test]
    fn waiting_texts_should_pulse_until_their_chunk_arrives(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
            });
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        window.update(|_, cx| {
            let selector = selector.read(cx);
            let (text, pulsing) = selector.display_text(&SmolStr::new_static("Event 0"));
            assert_eq!(text.as_ref(), "Event 0");
            assert!(pulsing, "a pending title should pulse");
            let (_, pulsing) = selector.display_text(&SmolStr::new_static("Yes"));
            assert!(pulsing, "a pending option label should pulse");
            assert!(selector.translation.as_ref().is_some_and(|run| {
                run.pending.contains("Event 0") && run.pending.contains("Yes")
            }));
        });
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window
                .debug_bounds("PREDICTION_TRANSLATE_SPINNER")
                .is_some(),
            "the busy button should draw its spinner"
        );
        assert!(
            window.update(|window, cx| window.simulate_next_frame(cx)) > 0,
            "the waiting text should be drawn with an animation that asks for frames"
        );

        translator.translate_gate.release();
        settle_translation(window, "the translation to finish", |cx| {
            selector.read(cx).translation.is_none()
        });
        window.update(|_, cx| {
            let selector = selector.read(cx);
            let (text, pulsing) = selector.display_text(&SmolStr::new_static("Event 0"));
            assert_eq!(text.as_ref(), "译:Event 0");
            assert!(!pulsing, "applied text stops pulsing");
            let (_, pulsing) = selector.display_text(&SmolStr::new_static("Yes"));
            assert!(!pulsing, "applied option text stops pulsing");
        });
    }

    /// The translate control the frame draws is what starts the run: a real click on
    /// it goes through the same path as a user's.
    #[gpui_kit::test]
    fn clicking_the_ready_button_should_translate_the_page(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        refresh(window);

        assert_eq!(affordance(window, &selector), TranslateAffordance::Ready);
        click(window, "PREDICTION_TRANSLATE");
        settle_translation(window, "the clicked translation to finish", |cx| {
            selector.read(cx).translation.is_none()
        });

        assert_eq!(translator.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(
            translator.translates.load(Ordering::SeqCst),
            2,
            "a full page is still two chunks"
        );
        window.update(|_, cx| {
            assert_eq!(selector.read(cx).translations.len(), 10);
        });
    }

    /// A disabled button has nothing to click: the frame draws it to state why, and
    /// the click on it starts nothing.
    #[gpui_kit::test]
    fn clicking_the_disabled_button_should_start_nothing(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        refresh(window);

        assert!(matches!(
            affordance(window, &selector),
            TranslateAffordance::Disabled(_)
        ));
        click(window, "PREDICTION_TRANSLATE");

        assert_eq!(translator.prepares.load(Ordering::SeqCst), 0);
        assert_eq!(translator.translates.load(Ordering::SeqCst), 0);
        window.update(|_, cx| assert!(selector.read(cx).translation.is_none()));
    }

    /// A disabled button states its reason: hovering it opens the tooltip its frame
    /// drew.
    #[gpui_kit::test]
    fn hovering_the_disabled_button_should_open_its_tooltip(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        refresh(window);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
        );
        hover(window, "PREDICTION_TRANSLATE");

        window
            .debug_bounds("BASE_UI_TOOLTIP")
            .expect("a disabled translate button should state why it cannot be used");
    }

    /// The tooltip keys the button names exist in the embedded locales; a missing one
    /// would draw an empty hint rather than fail the frame.
    #[gpui_kit::test]
    fn the_translate_tooltip_messages_should_exist(cx: &mut TestAppContext) {
        install(cx);
        cx.update(|cx| {
            for key in [
                "prediction_selector.translate_tooltip_model_unset",
                "prediction_selector.translate_tooltip_page_loading",
                "prediction_selector.translate_tooltip_no_content",
                "prediction_selector.translate_tooltip_unavailable",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} should have a message");
            }
        });
    }

    /// A page whose cards carry only whitespace is a page with nothing to translate,
    /// even though it has cards.
    #[gpui_kit::test]
    fn an_all_whitespace_page_should_disable_the_translate_button(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let source = SlowSource {
            events: Arc::new(
                (0..8)
                    .map(|index| {
                        let mut event_card = card(index);
                        let event = event_card.event.clone();
                        event_card.title = SmolStr::new_static("   ");
                        event_card.options = vec![option(&event, "  "), option(&event, "\t")];
                        event_card
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_no_content")
        );
    }

    /// The engine needs a directory to load the model from, so a selected model
    /// without one cannot translate anything either.
    #[gpui_kit::test]
    fn a_selected_model_without_a_directory_should_disable_the_translate_button(
        cx: &mut TestAppContext,
    ) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.translation.model_directory = None;
                        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
                    },
                    cx,
                );
            });
        });
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
        );
    }

    /// The settings are read every frame, so a model selected while the dialog is open
    /// is usable without reopening it, and removing it disables the button again.
    #[gpui_kit::test]
    fn changing_the_model_while_open_should_change_the_affordance(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
        );

        select_translation_model(window);

        assert_eq!(affordance(window, &selector), TranslateAffordance::Ready);

        clear_translation_model(window);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_model_unset")
        );
    }

    /// A page that could not be read disables translation, shows the error state, and
    /// its retry loads the page successfully.
    #[gpui_kit::test]
    fn an_error_page_should_disable_translation_and_retry_should_reload(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::open());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let source = SlowSource {
            events: Arc::new((0..8).map(translatable_card).collect()),
            fail_next_read: Arc::new(Mutex::new(Some(MarketError::ServiceClosed))),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source.clone());
        open_page(window, &selector);

        assert_eq!(
            affordance(window, &selector),
            TranslateAffordance::Disabled("prediction_selector.translate_tooltip_no_content")
        );
        refresh(window);
        window
            .debug_bounds("PREDICTION_ERROR")
            .expect("the failed page should draw its error state");
        window
            .debug_bounds("PREDICTION_RETRY")
            .expect("the failed page should offer a retry");
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);

        click(window, "PREDICTION_RETRY");
        settle_until(window, "the retried page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.error.is_none(), "the retry should clear the error");
            assert!(selector.page.is_some(), "the retry should load a page");
        });
        assert_eq!(source.reads.load(Ordering::SeqCst), 2);
        assert_eq!(affordance(window, &selector), TranslateAffordance::Ready);
        refresh(window);
        assert!(
            window.debug_bounds("PREDICTION_ERROR").is_none(),
            "the error state should be gone after the retry"
        );
        assert_eq!(
            translator.prepares.load(Ordering::SeqCst),
            0,
            "no translation should have been started"
        );
    }

    /// The card a click selects is the one the footer confirms, and the title handed
    /// back is the original one the card translated away from.
    #[gpui_kit::test]
    fn clicking_a_card_then_confirm_should_pass_the_original_title(cx: &mut TestAppContext) {
        install(cx);
        let confirmed = Rc::new(RefCell::new(None));
        let captured = confirmed.clone();
        let event = card(0).event;
        let title = card(0).title;
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    translatable_source(),
                    vec![VenueId::Polymarket],
                    None,
                    None,
                    search,
                    Rc::new(
                        move |event: PredictionEventRef,
                              title: SmolStr,
                              _: &mut Window,
                              _: &mut App| {
                            *captured.borrow_mut() = Some((event, title));
                        },
                    ),
                    cx,
                )
            })
        });
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, cx| {
                selector
                    .translations
                    .insert(title.clone(), SmolStr::new_static("译:the original title"));
                cx.notify();
            });
        });
        refresh(window);

        click(window, "PREDICTION_EVENT_CARD_0");
        refresh(window);
        click(window, "PREDICTION_CONFIRM");

        assert_eq!(
            confirmed.borrow().clone(),
            Some((event, title)),
            "the footer should confirm the card the click selected, with its original title"
        );
    }

    /// Cancelling the dialog stops the run and drops what it produced.
    #[gpui_kit::test]
    fn clicking_cancel_should_stop_the_running_translation(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
            });
        });
        refresh(window);

        click(window, "PREDICTION_TRANSLATE");
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });
        refresh(window);
        click(window, "PREDICTION_CANCEL");

        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.translation.is_none(), "the run should be stopped");
            assert!(selector.translations.is_empty(), "nothing should be kept");
        });
        settle_translation(window, "the translator to observe the cancel", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
    }

    /// A run started for one interface language stops when the interface switches:
    /// translated text must not stay on a page whose button is now hidden.
    #[gpui_kit::test]
    fn changing_the_language_should_cancel_the_running_translation(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
            });
        });
        refresh(window);

        click(window, "PREDICTION_TRANSLATE");
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });

        set_language(window, Language::En);

        window.update(|_, cx| {
            let selector = selector.read(cx);
            assert!(selector.translation.is_none(), "the run should be stopped");
            assert!(selector.translations.is_empty());
        });
        settle_translation(window, "the translator to observe the cancel", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
    }

    /// A blocking translator that panics must end the run at once: no answer will ever
    /// reach the slot, and waiting for the deadline instead would hang the dialog.
    #[gpui_kit::test]
    fn a_panicking_translator_should_end_the_run(cx: &mut TestAppContext) {
        install(cx);
        let translator: Arc<dyn LocalTranslator> = Arc::new(PanickingTranslator);
        cx.update(|cx| prediction_translation::install(cx, translator));
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        // Long enough that only the panic detection can end the run.
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(60);
            });
        });

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        settle_translation(window, "the panicked run to end", |cx| {
            selector.read(cx).translation.is_none()
        });

        window.update(|_, cx| {
            assert!(selector.read(cx).translations.is_empty());
            assert!(
                selector.read(cx).error.is_none(),
                "a panic is not an error state"
            );
        });
    }

    /// Clicking the button again while it is busy starts no second run.
    #[gpui_kit::test]
    fn a_second_click_while_busy_should_start_nothing(cx: &mut TestAppContext) {
        install(cx);
        let translator = Arc::new(FakeTranslator::suspended());
        install_translator(cx, &translator);
        select_translation_model(cx);
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, translatable_source());
        open_page(window, &selector);
        window.update(|_, cx| {
            selector.update(cx, |selector, _| {
                selector.translation_timeout = Duration::from_secs(2);
            });
        });
        refresh(window);

        click(window, "PREDICTION_TRANSLATE");
        settle_translation(window, "the translation to start", |_| {
            translator.translates.load(Ordering::SeqCst) == 1
        });
        let prepares = translator.prepares.load(Ordering::SeqCst);
        let translates = translator.translates.load(Ordering::SeqCst);

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.start_translation(cx)));
        window.run_until_parked();

        assert_eq!(translator.prepares.load(Ordering::SeqCst), prepares);
        assert_eq!(translator.translates.load(Ordering::SeqCst), translates);
        window.update(|_, cx| assert!(selector.read(cx).translation.is_some()));

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.cancel_translation(cx)));
        settle_translation(window, "the cancelled translation to stop", |_| {
            translator.cancelled.load(Ordering::SeqCst) >= 1
        });
    }

    /// One text leaf drawn with the pulse decision the test controls.
    struct PulseHarness {
        pulsing: bool,
        reduce_motion: bool,
    }

    impl Render for PulseHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(pulse_text(
                div().child("text"),
                ElementId::Name(SharedString::new_static("PulseHarnessText")),
                self.pulsing,
                self.reduce_motion,
            ))
        }
    }

    /// The pulse asks for animation frames only while the motion is allowed.
    #[gpui_kit::test]
    fn reduce_motion_should_stop_the_translation_pulse(cx: &mut TestAppContext) {
        install(cx);
        let harness = cx.update(|cx| {
            cx.new(|_| PulseHarness {
                pulsing: true,
                reduce_motion: false,
            })
        });
        let rendered = harness.clone();
        let (_, window) = cx.add_window_view(move |_, _| Layer::new(rendered));
        refresh(window);
        assert!(
            window.update(|window, cx| window.simulate_next_frame(cx)) > 0,
            "a pulse should ask for the next frame"
        );

        window.update(|_, cx| {
            harness.update(cx, |harness, cx| {
                harness.reduce_motion = true;
                cx.notify();
            });
        });
        // The animated frame drawn before the switch already asked for one more
        // frame; let that request go before asking what a reduced-motion frame does.
        refresh(window);
        window.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        refresh(window);

        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "reduce motion should stop the pulse from asking for frames"
        );
    }
}
