//! The instrument selector: the content area of the dialog that picks a venue
//! listing a pane should follow.
//!
//! The dialog wrapper only provides the backdrop and the panel, so this module
//! owns the whole content area — the column of venues and their scopes, the
//! filter row, the listing list and the footer — at a fixed size. It is opened
//! through [`InstrumentSelector::show_with_filters`], which mounts it as a dialog
//! on the window layer; whatever needs the confirmed selection calls it, and
//! [`crate::instrument_button`] is the control that does.
//!
//! The column is a tree of what the loaded venues answer: a venue is a row with
//! the scopes it loaded under it, and a scope is a row that opens the route it
//! names. A row reads the half of that route it stands for — the venue on the
//! fold, named the way the catalogue names it in the language the window is
//! showing, and the scope under it in the protocol's own upper case — while the
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

use std::{cmp::Ordering, rc::Rc, sync::Arc};

use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    separator::Separator,
    size::UnifiedSizable,
    static_sized_list::StaticSizedList,
    style::theme::ThemeExt,
    tree::Tree,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window,
    base::{Disableable, Selectable, TreeItem, TreeState},
    div, px,
};
use i18n::{language::codes::LanguageCode, t};
use market_participants_connector::{
    catalog::{
        batch::CatalogEntry,
        display::catalog::{DisplayResolver, DisplaySnapshot},
    },
    connector::MarketParticipantsConnector,
    market::provider::MarketCapability,
    venue::loaded::{LoadedVenues, VenuesUpdated},
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
use tokio::sync::Notify;
use widget::venue::VenueDisplay;

/// Size of the content area inside the dialog panel.
const DIALOG_WIDTH: f32 = 720.0;
const DIALOG_HEIGHT: f32 = 520.0;
const VENUE_COLUMN_WIDTH: f32 = 180.0;
/// Element id of the listing list. Stable across frames: the list keeps its
/// layout state, and the scroll handle keeps the viewport, under this id.
const LIST_ID: &str = "instrument-selector-list";
/// Debug selectors of the selector's own parts: the content area of the dialog, and the control
/// that leaves it without choosing.
const PANEL_SELECTOR: &str = "INSTRUMENT_SELECTOR";
const CANCEL_SELECTOR: &str = "INSTRUMENT_SELECTOR_CANCEL";

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

/// Runs after the user confirms a selection.
type ConfirmHandler = Rc<dyn Fn(InstrumentSelection, &mut Window, &mut App) + 'static>;

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
}

/// Selects a venue scope and the listing a pane should follow.
pub struct InstrumentSelector {
    market: MarketCapability,
    route: VenueRoute,
    filters: InstrumentFilters,
    selected_listing: Option<VenueListingKey>,
    filter_query: SharedString,
    filter_input: Entity<InputState>,
    /// The column of venues and scopes, which the tree draws and keeps the selection of.
    ///
    /// Which route is open is the row the tree has selected; what this module holds is the route
    /// behind that row, and the two are kept in step through [`route_of_row`].
    venue_tree: Entity<TreeState>,
    /// The language the rows of the column were labelled in.
    ///
    /// A row holds its label, so rows of a language the window has left are rows that have to be
    /// built again; the venue names are brands and are not translated, but the scopes are drawn
    /// from their canonical identity.
    language: LanguageCode,
    list_state: InstrumentListState,
    matcher: Nucleo<ListingRow>,
    /// The ranked matches of the current query, refreshed once per completed
    /// pass.
    ///
    /// The list is built from values moved into its closures, so the ranking is
    /// copied out of the matcher rather than read from it while the rows are
    /// built. It holds listing keys, not positions: the same listing stays the
    /// same listing when the query, the route or the catalog changes.
    matched: Arc<[ListingRow]>,
    /// The ranked matches as the rows a reader scrolls through, which is what a new ranking is
    /// given to.
    list: Entity<StaticSizedList<ListingRow>>,
    /// The ranking the list was last told to draw, which is how a frame knows whether the
    /// matcher has published one the list has not been given.
    shown: Option<Arc<[ListingRow]>>,
    /// Held so the task that wakes the view when the matcher publishes stays
    /// alive; dropping it would cancel the task and freeze the list.
    _matcher_task: Task<()>,
    /// Held because the subscription is what follows the column: dropping it would leave a row
    /// that is clicked and never opened.
    _venue_selection: Subscription,
    /// Held so the dialog keeps following the loaded venues while it is open.
    ///
    /// Absent for a selector mounted without a market participants root — a test's window, an embedder's —
    /// where there is no commit to follow.
    _venue_events: Option<Subscription>,
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
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let market = cx.global::<MarketParticipantsConnector>().market();
        let Some(route) = initial_route(
            current.as_ref().map(|selection| &selection.listing.route),
            &filters,
        ) else {
            return false;
        };
        let selected_listing = initial_listing(&current, &route);
        let filter_input = cx.new(|cx| {
            InputStateBuilder::new(FILTER_INPUT_ID)
                .placeholder(t!(cx, "instrument_selector.filter_placeholder"))
                .build(window, cx)
        });
        let selector = cx.new(|cx| {
            Self::new(
                market,
                route,
                selected_listing,
                filters,
                filter_input,
                on_confirm,
                cx,
            )
        });

        window.dialog(cx, selector.clone());
        selector.update(cx, |selector, cx| selector.reload_instruments(cx));
        true
    }

    fn new(
        market: MarketCapability,
        route: VenueRoute,
        selected_listing: Option<VenueListingKey>,
        filters: InstrumentFilters,
        filter_input: Entity<InputState>,
        on_confirm: ConfirmHandler,
        cx: &mut Context<Self>,
    ) -> Self {
        let wakeup = Arc::new(Notify::new());
        let matcher = Nucleo::new(
            MatcherConfig::DEFAULT,
            Arc::new({
                let wakeup = wakeup.clone();
                move || wakeup.notify_one()
            }),
            Some(MATCHER_THREADS),
            MATCHER_COLUMNS,
        );
        let matcher_task = cx.spawn(async move |this, cx| {
            loop {
                wakeup.notified().await;
                let updated = this.update(cx, |this, cx| {
                    if this.advance_matching() {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        // Every keystroke is a query change; the view reacts to the change
        // events rather than to the input element, which is rebuilt every frame.
        cx.subscribe(&filter_input, |this, _, event: &InputStateEvent, cx| {
            if let InputStateEvent::Change(query) = event {
                this.set_filter_query(query.clone(), cx);
            }
        })
        .detach();

        // The column is built from the routing this selector's filters offer and is given the
        // route it opens on as its selection, so the row that is highlighted is the route being
        // followed from the first frame.
        let venue_tree = cx.new(|cx| TreeState::new(cx).items(venue_rows(&filters, cx)));
        venue_tree.update(cx, |tree, cx| {
            let selected = scope_row_name(route.clone());
            tree.set_selected_index(tree.index_of(&selected), cx);
        });
        // The tree moves its own selection — a row is selected by clicking it, and the arrow keys
        // move it while the tree has focus — so this follows the row the tree reports rather than
        // the row a click happened to land on.
        let venue_selection = cx.observe(&venue_tree, |selector, tree, cx| {
            let selected = tree.read(cx).selected_item().map(|item| item.id.clone());
            selector.follow_venue_row(selected.as_ref(), cx);
        });
        // A dialog stays open across commits — the user may load the market they are looking for
        // while the selector is in front of them — so the column follows the loaded venues rather
        // than the snapshot the caller opened it with. A selector mounted without a market participants root
        // has no commit to follow, and its caller's snapshot is the whole of what it has.
        let venue_events = match cx.try_global::<MarketParticipantsConnector>() {
            Some(connector) => {
                let events = connector.venues().events();
                Some(cx.subscribe(&events, |selector, _, _: &VenuesUpdated, cx| {
                    selector.reload_venues(cx);
                }))
            }
            None => None,
        };

        // The list is built once and told what to draw, so the row it draws is given the
        // selector rather than the state of the frame it is drawn in: what a row reports is
        // the listing the selector holds as chosen, whenever it is drawn.
        let selector = cx.entity().downgrade();
        let list = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(LIST_ID)),
                move |_index, row: &ListingRow, _window, cx| listing_row(row, &selector, cx),
            )
        });

        Self {
            market,
            route,
            filters,
            selected_listing,
            filter_query: SharedString::default(),
            filter_input,
            venue_tree,
            language: *cx.global::<LanguageCode>(),
            list_state: InstrumentListState::NotLoaded,
            matcher,
            matched: Arc::default(),
            list,
            shown: None,
            _matcher_task: matcher_task,
            _venue_selection: venue_selection,
            _venue_events: venue_events,
            on_confirm,
        }
    }

    /// Re-reads the loaded venues a commit published and rebuilds the column.
    ///
    /// The filters a caller hands over are the snapshot it opened the dialog with; a commit while
    /// the dialog is open is something that caller could not have known, so the column follows the
    /// loaded venues instead — a scope that came into them is a row the user may pick, and one a
    /// shrinking commit took away is not a row that takes no clicks.
    ///
    /// The route the selector is on is kept even when it left the venues: substituting another
    /// route would report a listing nobody chose, and the list says for itself what it finds.
    fn reload_venues(&mut self, cx: &mut Context<Self>) {
        let Some(connector) = cx.try_global::<MarketParticipantsConnector>() else {
            return;
        };
        self.filters.routes = LoadedScope::all(connector.venues());
        let rows = venue_rows(&self.filters, cx);
        let selected = scope_row_name(self.route.clone());
        self.venue_tree.update(cx, |tree, cx| {
            tree.set_items(rows, cx);
            tree.set_selected_index(tree.index_of(&selected), cx);
        });
        cx.notify();
    }

    /// Opens another route, loading the catalog it needs.
    ///
    /// A route the selector is already on is left alone: clicking the row of the scope that is
    /// open is not a reason to fetch its catalog again.
    fn select_route(&mut self, route: VenueRoute, cx: &mut Context<Self>) {
        if self.route == route {
            return;
        }
        self.route = route;
        self.reload_instruments(cx);
    }

    /// Follows a row the tree has selected.
    ///
    /// A scope row opens the route it is named after. A venue row is the fold of the scopes
    /// under it rather than a choice — the same way a group of the settings tree opens and closes
    /// and selects no page — so what follows a click on one is the highlight moving back to the
    /// route the selector is on, which is what a column that can be folded has to do to keep
    /// saying which route is open.
    fn follow_venue_row(&mut self, selected: Option<&SharedString>, cx: &mut Context<Self>) {
        if let Some(route) = selected.and_then(|name| route_of_row(name.as_str(), &self.filters)) {
            self.select_route(route, cx);
        }

        self.show_route_row(cx);
    }

    /// Points the tree's highlight at the route the selector is on.
    ///
    /// The row of a route a folded venue hides is not a row the tree is drawing, and a highlight
    /// that cannot be put back is left where the fold put it.
    fn show_route_row(&mut self, cx: &mut Context<Self>) {
        let selected = scope_row_name(self.route.clone());
        let Some(index) = self.venue_tree.read(cx).index_of(&selected) else {
            return;
        };
        if self.venue_tree.read(cx).selected_index() == Some(index) {
            return;
        }

        self.venue_tree
            .update(cx, |tree, cx| tree.set_selected_index(Some(index), cx));
    }

    /// Rebuilds the rows of the column.
    ///
    /// A row holds the label it was built with, so the column is the one part of the dialog that
    /// does not follow a language by being drawn again: it is built again here, with the route the
    /// selector is on selected, because the tree is given its rows afresh.
    fn adopt_language(&mut self, cx: &mut Context<Self>) {
        let language = *cx.global::<LanguageCode>();
        if language == self.language {
            return;
        }

        self.language = language;
        let items = venue_rows(&self.filters, cx);
        let selected = scope_row_name(self.route.clone());
        self.venue_tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            tree.set_selected_index(tree.index_of(&selected), cx);
        });
    }

    /// Replaces the query the matcher ranks against.
    ///
    /// The filter survives a venue or scope change: the same symbol is usually
    /// what the user wants to look up on the route they just switched to.
    fn set_filter_query(&mut self, query: SharedString, cx: &mut Context<Self>) {
        if self.filter_query == query {
            return;
        }
        self.filter_query = query;
        self.matcher.pattern.reparse(
            0,
            &self.filter_query,
            CaseMatching::Ignore,
            Normalization::Smart,
            false,
        );
        self.advance_matching();
        cx.notify();
    }

    /// Shows the listings of the current route, from the catalog already in
    /// memory.
    ///
    /// Nothing here loads a catalog: a route whose catalog is not published is
    /// what [`InstrumentListState::NotLoaded`] says, and loading the market is
    /// what leaves that state.
    fn reload_instruments(&mut self, cx: &mut Context<Self>) {
        let route = self.route.clone();

        match self.market.catalog().cached_listings(&route) {
            Some(catalog) => self.apply_catalog(&route, catalog),
            None => {
                self.discard_catalog();
                self.list_state = InstrumentListState::NotLoaded;
            }
        }
        cx.notify();
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
    fn apply_catalog(&mut self, route: &VenueRoute, catalog: Arc<[CatalogEntry]>) {
        let display = self.market.catalog().cached_display();
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

    /// The column of venues: every venue this selector may offer, with the scopes its filters
    /// leave it under it.
    fn render_venue_column(&self) -> AnyElement {
        div()
            .w(px(VENUE_COLUMN_WIDTH))
            .h_full()
            .flex_none()
            .min_h_0()
            .child(Tree::new(
                "instrument-selector-venues".into(),
                self.venue_tree.clone(),
            ))
            .into_any_element()
    }

    /// The query input the listing list is filtered by.
    ///
    /// It is a sibling of the list rather than part of its panel: the query is
    /// what decides which instruments the list offers, not a control of the list.
    fn render_filter_input(&self) -> AnyElement {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(Input::new(&self.filter_input).clearable(true)),
            )
            .into_any_element()
    }

    /// The listing list.
    ///
    /// The rows are built from the ranking copied out of the matcher rather than
    /// from the matcher itself: the list's closures must own what they read, and a
    /// matcher read while its own view renders would be a second borrow of the
    /// entity that is drawing.
    fn render_instrument_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match &self.list_state {
            InstrumentListState::NotLoaded => {
                centered_message(t!(cx, "instrument_selector.market_not_loaded"), cx)
            }
            InstrumentListState::Empty => centered_message(t!(cx, "instrument_selector.empty"), cx),
            InstrumentListState::Ready(_) => self.render_matches(cx),
        }
    }

    fn render_matches(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // An empty result means two different things: before the matcher has
        // published anything for this catalog it means "still working", and only
        // afterwards does it mean the query matches nothing.
        if self.matcher.snapshot().item_count() == 0 {
            return centered_message(t!(cx, "instrument_selector.loading"), cx);
        }
        if self.matched.is_empty() {
            return centered_message(t!(cx, "instrument_selector.no_matches"), cx);
        }

        self.show_matches(cx);

        div()
            .debug_selector(|| "TEST_INSTRUMENT_MATCH_LIST".into())
            .size_full()
            .min_h_0()
            .child(self.list.clone())
            .into_any_element()
    }

    /// Shows the ranking the matcher published, if it is not the one the list holds.
    ///
    /// A list draws what it was given and remembers where the reader is, so it is told about
    /// another ranking rather than rebuilt every frame: being told is what puts the reader back
    /// at the top, which is where another query or another catalog leaves them.
    fn show_matches(&mut self, cx: &mut Context<Self>) {
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, &self.matched))
        {
            return;
        }

        self.shown = Some(self.matched.clone());
        self.list.update(cx, |list, cx| {
            list.replace(self.matched.iter().cloned());
            cx.notify();
        });
    }

    /// The action row, under a separator of its own so it reads as the dialog's
    /// footer rather than as part of what the user is working in.
    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let can_confirm = self.selection().is_ok();

        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(Separator::horizontal())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("instrument-selector-cancel")
                            .tonal()
                            .debug_selector(|| String::from(CANCEL_SELECTOR))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                            .child(t!(cx, "common.cancel")),
                    )
                    .child(
                        Button::new("instrument-selector-confirm")
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|selector, _, window, cx| {
                                let Ok(selection) = selector.selection() else {
                                    return;
                                };
                                // The choice is handed over before the selector asks to close: the
                                // emit is deferred to the end of the effect cycle, so the handler
                                // still runs and the dialog closes behind it.
                                let on_confirm = selector.on_confirm.clone();
                                on_confirm(selection, window, cx);
                                cx.emit(DismissEvent);
                            }))
                            .child(t!(cx, "common.confirm")),
                    ),
            )
            .into_any_element()
    }
}

/// The selector closes its dialog by emitting this, which is the signal the dialog listens for:
/// saying it is done comes from the selector's own context, not from the window.
impl EventEmitter<DismissEvent> for InstrumentSelector {}

impl Render for InstrumentSelector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pick up a pass the matcher finished since the last frame, so what is
        // drawn is never a ranking the query has already replaced.
        self.advance_matching();
        self.adopt_language(cx);

        let theme = cx.theme();
        let border = theme.roles.line.divider.hsla;

        div()
            .debug_selector(|| String::from(PANEL_SELECTOR))
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .flex()
                    .gap_2()
                    .child(self.render_venue_column())
                    .child(Separator::vertical())
                    .child(
                        div()
                            .min_w_0()
                            .min_h_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(self.render_filter_input())
                            .child(
                                // The panel belongs to the list alone; the query
                                // above it is not one of the list's controls.
                                div()
                                    .min_w_0()
                                    .min_h_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(border)
                                    .bg(theme.roles.surface.raised.hsla)
                                    .flex()
                                    .flex_col()
                                    .child(self.render_instrument_list(cx)),
                            ),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

/// Element id of the filter input, so the state cell survives a re-render.
const FILTER_INPUT_ID: &str = "instrument-selector-filter";

/// One ranked match: the listing, and whether it is the one the selector holds as chosen.
///
/// The selector is read for what the row is drawn as rather than handed the answer, so that a
/// row drawn by the list reads the choice it holds now: a list draws its rows again on every
/// frame of the selector, and choosing a row is one of those frames.
///
/// The row is drawn by the name of the generation its catalog was accepted from, and
/// by nothing else: the native symbol, the route and the listing id are search aliases and
/// diagnostics, never the text a reader picks a listing by. A listing that generation published
/// no name for is drawn as the localized unknown, and the key a click reports is the typed key
/// the row holds either way.
fn listing_row(
    row: &ListingRow,
    selector: &WeakEntity<InstrumentSelector>,
    cx: &App,
) -> AnyElement {
    let label = row_label(row, cx);
    let is_selected = selector.upgrade().is_some_and(|selector| {
        selector.read(cx).selected_listing.as_ref() == Some(&row.entry.listing.key)
    });

    Button::new((
        ElementId::Name(SharedString::new_static("InstrumentMatchRow")),
        // The row is named by the listing it stands for rather than by the text it
        // happens to be drawn by, so two rows never share an element id — not even
        // when neither of them has a published name.
        SharedString::new(format_smolstr!(
            "{} {}",
            row.entry.listing.key.route,
            row.entry.listing.key.listing_id
        )),
    ))
    .ghost()
    .medium()
    .selected(is_selected)
    .on_click({
        let listing = row.entry.listing.key.clone();
        let selector = selector.clone();
        move |_, _, cx| {
            let Some(selector) = selector.upgrade() else {
                return;
            };
            selector.update(cx, |selector, cx| {
                selector.selected_listing = Some(listing.clone());
                cx.notify();
            });
        }
    })
    .child(
        div()
            .debug_selector(|| "TEST_INSTRUMENT_MATCH_ROW".into())
            .min_w_0()
            .flex_1()
            .truncate()
            .child(label),
    )
    .into_any_element()
}

/// The text the visual column draws for one row.
///
/// The column states the name of the generation the row was read
/// from and nothing else: the native symbol, the route and the listing id stay
/// search aliases and diagnostics, so a reader never picks a listing by an
/// identity. A listing whose generation published no name is drawn as the
/// localized unknown — never as the key, a hash or a symbol dressed up as one.
fn row_label(row: &ListingRow, cx: &App) -> SharedString {
    match row.display.as_ref() {
        Some(name) => SharedString::from(name.as_str()),
        None => t!(cx, UNKNOWN_LISTING_KEY),
    }
}

/// A single line of text centered in the space the listing list would fill.
fn centered_message(message: impl Into<SharedString>, cx: &App) -> AnyElement {
    let color = cx.theme().roles.fg.muted.hsla;

    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(color)
        .child(message.into())
        .into_any_element()
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
    display: DisplaySnapshot,
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

/// The name the tree knows the row of one venue by.
///
/// A row is named after what it stands for rather than by the position it is drawn at, so a row the
/// tree reports is read back by naming the routing again — see [`route_of_row`].
fn venue_row_name(venue: &VenueId) -> SharedString {
    SharedString::new(format_smolstr!("{venue}"))
}

/// The name the tree knows the row of one scope by: the route that row opens.
fn scope_row_name(route: VenueRoute) -> SharedString {
    SharedString::new(format_smolstr!("{route}"))
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
fn venue_rows(filters: &InstrumentFilters, cx: &App) -> Vec<TreeItem> {
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

            (!scopes.is_empty()).then(|| {
                TreeItem::new(venue_row_name(&venue), venue.display_string(cx))
                    .expanded(true)
                    .children(scopes.into_iter().map(|route| {
                        let label = SharedString::new(scope_display_code(&route.scope));
                        TreeItem::new(scope_row_name(route), label)
                    }))
            })
        })
        .collect()
}

/// The route one row of the column opens, from the name the tree reports it under.
///
/// Only a row these filters built is a row the tree can report, so a name that opens a route is a
/// route the selector may open. A venue row names no route: a venue is open or folded rather
/// than chosen, and the venue the user is in is the venue of the scope they picked.
fn route_of_row(name: &str, filters: &InstrumentFilters) -> Option<VenueRoute> {
    for scope in filters.offered() {
        if scope_row_name(scope.route.clone()) == name {
            return Some(scope.route.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    //! The selector's pure decisions, and the dialog it is mounted in.

    use std::{cmp::Ordering, rc::Rc, sync::Arc, time::Duration};

    use base_ui::{animation::ANIMATION_TICK, input::InputStateBuilder, layer::Layer};
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, Modifiers, Render, TestAppContext,
        VisualTestContext, Window, base::TreeState, div, px, size,
    };
    use i18n::{language::codes::LanguageCode, translator::types::I18n};
    use market_participants_connector::{
        catalog::{
            batch::CatalogEntry,
            generation::{CatalogGeneration, CatalogGenerationPublisher},
            test_support,
        },
        connector::MarketParticipantsConnector,
    };
    use market_participants_model::{
        decimal::UnixMillis,
        identifier::VenueId,
        instrument::{Instrument, InstrumentKind},
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
    use widget_tokio_runtime::global;

    use super::{
        CANCEL_SELECTOR, EntryPredicate, FILTER_INPUT_ID, InstrumentFilters, InstrumentKindFilter,
        InstrumentListState, InstrumentSelector, InstrumentStatusFilter, ListingRow, LoadedScope,
        MATCHER_COLUMNS, MATCHER_THREADS, PANEL_SELECTOR, SelectionRejection, UNKNOWN_LISTING_KEY,
        compare_rows, confirmed_selection, initial_listing, initial_route, prepare_rows,
        route_of_row, row_label, rows_of, scope_row_name, selected_listing_if_available,
        venue_row_name, venue_rows,
    };
    use crate::test_support::{loaded, scope};

    /// The view the layer is mounted over: the dialog is what a test draws.
    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue is canonical")
    }

    fn route(name: &str, scope: &str) -> VenueRoute {
        test_support::route(name, scope)
    }

    fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
        test_support::listing_key(route, symbol)
    }

    fn selection(route: &VenueRoute, symbol: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: listing_key(route, symbol),
        }
    }

    /// The loaded scopes the fixtures are described by: two Binance scopes and one OKX scope.
    fn registry() -> Arc<[LoadedScope]> {
        loaded(vec![
            scope(
                route("binance", "spot"),
                &[CapabilityId::Candlesticks, CapabilityId::Tickers],
            ),
            scope(
                route("binance", "usd-m-perp"),
                &[CapabilityId::Candlesticks, CapabilityId::ForceOrders],
            ),
            scope(route("okx", "spot"), &[CapabilityId::Candlesticks]),
        ])
    }

    /// A snapshot that holds no scope at all.
    fn empty_registry() -> Arc<[LoadedScope]> {
        loaded(Vec::new())
    }

    fn filters(
        kind_filter: InstrumentKindFilter,
        required_capability: CapabilityId,
    ) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter,
            status_filter: InstrumentStatusFilter::Any,
            required_capability,
            entry_predicate: None,
            routes: registry(),
        }
    }

    fn spot(base: &str, quote: &str) -> Instrument {
        test_support::spot_token(test_support::VENUE, base, quote)
    }

    /// The moment the confirmation fixtures are judged at.
    const CONFIRMED_AT: UnixMillis = 1_700_000_000_000;

    /// One fixture catalog entry of a Binance spot listing under another status.
    fn entry(
        route: &VenueRoute,
        symbol: &str,
        instrument: Instrument,
        status: ListingStatus,
    ) -> CatalogEntry {
        test_support::with_status(test_support::entry(route, instrument, symbol), status)
    }

    /// One row of a listing whose generation published no name for it.
    ///
    /// The tests that are about a key, a status or a filter drive the selector
    /// with such rows: what a reader cannot be told apart by name is decided by
    /// the typed key, which is exactly the fallback under test. A test about what
    /// a row is drawn by drives a fixture that published its display instead.
    fn unnamed_row(
        route: &VenueRoute,
        symbol: &str,
        instrument: Instrument,
        status: ListingStatus,
    ) -> ListingRow {
        let entry = entry(route, symbol, instrument, status);
        ListingRow {
            route_order: format_smolstr!("{}", entry.listing.key.route),
            display: None,
            aliases: Arc::default(),
            entry,
        }
    }

    /// A generation that published no name at all, for the typed-key fallback.
    fn empty_generation() -> CatalogGeneration {
        CatalogGeneration::empty(&CatalogGenerationPublisher::new())
    }

    /// One unnamed row of `entry`, ordered under `route_order`.
    ///
    /// This is how the comparator's route segment is pinned for two rows the
    /// primary key cannot tell apart, which a catalog of one route cannot produce.
    fn ordered_under(entry: &CatalogEntry, route_order: &str) -> ListingRow {
        ListingRow {
            route_order: SmolStr::new(route_order),
            display: None,
            aliases: Arc::default(),
            entry: entry.clone(),
        }
    }

    /// A fixture catalog whose spot route published `entries`, with the runtime
    /// its fetch runs on installed in the test's application state.
    fn published(cx: &mut TestAppContext, entries: Vec<CatalogEntry>) -> test_support::Fixture {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test runtime should build");
        });
        let fixture = test_support::Fixture::new(entries);
        let runtime = cx.update(|cx| global::handle(cx));
        runtime
            .handle()
            .block_on(fixture.ensure())
            .expect("the fixture spot route should publish its catalog");

        fixture
    }

    /// The rows one published fixture offers under `filters`.
    ///
    /// The rows are built through the same capture the selector uses, so what these
    /// tests read is what a list built from that generation would draw.
    fn fixture_rows(
        fixture: &test_support::Fixture,
        entries: Vec<CatalogEntry>,
        filters: &InstrumentFilters,
        entry_predicate: Option<&EntryPredicate>,
    ) -> Vec<ListingRow> {
        rows_of(
            fixture.catalog().cached_display(),
            fixture.route(),
            entries,
            filters,
            entry_predicate,
        )
    }

    /// Whether the matcher answers `query` with the one row whose column is `column`.
    ///
    /// Two candidates are fed to the matcher — the row and an empty column — so the
    /// count tells a hit from the state before the query is applied: an empty
    /// pattern matches every candidate, and an empty column matches no query.
    fn matcher_hits(column: &str, query: &str) -> bool {
        let mut matcher: Nucleo<()> = Nucleo::new(
            MatcherConfig::DEFAULT,
            Arc::new(|| {}),
            Some(MATCHER_THREADS),
            MATCHER_COLUMNS,
        );
        let injector = matcher.injector();
        injector.push((), |_, columns| columns[0] = Utf32String::from(column));
        injector.push((), |_, columns| columns[0] = Utf32String::from(""));
        drop(injector);
        matcher
            .pattern
            .reparse(0, query, CaseMatching::Ignore, Normalization::Smart, false);
        while matcher.tick(10).running {}

        matcher.snapshot().matched_item_count() == 1
    }

    #[test]
    fn kind_filters_should_apply_to_every_venue() {
        let spot_filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);

        assert_eq!(
            spot_filters
                .offered()
                .iter()
                .map(|scope| scope.route.clone())
                .collect::<Vec<_>>(),
            vec![
                route("binance", "spot"),
                route("binance", "usd-m-perp"),
                route("okx", "spot"),
            ]
        );
        assert!(spot_filters.kind_filter.allows(InstrumentKind::Spot));
        assert!(!spot_filters.kind_filter.allows(InstrumentKind::Perpetual));
        assert!(
            InstrumentKindFilter::All.allows(InstrumentKind::Outcome),
            "the default filter offers every family"
        );
    }

    #[test]
    fn capability_filter_should_exclude_routes_that_do_not_declare_it() {
        let force_orders = filters(InstrumentKindFilter::Perpetual, CapabilityId::ForceOrders);

        assert_eq!(
            force_orders
                .offered()
                .iter()
                .map(|scope| scope.route.clone())
                .collect::<Vec<_>>(),
            vec![route("binance", "usd-m-perp")]
        );
    }

    #[test]
    fn initial_route_should_fall_back_to_first_permitted_route() {
        let okx_spot = route("okx", "spot");

        assert_eq!(
            initial_route(
                Some(&okx_spot),
                &filters(InstrumentKindFilter::Spot, CapabilityId::ForceOrders)
            )
            .as_ref(),
            Some(&route("binance", "usd-m-perp"))
        );
        assert_eq!(
            initial_route(
                None,
                &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks)
            )
            .as_ref(),
            Some(&route("binance", "spot"))
        );
        assert_eq!(
            initial_route(None, &empty_filters()),
            None,
            "a registry with nothing in it offers no route to open on"
        );
    }

    fn empty_filters() -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Candlesticks,
            entry_predicate: None,
            routes: empty_registry(),
        }
    }

    #[test]
    fn initial_listing_should_be_cleared_after_route_fallback() {
        let binance_spot = route("binance", "spot");

        assert_eq!(
            initial_listing(
                &Some(selection(&binance_spot, "BTCUSDT")),
                &route("binance", "usd-m-perp")
            ),
            None,
            "a listing key means something else on another route and is never translated"
        );
        assert_eq!(
            initial_listing(&Some(selection(&binance_spot, "BTCUSDT")), &binance_spot),
            Some(listing_key(&binance_spot, "BTCUSDT"))
        );
        assert_eq!(initial_listing(&None, &binance_spot), None);
    }

    #[test]
    fn confirmation_should_refuse_a_listing_the_catalog_does_not_hold() {
        let binance_spot = route("binance", "spot");
        let published = listing_key(&binance_spot, "BTCUSDT");
        let absent = listing_key(&binance_spot, "ETHUSDT");
        let rows = vec![unnamed_row(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        )];

        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &rows,
                Some(&published),
                CONFIRMED_AT
            ),
            Ok(InstrumentSelection { listing: published })
        );
        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &rows,
                Some(&absent),
                CONFIRMED_AT
            ),
            Err(SelectionRejection::ListingNotOffered { listing: absent })
        );
    }

    #[test]
    fn confirmation_should_refuse_a_route_the_registry_revoked() {
        let revoked = route("hyperliquid", "core-perp");
        let rows = vec![unnamed_row(
            &revoked,
            "BTCUSD",
            spot("BTC", "USD"),
            ListingStatus::Trading,
        )];

        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &rows,
                Some(&listing_key(&revoked, "BTCUSD")),
                CONFIRMED_AT
            ),
            Err(SelectionRejection::RouteNotOffered { route: revoked })
        );
    }

    #[test]
    fn confirmation_should_refuse_a_status_the_filters_do_not_offer() {
        let binance_spot = route("binance", "spot");
        let halted = listing_key(&binance_spot, "BTCUSDT");
        let rows = vec![unnamed_row(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Halted,
        )];
        let mut tradable = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);
        tradable.status_filter = InstrumentStatusFilter::Tradable;

        assert_eq!(
            confirmed_selection(&tradable, &rows, Some(&halted), CONFIRMED_AT),
            Err(SelectionRejection::StatusNotOffered {
                listing: halted,
                status: ListingStatus::Halted,
            })
        );
    }

    /// A listing that declares a window is only selectable inside it: the window is
    /// part of what a selection satisfies, not only what the list was filtered by.
    #[test]
    fn confirmation_should_refuse_a_listing_whose_window_has_not_opened() {
        let binance_spot = route("binance", "spot");
        let listing = listing_key(&binance_spot, "BTCUSDT");
        let mut row = unnamed_row(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        row.entry.snapshot.available_from = Some(CONFIRMED_AT + 1);

        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &[row],
                Some(&listing),
                CONFIRMED_AT
            ),
            Err(SelectionRejection::NotAvailableAt {
                listing,
                at: CONFIRMED_AT,
            })
        );
    }

    /// The same listing is selectable once its window covers the moment of confirmation.
    #[test]
    fn confirmation_should_accept_a_listing_inside_its_window() {
        let binance_spot = route("binance", "spot");
        let listing = listing_key(&binance_spot, "BTCUSDT");
        let mut row = unnamed_row(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        row.entry.snapshot.available_from = Some(CONFIRMED_AT - 1);
        row.entry.snapshot.available_until = Some(CONFIRMED_AT + 1);

        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &[row],
                Some(&listing),
                CONFIRMED_AT
            ),
            Ok(InstrumentSelection { listing })
        );
    }

    #[test]
    fn confirmation_should_require_a_selection() {
        let rows = Vec::new();

        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &rows,
                None,
                CONFIRMED_AT
            ),
            Err(SelectionRejection::NoneSelected)
        );
    }

    /// The key of every row, in the order the rows are offered.
    fn row_keys(rows: &[ListingRow]) -> Vec<VenueListingKey> {
        rows.iter()
            .map(|row| row.entry.listing.key.clone())
            .collect()
    }

    /// The name every row is drawn by, in the order the rows are offered.
    fn row_names(rows: &[ListingRow]) -> Vec<&str> {
        rows.iter()
            .map(|row| {
                row.display
                    .as_ref()
                    .expect("a published fixture names every row it offers")
                    .as_str()
            })
            .collect()
    }

    #[gpui_kit::test]
    fn preparation_should_filter_route_kind_and_status(cx: &mut TestAppContext) {
        let binance_spot = test_support::spot_route();
        let binance_perp = route("binance", "usd-m-perp");
        let btc = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let eth = entry(
            &binance_spot,
            "ETHUSDT",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![btc.clone(), eth.clone()]);
        let elsewhere = entry(
            &binance_perp,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let halted = entry(
            &binance_spot,
            "SOLUSDT",
            spot("SOL", "USDT"),
            ListingStatus::Halted,
        );
        let mut tradable = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);
        tradable.status_filter = InstrumentStatusFilter::Tradable;

        let rows = fixture_rows(&fixture, vec![eth, elsewhere, halted, btc], &tradable, None);

        assert_eq!(
            row_keys(&rows),
            vec![
                listing_key(&binance_spot, "BTCUSDT"),
                listing_key(&binance_spot, "ETHUSDT"),
            ],
            "another route and a halted listing are not rows of this selector"
        );
    }

    /// Two listings of one canonical instrument are two rows, and a generation that
    /// published no name for either leaves their order to their own typed keys: the
    /// instrument identity they share never decides it.
    #[test]
    fn two_listings_of_one_instrument_should_stay_distinct_and_ordered_by_their_keys() {
        let binance_spot = route("binance", "spot");
        let instrument = spot("BTC", "USDT");
        let second = unnamed_row(
            &binance_spot,
            "BTCUSDT-SECOND",
            instrument.clone(),
            ListingStatus::Trading,
        );
        let first = unnamed_row(&binance_spot, "BTCUSDT", instrument, ListingStatus::Trading);
        let mut keys = vec![
            first.entry.listing.key.clone(),
            second.entry.listing.key.clone(),
        ];
        keys.sort();
        let generation = empty_generation();
        let resolver = generation.display_resolver();

        let sorted = prepare_rows(
            &binance_spot,
            vec![second.entry, first.entry],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
            &resolver,
        );

        assert_eq!(
            sorted.len(),
            2,
            "one instrument with two listings is two rows"
        );
        assert!(
            sorted.iter().all(|row| row.display.as_ref().is_none()),
            "neither listing has a published name to be drawn by"
        );
        assert_eq!(
            sorted[0].entry.instrument.id(),
            sorted[1].entry.instrument.id(),
            "the two rows are two listings of one instrument"
        );
        assert_ne!(
            sorted[0].entry.listing.key, sorted[1].entry.listing.key,
            "the two rows are two distinct listing identities"
        );
        assert_eq!(
            row_keys(&sorted),
            keys,
            "the order of two unnamed listings is their typed keys'"
        );
    }

    #[gpui_kit::test]
    fn preparation_should_apply_the_predicate_before_a_selection_is_resolved(
        cx: &mut TestAppContext,
    ) {
        let binance_spot = test_support::spot_route();
        let refused = entry(
            &binance_spot,
            "ETHUSDT",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let linear = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![refused.clone(), linear.clone()]);
        let predicate: EntryPredicate = Rc::new(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == "BTCUSDT")
        });

        let rows = fixture_rows(
            &fixture,
            vec![refused.clone(), linear.clone()],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            Some(&predicate),
        );

        // The pair the predicate refuses is filtered out, so this route no longer
        // offers it as a selection, while the accepted one is still offered.
        assert_eq!(
            selected_listing_if_available(&rows, Some(&refused.listing.key)),
            None
        );
        assert_eq!(
            selected_listing_if_available(&rows, Some(&linear.listing.key)),
            Some(&linear.listing.key)
        );
    }

    /// The single matcher column answers every spelling of one listing: the
    /// published name, the venue's own symbol and the typed aliases of its display.
    #[gpui_kit::test]
    fn the_matcher_column_should_carry_the_name_the_native_symbol_and_the_aliases(
        cx: &mut TestAppContext,
    ) {
        let binance_spot = test_support::spot_route();
        let listed = test_support::entry_with_aliases(
            &binance_spot,
            spot("BTC", "USDT"),
            "BTCUSDT",
            &["BTC-USDT"],
        );
        let fixture = published(cx, vec![listed.clone()]);

        let rows = fixture_rows(
            &fixture,
            vec![listed],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        let column = rows
            .first()
            .expect("the fixture published one listing")
            .matcher_column();

        assert_eq!(rows.len(), 1, "one listing is one row and one column");
        for query in ["BTC/USDT", "BTCUSDT", "btc-usdt"] {
            assert!(
                matcher_hits(&column, query),
                "the one matcher column should answer `{query}`: {column}"
            );
        }
        assert!(
            !matcher_hits(&column, "ETHUSDT"),
            "a listing this route does not publish is not a hit: {column}"
        );
    }

    /// The visual column is the published name and nothing else: the
    /// native symbol, the listing identity and the economic identity digest of the
    /// same listing are search and diagnostic values, never the drawn name.
    #[gpui_kit::test]
    fn the_visual_column_should_draw_the_published_name(cx: &mut TestAppContext) {
        let binance_spot = test_support::spot_route();
        let listed = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![listed.clone()]);
        let rows = fixture_rows(
            &fixture,
            vec![listed.clone()],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        let label = cx.update(|cx| row_label(&rows[0], cx));

        assert_eq!(label.as_ref(), "BTC/USDT");
        assert!(
            !label.contains(listed.listing.key.listing_id.as_str())
                && !label.contains("v1/")
                && !label.contains("BTCUSDT"),
            "the row is drawn by the display, not by a native symbol, a listing id or a digest: {label}"
        );
    }

    /// The rows are ordered by the published name, which is not the
    /// order of the venue's own symbols: a symbol is a protocol spelling of a
    /// listing, not its name.
    #[gpui_kit::test]
    fn rows_should_be_ordered_by_the_published_name(cx: &mut TestAppContext) {
        let binance_spot = test_support::spot_route();
        let btc = entry(
            &binance_spot,
            "ZZZBTC",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let eth = entry(
            &binance_spot,
            "AAAETH",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let sol = entry(
            &binance_spot,
            "MMMSOL",
            spot("SOL", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![btc.clone(), eth.clone(), sol.clone()]);

        let rows = fixture_rows(
            &fixture,
            vec![eth, sol, btc],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        let mut typed_key_order = row_keys(&rows);
        typed_key_order.sort();

        assert_eq!(
            row_names(&rows),
            vec!["BTC/USDT", "ETH/USDT", "SOL/USDT",],
            "the rows are ordered by the published name"
        );
        assert_eq!(
            rows[0].route_order.as_str(),
            "binance/spot",
            "the route segment of the order is the canonical identity, which no language moves"
        );
        assert_ne!(
            row_keys(&rows),
            typed_key_order,
            "the fixture is discriminating: its order is not the typed keys' either"
        );
    }

    /// A row whose generation published no name has no name to sort by: it follows
    /// the named rows and is ordered by its own typed key, whose route segment is
    /// the user-visible form.
    #[test]
    fn rows_without_a_published_name_should_be_ordered_by_their_typed_key() {
        let binance_spot = route("binance", "spot");
        let binance_perp = route("binance", "usd-m-perp");
        let okx_spot = route("okx", "spot");
        let mut rows = vec![
            unnamed_row(
                &okx_spot,
                "BTCUSDT",
                spot("BTC", "USDT"),
                ListingStatus::Trading,
            ),
            unnamed_row(
                &binance_perp,
                "BTCUSDT",
                spot("BTC", "USDT"),
                ListingStatus::Trading,
            ),
            unnamed_row(
                &binance_spot,
                "ETHUSDT",
                spot("ETH", "USDT"),
                ListingStatus::Trading,
            ),
        ];

        rows.sort_by(compare_rows);

        assert_eq!(
            rows.iter()
                .map(|row| row.route_order.as_str())
                .collect::<Vec<_>>(),
            vec!["binance/spot", "binance/usd-m-perp", "okx/spot"],
            "the route segment of the order is the canonical identity"
        );
        assert_eq!(
            row_keys(&rows),
            vec![
                listing_key(&binance_spot, "ETHUSDT"),
                listing_key(&binance_perp, "BTCUSDT"),
                listing_key(&okx_spot, "BTCUSDT"),
            ],
            "two rows no name tells apart are ordered by their own typed key"
        );
    }

    /// The route segment of the fixed order decides for two rows the primary key
    /// cannot tell apart.
    ///
    /// Two published listings of one route never share a listing id, so what this
    /// pins is the shape of the fixed order — `(display, canonical route, listing
    /// id)` rather than `(display, listing id)`.
    #[test]
    fn the_route_segment_should_decide_for_rows_the_display_cannot_tell_apart() {
        let binance_spot = route("binance", "spot");
        let listed = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let binance = ordered_under(&listed, "binance/spot");
        let okx = ordered_under(&listed, "okx/spot");

        assert_eq!(
            compare_rows(&binance, &okx),
            Ordering::Less,
            "the canonical route orders two otherwise equal rows"
        );
        assert_eq!(compare_rows(&okx, &binance), Ordering::Greater);
    }

    /// A listing the generation published no name for is still a row of its own:
    /// it is drawn as the localized unknown, keeps its typed key, and follows the
    /// rows a reader can read.
    #[gpui_kit::test]
    fn a_listing_without_a_published_name_should_be_drawn_as_the_localized_unknown(
        cx: &mut TestAppContext,
    ) {
        let binance_spot = test_support::spot_route();
        let listed = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let unpublished = entry(
            &binance_spot,
            "ETHUSDT",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![listed.clone()]);
        let rows = fixture_rows(
            &fixture,
            vec![unpublished.clone(), listed],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );

        assert_eq!(
            rows[0].display.as_ref().map(|name| name.as_str()),
            Some("BTC/USDT"),
            "a named row is drawn by its name and comes first"
        );
        assert_eq!(
            rows[1].entry.listing.key, unpublished.listing.key,
            "the listing without a published name is still its own row"
        );
        assert!(
            rows[1].display.is_none(),
            "no name is invented for a listing the generation did not publish one for"
        );

        let english = cx.update(|cx| row_label(&rows[1], cx));
        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));
        let chinese = cx.update(|cx| row_label(&rows[1], cx));

        assert_ne!(
            english, chinese,
            "the fallback is the localized unknown rather than a fixed spelling"
        );
        assert!(
            !english.contains("ETHUSDT")
                && !english.contains(rows[1].entry.listing.key.listing_id.as_str())
                && !chinese.contains("ETHUSDT"),
            "the fallback never names the listing by its symbol or its id"
        );
    }

    /// Every name a list of rows is drawn by comes from one published generation:
    /// the selector captures one generation per load, so a publication that renames
    /// a listing does not rename the rows that were read before it.
    #[gpui_kit::test]
    fn every_row_should_be_named_from_one_generation(cx: &mut TestAppContext) {
        let binance_spot = test_support::spot_route();
        let btc = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let eth = entry(
            &binance_spot,
            "ETHUSDT",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![btc.clone(), eth.clone()]);
        let rows = fixture_rows(
            &fixture,
            vec![btc.clone(), eth],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        let read: Vec<Option<SmolStr>> = rows
            .iter()
            .map(|row| row.display.as_ref().map(|name| SmolStr::new(name.as_str())))
            .collect();
        assert_eq!(
            read,
            vec![
                Some(SmolStr::new_static("BTC/USDT")),
                Some(SmolStr::new_static("ETH/USDT"))
            ],
            "the fixture published the two names the rows were read from"
        );

        // The same key is listed against another quote asset, so the generation that
        // is published from here on names it differently.
        test_support::publish_fixture_route(
            fixture.catalog(),
            &binance_spot,
            vec![entry(
                &binance_spot,
                "BTCUSDT",
                spot("BTC", "USDC"),
                ListingStatus::Trading,
            )],
        )
        .expect("the second generation should publish");

        assert_eq!(
            fixture
                .catalog()
                .cached_listing_instrument(&btc.listing.key)
                .expect("the second generation names the listing")
                .as_str(),
            "BTC/USDC",
            "the published generation renamed the pair"
        );
        assert_eq!(
            read,
            vec![
                Some(SmolStr::new_static("BTC/USDT")),
                Some(SmolStr::new_static("ETH/USDT"))
            ],
            "the rows a reader already has keep the names they were read from"
        );

        // A list built now reads the generation that is published now, so the two
        // sets of rows really do come from two generations.
        let fresh = fixture_rows(
            &fixture,
            vec![entry(
                &binance_spot,
                "BTCUSDT",
                spot("BTC", "USDC"),
                ListingStatus::Trading,
            )],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        assert_eq!(
            fresh
                .iter()
                .map(|row| row.display.as_ref().map(|name| SmolStr::new(name.as_str())))
                .collect::<Vec<_>>(),
            vec![Some(SmolStr::new_static("BTC/USDC"))],
            "a row read after the republication carries the name of that generation"
        );
    }

    /// Reading what a generation published is a snapshot read: a selector names its
    /// rows without fetching a catalog, so opening one on a route nobody loaded
    /// draws the localized unknown instead of waiting for a network round trip.
    #[gpui_kit::test]
    fn reading_the_display_of_a_listing_should_not_fetch_its_catalog(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test runtime should build");
        });
        let binance_spot = test_support::spot_route();
        let listed = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let (gate, source) = test_support::gated(vec![listed.clone()]);
        let fixture =
            test_support::Fixture::with_source(binance_spot, test_support::SPOT_SOURCE, source);
        // The catalogue is installed into the global connector, exactly as a running
        // application installs it, so it carries the application runtime: a read that
        // fetched behind its caller's back would have somewhere to run and would show
        // up in the fetch count below.
        cx.update(|cx| {
            market_participants_connector::test_support::install_root(
                cx,
                Arc::clone(fixture.catalog_arc()),
            );
        });

        let rows = fixture_rows(
            &fixture,
            vec![listed],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );

        assert_eq!(rows.len(), 1, "the accepted catalog is still a row");
        assert!(
            rows[0].display.is_none(),
            "a route nobody loaded states no name"
        );
        assert_eq!(
            gate.calls(),
            0,
            "naming a row reads the published snapshot and never fetches"
        );
    }

    /// A confirmation reports the typed key the row stands for, whatever the row was
    /// drawn by: the display is a name of the current generation, and the key is the
    /// identity the caller's pane follows.
    #[gpui_kit::test]
    fn confirmation_should_report_the_key_the_row_stands_for(cx: &mut TestAppContext) {
        let binance_spot = test_support::spot_route();
        let named = entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        );
        let unpublished = entry(
            &binance_spot,
            "ETHUSDT",
            spot("ETH", "USDT"),
            ListingStatus::Trading,
        );
        let fixture = published(cx, vec![named.clone()]);
        let rows = fixture_rows(
            &fixture,
            vec![named, unpublished.clone()],
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            None,
        );
        // The named row is the first of the two, so the row this test confirms is not
        // the one a confirmation answering with the ranking's own first row would
        // report.
        let chosen = rows.get(1).expect("the fixture offers two rows");
        let drawn = cx.update(|cx| row_label(chosen, cx));

        assert_eq!(
            chosen.entry.listing.key, unpublished.listing.key,
            "the second row is the listing the generation published no name for"
        );
        assert_ne!(
            drawn.as_ref(),
            unpublished.listing.key.listing_id.as_str(),
            "the row is not drawn by the identity it reports"
        );
        assert_eq!(
            confirmed_selection(
                &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
                &rows,
                Some(&unpublished.listing.key),
                CONFIRMED_AT
            ),
            Ok(InstrumentSelection {
                listing: unpublished.listing.key
            }),
            "a confirmation reports the original key, not the text the row was drawn by"
        );
    }

    /// The unknown listing label is defined by every locale, so a row without a
    /// published name never reaches a reader as an empty line.
    #[test]
    fn the_unknown_listing_label_should_be_defined_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales should load");

        assert!(
            i18n.languages().len() > 1,
            "the fallback is a localized message and not a fixed spelling"
        );
        for language in i18n.languages() {
            assert!(
                i18n.try_translate(language, UNKNOWN_LISTING_KEY)
                    .is_some_and(|text| !text.is_empty()),
                "the unknown listing label should render in {language}"
            );
        }
    }

    #[test]
    fn selection_should_survive_while_the_route_still_offers_it() {
        let binance_spot = route("binance", "spot");
        let preferred = listing_key(&binance_spot, "ETHUSDT");
        let rows = [
            unnamed_row(
                &binance_spot,
                "BTCUSDT",
                spot("BTC", "USDT"),
                ListingStatus::Trading,
            ),
            unnamed_row(
                &binance_spot,
                "ETHUSDT",
                spot("ETH", "USDT"),
                ListingStatus::Trading,
            ),
        ];

        assert_eq!(
            selected_listing_if_available(&rows, Some(&preferred)),
            Some(&preferred)
        );
    }

    /// A venue switch is where the caller's listing most often has no counterpart:
    /// the route opens with nothing selected rather than with a listing of its own
    /// choosing.
    #[test]
    fn selection_should_be_cleared_when_a_venue_switch_leaves_it_behind() {
        let hyperliquid = route("hyperliquid", "core-spot");
        let rows = [
            unnamed_row(
                &hyperliquid,
                "PURRUSDC",
                spot("PURR", "USDC"),
                ListingStatus::Trading,
            ),
            unnamed_row(
                &hyperliquid,
                "HYPEUSDC",
                spot("HYPE", "USDC"),
                ListingStatus::Trading,
            ),
        ];
        let previous = listing_key(&route("binance", "spot"), "BTCUSDT");

        assert_eq!(selected_listing_if_available(&rows, Some(&previous)), None);
    }

    /// A route nothing has loaded leaves the list in its own state and says so: the
    /// listing list is read from memory alone, and "this market is not loaded" is
    /// not "this market lists nothing".
    #[gpui_kit::test]
    fn a_route_with_no_loaded_catalog_should_report_that_state(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let (translations, language) = cx.update(|_, cx| {
            let i18n = cx.global::<I18n>();
            let language = *cx.global::<LanguageCode>();
            let translations: Vec<gpui_kit::SharedString> = i18n
                .languages()
                .into_iter()
                .map(|candidate| i18n.translate(candidate, "instrument_selector.market_not_loaded"))
                .collect();
            (translations, language)
        });
        assert!(
            translations.iter().all(|text| {
                !text.is_empty() && text.as_ref() != "instrument_selector.market_not_loaded"
            }),
            "every embedded locale defines the state's message: {translations:?}"
        );
        assert_eq!(language, LanguageCode::En);

        cx.update(|window, cx| {
            let filter_input =
                cx.new(|cx| InputStateBuilder::new(FILTER_INPUT_ID).build(window, cx));
            let selector = cx.new(|cx| {
                InstrumentSelector::new(
                    cx.global::<MarketParticipantsConnector>().market(),
                    route("binance", "spot"),
                    None,
                    filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
                    filter_input,
                    Rc::new(|_, _, _| {}),
                    cx,
                )
            });

            selector.update(cx, |selector, cx| {
                selector.reload_instruments(cx);

                assert!(
                    matches!(selector.list_state, InstrumentListState::NotLoaded),
                    "a route with no published snapshot is not a catalog of no listings"
                );
            });
        });
    }

    /// A listing the current catalog does not offer is dropped from the selection:
    /// what a reader confirmed is never kept against a catalog that does not list
    /// it.
    #[gpui_kit::test]
    fn a_catalog_with_no_offered_listing_should_clear_the_selection(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let binance_spot = route("binance", "spot");
        let catalog: Arc<[CatalogEntry]> = vec![entry(
            &binance_spot,
            "BTCUSDT",
            spot("BTC", "USDT"),
            ListingStatus::Trading,
        )]
        .into();

        cx.update(|window, cx| {
            let filter_input =
                cx.new(|cx| InputStateBuilder::new(FILTER_INPUT_ID).build(window, cx));
            let selector = cx.new(|cx| {
                InstrumentSelector::new(
                    cx.global::<MarketParticipantsConnector>().market(),
                    binance_spot.clone(),
                    Some(listing_key(&binance_spot, "BTCUSDT")),
                    InstrumentFilters {
                        kind_filter: InstrumentKindFilter::All,
                        status_filter: InstrumentStatusFilter::Any,
                        required_capability: CapabilityId::Candlesticks,
                        entry_predicate: Some(Rc::new(|_| false)),
                        routes: registry(),
                    },
                    filter_input,
                    Rc::new(|_, _, _| {}),
                    cx,
                )
            });

            selector.update(cx, |selector, _| {
                assert!(
                    selector.selected_listing.is_some(),
                    "the fixture opens with the listing under test"
                );
                selector.apply_catalog(&binance_spot, catalog);

                assert!(
                    matches!(selector.list_state, InstrumentListState::Empty),
                    "a market that answered with nothing this selector offers is empty, not failed"
                );
                assert!(
                    selector.selected_listing.is_none(),
                    "a listing the catalog does not offer does not stay selected"
                );
            });
        });
    }

    /// Opening the selector with nothing selected is how a chart asks the user to
    /// choose. Answering for them would select a listing nobody asked for.
    #[test]
    fn selection_should_not_be_invented_for_a_selector_opened_without_one() {
        let binance_spot = route("binance", "spot");
        let rows = [
            unnamed_row(
                &binance_spot,
                "BTCUSDT",
                spot("BTC", "USDT"),
                ListingStatus::Trading,
            ),
            unnamed_row(
                &binance_spot,
                "ETHUSDT",
                spot("ETH", "USDT"),
                ListingStatus::Trading,
            ),
        ];

        assert_eq!(selected_listing_if_available(&rows, None), None);
    }

    /// The column is the registered routing: a venue is a row, and a scope is a row
    /// under it that opens the route it names.
    #[gpui_kit::test]
    fn the_column_should_hold_every_venue_with_the_scopes_under_it(cx: &mut TestAppContext) {
        let column = column(
            cx,
            &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
        );

        assert_eq!(
            drawn(&column, cx),
            vec![
                "binance".to_owned(),
                "binance/spot".to_owned(),
                "binance/usd-m-perp".to_owned(),
                "okx".to_owned(),
                "okx/spot".to_owned(),
            ]
        );
    }

    /// A venue whose registered scopes do not declare the capability the caller needs
    /// is not a row: a fold that opens on nothing is not offered at all.
    #[gpui_kit::test]
    fn a_venue_without_an_offered_scope_should_not_be_a_row(cx: &mut TestAppContext) {
        let column = column(
            cx,
            &filters(InstrumentKindFilter::All, CapabilityId::ForceOrders),
        );

        assert_eq!(
            drawn(&column, cx),
            vec!["binance".to_owned(), "binance/usd-m-perp".to_owned()]
        );
    }

    /// A scope row opens the route it is named after, and nothing else does: a venue
    /// is a row that folds, and a route the filters do not offer is not a row at all.
    #[test]
    fn a_scope_row_should_open_the_route_it_is_named_after() {
        let all = filters(InstrumentKindFilter::All, CapabilityId::Candlesticks);
        let perp = route("binance", "usd-m-perp");

        assert_eq!(
            route_of_row(scope_row_name(perp.clone()).as_str(), &all),
            Some(perp)
        );
        assert_eq!(
            route_of_row(venue_row_name(&venue("binance")).as_str(), &all),
            None
        );
        assert_eq!(
            route_of_row(
                scope_row_name(route("okx", "swap")).as_str(),
                &filters(InstrumentKindFilter::All, CapabilityId::ForceOrders),
            ),
            None
        );
    }

    /// The column a selector with these filters would build, as the tree flattens it.
    ///
    /// The translator is an application-wide global installed exactly once, so a test
    /// building two columns installs it once and builds the second with [`column_over`].
    fn column(cx: &mut TestAppContext, filters: &InstrumentFilters) -> Entity<TreeState> {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });
        column_over(cx, filters)
    }

    /// The same column over the translator [`column`] already installed.
    fn column_over(cx: &mut TestAppContext, filters: &InstrumentFilters) -> Entity<TreeState> {
        cx.update(|cx| {
            let rows = venue_rows(filters, cx);

            cx.new(|cx| TreeState::new(cx).items(rows))
        })
    }

    /// The rows of a column as the tree draws them: the name of each.
    fn drawn(column: &Entity<TreeState>, cx: &TestAppContext) -> Vec<String> {
        cx.read(|cx| {
            let column = column.read(cx);

            (0..)
                .map_while(|index| column.entry(index))
                .map(|entry| entry.item().id.to_string())
                .collect()
        })
    }

    /// The rows of a column as a reader reads them: the text of each.
    fn drawn_labels(column: &Entity<TreeState>, cx: &TestAppContext) -> Vec<String> {
        cx.read(|cx| {
            let column = column.read(cx);

            (0..)
                .map_while(|index| column.entry(index))
                .map(|entry| entry.item().label.to_string())
                .collect()
        })
    }

    /// The text a reader sees for a row is the half of the route that
    /// row stands for — `Binance` above, `SPOT` under it — while the row's own
    /// identity — the name [`route_of_row`] reads back — stays the whole canonical
    /// lower-case route.
    ///
    /// A scope row does not repeat the venue it hangs under: the fold above it is
    /// what states the venue.
    #[gpui_kit::test]
    fn the_column_should_draw_the_venue_above_its_scopes(cx: &mut TestAppContext) {
        let column = column(
            cx,
            &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
        );

        assert_eq!(
            drawn_labels(&column, cx),
            vec![
                "Binance".to_owned(),
                "SPOT".to_owned(),
                "USD-M-PERP".to_owned(),
                "OKX".to_owned(),
                "SPOT".to_owned(),
            ],
            "a venue row reads as the venue, and each scope under it as that scope alone"
        );
        assert_eq!(
            drawn(&column, cx),
            vec![
                "binance".to_owned(),
                "binance/spot".to_owned(),
                "binance/usd-m-perp".to_owned(),
                "okx".to_owned(),
                "okx/spot".to_owned(),
            ],
            "the identity of a row stays the canonical route the tree reports"
        );
    }

    /// The upper-case label of a scope row is derived from the scope the row opens,
    /// so a scope of several segments reads as all of them — `PERP-DEX/XYZ` rather
    /// than its last segment.
    #[gpui_kit::test]
    fn a_multi_segment_scope_should_read_as_its_whole_uppercase_scope(cx: &mut TestAppContext) {
        let column = column(
            cx,
            &InstrumentFilters {
                kind_filter: InstrumentKindFilter::All,
                status_filter: InstrumentStatusFilter::Any,
                required_capability: CapabilityId::Candlesticks,
                entry_predicate: None,
                routes: loaded(vec![scope(
                    route("hyperliquid", "perp-dex/xyz"),
                    &[CapabilityId::Candlesticks],
                )]),
            },
        );

        assert_eq!(
            drawn_labels(&column, cx),
            vec!["Hyperliquid".to_owned(), "PERP-DEX/XYZ".to_owned()],
            "every segment of the scope is uppercased"
        );
    }

    /// The selector is what closes the dialog it is shown in: the control that leaves it without
    /// choosing raises the signal its dialog subscribes to, so the content is off the window once
    /// the exit transition is over, without anything outside it dismissing the layer.
    #[gpui_kit::test]
    fn the_cancel_control_should_close_the_dialog_the_selector_is_shown_in(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            assert!(
                InstrumentSelector::show_with_filters(
                    None,
                    filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
                    Rc::new(|_, _, _| {}),
                    window,
                    cx,
                ),
                "a selector with a route its filters offer should be presented"
            );
        });
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_some(),
            "the selector should be the window's dialog"
        );

        let cancel = cx
            .debug_bounds(CANCEL_SELECTOR)
            .expect("the dialog should offer a way out of it");
        cx.simulate_click(cancel.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "the selector's own control should have taken its dialog off the window once the exit \
             transition was over"
        );
    }

    /// A registry with no offered route leaves the caller's own state alone instead of
    /// presenting a dialog that cannot be confirmed.
    #[gpui_kit::test]
    fn a_selector_without_a_route_should_not_be_presented(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            assert!(
                !InstrumentSelector::show_with_filters(
                    None,
                    InstrumentFilters {
                        kind_filter: InstrumentKindFilter::All,
                        status_filter: InstrumentStatusFilter::Any,
                        required_capability: CapabilityId::Candlesticks,
                        entry_predicate: None,
                        routes: empty_registry(),
                    },
                    Rc::new(|_, _, _| {}),
                    window,
                    cx,
                ),
                "a registry with no offered route has nothing to select from"
            );
        });
    }

    /// A registration view that offers no route leaves the caller with no selector and the
    /// window with no dialog: the entry point is what turns that into the "load a market"
    /// alert, and the selector itself must not open on a column that would be empty.
    ///
    /// The column of such a view holds no row either — the wiring is pinned once, through the
    /// view a caller reads off the registry, rather than stated twice.
    #[gpui_kit::test]
    fn an_empty_registration_view_should_leave_the_selector_closed(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let registration = empty_registry();

        cx.update(|window, cx| {
            assert!(
                !InstrumentSelector::show_with_filters(
                    None,
                    InstrumentFilters {
                        kind_filter: InstrumentKindFilter::All,
                        status_filter: InstrumentStatusFilter::Any,
                        required_capability: CapabilityId::Candlesticks,
                        entry_predicate: None,
                        routes: Arc::clone(&registration),
                    },
                    Rc::new(|_, _, _| {}),
                    window,
                    cx,
                ),
                "a caller with no registered route has nothing to select from"
            );
        });
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "a refused selector must not have been presented"
        );
        assert!(
            cx.update(|_, cx| {
                venue_rows(
                    &InstrumentFilters {
                        kind_filter: InstrumentKindFilter::All,
                        status_filter: InstrumentStatusFilter::Any,
                        required_capability: CapabilityId::Candlesticks,
                        entry_predicate: None,
                        routes: registration,
                    },
                    cx,
                )
                .is_empty()
            }),
            "the column of an empty registration view has no venue to list"
        );
    }

    /// The column is the caller's registration view and nothing else: a venue the caller
    /// does not register is not a row, however many routes the build declares for it.
    ///
    /// The other venue of the fixture is the one the two columns differ in — the same filter
    /// lists its scope once it is registered, and
    /// `the_column_should_hold_every_venue_with_the_scopes_under_it` pins that it is a row at
    /// all — so what narrows the column here is the caller's own view and not a route the
    /// column could never build.
    #[gpui_kit::test]
    fn an_unregistered_venue_should_not_be_a_row(cx: &mut TestAppContext) {
        let binance_only = loaded(vec![
            scope(route("binance", "spot"), &[CapabilityId::Candlesticks]),
            scope(
                route("binance", "usd-m-perp"),
                &[CapabilityId::Candlesticks],
            ),
        ]);
        let one_venue = column(
            cx,
            &InstrumentFilters {
                kind_filter: InstrumentKindFilter::All,
                status_filter: InstrumentStatusFilter::Any,
                required_capability: CapabilityId::Candlesticks,
                entry_predicate: None,
                routes: Arc::clone(&binance_only),
            },
        );
        let several_venues = column_over(
            cx,
            &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
        );

        assert_eq!(
            drawn(&one_venue, cx),
            vec![
                "binance".to_owned(),
                "binance/spot".to_owned(),
                "binance/usd-m-perp".to_owned()
            ],
            "only the registered venue is listed"
        );
        assert!(
            !drawn(&one_venue, cx)
                .iter()
                .any(|row| row.starts_with("okx")),
            "a venue the caller did not register is not a row"
        );
        assert!(
            drawn(&several_venues, cx)
                .iter()
                .any(|row| row == "okx/spot"),
            "the fixture is discriminating: the same filter lists okx once it is registered"
        );
    }

    /// A route whose catalog is present but holds nothing is a market that answered with
    /// nothing: the list says it is empty, which is neither "not loaded" nor a failure.
    #[gpui_kit::test]
    fn an_empty_catalog_should_leave_the_list_empty(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let binance_spot = route("binance", "spot");

        cx.update(|window, cx| {
            test_support::publish_fixture_catalogs(cx, &[(binance_spot.clone(), Vec::new())]);
            let published = cx
                .global::<MarketParticipantsConnector>()
                .catalog()
                .cached_listings(&binance_spot)
                .expect("the fixture snapshot is in memory");
            assert!(published.is_empty(), "the fixture catalog really is empty");

            let filter_input =
                cx.new(|cx| InputStateBuilder::new(FILTER_INPUT_ID).build(window, cx));
            let selector = cx.new(|cx| {
                InstrumentSelector::new(
                    cx.global::<MarketParticipantsConnector>().market(),
                    binance_spot.clone(),
                    Some(listing_key(&binance_spot, "BTCUSDT")),
                    filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
                    filter_input,
                    Rc::new(|_, _, _| {}),
                    cx,
                )
            });

            selector.update(cx, |selector, cx| {
                selector.reload_instruments(cx);

                assert!(
                    matches!(selector.list_state, InstrumentListState::Empty),
                    "a loaded catalog that lists nothing is empty, not unloaded and not a failure"
                );
                assert!(
                    selector.selected_listing.is_none(),
                    "a listing the empty catalog does not hold does not stay selected"
                );
            });
        });
    }

    /// A window whose root is a [`Layer`], the way the application mounts one, with the globals the
    /// selector needs installed and a connector whose routes never answer.
    fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let fixture = test_support::Fixture::unanswering();
            market_participants_connector::test_support::install_root(
                cx,
                Arc::clone(fixture.catalog_arc()),
            );
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        cx
    }

    /// Lets the dialog's own fade finish and repaints, so what is drawn is what has settled: a
    /// dialog in its first frames is painted at zero opacity and cannot be told from one that stays.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}
