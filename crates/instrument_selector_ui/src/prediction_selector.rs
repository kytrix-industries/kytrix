use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use ahash::{AHashMap, AHashSet};
use app_config::settings::Language;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
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
use ui_core::{
    app::{App, AppHandle},
    error::EngineError,
    event::Action,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::{primitive::Border, style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        badge::Badge,
        button::Button,
        dialog::{Dialog, DialogBuilder},
        empty_state::EmptyState,
        icon::IconKind,
        input::{self, InputNode, InputSpec, InputState},
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
        tab_bar::{IndicatorAnimation, TabBar, TabBarOptions, TabItem},
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    theme::{components::ControlSize, role::Role},
};
use widget::{
    error::TranslatedError,
    frame::{RouteId, WeakRouter},
    venue::VenueDisplay,
};

use crate::{
    host::SelectorHost,
    prediction_button::PredictionConfirm,
    prediction_translation::{
        LocalTranslator, TranslatedSegment, TranslationCancel, TranslationChunk,
        TranslationFailure, TranslationSetup, page_has_translatable_text, plan_translation,
    },
    ui::{build_node, remove_node},
};

const PAGE_SIZE: u16 = DEFAULT_PREDICTION_PAGE_SIZE;
const GRID_COLUMNS: usize = 4;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const BAR_WIDTH: f32 = 96.0;
const DIALOG_WIDTH: f32 = 960.0;
const DIALOG_HEIGHT: f32 = 680.0;
const TRANSLATION_TIMEOUT: Duration = Duration::from_secs(20);
const KEY_BASE: &str = "prediction-selector";
const PANEL_ID: &str = "PREDICTION_EVENT_SELECTOR";

type TranslationOutcome = Result<Vec<TranslatedSegment>, TranslationFailure>;

pub trait PredictionEventSource: Clone + Send + Sync + 'static {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionSelectorError {
    Market(MarketError),
    Interrupted,
}

impl TranslatedError for PredictionSelectorError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Market(error) => error.translated_message(translate),
            Self::Interrupted => t_smol!(translate, "error.prediction_selector.interrupted"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranslateAffordance {
    Hidden,
    Disabled(&'static str),
    Busy,
    Ready,
}

struct TranslationRun {
    generation: u64,
    pending: AHashSet<SmolStr>,
    cancel: TranslationCancel,
}

enum TranslationEvent {
    Prepared {
        generation: u64,
        outcome: Result<(), TranslationFailure>,
    },
    Chunk {
        generation: u64,
        chunk: TranslationChunk,
        outcome: TranslationOutcome,
    },
    TimedOut {
        generation: u64,
    },
}

struct LoadOutcome {
    generation: u64,
    result: Result<PredictionEventsPage, PredictionSelectorError>,
}

pub struct PredictionEventSelector<S: PredictionEventSource> {
    inner: RefCell<SelectorState<S>>,
}

struct SelectorState<S: PredictionEventSource> {
    weak: Weak<PredictionEventSelector<S>>,
    host: SelectorHost,
    window: WindowId,
    key: SmolStr,
    scope: SignalScope,
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
    search_input_text: SmolStr,
    search_input: Signal<InputState>,
    search_placeholder: Signal<SmolStr>,
    search_node: Option<InputNode>,
    load_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LoadOutcome>>,
    translations: AHashMap<SmolStr, SmolStr>,
    translation: Option<TranslationRun>,
    translation_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TranslationEvent>>,
    translation_generation: u64,
    translation_timeout: Duration,
    language: LanguageCode,
    settings_language: Language,
    revision: u64,
    drawn_revision: u64,
    dialog: Option<Dialog>,
    select: Option<Select>,
    tabs: Option<TabBar>,
    tabs_holder: Option<NodeId>,
    tabs_node: Option<NodeId>,
    tabs_count: usize,
    body_node: Option<NodeId>,
    body_children: Vec<NodeId>,
    card_actions: Vec<Action>,
    page_text: Signal<SmolStr>,
    translate_visible: Signal<bool>,
    translate_button: Option<Button>,
    spinner: Option<Spinner>,
    retry_button: Option<Button>,
    translate_tooltip: Option<Tooltip>,
    previous: Option<Button>,
    next: Option<Button>,
    closed: Rc<Cell<bool>>,
    route_id: Option<RouteId>,
    router: WeakRouter,
    on_confirm: PredictionConfirm,
}

impl PredictionEventSelector<MarketCapability> {
    pub fn show(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        on_confirm: PredictionConfirm,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
    ) -> bool {
        let Some(connector) = host.services().market_participants() else {
            return false;
        };
        let venues = loaded_prediction_venues(connector.venues());
        if venues.is_empty() {
            return false;
        }
        let source = connector.market();
        PredictionEventSelector::<MarketCapability>::open(
            current,
            current_title,
            source,
            venues,
            on_confirm,
            host,
            app,
            window,
        )
    }
}

impl<S: PredictionEventSource> PredictionEventSelector<S> {
    pub fn show_with_source(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        source: S,
        on_confirm: PredictionConfirm,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
    ) -> bool {
        let Some(connector) = host.services().market_participants() else {
            return false;
        };
        let venues = loaded_prediction_venues(connector.venues());
        if venues.is_empty() {
            return false;
        }
        Self::open(
            current,
            current_title,
            source,
            venues,
            on_confirm,
            host,
            app,
            window,
        )
    }

    fn open(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        source: S,
        venues: Vec<VenueId>,
        on_confirm: PredictionConfirm,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
    ) -> bool {
        let selector =
            Rc::new_cyclic(
                |weak: &Weak<PredictionEventSelector<S>>| PredictionEventSelector {
                    inner: RefCell::new(SelectorState::new(
                        weak.clone(),
                        host,
                        window,
                        source,
                        venues,
                        current,
                        current_title,
                        on_confirm,
                        app,
                    )),
                },
            );
        let dialog = SelectorState::build_dialog(&selector, app);
        selector.inner.borrow_mut().dialog = Some(dialog.clone());
        let route_id = SelectorState::register_route(&selector, host);
        selector.inner.borrow_mut().route_id = Some(route_id);

        if dialog.open(app, window).is_err() {
            selector.close(app);
            return false;
        }
        selector.inner.borrow_mut().reload(app);
        let _ = app.request_repaint(window);
        true
    }

    fn sync(&self, app: &mut App) {
        self.inner.borrow_mut().sync(app);
    }

    fn close(&self, app: &mut App) {
        self.inner.borrow_mut().finish(app);
    }
}

impl<S: PredictionEventSource> SelectorState<S> {
    fn new(
        weak: Weak<PredictionEventSelector<S>>,
        host: &SelectorHost,
        window: WindowId,
        source: S,
        venues: Vec<VenueId>,
        selected: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        on_confirm: PredictionConfirm,
        app: &mut App,
    ) -> Self {
        let selected = selected.filter(|event| venues.contains(&event.route.venue));
        let selected_title = selected.as_ref().and(current_title);
        let venue = selected
            .as_ref()
            .map(|event| event.route.venue)
            .unwrap_or(venues[0]);
        let scope = app.signal_scope();
        let key = format_smolstr!("{KEY_BASE}@{}", scope.index());
        let search_input = app.signal_scoped(scope, InputState::new(""));
        let search_placeholder = app.signal_scoped(
            scope,
            t_smol!(host.translate(), "prediction_selector.search_placeholder"),
        );
        let page_text = app.signal_scoped(scope, SmolStr::default());
        let translate_visible = app.signal_scoped(
            scope,
            host.config_ref(app, |config| config.language) != Language::En,
        );
        Self {
            weak,
            host: host.clone(),
            window,
            key,
            scope,
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
            search_input_text: SmolStr::default(),
            search_input,
            search_placeholder,
            search_node: None,
            load_rx: None,
            translations: AHashMap::with_capacity(48),
            translation: None,
            translation_rx: None,
            translation_generation: 0,
            translation_timeout: TRANSLATION_TIMEOUT,
            language: host.language(app),
            settings_language: host.config_ref(app, |config| config.language),
            revision: 0,
            drawn_revision: u64::MAX,
            dialog: None,
            select: None,
            tabs: None,
            tabs_holder: None,
            tabs_node: None,
            tabs_count: 0,
            body_node: None,
            body_children: Vec::new(),
            card_actions: Vec::new(),
            page_text,
            translate_visible,
            translate_button: None,
            spinner: None,
            retry_button: None,
            translate_tooltip: None,
            previous: None,
            next: None,
            closed: Rc::new(Cell::new(false)),
            route_id: None,
            router: host.router().downgrade(),
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

    fn reload(&mut self, app: &mut App) {
        self.cancel_translation(app);
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.bump_revision();
        let source = self.source.clone();
        let request = self.request();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.load_rx = Some(rx);
        let repaint = app.handle();
        let window = self.window;
        app.handle().spawn(async move {
            let result = source
                .page(request)
                .await
                .map_err(PredictionSelectorError::Market);
            if tx.send(LoadOutcome { generation, result }).is_ok() {
                repaint.send(move |app| {
                    let _ = app.request_repaint(window);
                });
            }
        });
    }

    fn apply_load(&mut self, outcome: LoadOutcome, app: &mut App) {
        if outcome.generation != self.generation {
            return;
        }
        self.loading = false;
        match outcome.result {
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
        self.bump_revision();
        let _ = app.request_repaint(self.window);
    }

    fn search_changed(&mut self, app: &mut App, query: &str) {
        if self.search == query {
            return;
        }
        self.cancel_translation(app);
        self.search = query.to_owned();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.navigation_backup = None;
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.bump_revision();
        let weak = Weak::clone(&self.weak);
        let _ = app.after(self.window, SEARCH_DEBOUNCE, move |app| {
            if let Some(selector) = weak.upgrade() {
                selector.inner.borrow_mut().finish_search(generation, app);
            }
        });
    }

    fn finish_search(&mut self, generation: u64, app: &mut App) {
        if self.search_generation != generation {
            return;
        }
        self.reload(app);
    }

    fn select_venue(&mut self, app: &mut App, venue: VenueId) {
        if self.venue == venue {
            return;
        }
        self.venue = venue;
        self.selected = None;
        self.selected_title = None;
        self.categories.clear();
        self.category = None;
        self.search.clear();
        self.search_input_text = SmolStr::default();
        if let Some(node) = self.search_node.as_ref() {
            let _ = node.update_value(app, "");
        }
        if self.tabs_holder.is_some() {
            self.tabs_count = 0;
            self.tabs = None;
        }
        self.navigation_backup = None;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.page = None;
        self.bump_revision();
        self.reload(app);
    }

    fn select_category(&mut self, app: &mut App, index: usize) {
        let next = index
            .checked_sub(1)
            .and_then(|index| self.categories.get(index))
            .map(|category| category.key.clone());
        if self.category == next {
            return;
        }
        self.navigation_backup = None;
        self.category = next;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.reload(app);
    }

    fn next_page(&mut self, app: &mut App) {
        if self.loading {
            return;
        }
        let Some(next) = self.page.as_ref().and_then(|page| page.next_cursor.clone()) else {
            return;
        };
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.push(self.cursor.clone());
        self.cursor = Some(next);
        self.page_number = self.page_number.saturating_add(1);
        self.reload(app);
    }

    fn previous_page(&mut self, app: &mut App) {
        if self.loading {
            return;
        }
        let Some(previous) = self.cursor_stack.last().cloned() else {
            return;
        };
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.pop();
        self.cursor = previous;
        self.page_number = self.page_number.saturating_sub(1).max(1);
        self.reload(app);
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn translate_affordance(&self, app: &App) -> TranslateAffordance {
        let config = self.host.config_ref(app, |config| config.clone());
        if config.language == Language::En {
            return TranslateAffordance::Hidden;
        }
        if self.host.translator().is_none() {
            return TranslateAffordance::Disabled(
                "prediction_selector.translate_tooltip_unavailable",
            );
        }
        if self.translation.is_some() {
            return TranslateAffordance::Busy;
        }
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

    fn start_translation(&mut self, app: &mut App) {
        if !matches!(self.translate_affordance(app), TranslateAffordance::Ready) {
            return;
        }
        let Some(translator) = self.host.translator() else {
            return;
        };
        let config = self.host.config_ref(app, |config| config.clone());
        let plan = self.page_translation_plan();
        if plan.is_empty() {
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
        self.bump_revision();

        let setup = TranslationSetup {
            directory: config.translation.model_directory.clone(),
            model: config.translation.selected_model,
        };
        let timeout = self.translation_timeout;
        let target = config.language;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.translation_rx = Some(rx);
        let repaint = app.handle();
        let runtime = self.host.services().runtime.clone();
        let window = self.window;
        app.handle().spawn(async move {
            Self::supervise_translation(
                runtime, translator, setup, target, plan, generation, cancel, timeout, tx, repaint,
                window,
            )
            .await;
        });
    }

    async fn supervise_translation(
        runtime: tokio::runtime::Handle,
        translator: Arc<dyn LocalTranslator>,
        setup: TranslationSetup,
        target: Language,
        plan: Vec<TranslationChunk>,
        generation: u64,
        cancel: TranslationCancel,
        timeout: Duration,
        tx: tokio::sync::mpsc::UnboundedSender<TranslationEvent>,
        repaint: AppHandle,
        window: WindowId,
    ) {
        let notify = {
            let repaint = repaint.clone();
            move || {
                repaint.send(move |app| {
                    let _ = app.request_repaint(window);
                });
            }
        };
        if cancel.is_cancelled() {
            return;
        }
        let prepared = runtime
            .spawn_blocking({
                let translator = Arc::clone(&translator);
                let setup = setup.clone();
                let cancel = cancel.clone();
                move || translator.prepare(&setup, cancel)
            })
            .await;
        let prepared = match prepared {
            Ok(outcome) => outcome,
            Err(error) => {
                log::warn!("the translation model load task failed: {error}");
                Err(TranslationFailure::Engine)
            }
        };
        let failed = prepared.is_err();
        if let Err(error) = &prepared {
            log::warn!("preparing the translation model failed: {error:?}");
        }
        if tx
            .send(TranslationEvent::Prepared {
                generation,
                outcome: prepared,
            })
            .is_err()
        {
            return;
        }
        notify();
        if failed || cancel.is_cancelled() {
            return;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        for chunk in plan {
            if cancel.is_cancelled() {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let _ = tx.send(TranslationEvent::TimedOut { generation });
                notify();
                return;
            }
            let join = runtime.spawn_blocking({
                let translator = Arc::clone(&translator);
                let setup = setup.clone();
                let cancel = cancel.clone();
                let segments = chunk.segments().to_vec();
                move || translator.translate(&setup, target, segments, cancel)
            });
            let outcome = match tokio::time::timeout(remaining, join).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    log::warn!("the translation worker failed: {error}");
                    let _ = tx.send(TranslationEvent::Chunk {
                        generation,
                        chunk,
                        outcome: Err(TranslationFailure::Engine),
                    });
                    notify();
                    return;
                }
                Err(_elapsed) => {
                    cancel.cancel();
                    let _ = tx.send(TranslationEvent::TimedOut { generation });
                    notify();
                    return;
                }
            };
            let failed = outcome.is_err();
            if tx
                .send(TranslationEvent::Chunk {
                    generation,
                    chunk,
                    outcome,
                })
                .is_err()
            {
                return;
            }
            notify();
            if failed {
                return;
            }
        }
    }

    fn apply_translation_event(&mut self, event: TranslationEvent, app: &mut App) {
        match event {
            TranslationEvent::Prepared {
                generation,
                outcome,
            } => {
                if self
                    .translation
                    .as_ref()
                    .is_none_or(|run| run.generation != generation)
                {
                    return;
                }
                if outcome.is_err() {
                    self.translation = None;
                    self.bump_revision();
                    let _ = app.request_repaint(self.window);
                }
            }
            TranslationEvent::Chunk {
                generation,
                chunk,
                outcome,
            } => {
                if self
                    .translation
                    .as_ref()
                    .is_none_or(|run| run.generation != generation)
                {
                    return;
                }
                if let Err(failure) = &outcome {
                    log::warn!(
                        "translating {} texts failed: {failure:?}",
                        chunk.segments().len()
                    );
                    if let Some(run) = self.translation.take() {
                        run.cancel.cancel();
                    }
                    self.bump_revision();
                    let _ = app.request_repaint(self.window);
                    return;
                }
                let Ok(translated) = outcome else {
                    return;
                };
                let mut by_id: AHashMap<SmolStr, SmolStr> =
                    AHashMap::with_capacity(translated.len());
                for segment in translated {
                    by_id.insert(segment.id, segment.text);
                }
                let Some(run) = self.translation.as_mut() else {
                    return;
                };
                for segment in chunk.segments() {
                    if let Some(text) = by_id.get(&segment.id) {
                        self.translations.insert(segment.text.clone(), text.clone());
                        run.pending.remove(&segment.text);
                    }
                }
                self.bump_revision();
                let _ = app.request_repaint(self.window);
            }
            TranslationEvent::TimedOut { generation } => {
                if self
                    .translation
                    .as_ref()
                    .is_none_or(|run| run.generation != generation)
                {
                    return;
                }
                log::warn!("the prediction translation deadline passed");
                if let Some(run) = self.translation.take() {
                    run.cancel.cancel();
                }
                self.bump_revision();
                let _ = app.request_repaint(self.window);
            }
        }
    }

    fn cancel_translation(&mut self, app: &mut App) {
        self.translation_generation = self.translation_generation.wrapping_add(1);
        if let Some(run) = self.translation.take() {
            run.cancel.cancel();
        }
        self.translation_rx = None;
        if !self.translations.is_empty() {
            self.translations.clear();
            self.bump_revision();
            let _ = app.request_repaint(self.window);
        }
    }

    fn page_translation_plan(&self) -> Vec<TranslationChunk> {
        let Some(page) = &self.page else {
            return Vec::new();
        };
        let cards = &page.events[..page.events.len().min(usize::from(PAGE_SIZE))];
        plan_translation(cards, &self.translations)
    }

    fn display_text(&self, source: &SmolStr) -> (SmolStr, bool) {
        let text = self
            .translations
            .get(source)
            .cloned()
            .unwrap_or_else(|| source.clone());
        let pulsing = self
            .translation
            .as_ref()
            .is_some_and(|run| run.pending.contains(source));
        (text, pulsing)
    }

    fn confirm(&mut self, app: &mut App) {
        self.cancel_translation(app);
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let Some(title) = self.selected_title.clone() else {
            return;
        };
        let on_confirm = self.on_confirm.clone();
        on_confirm(selected, title, app, self.window);
    }

    fn build_dialog(selector: &Rc<PredictionEventSelector<S>>, app: &mut App) -> Dialog {
        let (theme, translate, closed, key) = {
            let inner = selector.inner.borrow();
            (
                inner.host.theme().clone(),
                inner.host.translate().clone(),
                Rc::clone(&inner.closed),
                inner.key.clone(),
            )
        };
        let weak = Rc::downgrade(selector);
        let content_weak = Weak::clone(&weak);
        let confirm_weak = Weak::clone(&weak);
        let cancel_weak = Weak::clone(&weak);
        let confirm_label = t_smol!(translate, "common.confirm");
        let cancel_label = t_smol!(translate, "common.cancel");
        let title = t_smol!(translate, "widget.names.prediction_market");

        DialogBuilder::new(title)
            .header(false)
            .close_button(false)
            .content(move |app, window, body| {
                let Some(selector) = content_weak.upgrade() else {
                    return Ok(());
                };
                selector.inner.borrow_mut().build_content(app, window, body)
            })
            .confirm(confirm_label, move |app| {
                if let Some(selector) = confirm_weak.upgrade() {
                    selector.inner.borrow_mut().confirm(app);
                }
            })
            .cancel(cancel_label, move |app| {
                if let Some(selector) = cancel_weak.upgrade() {
                    selector.inner.borrow_mut().cancel_translation(app);
                }
            })
            .on_close(move |_close, _app| closed.set(true))
            .build(app, theme, key)
    }

    fn register_route(selector: &Rc<PredictionEventSelector<S>>, host: &SelectorHost) -> RouteId {
        let held = Rc::clone(selector);
        host.router().register(move |app| {
            held.sync(app);
            Ok(())
        })
    }

    fn build_content(
        &mut self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let theme = self.host.theme().clone();
        let translate = self.host.translate().clone();
        let text_size = theme.tokens().small.font_size;

        let root = build_node(app, window, body, |tree, body| {
            NodeBuilder::boxed()
                .id(PANEL_ID)
                .style(
                    StyleBuilder::new()
                        .width(px(DIALOG_WIDTH))
                        .height(px(DIALOG_HEIGHT))
                        .flex_col()
                        .gap(px(8.0))
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;

        let header = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .build(),
                )
                .build(tree, root)
        })?;
        let select_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, header)
        })?;
        let search_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .build(tree, header)
        })?;

        let select = Select::new(
            app,
            theme.clone(),
            format_smolstr!("{}:venue", self.key),
            SelectOptions::default(),
        )
        .on_change({
            let weak = Weak::clone(&self.weak);
            move |app, chosen| {
                let Some(chosen) = chosen.first() else {
                    return;
                };
                let Ok(venue) = VenueId::from_name(chosen) else {
                    return;
                };
                if let Some(selector) = weak.upgrade() {
                    selector.inner.borrow_mut().select_venue(app, venue);
                }
            }
        });
        if let Err(error) = select.set_items(
            app,
            self.venues
                .iter()
                .map(|venue| SelectItem::new(venue.as_str(), venue.display_string(&translate))),
        ) {
            log::warn!("building the prediction venue items failed: {error}");
        }
        select.set_chosen(app, [self.venue.as_str()], false);
        select.mount(app, window, select_holder)?;
        self.select = Some(select);

        let spec = Arc::new(InputSpec {
            name: format_smolstr!("{}:search", self.key),
            label: None,
            placeholder: SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Small,
            submit: None,
        });
        let search = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                search_holder,
                spec,
                &theme,
                &self.search_input,
                &self.search_placeholder,
            )
        };
        search.install(app)?;
        self.search_node = Some(search);

        let tab_row = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .build(),
                )
                .build(tree, root)
        })?;
        let tab_holder = build_node(app, window, tab_row, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{PANEL_ID}:tabs"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, parent)
        })?;
        self.tabs_holder = Some(tab_holder);
        let translate_holder = build_node(app, window, tab_row, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{PANEL_ID}:translate"))
                .style(StyleBuilder::new().flex_none().build())
                .bind_style({
                    let visible = self.translate_visible;
                    move |cx| StylePatch {
                        visible: Some(*cx.read(&visible)),
                        ..StylePatch::default()
                    }
                })
                .build(tree, parent)
        })?;
        let translate_button = Button::new(
            app,
            theme.clone(),
            format_smolstr!("{}:translate", self.key),
        )
        .secondary()
        .with_unified_size(UnifiedSize::Medium)
        .icon(IconKind::Translate)
        .a11y_label(t_smol!(translate, "prediction_selector.translate"))
        .on_click({
            let weak = Weak::clone(&self.weak);
            move |ctx| {
                if let Some(selector) = weak.upgrade() {
                    selector.inner.borrow_mut().start_translation(ctx.app_mut());
                }
            }
        });
        translate_button.build(app, window, translate_holder)?;
        let tooltip = Tooltip::new(
            app,
            theme.clone(),
            format_smolstr!("{}:translate-tip", self.key),
            TooltipOptions::default(),
        );
        tooltip.set_content(Some(TooltipContent::new(t_smol!(
            translate,
            "prediction_selector.translate"
        ))));
        if let Some(node) = translate_button.node() {
            tooltip.attach(app, window, node);
        }
        self.translate_tooltip = Some(tooltip);
        self.translate_button = Some(translate_button);

        let body_panel = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{PANEL_ID}:body"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .overflow_hidden()
                        .padding(px(8.0))
                        .build(),
                )
                .bind_style({
                    let theme = theme.clone();
                    move |cx| StylePatch {
                        background: Some(theme.role(cx, Role::SurfaceSunken)),
                        border: Some(Border {
                            width: px(1.0),
                            color: theme.role(cx, Role::LineDivider),
                        }),
                        ..StylePatch::default()
                    }
                })
                .build(tree, root)
        })?;
        self.body_node = Some(body_panel);

        let footer = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .build(),
                )
                .build(tree, root)
        })?;
        let previous = Button::new(app, theme.clone(), format_smolstr!("{}:previous", self.key))
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .label(t_smol!(translate, "prediction_selector.previous"))
            .on_click({
                let weak = Weak::clone(&self.weak);
                move |ctx| {
                    if let Some(selector) = weak.upgrade() {
                        selector.inner.borrow_mut().previous_page(ctx.app_mut());
                    }
                }
            });
        previous.build(app, window, footer)?;
        self.previous = Some(previous);
        let page_text = self.page_text;
        build_node(app, window, footer, |tree, footer| {
            NodeBuilder::text(SmolStr::default(), TextStyle::new(text_size))
                .style(StyleBuilder::new().flex_none().build())
                .bind_text(move |cx| cx.read(&page_text).clone())
                .bind_style({
                    let theme = theme.clone();
                    move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgMuted)),
                        ..StylePatch::default()
                    }
                })
                .build(tree, footer)
        })?;
        let next = Button::new(app, theme, format_smolstr!("{}:next", self.key))
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .label(t_smol!(translate, "prediction_selector.next"))
            .on_click({
                let weak = Weak::clone(&self.weak);
                move |ctx| {
                    if let Some(selector) = weak.upgrade() {
                        selector.inner.borrow_mut().next_page(ctx.app_mut());
                    }
                }
            });
        next.build(app, window, footer)?;
        self.next = Some(next);

        self.rebuild_tabs(app, window, tab_holder);
        self.write_page_text(app);
        Ok(())
    }

    fn rebuild_tabs(&mut self, app: &mut App, window: WindowId, holder: NodeId) {
        if self.tabs_count == self.categories.len() {
            return;
        }
        self.tabs_count = self.categories.len();
        if let Some(node) = self.tabs_node.take() {
            remove_node(app, window, node);
        }
        if let Some(tabs) = self.tabs.take() {
            tabs.release(app);
        }
        let translate = self.host.translate().clone();
        let mut items = Vec::with_capacity(self.categories.len() + 1);
        items.push(TabItem::new(t_smol!(translate, "prediction_selector.all")));
        items.extend(
            self.categories
                .iter()
                .map(|category| TabItem::new(category.label.clone())),
        );
        let tabs = TabBar::new(
            app,
            self.host.theme().clone(),
            format_smolstr!("{}:categories", self.key),
            items,
            TabBarOptions {
                size: UnifiedSize::Small,
                indicator: IndicatorAnimation::Fade,
                closable: false,
            },
        )
        .on_change({
            let weak = Weak::clone(&self.weak);
            move |index, app| {
                if let Some(selector) = weak.upgrade() {
                    selector.inner.borrow_mut().select_category(app, index);
                }
            }
        });
        let selected = self
            .category
            .as_ref()
            .and_then(|category| {
                self.categories
                    .iter()
                    .position(|known| &known.key == category)
            })
            .map_or(0, |index| index + 1);
        tabs.set_selected(app, selected);
        match tabs.mount(app, window, holder) {
            Ok(node) => {
                self.tabs_node = Some(node);
                self.tabs = Some(tabs);
            }
            Err(error) => log::warn!("building the category tabs failed: {error}"),
        }
    }

    fn write_page_text(&self, app: &mut App) {
        let text = t_smol!(
            self.host.translate(),
            "prediction_selector.page",
            page = self.page_number
        );
        if *app.read(&self.page_text) != text {
            let _ = app.write(&self.page_text, text);
        }
    }

    fn sync(&mut self, app: &mut App) {
        if self.closed.get() {
            self.finish(app);
            return;
        }
        let open = self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app) || dialog.is_closing(app));
        if !open {
            self.closed.set(true);
            self.finish(app);
            return;
        }

        self.follow_language(app);
        let mut changed = false;
        if let Some(search) = self.search_node.as_ref() {
            changed |= search.sync(app);
        }
        self.follow_search(app);
        self.drain_loads(app);
        self.drain_translations(app);
        self.update_controls(app);
        self.rebuild_body(app);
        self.write_page_text(app);

        if let Some(select) = self.select.as_ref() {
            changed |= select.sync(app).unwrap_or(false);
        }
        if let Some(tabs) = self.tabs.as_ref() {
            changed |= tabs.sync(app).unwrap_or(false);
        }
        if let Some(spinner) = self.spinner.as_ref() {
            changed |= spinner.sync(app).unwrap_or(false);
        }
        if let Some(tooltip) = self.translate_tooltip.as_ref() {
            changed |= tooltip.sync(app);
        }
        changed |= self
            .dialog
            .as_ref()
            .and_then(|dialog| dialog.sync(app).ok())
            .unwrap_or(false);
        if changed {
            let _ = app.request_repaint(self.window);
        }
        if self.closed.get() {
            self.finish(app);
        }
    }

    fn follow_language(&mut self, app: &mut App) {
        let language = self.host.language(app);
        let settings_language = self.host.config_ref(app, |config| config.language);
        if language == self.language && settings_language == self.settings_language {
            return;
        }
        self.language = language;
        self.settings_language = settings_language;
        self.cancel_translation(app);
        self.bump_revision();
        let _ = app.request_repaint(self.window);
    }

    fn follow_search(&mut self, app: &mut App) {
        let query = SmolStr::new(app.read(&self.search_input).text().trim());
        if query == self.search_input_text {
            return;
        }
        self.search_input_text = query.clone();
        self.search_changed(app, &query);
    }

    fn drain_loads(&mut self, app: &mut App) {
        let received: Vec<LoadOutcome> = self
            .load_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        for outcome in received {
            self.apply_load(outcome, app);
        }
    }

    fn drain_translations(&mut self, app: &mut App) {
        let received: Vec<TranslationEvent> = self
            .translation_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        for event in received {
            self.apply_translation_event(event, app);
        }
    }

    fn update_controls(&mut self, app: &mut App) {
        let affordance = self.translate_affordance(app);
        let translate = self.host.translate().clone();
        let visible = !matches!(affordance, TranslateAffordance::Hidden);
        let visible_signal = self.translate_visible;
        if *app.read(&visible_signal) != visible {
            let _ = app.write(&visible_signal, visible);
        }
        if let Some(button) = self.translate_button.as_ref() {
            let disabled = !matches!(affordance, TranslateAffordance::Ready);
            let _ = button.set_disabled(app, disabled);
            let _ = button.set_selected(app, matches!(affordance, TranslateAffordance::Busy));
        }
        if let Some(tooltip) = self.translate_tooltip.as_ref() {
            let content = match affordance {
                TranslateAffordance::Hidden => None,
                TranslateAffordance::Busy => Some(TooltipContent::new(t_smol!(
                    translate,
                    "prediction_selector.translating"
                ))),
                TranslateAffordance::Disabled(key) => {
                    Some(TooltipContent::new(t_smol!(translate, key)))
                }
                TranslateAffordance::Ready => Some(TooltipContent::new(t_smol!(
                    translate,
                    "prediction_selector.translate"
                ))),
            };
            let _ = tooltip.set_content(content);
        }
        let page_loading = self.loading;
        if let Some(previous) = self.previous.as_ref() {
            let _ = previous.set_disabled(app, self.cursor_stack.is_empty() || page_loading);
        }
        if let Some(next) = self.next.as_ref() {
            let disabled = self
                .page
                .as_ref()
                .is_none_or(|page| page.next_cursor.is_none() || page_loading);
            let _ = next.set_disabled(app, disabled);
        }
        let can_confirm = self.selected.is_some() && self.selected_title.is_some();
        if let Some(dialog) = self.dialog.as_ref()
            && let Some(button) = dialog.confirm_button()
        {
            let _ = button.set_disabled(app, !can_confirm);
        }
    }

    fn rebuild_body(&mut self, app: &mut App) {
        if self.drawn_revision == self.revision {
            return;
        }
        self.drawn_revision = self.revision;
        let Some(body) = self.body_node else {
            return;
        };
        for child in self.body_children.drain(..) {
            remove_node(app, self.window, child);
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.release(app);
        }
        if let Some(retry) = self.retry_button.take() {
            retry.release(app);
        }
        let theme = self.host.theme().clone();
        let translate = self.host.translate().clone();
        let text_size = theme.tokens().small.font_size;

        if self.loading {
            let holder = build_node(app, self.window, body, |tree, body| {
                NodeBuilder::boxed()
                    .id(format_smolstr!("{PANEL_ID}:spinner"))
                    .style(
                        StyleBuilder::new()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .build(),
                    )
                    .build(tree, body)
            });
            match holder {
                Ok(holder) => {
                    self.body_children.push(holder);
                    let spinner = Spinner::new(app, theme, format_smolstr!("{}:spinner", self.key));
                    if let Err(error) = spinner.build(app, self.window, holder) {
                        log::warn!("building the prediction spinner failed: {error}");
                    }
                    self.spinner = Some(spinner);
                }
                Err(error) => log::warn!("building the prediction spinner row failed: {error}"),
            }
            return;
        }

        if let Some(error) = self.error.as_ref() {
            let message = error.translated_message(&translate);
            let holder = build_node(app, self.window, body, |tree, body| {
                NodeBuilder::boxed()
                    .id(format_smolstr!("{PANEL_ID}:error"))
                    .style(
                        StyleBuilder::new()
                            .flex_col()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .gap(px(8.0))
                            .build(),
                    )
                    .build(tree, body)
            });
            let holder = match holder {
                Ok(holder) => holder,
                Err(error) => {
                    log::warn!("building the prediction error row failed: {error}");
                    return;
                }
            };
            self.body_children.push(holder);
            if let Err(error) = build_node(app, self.window, holder, |tree, holder| {
                EmptyState::new(theme.clone(), format_smolstr!("{}:error", self.key))
                    .text(message)
                    .build(tree, holder)
            }) {
                log::warn!("building the prediction error message failed: {error}");
            }
            let retry = Button::new(app, theme, format_smolstr!("{}:retry", self.key))
                .secondary()
                .with_unified_size(UnifiedSize::Small)
                .label(t_smol!(translate, "common.retry"))
                .on_click({
                    let weak = Weak::clone(&self.weak);
                    move |ctx| {
                        if let Some(selector) = weak.upgrade() {
                            selector.inner.borrow_mut().reload(ctx.app_mut());
                        }
                    }
                });
            if let Err(error) = retry.build(app, self.window, holder) {
                log::warn!("building the prediction retry control failed: {error}");
            }
            self.retry_button = Some(retry);
            return;
        }

        let Some(page) = self.page.as_ref() else {
            return;
        };
        if page.events.is_empty() {
            let empty = build_node(app, self.window, body, |tree, body| {
                NodeBuilder::boxed()
                    .id(format_smolstr!("{PANEL_ID}:empty"))
                    .style(
                        StyleBuilder::new()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .build(),
                    )
                    .build(tree, body)
            });
            if let Ok(empty) = empty {
                self.body_children.push(empty);
                if let Err(error) = build_node(app, self.window, empty, |tree, empty| {
                    EmptyState::new(theme, format_smolstr!("{}:empty", self.key))
                        .text(t_smol!(translate, "prediction_selector.no_matching_events"))
                        .build(tree, empty)
                }) {
                    log::warn!("building the prediction empty state failed: {error}");
                }
            }
            return;
        }

        let cards: Vec<PredictionEventCard> = page
            .events
            .iter()
            .take(usize::from(PAGE_SIZE))
            .cloned()
            .collect();
        let rows: Vec<Vec<PredictionEventCard>> = cards
            .chunks(GRID_COLUMNS)
            .map(<[PredictionEventCard]>::to_vec)
            .collect();
        for (row_index, row_cards) in rows.into_iter().enumerate() {
            let row = build_node(app, self.window, body, |tree, body| {
                NodeBuilder::boxed()
                    .id(format_smolstr!("{PANEL_ID}:row:{row_index}"))
                    .style(
                        StyleBuilder::new()
                            .flex_row()
                            .flex_1()
                            .gap(px(8.0))
                            .min_height_0()
                            .build(),
                    )
                    .build(tree, body)
            });
            let Ok(row) = row else {
                log::warn!("building a prediction card row failed");
                continue;
            };
            self.body_children.push(row);
            for (column, card) in row_cards.iter().enumerate() {
                self.build_card(app, row, row_index * GRID_COLUMNS + column, card, text_size);
            }
        }
    }

    fn build_card(
        &mut self,
        app: &mut App,
        parent: NodeId,
        index: usize,
        card: &PredictionEventCard,
        text_size: LogicalPx,
    ) {
        let theme = self.host.theme().clone();
        let translate = self.host.translate().clone();
        let selected = self.selected.as_ref() == Some(&card.event);
        let reduce_motion = self.host.reduce_motion(app);
        let action = Action::Custom(format_smolstr!("{}:select:{index}", self.key));
        if !self.card_actions.contains(&action) {
            self.card_actions.push(action.clone());
        }
        let event = card.event.clone();
        let title = card.title.clone();
        let weak = Weak::clone(&self.weak);
        app.on_action(action.clone(), move |ctx| {
            if let Some(selector) = weak.upgrade() {
                selector.inner.borrow_mut().select_card(
                    ctx.app_mut(),
                    event.clone(),
                    title.clone(),
                );
            }
        });
        let (title_text, title_pending) = self.display_text(&card.title);
        let card_theme = theme.clone();
        let node = build_node(app, self.window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{PANEL_ID}:card:{index}"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .gap(px(6.0))
                        .padding(px(10.0))
                        .overflow_hidden()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    background: Some(card_theme.role(cx, Role::SurfaceRaised)),
                    border: Some(Border {
                        width: px(1.0),
                        color: if selected {
                            card_theme.role(cx, Role::BrandBase)
                        } else {
                            card_theme.role(cx, Role::LineDivider)
                        },
                    }),
                    ..StylePatch::default()
                })
                .on_click(action)
                .build(tree, parent)
        });
        let Ok(node) = node else {
            log::warn!("building a prediction card failed");
            return;
        };

        let title_theme = theme.clone();
        let dimmed = title_pending && !reduce_motion;
        let title_node = build_node(app, self.window, node, |tree, node| {
            NodeBuilder::text(
                title_text,
                TextStyle::new(text_size)
                    .with_weight(600)
                    .with_wrap(TextWrap::Wrap)
                    .with_overflow(TextOverflow::Ellipsis, 2),
            )
            .style(StyleBuilder::new().width_full().min_width_0().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(title_theme.role(cx, Role::FgDefault)),
                opacity: dimmed.then_some(0.55),
                ..StylePatch::default()
            })
            .build(tree, node)
        });
        if let Err(error) = title_node {
            log::warn!("building a prediction title failed: {error}");
        }

        let meta_theme = theme.clone();
        let meta = build_node(app, self.window, node, |tree, node| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .build(),
                )
                .build(tree, node)
        });
        let meta = match meta {
            Ok(meta) => meta,
            Err(error) => {
                log::warn!("building a prediction card meta row failed: {error}");
                return;
            }
        };
        if let Ok(meta_tree) = app
            .window_mut(self.window)
            .map(|window| window.tree_mut())
            .ok_or(EngineError::UnknownWindow {
                window: self.window,
            })
            && let Err(error) = Badge::new(theme)
                .info()
                .text(card.category.label.clone())
                .build(meta_tree, meta)
        {
            log::warn!("building a prediction category badge failed: {error}");
        }
        if let Some(end) = card.ends_at {
            let offset = self.host.data_time_zone(app);
            if let Some(label) = format_end(end, offset) {
                let muted = meta_theme;
                let muted_size = text_size - 1.0;
                let _ = build_node(app, self.window, meta, |tree, meta| {
                    NodeBuilder::text(SmolStr::new(label), TextStyle::new(muted_size))
                        .bind_style(move |cx| StylePatch {
                            foreground: Some(muted.role(cx, Role::FgMuted)),
                            ..StylePatch::default()
                        })
                        .build(tree, meta)
                });
            }
        }

        for option in &card.options {
            self.build_option(app, node, option, reduce_motion, text_size);
        }
        if card.remaining_markets > 0 {
            let note = t_smol!(
                translate,
                "prediction_selector.more_markets",
                count = card.remaining_markets
            );
            self.build_card_note(app, node, note, text_size);
        } else if card.remaining_options > 0 {
            let note = t_smol!(
                translate,
                "prediction_selector.more_options",
                count = card.remaining_options
            );
            self.build_card_note(app, node, note, text_size);
        }
    }

    fn build_card_note(&self, app: &mut App, parent: NodeId, text: SmolStr, text_size: LogicalPx) {
        let theme = self.host.theme().clone();
        let _ = build_node(app, self.window, parent, |tree, parent| {
            NodeBuilder::text(text, TextStyle::new(text_size - 1.0))
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
        });
    }

    fn build_option(
        &self,
        app: &mut App,
        parent: NodeId,
        option: &PredictionOptionCard,
        reduce_motion: bool,
        text_size: LogicalPx,
    ) {
        let theme = self.host.theme().clone();
        let (text, pending) = self.display_text(&option.label);
        let percentage = option
            .probability
            .map(|probability| (probability.ratio().inner() as f64 / 1e16).clamp(0.0, 100.0))
            .unwrap_or(0.0);
        let row = build_node(app, self.window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .width_full()
                        .build(),
                )
                .build(tree, parent)
        });
        let Ok(row) = row else {
            log::warn!("building a prediction option row failed");
            return;
        };
        let label_theme = theme.clone();
        let dimmed = pending && !reduce_motion;
        let _ = build_node(app, self.window, row, |tree, row| {
            NodeBuilder::text(
                text,
                TextStyle::new(text_size - 1.0)
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(label_theme.role(cx, Role::FgDefault)),
                opacity: dimmed.then_some(0.55),
                ..StylePatch::default()
            })
            .build(tree, row)
        });
        let track = build_node(app, self.window, row, |tree, row| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BAR_WIDTH))
                        .height(px(4.0))
                        .flex_none()
                        .build(),
                )
                .bind_style({
                    let theme = theme.clone();
                    move |cx| StylePatch {
                        background: Some(theme.role(cx, Role::SurfaceOverlay)),
                        ..StylePatch::default()
                    }
                })
                .build(tree, row)
        });
        let Ok(track) = track else {
            log::warn!("building a prediction probability track failed");
            return;
        };
        let fill_width = BAR_WIDTH * (percentage / 100.0) as f32;
        let fill_theme = theme.clone();
        let _ = build_node(app, self.window, track, |tree, track| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(fill_width))
                        .height_full()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    background: Some(fill_theme.role(cx, Role::ChartSeries0)),
                    ..StylePatch::default()
                })
                .build(tree, track)
        });
        let percent_theme = theme.clone();
        let percent = format_smolstr!("{percentage:.0}%");
        let _ = build_node(app, self.window, row, |tree, row| {
            NodeBuilder::text(percent, TextStyle::new(text_size - 1.0))
                .style(StyleBuilder::new().width(px(34.0)).flex_none().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(percent_theme.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, row)
        });
    }

    fn select_card(&mut self, app: &mut App, event: PredictionEventRef, title: SmolStr) {
        if self.selected.as_ref() == Some(&event) {
            return;
        }
        self.selected = Some(event);
        self.selected_title = Some(title);
        self.bump_revision();
        let _ = app.request_repaint(self.window);
    }

    fn finish(&mut self, app: &mut App) {
        self.cancel_translation(app);
        if let Some(dialog) = self.dialog.take() {
            dialog.release(app);
        }
        if let Some(tabs) = self.tabs.take() {
            tabs.release(app);
        }
        if let Some(select) = self.select.take() {
            select.release(app);
        }
        if let Some(tooltip) = self.translate_tooltip.take() {
            tooltip.release(app);
        }
        if let Some(button) = self.translate_button.take() {
            button.release(app);
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.release(app);
        }
        if let Some(retry) = self.retry_button.take() {
            retry.release(app);
        }
        if let Some(previous) = self.previous.take() {
            previous.release(app);
        }
        if let Some(next) = self.next.take() {
            next.release(app);
        }
        if let Some(search) = self.search_node.take() {
            search.release(app);
        }
        for action in self.card_actions.drain(..) {
            app.off_action(&action);
        }
        if let Some(id) = self.route_id.take() {
            self.router.unregister(id);
        }
        let _ = app.release_scope(self.scope);
    }
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

fn format_end(millis: i64, offset: time::UtcOffset) -> Option<String> {
    let seconds = millis.checked_div(1_000)?;
    let date = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let date = date.to_offset(offset);
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    ))
}

#[cfg(test)]
mod tests;
