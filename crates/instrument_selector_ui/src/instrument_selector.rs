//! The instrument selector: the content area of the dialog that picks a venue
//! listing a pane should follow.
//!
//! The dialog wrapper only provides the backdrop, the panel and the button row, so this
//! module owns the whole content area — the column of venues and their scopes, the
//! filter row and the listing list — at a fixed size. It is opened through
//! [`InstrumentSelector::show_with_filters`], which builds the dialog through
//! `base_ui::kit::dialog`; whatever needs the confirmed selection calls it, and
//! [`crate::instrument_button`] is the control that does.
//!
//! The column is a tree of what the loaded venues answer: a venue is a row with
//! the scopes it loaded under it, and a scope is a row that opens the route it
//! names. A row reads the half of that route it stands for — the venue on the
//! fold, named the way the catalogue names it in the language the host renders, and
//! the scope under it in the protocol's own upper case — while the
//! canonical route stays the row's identity. Only the scopes
//! the caller's snapshot of the loaded venues holds is a row: a venue or a scope
//! outside it is left out of the column rather than drawn as a row that takes no
//! clicks. Which scope is open is therefore not drawn by this module but
//! selected in the tree, and what the tree reports is read back through
//! [`route_of_row`].
//!
//! Matching does not run here. The accepted catalog is pushed into a `nucleo`
//! worker once per load, and a query is answered by the ranked snapshot that
//! worker publishes, so the UI thread never fuzzy-matches thousands of
//! instruments per keystroke. What the worker ranks is one column per listing
//! that merges the published listing name with the native symbol and aliases of
//! the same generation, never a position in a list: a position means something
//! else as soon as the ranking, the filter or the catalog changes under it.
//!
//! What a row is drawn by is the name of one published generation,
//! not a name this module assembles. The row keeps the typed listing key beside
//! it, so a click and a confirmation always report the identity the catalog
//! published — a listing whose generation published no name is drawn as the
//! localized unknown and keeps its own key.
//!
//! Pure decisions — which routes a filter admits, whether the caller's listing
//! is on the route it settled on, what a confirmation is allowed to be — are
//! free functions so they can be tested without a window.
//!
//! # One frame route, no globals
//!
//! The dialog is not a widget, so nothing hands it a `WidgetCx`. It is synchronised by
//! a route the selector registers in the injected
//! [`FrameRouter`](widget::frame::FrameRouter) when it opens and removes when the
//! dialog has finished closing: the tree, the list and the input mirror their state, the
//! matcher's newly published ranking is copied into the list, the confirm button
//! follows the selection, and the dialog finishes its fade-out. The route holds the
//! state, so a closed dialog releases every kit component it owns instead of leaving
//! them to the process.

use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    rc::{Rc, Weak},
    sync::Arc,
};

use base_ui::{
    kit::{
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        separator::Separator,
        tree::{Tree, TreeItem, TreeOptions},
    },
    style_builder::StyleBuilder,
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
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
    paint::{primitive::Border, style::StylePatch},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use widget::{
    frame::{RouteId, WeakRouter},
    venue::VenueDisplay,
};

use crate::{
    host::SelectorHost,
    instrument_button::ConfirmHandler,
    ui::{build_node, release_input_actions},
};

/// Size of the content area inside the dialog panel.
const DIALOG_WIDTH: f32 = 720.0;
const DIALOG_HEIGHT: f32 = 520.0;
const VENUE_COLUMN_WIDTH: f32 = 180.0;
/// The height of one listing row, and the height the list is told to use before it has
/// measured anything.
const ROW_HEIGHT: f32 = 26.0;
/// The base of every kit key one dialog registers.
///
/// One dialog appends its own signal-scope index ([`SelectorState::key`]), so two panes that
/// open the selector at the same time own two identities. A constant key would make the later
/// dialog replace the earlier one's handlers: `App::on_action` replaces a handler of the same
/// `Action`, and the kit derives every button / list / tree / input action from its key.
const KEY_BASE: &str = "instrument-selector";
/// The node id of the selector's own content area (and of its listing rows), which tests of
/// this dialog and of the panes that open it look for.
///
/// Node ids are diagnostics rather than action identities, so they stay the same in every
/// dialog instance: a pane that opens a selector names the selector it opened by this id.
const PANEL_ID: &str = "INSTRUMENT_SELECTOR";

/// The matcher ranks candidates by a single column.
const MATCHER_COLUMNS: u32 = 1;
/// One worker thread is enough for a catalog, and it keeps a dialog from
/// claiming every core of the machine while it is open.
const MATCHER_THREADS: usize = 1;
/// The message a listing is drawn by when the generation published no name for
/// it. The identity of such a listing stays in its typed key and is never shown
/// as a name of its own.
///
/// The text is the shared fallback of the display boundary rather than a wording
/// of this dialog's own: a listing the selector cannot name is the same listing
/// every other pane cannot name, and one message keeps the two from disagreeing.
const UNKNOWN_LISTING_KEY: &str = "error.display.unknown_listing";

/// Decides whether one listing the catalog published is offered.
///
/// The predicate is answered for a whole [`CatalogEntry`], so it sees
/// the canonical instrument, the venue listing and its snapshot together — the
/// listing status, the availability window and the trading rules included.
pub type EntryPredicate = Rc<dyn Fn(&CatalogEntry) -> bool + 'static>;

/// Limits the instrument families presented by an instrument selector.
///
/// The filter names a canonical [`InstrumentKind`] rather than a venue market:
/// one scope may list several families, and a family is what a caller's
/// capability actually serves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstrumentKindFilter {
    /// Show spot listings only.
    Spot,
    /// Show perpetual listings only.
    Perpetual,
    /// Show future listings only.
    Future,
    /// Show option listings only.
    Option,
    /// Show outcome listings only.
    Outcome,
    /// Show every family the route lists.
    #[default]
    All,
}

impl InstrumentKindFilter {
    /// Returns whether this filter permits `kind`.
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

/// Limits the listing lifecycle states presented by an instrument selector.
///
/// A chart can follow a listing that is not tradable yet, while a trading panel
/// only ever offers one that accepts new commands; the two callers differ in
/// this filter and in nothing else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstrumentStatusFilter {
    /// Show listings that accept new trading.
    Tradable,
    /// Show listings of every lifecycle state.
    #[default]
    Any,
}

impl InstrumentStatusFilter {
    /// Returns whether this filter permits a listing in `status`.
    #[must_use]
    pub const fn allows(self, status: ListingStatus) -> bool {
        match self {
            Self::Any => true,
            Self::Tradable => status.allows_new_trading(),
        }
    }
}

/// One scope a load has committed, as this selector reads it.
///
/// The three facts are one reading of one commit: the route, the capability names
/// the scope carries and whether it publishes a catalog all come from the same
/// committed entry, so a scope this view offers is a scope whose catalog is in
/// memory and nothing here has to ask a second view whether the two agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedScope {
    /// The route of this scope.
    pub route: VenueRoute,
    /// Every capability this scope carries.
    pub capabilities: Vec<CapabilityId>,
    /// Whether this scope publishes a catalog.
    pub provides_catalog: bool,
}

impl LoadedScope {
    /// Reads every scope the loaded venues answer, in the order they answer it.
    ///
    /// The routes and the entries they are read from come from one reading of one
    /// commit — [`LoadedVenues::entries`] takes the venue table's lock once — so a
    /// route this view offers and the capabilities it is judged by are the same
    /// commit's, and no load landing while this runs can put one commit's route
    /// beside another's entry.
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

/// Whether a snapshot of loaded scopes offers `route` for `capability`.
///
/// A pane that keeps a snapshot of what was loaded when it last read the venues asks
/// this instead of looking inside the entries itself: the snapshot is the whole
/// answer to "may this pane still ask that route for that capability", and a route
/// that left the snapshot is refused without reaching a connector.
#[must_use]
pub fn offers_route(routes: &[LoadedScope], route: &VenueRoute, capability: CapabilityId) -> bool {
    routes
        .iter()
        .find(|scope| &scope.route == route)
        .is_some_and(|scope| scope.capabilities.contains(&capability))
}

/// The scope an instrument selector is presented with.
///
/// `routes` is the caller's snapshot of what the loaded venues answer, and it is
/// the only source of what the column draws and what a confirmation accepts: a
/// scope that is not in the snapshot is not offered, and a listing whose route
/// left the snapshot is refused instead of being mapped onto another venue.
#[derive(Clone)]
pub struct InstrumentFilters {
    pub kind_filter: InstrumentKindFilter,
    pub status_filter: InstrumentStatusFilter,
    pub required_capability: CapabilityId,
    pub entry_predicate: Option<EntryPredicate>,
    pub routes: Arc<[LoadedScope]>,
}

impl InstrumentFilters {
    /// Whether these filters offer `scope`.
    ///
    /// The catalog the listing list is built from and the capability the caller
    /// needs both have to be carried by the scope's own committed entry: a scope
    /// that exists but cannot answer what the caller needs is not a scope this
    /// selector may offer.
    fn admits(&self, scope: &LoadedScope) -> bool {
        scope.provides_catalog && scope.capabilities.contains(&self.required_capability)
    }

    /// The scopes these filters offer, in the order the loaded venues answered them.
    ///
    /// The loaded venues are the order, so a venue's scopes follow one another in
    /// this list and the column is the list with a fold around each venue. Nothing
    /// else decides what a row is: a venue is a row while this list holds one of
    /// its scopes, and a venue or a scope that is not in it is not a row at all.
    fn offered(&self) -> Vec<&LoadedScope> {
        self.routes
            .iter()
            .filter(|scope| self.admits(scope))
            .collect()
    }

    /// Whether the loaded venues still offer `route` for the required capability.
    fn offers_route(&self, route: &VenueRoute) -> bool {
        self.routes
            .iter()
            .find(|scope| &scope.route == route)
            .is_some_and(|scope| self.admits(scope))
    }

    /// Whether these filters offer any scope at all.
    ///
    /// This is the condition [`InstrumentSelector::show_with_filters`] refuses on,
    /// asked as a question rather than by opening a dialog that declines to appear:
    /// an entry point that has nothing to offer has to say so instead of closing
    /// silently. A loaded venue is not enough — a venue that serves none of what
    /// the caller requires offers the selector nothing either.
    #[must_use]
    pub(crate) fn offers_anything(&self) -> bool {
        !self.offered().is_empty()
    }
}

/// What the listing list is showing.
enum InstrumentListState {
    /// The catalog for the current route is being shown.
    Ready(Arc<[ListingRow]>),
    /// The current route's catalog is not in memory.
    ///
    /// This is the state a route the user has not loaded leaves the list in,
    /// and it is deliberately not [`Self::Empty`]: "this market has not been
    /// loaded" and "this market lists nothing this selector may offer" are two
    /// different things to tell a reader.
    NotLoaded,
    /// The accepted catalog holds no listing this selector may offer.
    Empty,
}

/// The message the list area draws instead of its rows.
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

/// One candidate the listing list offers: the accepted catalog entry together
/// with the name one published generation states for it.
///
/// The entry and the name are read from the same generation, so the name a row is
/// drawn by and the key a click reports can never come from two publications. The
/// name is `None` when that generation published none for the listing: the row
/// keeps its typed key, is drawn as the localized unknown and is ordered by that
/// key rather than by a name nobody published.
#[derive(Clone, Debug)]
struct ListingRow {
    /// The canonical instrument, the venue listing and its current snapshot.
    entry: CatalogEntry,
    /// The name of this listing in the generation the rows were read from.
    display: Option<InstrumentDisplayString>,
    /// The search aliases that generation published for this listing.
    aliases: Arc<[SmolStr]>,
    /// The route this row's fixed order falls back on, as the canonical lower-case
    /// identity.
    ///
    /// The order of the list is one order in every language, so it is taken from the
    /// identity rather than from the name a reader sees: sorting by the drawn form
    /// would reorder the rows when the interface changes language.
    route_order: SmolStr,
}

impl ListingRow {
    /// The text the single matcher column ranks this row by.
    ///
    /// The column merges the published name with every search alias the same
    /// publication stated for the listing — its native symbol and its published
    /// aliases — so the venue's spelling of a pair, the name and a typed alias all
    /// find the row through one query. A row whose generation published no name has
    /// no spelling to merge and stays unsearchable rather than being matched by an
    /// identity nobody published as a name.
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

    /// The typed key of the listing this row stands for, as the canonical text the
    /// selection signal carries.
    fn key_text(&self) -> SmolStr {
        Self::key_text_of(&self.entry.listing.key)
    }

    /// The canonical text of one listing key: `route:listing-id`.
    ///
    /// The identity is what a selection carries; the text is only what the row closures
    /// compare, because a node attribute cannot hold the typed key.
    fn key_text_of(key: &VenueListingKey) -> SmolStr {
        format_smolstr!("{}:{}", key.route, key.listing_id)
    }
}

/// Selects a venue scope and the listing a pane should follow.
///
/// The struct is private state behind an `Rc`: [`InstrumentSelector::show_with_filters`]
/// is the only entry point, and a caller confirms through the closure it passed.
pub struct InstrumentSelector {
    inner: RefCell<SelectorState>,
}

struct SelectorState {
    host: SelectorHost,
    window: WindowId,
    /// The per-instance prefix of every kit key this dialog registers:
    /// `"instrument-selector@{scope}"`.
    ///
    /// The selector is not a widget, so it has no
    /// [`WidgetCx::unique_key`](widget::cx::WidgetCx::unique_key); it mints its own signal
    /// scope instead (the engine appends one scope slot per dialog and never recycles an
    /// index), and this prefix is derived from that scope. Every button, list, tree and
    /// input the dialog builds is keyed by it, so a second pane's selector cannot overwrite
    /// the first's confirm / cancel / selection handlers.
    key: SmolStr,
    /// The selector's own signal scope: every signal this dialog mints belongs to it, and
    /// [`Self::finish`] releases it after the kit components have unmounted their nodes
    /// (E16). A root-scope signal would never be reclaimed, so every open/close cycle
    /// would leak its slots for the rest of the process.
    scope: SignalScope,
    filters: InstrumentFilters,
    route: VenueRoute,
    selected_listing: Option<VenueListingKey>,
    filter_query: SmolStr,
    language: i18n::language::codes::LanguageCode,
    list_state: InstrumentListState,
    matcher: Nucleo<ListingRow>,
    /// The ranked matches of the current query, refreshed once per completed pass.
    matched: Arc<[ListingRow]>,
    /// The ranking the list was last told to draw.
    shown: Option<Arc<[ListingRow]>>,
    /// The rows the current listing window draws, by index: what the list's row content
    /// closure reads (it is given no `App` and no state borrow).
    rows: Rc<RefCell<Arc<[ListingRow]>>>,
    tree: Tree,
    list: List,
    input_state: Signal<InputState>,
    input_placeholder: Signal<SmolStr>,
    input: Option<InputNode>,
    /// The listing the rows draw as chosen, as the text the row closures compare.
    selected: Signal<Option<SmolStr>>,
    /// The message the list area draws, or `None` when the rows fill it.
    message: Signal<Option<ListMessage>>,
    /// The generation the venue column was last built against.
    catalog_generation: u64,
    dialog: Option<Dialog>,
    closed: Rc<Cell<bool>>,
    route_id: Option<RouteId>,
    router: WeakRouter,
    on_confirm: ConfirmHandler,
}

impl InstrumentSelector {
    /// Presents an instrument selector with the complete filter set.
    ///
    /// The route it opens on is the caller's own when its filters offer it, and
    /// the first route they offer otherwise: a caller that follows nothing has no
    /// route to offer, and choosing one for it here is what keeps every caller
    /// from having to invent one.
    ///
    /// Returns whether the dialog was presented: no route is offered, so there is
    /// nothing to select from and the caller keeps its state.
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

    /// One frame of the dialog: read what the engine and the kit components observed,
    /// write the state back, and finish the close when the dialog has faded out.
    fn sync(&self, app: &mut App) {
        self.inner.borrow_mut().sync(app);
    }

    /// Releases every kit component the dialog owns and stops the frame route.
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
                // A completed pass wakes the window instead of the UI thread polling it:
                // the worker posts a frame request through the engine's cross-thread handle.
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
        // One scope per dialog: every signal below is reclaimed when `finish` releases it, and
        // the scope's index is what tells this dialog's kit keys from another pane's selector
        // (see `SelectorState::key`).
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
        // The list owns the hovered-row signal, and the row closure is built before the
        // list exists; the cell is filled in right after construction, before any row
        // mounts (rows mount in the first frame).
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

    /// Reads the catalogue of the route it opens on.
    fn build_components(&mut self, app: &mut App) {
        self.reload_instruments(app);
    }

    /// Builds the kit dialog: the content is this selector's two-pane area, the button
    /// row is the dialog's own (so the confirm button is the one the dialog closes with).
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

    /// Registers the per-frame route that drives the dialog.
    ///
    /// The route holds the selector strongly: it is what keeps the dialog's state —
    /// and with it the kit components — alive between frames, and unregistering it is
    /// what lets the state be released when the dialog has closed.
    fn register_route(selector: &Rc<InstrumentSelector>, host: &SelectorHost) -> RouteId {
        let held = Rc::clone(selector);
        host.router().register(move |app| {
            held.sync(app);
            Ok(())
        })
    }

    /// The content the dialog panel holds: the venue column, the filter row and the list.
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

    /// One frame: read what the engine and the kit components observed, write the state
    /// back, and finish the close when the dialog has faded out.
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

    /// Feeds this dialog's text input the events the application drained.
    ///
    /// # Ownership
    ///
    /// A dialog owns **the typed text, key presses and IME input of its own window, and
    /// only while it is that window's modal scope**. Nothing else: the engine's
    /// interaction dispatch (hit testing, actions, scrolling) already ran when the platform
    /// pushed the event, so what is left in the observation queue is what the application
    /// is entitled to read. Taking another window's events — or the pointer / resize /
    /// focus / close events of its own — would be the dialog swallowing the composition
    /// root's events, which is why [`App::drain_events_where`] takes only this dialog's
    /// share and leaves the rest queued. The top-most modal is the keyboard's only
    /// consumer, so a second open dialog takes nothing.
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

    /// Whether this dialog is the window's modal scope, and therefore the keyboard's owner.
    fn owns_modal(&self, app: &App) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        matches!(
            (app.modal_scope(self.window), dialog.overlay_node()),
            (Some(top), Some(own)) if top == own
        )
    }

    /// Rebuilds the venue column when the language moved.
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

    /// Expands every venue the filters offer, so its scopes are rows the user may pick.
    ///
    /// A freshly built tree has every fold closed; the selector's column is the list of
    /// offers, and a venue row that hid its scopes would make the user open each venue
    /// before seeing that the scope they need is not loaded.
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

    /// Opens the route the venue tree reports as selected.
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

    /// Replaces the query the matcher ranks against when the filter input changed.
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

    /// Mirrors the list's own selection (pointer or keyboard) into the selector's choice.
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

    /// Writes the choice the row closures draw as chosen.
    fn write_selected(&self, app: &mut App) {
        let text = self.selected_listing.as_ref().map(ListingRow::key_text_of);
        if *app.read(&self.selected) != text {
            let _ = app.write(&self.selected, text);
        }
    }

    /// Writes the message the list area draws, if it changed.
    fn write_list_message(&self, app: &mut App) {
        let message = self.list_message();
        if *app.read(&self.message) != message {
            let _ = app.write(&self.message, message);
        }
    }

    /// The message the list area should draw, or `None` when it should draw rows.
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

    /// Re-reads the directory when the composition root published a new generation.
    fn follow_catalog_generation(&mut self, app: &mut App) {
        let generation = *app.read(&self.host.services().catalog_generation);
        if generation == self.catalog_generation {
            return;
        }
        self.catalog_generation = generation;
        let Some(connector) = self.host.services().market_participants() else {
            return;
        };
        // The filters a caller hands over are the snapshot it opened the dialog with; a
        // commit while the dialog is open is something that caller could not have known,
        // so the column follows the loaded venues instead.
        self.filters.routes = LoadedScope::all(connector.venues());
        let translate = self.host.translate().clone();
        let roots = venue_rows(&self.filters, &translate);
        if let Err(error) = self.tree.set_roots(app, roots) {
            log::warn!("rebuilding the venue column failed: {error}");
        }
        self.expand_venues(app);
        self.reload_instruments(app);
    }

    /// Shows the listings of the current route, from the catalog already in memory.
    ///
    /// Nothing here loads a catalog: a route whose catalog is not published is
    /// what [`InstrumentListState::NotLoaded`] says, and loading the market is
    /// what leaves that state.
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

    /// Accepts a catalog as the candidates of the current route.
    ///
    /// A catalog that holds nothing this selector may offer is
    /// [`InstrumentListState::Empty`] rather than a failure: the market answered,
    /// and its answer lists nothing the caller can use.
    ///
    /// The display catalog the rows are named from is captured once, from the
    /// generation published at this moment, and never fetched: the route's
    /// snapshot is named by whatever generation is published now. Every row of the
    /// list is named by that one capture, so no row can be drawn by a publication
    /// another row does not belong to. A listing that generation published no name
    /// for is drawn as the localized unknown rather than under a name this module
    /// would invent.
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

    /// Opens another route, loading the catalog it needs.
    fn select_route(&mut self, app: &mut App, route: VenueRoute) {
        if self.route == route {
            return;
        }
        self.route = route;
        self.reload_instruments(app);
    }

    /// Hands the accepted catalog to the background matcher.
    ///
    /// The matcher ranks listings, so it is fed one value per listing: what the
    /// row label is, how a match is compared to the current selection, and what a
    /// click reports are all the same value afterwards. The single column is the
    /// merged name, native symbol and aliases of the row.
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

    /// Drops the candidates of the route that is being left behind.
    fn discard_catalog(&mut self) {
        self.selected_listing = None;
        self.matched = Arc::default();
        self.matcher.restart(true);
    }

    /// Advances the matcher and reports whether the ranked result changed.
    ///
    /// The wait is zero: a busy worker is never waited for, it wakes this view
    /// through the notification its `notify` callback signals.
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

    /// Clears a selection the current query no longer offers.
    ///
    /// This runs on a completed pass only. Until then the snapshot still holds
    /// the previous query's ranking, where a missing selection would mean
    /// nothing.
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

    /// Whether the current ranking still offers the selected listing.
    fn selection_is_offered(&self) -> bool {
        match self.selected_listing.as_ref() {
            Some(selected) => self
                .matched
                .iter()
                .any(|row| &row.entry.listing.key == selected),
            None => false,
        }
    }

    /// The selection a confirmation would report, or `None` when there is none.
    ///
    /// The answer is validated against the catalog the list was built from and
    /// against the caller's registry snapshot: a listing the catalog no longer
    /// publishes, a route the registry revoked, a lifecycle state the filters do
    /// not offer or a capability the route does not publish all refuse the confirmation
    /// instead of propagating a key the target pane cannot use.
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

    /// Shows the ranking the matcher published, if it is not the one the list holds.
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

    /// Keeps the dialog's confirm button in step with what the selection allows.
    fn update_confirm_button(&self, app: &mut App) -> bool {
        let disabled = self.selection().is_err();
        self.dialog
            .as_ref()
            .and_then(Dialog::confirm_button)
            .is_some_and(|button| button.set_disabled(app, disabled))
    }

    /// Closes the dialog with the choice the caller asked for.
    ///
    /// Kit's confirm button starts closing first and then runs the action, so the
    /// caller's handler runs with the dialog already closing and the dialog lands on
    /// `Confirmed` behind it; nothing here closes the dialog a second time.
    fn confirm(&self, app: &mut App) {
        let Ok(selection) = self.selection() else {
            return;
        };
        let on_confirm = self.on_confirm.clone();
        on_confirm(selection, app, self.window);
    }

    /// Notes that the user cancelled; the dialog closes itself with `Cancelled`.
    fn cancel(&self) {}

    /// Releases every kit component the dialog owns and stops the frame route.
    ///
    /// The order is the engine's release contract (E16): the nodes that read this
    /// dialog's signals are unmounted first ([`Dialog::release`] unloads the overlay
    /// subtree, and the list / tree / input own no node of their own any more), and only
    /// then is the selector's signal scope released — a `bind_*` closure that read a
    /// released handle would panic on its next evaluation.
    fn finish(&mut self, app: &mut App) {
        if let Some(dialog) = self.dialog.take() {
            dialog.release(app);
        }
        self.list.release(app);
        self.tree.release(app);
        if let Some(input) = self.input.take() {
            release_input_actions(app, &input.spec.name);
        }
        if let Some(id) = self.route_id.take() {
            self.router.unregister(id);
        }
        let _ = app.release_scope(self.scope);
    }
}

/// The style patch of a bordered sunken panel (the venue column and the list area).
///
/// The border and the fill follow an appearance, so they are bound rather than painted:
/// a static paint would keep the old theme's colours after the G3 switch.
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

/// One ranked match: the listing, and whether it is the one the selector holds as chosen.
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

/// The route a selector opens on.
///
/// The caller's own route is used when its filters offer it. A caller that
/// follows nothing hands over nothing, and the selector opens on the first route
/// its filters offer rather than on a route the caller had to make up.
fn initial_route(current: Option<&VenueRoute>, filters: &InstrumentFilters) -> Option<VenueRoute> {
    let offered = filters.offered();

    current
        .filter(|current| offered.iter().any(|scope| &scope.route == *current))
        .cloned()
        .or_else(|| offered.first().map(|scope| scope.route.clone()))
}

/// The caller's listing, if it belongs to the route the selector settled on.
///
/// A route the caller did not ask for carries a listing key that means something
/// else there, so the selection is dropped rather than carried over. The listing
/// identity is never translated between routes.
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

/// The moment a confirmation is judged at.
///
/// A listing's availability window is a wall-clock fact, so what a confirmation is
/// validated against is the system clock rather than anything the selector holds.
fn current_instant() -> UnixMillis {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    i64::try_from(elapsed.as_millis()).unwrap_or(UnixMillis::MAX)
}

/// The selection a confirmation reports, or why it is refused.
///
/// Every condition is answered from values this selector holds: the catalog the
/// list was built from, the filters the caller opened it with, and the caller's
/// own registry snapshot. A listing the catalog no longer publishes, a route the
/// registry revoked, a lifecycle state the filter does not offer or a route that
/// does not declare the capability all refuse the confirmation.
///
/// What a confirmation reports is the selected typed key, whatever the row was
/// drawn by: the display of a listing is a name of the current generation, and
/// the key is the identity the caller's pane follows.
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
    // A listing whose own availability window does not cover this moment is one the
    // selector may have drawn before the window closed: what a selection has to
    // satisfy is the listing as it stands now, not as it stood when it was listed.
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

/// Why a confirmation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionRejection {
    /// The catalog of the current route has not been accepted yet.
    NotLoaded,
    /// Nothing is selected.
    NoneSelected,
    /// The registry no longer offers the selected listing's route.
    RouteNotOffered { route: VenueRoute },
    /// The accepted catalog does not hold the selected listing.
    ListingNotOffered { listing: VenueListingKey },
    /// The listing is in a lifecycle state the filters do not offer.
    StatusNotOffered {
        listing: VenueListingKey,
        status: ListingStatus,
    },
    /// The listing's own availability window does not cover the moment of confirmation.
    NotAvailableAt {
        listing: VenueListingKey,
        at: UnixMillis,
    },
}

/// The catalog entries one route offers under these filters, in fixed order.
/// The order is `(name, canonical route, listing id)`: the
/// canonical instrument identity never decides it, because two listings of one
/// instrument are two rows and the listing key is what tells them apart.
///
/// The name is read from `resolver`, which is bound to one published
/// generation, so every name the returned rows are drawn by belongs to the same
/// publication as the name of every other row. A listing that generation
/// published no name for keeps its typed key and no name: see [`compare_rows`]
/// for where such a row is ordered.
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

/// The rows one captured generation offers for `entries`.
///
/// The capture is taken here and used for every row, so the names, the search aliases
/// and the ordering of one list all describe the same publication. A caller that
/// captured a generation and then read names again would be reading two publications
/// into one list; going through this function is what makes that impossible.
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

/// Orders two rows by the fixed key `(name, canonical route, listing id)`.
///
/// The published name decides first, then the route — taken from the identity
/// rather than from the form a reader sees, so the list does not reorder when the
/// interface changes language — and finally the listing id, so the order of the
/// list depends on nothing but the
/// rows themselves: not on the order the catalog was iterated in, not on the order
/// of any hash map the catalog read, and not on the identity of the canonical
/// instrument two listings share.
///
/// A row whose generation published no name has none to sort by: it follows every
/// named row and is ordered by its own typed key, which is the same `(canonical
/// route, listing id)` the named rows fall back to. The rows a reader cannot tell
/// apart by name therefore stay in a fixed order of their own, and a row never
/// moves because another listing lost its name.
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

/// Resolves the caller's selection against the listings a route offers.
///
/// A listing the route still publishes is kept; one it does not is cleared. The
/// selector never substitutes a listing of its own — not the venue's customary
/// pair, not the first row of the list — because a selection nobody made would be
/// reported back as if the user had made it. A second listing of the same
/// canonical instrument is never folded into the one that was asked for.
fn selected_listing_if_available<'a>(
    rows: &[ListingRow],
    selected: Option<&'a VenueListingKey>,
) -> Option<&'a VenueListingKey> {
    selected.filter(|selected| rows.iter().any(|row| &row.entry.listing.key == *selected))
}

/// The column of venues as the tree draws it: every venue these filters offer, each with its
/// offered scopes under it.
///
/// A venue is a row only while it has a scope under it. The loaded venues are what leave a venue
/// without one — a scope the venue did not load, or a capability the caller needs and that
/// scope does not carry — and a venue that cannot be opened is not drawn as a row that cannot
/// be clicked.
///
/// The two rows state the two halves of a route, each in the user-visible form it has and each of
/// them once: the fold of a venue's scopes reads the venue's name — `Binance` in English, `币安` in
/// Chinese — and a scope under it reads `SPOT`, or `PERP-DEX/XYZ` when the scope has several
/// segments. A scope row does not repeat the venue above it, because the row that states the venue
/// is the row it hangs under. What a row *is* and what it *reads* stay two different things: the
/// canonical lower-case `VenueRoute` is still the row's identity for [`route_of_row`], the machine
/// logs and explicitly marked identity diagnostics, and only the text a reader sees is the name.
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

/// The route one row of the column opens, from the identity the tree reports it under.
///
/// Only a row these filters built is a row the tree can report, so a name that opens a route is a
/// route the selector may open. A venue row names no route: a venue is open or folded rather
/// than chosen, and the venue the user is in is the venue of the scope they picked.
fn route_of_row(name: &str, filters: &InstrumentFilters) -> Option<VenueRoute> {
    filters
        .offered()
        .into_iter()
        .find(|scope| format_smolstr!("{}", scope.route) == name)
        .map(|scope| scope.route.clone())
}

#[cfg(test)]
mod tests;
