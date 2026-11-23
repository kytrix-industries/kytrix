use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    rc::{Rc, Weak},
    sync::Arc,
};

use i18n::{t_smol, translate::Translate};
use market_participants_connector::{
    catalog::{batch::CatalogEntry, display::catalog::DisplayResolver},
    venue::loaded::LoadedVenues,
};
use market_participants_model::{
    decimal::UnixMillis,
    display::{lexical::scope_display_code, values::InstrumentDisplayString},
    identifier::VenueId,
    instrument::InstrumentKind,
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use nucleo::{
    Config as MatcherConfig, Nucleo, Utf32String,
    pattern::{CaseMatching, Normalization},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{primitive::Border, style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        separator::Separator,
        tree::{Tree, TreeItem, TreeOptions},
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use widget::{
    frame::{RouteId, WeakRouter},
    venue::VenueDisplay,
};

use crate::{host::SelectorHost, instrument_button::ConfirmHandler, ui::build_node};

const DIALOG_WIDTH: f32 = 720.0;
const DIALOG_HEIGHT: f32 = 520.0;
const VENUE_COLUMN_WIDTH: f32 = 180.0;
const ROW_HEIGHT: f32 = 26.0;
const KEY_BASE: &str = "instrument-selector";
const PANEL_ID: &str = "INSTRUMENT_SELECTOR";

const MATCHER_COLUMNS: u32 = 1;
const MATCHER_THREADS: usize = 1;
const UNKNOWN_LISTING_KEY: &str = "error.display.unknown_listing";

pub type EntryPredicate = Rc<dyn Fn(&CatalogEntry) -> bool + 'static>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstrumentKindFilter {
    Spot,
    Perpetual,
    Future,
    Option,
    Outcome,
    #[default]
    All,
}

impl InstrumentKindFilter {
    #[must_use]
    pub const fn allows(self, kind: InstrumentKind) -> bool {
        match self {
            Self::All => true,
            Self::Spot => matches!(kind, InstrumentKind::Spot),
            Self::Perpetual => matches!(kind, InstrumentKind::Perpetual),
            Self::Future => matches!(kind, InstrumentKind::Future),
            Self::Option => matches!(kind, InstrumentKind::Option),
            Self::Outcome => matches!(kind, InstrumentKind::Outcome),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstrumentStatusFilter {
    Tradable,
    #[default]
    Any,
}

impl InstrumentStatusFilter {
    #[must_use]
    pub const fn allows(self, status: ListingStatus) -> bool {
        match self {
            Self::Any => true,
            Self::Tradable => status.allows_new_trading(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedScope {
    pub route: VenueRoute,
    pub capabilities: Vec<CapabilityId>,
    pub provides_catalog: bool,
}

impl LoadedScope {
    #[must_use]
    pub fn all(venues: &LoadedVenues) -> Arc<[LoadedScope]> {
        venues
            .entries()
            .iter()
            .map(|(route, scope)| LoadedScope {
                route: route.clone(),
                capabilities: scope
                    .capabilities
                    .iter()
                    .map(|support| support.capability)
                    .collect(),
                provides_catalog: scope.catalog_source.is_some(),
            })
            .collect()
    }
}

#[must_use]
pub fn offers_route(routes: &[LoadedScope], route: &VenueRoute, capability: CapabilityId) -> bool {
    routes
        .iter()
        .find(|scope| &scope.route == route)
        .is_some_and(|scope| scope.capabilities.contains(&capability))
}

#[derive(Clone)]
pub struct InstrumentFilters {
    pub kind_filter: InstrumentKindFilter,
    pub status_filter: InstrumentStatusFilter,
    pub required_capability: CapabilityId,
    pub entry_predicate: Option<EntryPredicate>,
    pub routes: Arc<[LoadedScope]>,
}

impl InstrumentFilters {
    fn admits(&self, scope: &LoadedScope) -> bool {
        scope.provides_catalog && scope.capabilities.contains(&self.required_capability)
    }

    fn offered(&self) -> Vec<&LoadedScope> {
        self.routes
            .iter()
            .filter(|scope| self.admits(scope))
            .collect()
    }

    fn offers_route(&self, route: &VenueRoute) -> bool {
        self.routes
            .iter()
            .find(|scope| &scope.route == route)
            .is_some_and(|scope| self.admits(scope))
    }

    #[must_use]
    pub(crate) fn offers_anything(&self) -> bool {
        !self.offered().is_empty()
    }
}

enum InstrumentListState {
    Ready(Arc<[ListingRow]>),
    NotLoaded,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListMessage {
    NotLoaded,
    Loading,
    NoMatches,
    NoListings,
}

impl ListMessage {
    fn key(self) -> &'static str {
        match self {
            Self::NotLoaded => "instrument_selector.market_not_loaded",
            Self::Loading => "instrument_selector.loading",
            Self::NoMatches => "instrument_selector.no_matches",
            Self::NoListings => "instrument_selector.empty",
        }
    }
}

#[derive(Clone, Debug)]
struct ListingRow {
    entry: CatalogEntry,
    display: Option<InstrumentDisplayString>,
    aliases: Arc<[SmolStr]>,
    route_order: SmolStr,
}

impl ListingRow {
    fn matcher_column(&self) -> SmolStr {
        let Some(display) = self.display.as_ref() else {
            return SmolStr::default();
        };

        let mut column = SmolStr::new(display.as_str());
        for alias in self.aliases.iter() {
            column = format_smolstr!("{column} {alias}");
        }
        column
    }

    fn key_text(&self) -> SmolStr {
        Self::key_text_of(&self.entry.listing.key)
    }

    fn key_text_of(key: &VenueListingKey) -> SmolStr {
        format_smolstr!("{}:{}", key.route, key.listing_id)
    }
}

pub struct InstrumentSelector {
    inner: RefCell<SelectorState>,
}

struct SelectorState {
    host: SelectorHost,
    window: WindowId,
    key: SmolStr,
    scope: SignalScope,
    filters: InstrumentFilters,
    route: VenueRoute,
    selected_listing: Option<VenueListingKey>,
    filter_query: SmolStr,
    language: i18n::language::codes::LanguageCode,
    list_state: InstrumentListState,
    matcher: Nucleo<ListingRow>,
    matched: Arc<[ListingRow]>,
    shown: Option<Arc<[ListingRow]>>,
    rows: Rc<RefCell<Arc<[ListingRow]>>>,
    tree: Tree,
    list: List,
    input_state: Signal<InputState>,
    input_placeholder: Signal<SmolStr>,
    input: Option<InputNode>,
    selected: Signal<Option<SmolStr>>,
    message: Signal<Option<ListMessage>>,
    catalog_generation: u64,
    dialog: Option<Dialog>,
    closed: Rc<Cell<bool>>,
    route_id: Option<RouteId>,
    router: WeakRouter,
    on_confirm: ConfirmHandler,
}

impl InstrumentSelector {
    pub fn show_with_filters(
        current: Option<InstrumentSelection>,
        filters: InstrumentFilters,
        on_confirm: ConfirmHandler,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
    ) -> bool {
        let Some(route) = initial_route(
            current.as_ref().map(|selection| &selection.listing.route),
            &filters,
        ) else {
            return false;
        };
        let selected_listing = initial_listing(&current, &route);

        let selector = Rc::new(InstrumentSelector {
            inner: RefCell::new(SelectorState::new(
                host,
                window,
                route,
                selected_listing,
                filters,
                on_confirm,
                app,
            )),
        });
        selector.inner.borrow_mut().build_components(app);

        let dialog = SelectorState::build_dialog(&selector, app);
        selector.inner.borrow_mut().dialog = Some(dialog.clone());
        let route_id = SelectorState::register_route(&selector, host);
        selector.inner.borrow_mut().route_id = Some(route_id);

        if dialog.open(app, window).is_err() {
            selector.finish(app);
            return false;
        }
        let _ = app.request_repaint(window);
        true
    }

    fn sync(&self, app: &mut App) {
        self.inner.borrow_mut().sync(app);
    }

    fn finish(&self, app: &mut App) {
        self.inner.borrow_mut().finish(app);
    }
}

impl SelectorState {
    fn new(
        host: &SelectorHost,
        window: WindowId,
        route: VenueRoute,
        selected_listing: Option<VenueListingKey>,
        filters: InstrumentFilters,
        on_confirm: ConfirmHandler,
        app: &mut App,
    ) -> Self {
        let translate = host.translate().clone();
        let theme = host.theme().clone();
        let language = host.language(app);
        let matcher = Nucleo::new(
            MatcherConfig::DEFAULT,
            {
                let handle = app.handle();
                Arc::new(move || {
                    handle.send(move |app| {
                        let _ = app.request_repaint(window);
                    });
                })
            },
            Some(MATCHER_THREADS),
            MATCHER_COLUMNS,
        );
        let scope = app.signal_scope();
        let key = format_smolstr!("{KEY_BASE}@{}", scope.index());
        let tree = Tree::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:venues"),
            venue_rows(&filters, &translate),
            TreeOptions::default(),
        );
        let selected = app.signal_scoped(scope, None);
        let rows: Rc<RefCell<Arc<[ListingRow]>>> = Rc::new(RefCell::new(Arc::default()));
        let hovered: Rc<Cell<Option<Signal<Option<usize>>>>> = Rc::new(Cell::new(None));
        let list = {
            let rows = Rc::clone(&rows);
            let hovered = Rc::clone(&hovered);
            let selected_signal = selected;
            let row_theme = theme.clone();
            let language_signal = host.services().language;
            List::new(
                app,
                theme,
                format_smolstr!("{key}:list"),
                ListOptions::fixed(ROW_HEIGHT).selection(SelectionMode::Single),
                move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                    let row = rows.borrow().get(index).cloned();
                    let Some(row) = row else {
                        return Ok(());
                    };
                    build_listing_row(
                        tree,
                        parent,
                        &row,
                        index,
                        selected_signal,
                        hovered.get(),
                        &row_theme,
                        &translate,
                        language_signal,
                    )
                },
            )
        };
        hovered.set(Some(list.hovered_signal()));
        let input_state = app.signal_scoped(scope, InputState::new(""));
        let input_placeholder = app.signal_scoped(
            scope,
            t_smol!(host.translate(), "instrument_selector.filter_placeholder"),
        );
        let message = app.signal_scoped(scope, Some(ListMessage::NotLoaded));

        Self {
            host: host.clone(),
            window,
            key,
            scope,
            filters,
            route,
            selected_listing,
            filter_query: SmolStr::default(),
            language,
            list_state: InstrumentListState::NotLoaded,
            matcher,
            matched: Arc::default(),
            shown: None,
            rows,
            tree,
            list,
            input_state,
            input_placeholder,
            input: None,
            selected,
            message,
            catalog_generation: 0,
            dialog: None,
            closed: Rc::new(Cell::new(false)),
            route_id: None,
            router: host.router().downgrade(),
            on_confirm,
        }
    }

    fn build_components(&mut self, app: &mut App) {
        self.reload_instruments(app);
    }

    fn build_dialog(selector: &Rc<InstrumentSelector>, app: &mut App) -> Dialog {
        let (theme, translate, layer, animation, closed, key) = {
            let inner = selector.inner.borrow();
            (
                inner.host.theme().clone(),
                inner.host.translate().clone(),
                inner.host.layer().clone(),
                inner.host.animation(),
                Rc::clone(&inner.closed),
                inner.key.clone(),
            )
        };
        let weak = Rc::downgrade(selector);
        let content_weak = Weak::clone(&weak);
        let confirm_weak = Weak::clone(&weak);
        let confirm_label = t_smol!(translate, "common.confirm");
        let cancel_label = t_smol!(translate, "common.cancel");
        let title = t_smol!(translate, "common.instrument");

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
                    selector.inner.borrow().confirm(app);
                }
            })
            .cancel(cancel_label, move |_app| {
                if let Some(selector) = weak.upgrade() {
                    selector.inner.borrow().cancel();
                }
            })
            .on_close(move |_close, _app| closed.set(true))
            .build(app, theme, layer, animation, key)
    }

    fn register_route(selector: &Rc<InstrumentSelector>, host: &SelectorHost) -> RouteId {
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
        let tokens = theme.tokens();
        let text_size = tokens.small.font_size;

        let root = build_node(app, window, body, |tree, body| {
            NodeBuilder::boxed()
                .id(PANEL_ID)
                .style(
                    StyleBuilder::new()
                        .width(DIALOG_WIDTH)
                        .height(DIALOG_HEIGHT)
                        .flex_col()
                        .gap(8.0)
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;

        let top = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .flex_1()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, root)
        })?;

        let column = build_node(app, window, top, |tree, top| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(VENUE_COLUMN_WIDTH)
                        .height_full()
                        .flex_none()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .bind_style(themed_panel(theme.clone()))
                .build(tree, top)
        })?;
        self.tree.mount(app, window, column)?;
        self.expand_venues(app);

        build_node(app, window, top, |tree, top| {
            Separator::vertical(theme.clone()).build(tree, top)
        })?;

        let right = build_node(app, window, top, |tree, top| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, top)
        })?;

        let filter_row = build_node(app, window, right, |tree, right| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().width_full().min_width_0().build())
                .build(tree, right)
        })?;
        let spec = Arc::new(InputSpec {
            name: format_smolstr!("{}:filter", self.key),
            placeholder: SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Small,
            submit: None,
        });
        let input = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                filter_row,
                spec,
                &theme,
                &self.input_state,
                &self.input_placeholder,
            )
        };
        input.install(app);
        self.input = Some(input);

        let list_panel = build_node(app, window, right, |tree, right| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .bind_style(themed_panel(theme.clone()))
                .build(tree, right)
        })?;

        let list_holder = build_node(app, window, list_panel, |tree, panel| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style({
                    let message = self.message;
                    move |cx| StylePatch {
                        visible: Some(cx.read(&message).is_none()),
                        ..StylePatch::default()
                    }
                })
                .build(tree, panel)
        })?;
        self.list.mount(app, window, list_holder)?;

        let message_node = build_node(app, window, list_panel, |tree, panel| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .padding(8.0)
                        .build(),
                )
                .bind_style({
                    let message = self.message;
                    move |cx| StylePatch {
                        visible: Some(cx.read(&message).is_some()),
                        ..StylePatch::default()
                    }
                })
                .build(tree, panel)
        })?;
        let translate = self.host.translate().clone();
        let message_theme = theme.clone();
        let message_signal = self.message;
        let language = self.host.services().language;
        build_node(app, window, message_node, |tree, node| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_wrap(TextWrap::Wrap),
            )
            .style(StyleBuilder::new().min_width_0().build())
            .bind_text(move |cx| {
                let _language = cx.read(&language);
                match *cx.read(&message_signal) {
                    Some(message) => t_smol!(translate, message.key()),
                    None => SmolStr::default(),
                }
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(message_theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, node)
        })?;

        Ok(())
    }

    fn sync(&mut self, app: &mut App) {
        if self.closed.get() {
            self.finish(app);
            return;
        }
        let open = self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app) || dialog.is_closing());
        if !open {
            self.closed.set(true);
            self.finish(app);
            return;
        }

        self.dispatch_events(app);
        self.adopt_language(app);
        self.follow_tree(app);
        self.follow_input(app);
        let ranked = self.advance_matching();
        self.show_matches(app);
        self.follow_list_selection(app);
        self.follow_catalog_generation(app);
        self.write_list_message(app);
        if ranked {
            let _ = app.request_repaint(self.window);
        }

        let mut changed = false;
        changed |= self.tree.sync(app).unwrap_or(false);
        changed |= self.list.sync(app).unwrap_or(false);
        if let Some(input) = self.input.as_ref() {
            changed |= input.sync(app);
        }
        changed |= self.update_confirm_button(app);
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

    fn dispatch_events(&self, app: &mut App) {
        let Some(input) = self.input.as_ref() else {
            return;
        };
        if !self.owns_modal(app) {
            return;
        }
        let window = self.window;
        for (_, event) in app
            .drain_events_where(|target, event| target == window && crate::ui::is_text_input(event))
        {
            let _ = input.dispatch(app, &event);
        }
    }

    fn owns_modal(&self, app: &App) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        matches!(
            (app.modal_scope(self.window), dialog.overlay_node()),
            (Some(top), Some(own)) if top == own
        )
    }

    fn adopt_language(&mut self, app: &mut App) {
        let language = self.host.language(app);
        if language == self.language {
            return;
        }
        self.language = language;
        let translate = self.host.translate().clone();
        let _ = app.write(
            &self.input_placeholder,
            t_smol!(translate, "instrument_selector.filter_placeholder"),
        );
        let roots = venue_rows(&self.filters, &translate);
        if let Err(error) = self.tree.set_roots(app, roots) {
            log::warn!("rebuilding the venue column failed: {error}");
        }
        self.expand_venues(app);
    }

    fn expand_venues(&self, app: &mut App) {
        let mut venues: Vec<VenueId> = Vec::with_capacity(self.filters.offered().len());
        for scope in self.filters.offered() {
            if !venues.contains(&scope.route.venue) {
                venues.push(scope.route.venue);
            }
        }
        for venue in venues {
            let id = format_smolstr!("{venue}");
            if !self.tree.state(app).is_expanded(&id) {
                let _ = self.tree.toggle(app, &id);
            }
        }
    }

    fn follow_tree(&mut self, app: &mut App) {
        let Some(selected) = self.tree.selected(app) else {
            return;
        };
        let Some(route) = route_of_row(&selected, &self.filters) else {
            return;
        };
        if route == self.route {
            return;
        }
        self.select_route(app, route);
    }

    fn follow_input(&mut self, app: &mut App) {
        let query = SmolStr::new(app.read(&self.input_state).text().trim());
        if query == self.filter_query {
            return;
        }
        self.matcher
            .pattern
            .reparse(0, &query, CaseMatching::Ignore, Normalization::Smart, false);
        self.filter_query = query;
    }

    fn follow_list_selection(&mut self, app: &mut App) {
        let Some(index) = self.list.selection(app).first() else {
            return;
        };
        let key = self
            .rows
            .borrow()
            .get(index)
            .map(|row| row.entry.listing.key.clone());
        let Some(key) = key else {
            return;
        };
        if self.selected_listing.as_ref() != Some(&key) {
            self.selected_listing = Some(key);
            self.write_selected(app);
        }
    }

    fn write_selected(&self, app: &mut App) {
        let text = self.selected_listing.as_ref().map(ListingRow::key_text_of);
        if *app.read(&self.selected) != text {
            let _ = app.write(&self.selected, text);
        }
    }

    fn write_list_message(&self, app: &mut App) {
        let message = self.list_message();
        if *app.read(&self.message) != message {
            let _ = app.write(&self.message, message);
        }
    }

    fn list_message(&self) -> Option<ListMessage> {
        match self.list_state {
            InstrumentListState::NotLoaded => Some(ListMessage::NotLoaded),
            InstrumentListState::Empty => Some(ListMessage::NoListings),
            InstrumentListState::Ready(_) => {
                if self.matcher.snapshot().item_count() == 0 {
                    Some(ListMessage::Loading)
                } else if self.matched.is_empty() {
                    Some(ListMessage::NoMatches)
                } else {
                    None
                }
            }
        }
    }

    fn follow_catalog_generation(&mut self, app: &mut App) {
        let generation = *app.read(&self.host.services().catalog_generation);
        if generation == self.catalog_generation {
            return;
        }
        self.catalog_generation = generation;
        let Some(connector) = self.host.services().market_participants() else {
            return;
        };
        self.filters.routes = LoadedScope::all(connector.venues());
        let translate = self.host.translate().clone();
        let roots = venue_rows(&self.filters, &translate);
        if let Err(error) = self.tree.set_roots(app, roots) {
            log::warn!("rebuilding the venue column failed: {error}");
        }
        self.expand_venues(app);
        self.reload_instruments(app);
    }

    fn reload_instruments(&mut self, app: &mut App) {
        let route = self.route.clone();
        let Some(catalog) = self.host.catalog() else {
            self.discard_catalog();
            self.list_state = InstrumentListState::NotLoaded;
            self.show_matches(app);
            return;
        };
        match catalog.cached_listings(&route) {
            Some(entries) => {
                let display = catalog.cached_display();
                self.apply_catalog(&route, entries, &display);
            }
            None => {
                self.discard_catalog();
                self.list_state = InstrumentListState::NotLoaded;
            }
        }
        self.show_matches(app);
    }

    fn apply_catalog(
        &mut self,
        route: &VenueRoute,
        catalog: Arc<[CatalogEntry]>,
        display: &market_participants_connector::catalog::display::catalog::DisplaySnapshot,
    ) {
        let rows: Arc<[ListingRow]> = rows_of(
            display,
            route,
            catalog.to_vec(),
            &self.filters,
            self.filters.entry_predicate.as_ref(),
        )
        .into();
        if rows.is_empty() {
            self.discard_catalog();
            self.list_state = InstrumentListState::Empty;
            return;
        }
        self.selected_listing =
            selected_listing_if_available(&rows, self.selected_listing.as_ref()).cloned();
        self.inject(&rows);
        self.list_state = InstrumentListState::Ready(rows);
    }

    fn select_route(&mut self, app: &mut App, route: VenueRoute) {
        if self.route == route {
            return;
        }
        self.route = route;
        self.reload_instruments(app);
    }

    fn inject(&mut self, rows: &Arc<[ListingRow]>) {
        self.matcher.restart(true);
        self.matched = Arc::default();
        let injector = self.matcher.injector();
        for row in rows.iter() {
            injector.push(row.clone(), |row, columns| {
                columns[0] = Utf32String::from(row.matcher_column().as_str());
            });
        }
    }

    fn discard_catalog(&mut self) {
        self.selected_listing = None;
        self.matched = Arc::default();
        self.matcher.restart(true);
    }

    fn advance_matching(&mut self) -> bool {
        let status = self.matcher.tick(0);
        if !status.changed {
            return false;
        }

        self.matched = self
            .matcher
            .snapshot()
            .matched_items(..)
            .map(|item| item.data.clone())
            .collect();
        self.reconcile_selection();
        true
    }

    fn reconcile_selection(&mut self) {
        if self.selected_listing.is_none() {
            return;
        }
        if !matches!(self.list_state, InstrumentListState::Ready(_)) {
            self.selected_listing = None;
            return;
        }
        if !self.selection_is_offered() {
            self.selected_listing = None;
        }
    }

    fn selection_is_offered(&self) -> bool {
        match self.selected_listing.as_ref() {
            Some(selected) => self
                .matched
                .iter()
                .any(|row| &row.entry.listing.key == selected),
            None => false,
        }
    }

    fn selection(&self) -> Result<InstrumentSelection, SelectionRejection> {
        let InstrumentListState::Ready(rows) = &self.list_state else {
            return Err(SelectionRejection::NotLoaded);
        };

        confirmed_selection(
            &self.filters,
            rows,
            self.selected_listing.as_ref(),
            current_instant(),
        )
    }

    fn show_matches(&mut self, app: &mut App) {
        let changed = !self
            .shown
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, &self.matched));
        if changed {
            self.shown = Some(Arc::clone(&self.matched));
            *self.rows.borrow_mut() = Arc::clone(&self.matched);
            if let Err(error) = self.list.set_items(app, self.matched.len()) {
                log::warn!("refilling the listing list failed: {error}");
            }
            let _ = app.request_repaint(self.window);
        }
        self.write_selected(app);
    }

    fn update_confirm_button(&self, app: &mut App) -> bool {
        let disabled = self.selection().is_err();
        self.dialog
            .as_ref()
            .and_then(Dialog::confirm_button)
            .is_some_and(|button| button.set_disabled(app, disabled))
    }

    fn confirm(&self, app: &mut App) {
        let Ok(selection) = self.selection() else {
            return;
        };
        let on_confirm = self.on_confirm.clone();
        on_confirm(selection, app, self.window);
    }

    fn cancel(&self) {}

    fn finish(&mut self, app: &mut App) {
        if let Some(dialog) = self.dialog.take() {
            dialog.release(app);
        }
        self.list.release(app);
        self.tree.release(app);
        if let Some(input) = self.input.take() {
            input.release(app);
        }
        if let Some(id) = self.route_id.take() {
            self.router.unregister(id);
        }
        let _ = app.release_scope(self.scope);
    }
}

fn themed_panel(
    theme: ThemeHandle,
) -> impl Fn(&mut ui_core::node::dependency::ReadCtx<'_>) -> StylePatch {
    move |cx| StylePatch {
        background: Some(theme.role(cx, Role::SurfaceRaised)),
        border: Some(Border {
            width: 1.0,
            color: theme.role(cx, Role::LineBorder),
        }),
        ..StylePatch::default()
    }
}

fn build_listing_row(
    tree: &mut NodeTree,
    parent: NodeId,
    row: &ListingRow,
    index: usize,
    selected: Signal<Option<SmolStr>>,
    hovered: Option<Signal<Option<usize>>>,
    theme: &ThemeHandle,
    translate: &Translate,
    language: Signal<i18n::language::codes::LanguageCode>,
) -> Result<(), EngineError> {
    let key = row.key_text();
    let display = row.display.as_ref().map(|name| SmolStr::new(name.as_str()));
    let row_theme = theme.clone();
    let row_key = key.clone();
    let node = NodeBuilder::boxed()
        .id(format_smolstr!("{PANEL_ID}:row:{key}"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .width_full()
                .padding_x(8.0)
                .min_height_0()
                .build(),
        )
        .bind_style(move |cx| {
            let chosen = *cx.read(&selected) == Some(row_key.clone());
            let pointer = hovered.is_some_and(|signal| *cx.read(&signal) == Some(index));
            StylePatch {
                background: if chosen {
                    Some(row_theme.role(cx, Role::Selection))
                } else if pointer {
                    Some(row_theme.role(cx, Role::VeilHover))
                } else {
                    None
                },
                ..StylePatch::default()
            }
        })
        .build(tree, parent)?;

    let translate = translate.clone();
    let text_theme = theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(text_theme.tokens().small.font_size)
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(TextOverflow::Ellipsis, 1),
    )
    .style(StyleBuilder::new().width_full().min_width_0().build())
    .bind_text(move |cx| {
        let _language = cx.read(&language);
        display
            .clone()
            .unwrap_or_else(|| t_smol!(translate, UNKNOWN_LISTING_KEY))
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(text_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, node)?;

    let _ = index;
    Ok(())
}

fn initial_route(current: Option<&VenueRoute>, filters: &InstrumentFilters) -> Option<VenueRoute> {
    let offered = filters.offered();

    current
        .filter(|current| offered.iter().any(|scope| &scope.route == *current))
        .cloned()
        .or_else(|| offered.first().map(|scope| scope.route.clone()))
}

fn initial_listing(
    current: &Option<InstrumentSelection>,
    resolved_route: &VenueRoute,
) -> Option<VenueListingKey> {
    current
        .as_ref()
        .map(|selection| &selection.listing)
        .filter(|listing| &listing.route == resolved_route)
        .cloned()
}

fn current_instant() -> UnixMillis {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    i64::try_from(elapsed.as_millis()).unwrap_or(UnixMillis::MAX)
}

fn confirmed_selection(
    filters: &InstrumentFilters,
    rows: &[ListingRow],
    selected: Option<&VenueListingKey>,
    at: UnixMillis,
) -> Result<InstrumentSelection, SelectionRejection> {
    let listing = selected.ok_or(SelectionRejection::NoneSelected)?;
    if !filters.offers_route(&listing.route) {
        return Err(SelectionRejection::RouteNotOffered {
            route: listing.route.clone(),
        });
    }
    let row = rows
        .iter()
        .find(|row| &row.entry.listing.key == listing)
        .ok_or_else(|| SelectionRejection::ListingNotOffered {
            listing: listing.clone(),
        })?;
    if !filters.status_filter.allows(row.entry.snapshot.status) {
        return Err(SelectionRejection::StatusNotOffered {
            listing: listing.clone(),
            status: row.entry.snapshot.status,
        });
    }
    if !row.entry.snapshot.is_available_at(at) {
        return Err(SelectionRejection::NotAvailableAt {
            listing: listing.clone(),
            at,
        });
    }

    Ok(InstrumentSelection {
        listing: listing.clone(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionRejection {
    NotLoaded,
    NoneSelected,
    RouteNotOffered {
        route: VenueRoute,
    },
    ListingNotOffered {
        listing: VenueListingKey,
    },
    StatusNotOffered {
        listing: VenueListingKey,
        status: ListingStatus,
    },
    NotAvailableAt {
        listing: VenueListingKey,
        at: UnixMillis,
    },
}

fn prepare_rows(
    route: &VenueRoute,
    entries: Vec<CatalogEntry>,
    filters: &InstrumentFilters,
    entry_predicate: Option<&EntryPredicate>,
    resolver: &DisplayResolver<'_>,
) -> Vec<ListingRow> {
    let mut entries = entries;
    entries.retain(|entry| &entry.listing.key.route == route);
    entries.retain(|entry| filters.kind_filter.allows(entry.instrument.kind()));
    entries.retain(|entry| filters.status_filter.allows(entry.snapshot.status));
    if let Some(entry_predicate) = entry_predicate {
        entries.retain(|entry| entry_predicate(entry));
    }

    let mut rows: Vec<ListingRow> = Vec::with_capacity(entries.len());
    rows.extend(entries.into_iter().map(|entry| {
        ListingRow {
            route_order: format_smolstr!("{}", entry.listing.key.route),
            display: resolver
                .listing_instrument(&entry.listing.key)
                .ok()
                .cloned(),
            aliases: resolver.listing_search_aliases(&entry.listing.key).into(),
            entry,
        }
    }));
    rows.sort_by(compare_rows);
    rows
}

fn rows_of(
    display: &market_participants_connector::catalog::display::catalog::DisplaySnapshot,
    route: &VenueRoute,
    entries: Vec<CatalogEntry>,
    filters: &InstrumentFilters,
    entry_predicate: Option<&EntryPredicate>,
) -> Vec<ListingRow> {
    let resolver = display.resolver();

    prepare_rows(route, entries, filters, entry_predicate, &resolver)
}

fn compare_rows(left: &ListingRow, right: &ListingRow) -> Ordering {
    match (left.display.as_ref(), right.display.as_ref()) {
        (Some(left), Some(right)) => left.cmp(right),
        (None, Some(_)) => return Ordering::Greater,
        (Some(_), None) => return Ordering::Less,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| left.route_order.cmp(&right.route_order))
    .then_with(|| {
        left.entry
            .listing
            .key
            .listing_id
            .cmp(&right.entry.listing.key.listing_id)
    })
}

fn selected_listing_if_available<'a>(
    rows: &[ListingRow],
    selected: Option<&'a VenueListingKey>,
) -> Option<&'a VenueListingKey> {
    selected.filter(|selected| rows.iter().any(|row| &row.entry.listing.key == *selected))
}

fn venue_rows(filters: &InstrumentFilters, translate: &Translate) -> Vec<TreeItem> {
    let offered = filters.offered();
    let mut venues: Vec<VenueId> = Vec::with_capacity(offered.len());
    for scope in &offered {
        if !venues.contains(&scope.route.venue) {
            venues.push(scope.route.venue);
        }
    }

    venues
        .into_iter()
        .filter_map(|venue| {
            let scopes = offered
                .iter()
                .filter(|scope| scope.route.venue == venue)
                .map(|scope| scope.route.clone())
                .collect::<Vec<_>>();

            if scopes.is_empty() {
                return None;
            }
            let item = scopes.into_iter().fold(
                TreeItem::new(format_smolstr!("{venue}"), venue.display_string(translate)),
                |item, route| {
                    item.child(TreeItem::new(
                        format_smolstr!("{route}"),
                        scope_display_code(&route.scope),
                    ))
                },
            );
            Some(item)
        })
        .collect()
}

fn route_of_row(name: &str, filters: &InstrumentFilters) -> Option<VenueRoute> {
    filters
        .offered()
        .into_iter()
        .find(|scope| format_smolstr!("{}", scope.route) == name)
        .map(|scope| scope.route.clone())
}

#[cfg(test)]
mod tests;
