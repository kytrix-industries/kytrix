//! The force-order pane: the stream it reads and the table it draws.
//!
//! The pane is the `WidgetView` the framework mounts: [`ForceOrderWidget::new`] builds
//! its collaborators and signals, [`load`](WidgetView::load) checks the directory and
//! starts the stream, [`body`](WidgetView::body) builds the table once, and
//! [`sync`](WidgetView::sync) drains what the binding delivered and hands the accepted
//! rows to the table.
//!
//! # The table is built once and kept up to date
//!
//! The cells are built by `kit::table`'s row factory, which runs **without an
//! application context**: a cell cannot read the theme, the settings snapshot, or the
//! translation context while it is built. So the factory builds text nodes whose text
//! and colour are `bind_text` / `bind_style` closures over the signals the widget
//! owns — the accepted rows, the settings snapshot (for the display time zone), and
//! the language (for the route and the localized unknown listing). The rows themselves
//! reach the table through [`Table::set_rows`], which is what rebuilds the row window
//! when the accepted snapshot grows, shrinks, or is cleared.
//!
//! # The table is display-only
//!
//! The old dataset was a table to read, not a table to pick: it had no selection state,
//! no click handling and no selected-row styling. The kit table defaults to
//! multi-select, so the pane turns selection off explicitly in `table_options`; a
//! test pins that, so the interaction is not added by accident.
//!
//! # The directory
//!
//! Which markets report liquidations, and whether one of them is loaded, is the
//! injected directory's answer — the connector in the application, a fixture in a
//! test. The pane never loads the directory and never polls the catalogue: it re-reads
//! the answer on every load and re-runs the load when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation) signal
//! moves (`crates/widgets/AGENTS.md` §8.3).
//!
//! # A generation is one number for the whole directory
//!
//! The signal says *that* the directory published a new generation, not *which venue*
//! changed (§8.3 has no per-venue version). So **any** write reloads this pane, even a
//! commit that only touched a venue it does not read: the rows are dropped and the
//! stream is reopened against the new catalogue. That is the accepted trade — a row
//! names and writes its listing at the generation its stream resolved against, so
//! keeping rows across a generation the pane cannot attribute would be showing data
//! from a retired catalogue — and the price is a visible reload on unrelated venue
//! commits. Narrowing it needs the composition root to publish the scope of a change,
//! which the frozen contract does not offer. The test
//! `an_unrelated_directory_generation_should_reload_the_pane` pins the current
//! behaviour.
//!
//! # The table's static copy
//!
//! The kit table takes its column headers and its empty-state sentence as resolved
//! text when it is created. A language switch therefore rebuilds the table (and only
//! the table: the rest of the body redraws through `bind_text`), which is the one
//! place this pane cannot express a language dependency as a binding.

use std::{mem, rc::Rc, sync::Arc};

use app_config::{
    settings::Config,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use base_ui::{
    kit::{
        list::SelectionMode,
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
    catalog::store::Catalog, connector::MarketParticipantsConnector, market::error::MarketError,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, market::units::PositionSide,
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use time::UtcOffset;
use ui_core::{
    a11y::A11yPatch,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::TextStyle,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::is_catalog_not_loaded,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    venue::{VenueDisplay, joined_names, route_display_string},
    view::{ReloadHandle, WidgetView},
};

#[cfg(test)]
use super::service::ForceOrderSource;
use super::{
    model::{ForceOrderRow, ForceOrderSnapshot, MAX_FORCE_ORDERS},
    service::{
        ForceOrderProtocol, ForceOrderRequest, ForceOrderResponse, ForceOrderService,
        ForceOrderStatus, ForceOrderStreamKey, ForceOrderStreamMessage,
    },
};

/// The number of columns the dataset has: route, listing, price, quantity, time.
const COLUMN_COUNT: usize = 5;

/// The width weight of each column, in header order.
///
/// The kit table distributes the remaining width by these weights, which is the
/// engine's spelling of the relative widths the dataset used to declare. The listing
/// column is the widest because a listing name is the longest value a row carries.
const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.15, 0.35, 0.175, 0.175, 0.15];

/// The alignment of each column, in header order: text on the left, numbers on the right.
const COLUMN_ALIGNS: [ColumnAlign; COLUMN_COUNT] = [
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
];

/// Force orders expose no persisted options yet.
///
/// The config exists so the persisted schema version is owned by the widget
/// rather than inferred from the absence of a config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceOrderConfig;

impl WidgetConfig for ForceOrderConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The loaded market directory one pane reads.
///
/// Production wraps the injected connector; a test drives a fixture, because the
/// framework's headless host cannot build a connector (it still needs the old
/// toolkit's runtime). The pane only asks the three questions a load needs: which
/// routes report liquidations, whether one of them is loaded, and the catalogue
/// generation a new stream resolves its listings against.
trait MarketDirectory {
    /// The routes whose market can report liquidations, in the directory's order.
    fn liquidation_routes(&self) -> Vec<VenueRoute>;

    /// Whether one route's market is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// The published catalogue a new stream resolves names and increments against.
    fn catalog(&self) -> Arc<Catalog>;
}

/// The application directory: the injected market-participants connector.
struct ConnectorDirectory(MarketParticipantsConnector);

impl MarketDirectory for ConnectorDirectory {
    fn liquidation_routes(&self) -> Vec<VenueRoute> {
        loaded_routes_carrying(&self.0, CapabilityId::ForceOrder)
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }

    fn catalog(&self) -> Arc<Catalog> {
        self.0.catalog_arc()
    }
}

/// The loaded routes that carry one capability, in the order the venues answered them.
///
/// One question is asked of the loaded venues — which scopes carry the capability — so
/// a pane that reads a whole market never enumerates venues of its own. The route and
/// the entry it is judged by come from one reading of one commit: `entries()` takes the
/// venue table's lock once, so a load landing while this runs cannot put one commit's
/// route beside another's capabilities.
fn loaded_routes_carrying(
    connector: &MarketParticipantsConnector,
    capability: CapabilityId,
) -> Vec<VenueRoute> {
    connector
        .venues()
        .entries()
        .iter()
        .filter(|(_, entry)| {
            entry
                .capabilities
                .iter()
                .any(|support| support.capability == capability)
        })
        .map(|(route, _)| route.clone())
        .collect()
}

/// Whether the routes the pane needs are a different set from the ones it holds.
///
/// The pane reads one stream over a *set* of routes, so a set that changed — every
/// scope a venue was loaded with, one a shrinking commit took away — is a different
/// stream rather than a repeat of the one being read. Compare as `(venue, scope)`
/// pairs: two sets that name the same markets in a different order are the same stream.
fn routes_changed(held: &[VenueRoute], needed: &[VenueRoute]) -> bool {
    held.len() != needed.len() || !needed.iter().all(|route| held.contains(route))
}

/// The markets a notice names while nothing is loaded: the venues whose loader can
/// report liquidations, in the order the static definition table answers.
///
/// This is the one static answer a pane may read: the capability's venue name set,
/// never a scope, an origin or a credential. It is what says which markets could report
/// liquidations at all, which is the only honest answer while nothing is loaded.
fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::ForceOrder)
}

/// The name keys of the venues [`market_venues`] answers, for the table's caption.
fn market_venue_name_keys() -> Vec<&'static str> {
    market_venues().iter().map(VenueDisplay::name_key).collect()
}

/// The row a table row index displays: the newest accepted order first.
///
/// The table draws the retained orders with the newest at the top, so its row index
/// counts backwards through the snapshot's chronological order.
fn displayed_row(snapshot: &ForceOrderSnapshot, row: usize) -> Option<&ForceOrderRow> {
    let index = snapshot.len().checked_sub(row.checked_add(1)?)?;
    snapshot.orders().get(index).map(|row| &**row)
}

/// The text one cell draws, from the row and column it was built for.
///
/// The route is written with the one formatter every route a reader sees is rendered
/// by, not with the canonical lower-case identity that belongs in logs; the listing is
/// the name the published generation gave it, or the localized unknown when the
/// generation named none. A cell that has no row (the table was asked for a row the
/// accepted snapshot no longer holds) draws nothing.
fn cell_text(
    row: Option<&ForceOrderRow>,
    column: usize,
    zone: UtcOffset,
    translate: &Translate,
) -> SmolStr {
    let Some(row) = row else {
        return SmolStr::default();
    };

    match column {
        0 => route_display_string(&row.order().listing.route, translate),
        1 => row
            .listing_label()
            .clone()
            .unwrap_or_else(|| t_smol!(translate, "error.display.unknown_listing")),
        2 => row.display(zone).price.clone(),
        3 => row.display(zone).quantity.clone(),
        4 => row.display(zone).time.clone(),
        _ => SmolStr::default(),
    }
}

/// Builds one cell of the force-order table.
///
/// The kit table calls this without an application context, so what it builds is a
/// text node whose text and colour are read on the frames that draw it: the cell
/// registers its dependencies inside `bind_text` / `bind_style` rather than resolving
/// anything here (AGENTS §4.4 / §9).
///
/// Every cell draws in a theme foreground. The engine's `TextStyle` colour is
/// `BLACK` by default and the cell box's `foreground` does not inherit into the text
/// node, so a cell that bound no colour would be unreadable on a dark appearance (and
/// differ from the old table, which resolved `theme.roles.fg.default` for every cell).
/// The listing column overrides that default with the direction colour of the
/// liquidation's side.
fn build_cell(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    column: usize,
    rows: Signal<ForceOrderSnapshot>,
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    translate: &Translate,
    theme: &ThemeHandle,
    font_size: f32,
) {
    let text = NodeBuilder::text(SmolStr::default(), TextStyle::new(font_size));

    let text = match column {
        // The route and the listing name are words, so they follow the language.
        0 | 1 => {
            let translate = translate.clone();
            text.bind_text(move |read| {
                let _language = *read.read(&language);
                let snapshot = read.read(&rows);
                cell_text(
                    displayed_row(&snapshot, row),
                    column,
                    UtcOffset::UTC,
                    &translate,
                )
            })
        }
        // The numbers are read in the display time zone, which is a settings read: the
        // cell resolves it on the frames that draw it, so moving the setting redraws it.
        2..=4 => {
            let translate = translate.clone();
            text.bind_text(move |read| {
                let zone = {
                    let config = read.read(&config);
                    data_time_zone(&config)
                };
                let snapshot = read.read(&rows);
                cell_text(displayed_row(&snapshot, row), column, zone, &translate)
            })
        }
        _ => text,
    };

    let theme = theme.clone();
    let text = text.bind_style(move |read| {
        let role = if column == 1 {
            let snapshot = read.read(&rows);
            displayed_row(&snapshot, row).map_or(Role::FgDefault, |row| {
                match row.order().position_side {
                    PositionSide::Long => Role::MarketUp,
                    _ => Role::MarketDown,
                }
            })
        } else {
            Role::FgDefault
        };
        StylePatch {
            foreground: Some(theme.role(read, role)),
            ..StylePatch::default()
        }
    });
    let _ = text.build(tree, parent);
}

/// The columns the table declares, in header order.
///
/// Built when the table is built, because the kit table takes its headers as resolved
/// text; a language switch rebuilds the table.
fn columns(translate: &Translate) -> Vec<Column> {
    let headers = [
        t_smol!(translate, "common.route"),
        t_smol!(translate, "common.pair"),
        t_smol!(translate, "common.price"),
        t_smol!(translate, "common.amount"),
        t_smol!(translate, "common.time"),
    ];

    headers
        .into_iter()
        .zip(COLUMN_WEIGHTS)
        .zip(COLUMN_ALIGNS)
        .map(|((header, weight), align)| Column::flex(header, weight).align(align))
        .collect()
}

/// One command this pane sent and has not seen the answer to.
///
/// The pane reads through one binding and issues one kind of command, so the tag names
/// the read rather than telling commands apart: it is the identity the binding returned
/// that decides which answer applies. A reply whose identity is not tracked answers a
/// call the pane has already replaced; it is dropped and counted instead of failing the
/// pane over a command no longer in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The read: `Load` or `Retry`.
    Start,
}

pub struct ForceOrderWidget {
    /// The executor bridge, when this host has a directory to drive a service from.
    ///
    /// A headless host without a connector has nothing to start: the pane waits for
    /// the directory instead of spawning a service it could never ask a question.
    binding: Option<WidgetBinding<ForceOrderProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    /// The table, once the body has been built (the framework builds a body only once
    /// the pane has content).
    table: Option<Table>,
    /// The node the table was built into; a language switch rebuilds the table there.
    table_slot: Option<NodeId>,
    status: WidgetStatus,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    /// The loaded directory, when the host has one.
    directory: Option<Rc<dyn MarketDirectory>>,
    /// The accepted rows, as the cells read them.
    rows: Signal<ForceOrderSnapshot>,
    /// The settings snapshot, for the display time zone the numbers are read in.
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    /// The directory generation the composition root publishes.
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    snapshot: Option<ForceOrderSnapshot>,
    pending_snapshot: Option<ForceOrderSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    /// The routes the stream the pane is running was started for.
    ///
    /// Which routes report liquidations is the directory's answer, and the user may
    /// load another venue at any time, so the set is re-read on every load rather than
    /// taken once. Empty means no market this pane reads is loaded, which is what keeps
    /// [`Self::has_content`] false while the pane waits.
    routes: Arc<[VenueRoute]>,
    /// Whether the catalogue generation moved under the stream the pane is reading.
    ///
    /// The rows were named and written at the generation the stream resolved when it
    /// was opened; a new generation may have replaced it, so the next load asks for the
    /// request that opens a new stream ([`ForceOrderRequest::Retry`]) rather than the
    /// idempotent [`ForceOrderRequest::Load`]. Read past the pane's own gate: a pane a
    /// shrinking commit sent back to waiting keeps the flag, so the load that brings
    /// the market back reopens the stream too.
    market_reloaded: bool,
    /// Whether the pane is waiting for its markets rather than reading them.
    ///
    /// The pane keeps its stream while it waits — cancelling the binding is terminal,
    /// and this pane has to read the markets again once the user loads them — so a
    /// snapshot that was already in flight when the routes went away can still arrive.
    /// Accepting it would write a status over the notice the framework is drawing, so
    /// every message entry refuses while this is set.
    awaiting_market: bool,
    /// The generation this pane has already reacted to.
    seen_generation: u64,
    /// The language the table's static copy was built in.
    seen_language: LanguageCode,
    /// Whether the accepted rows changed since the table was last updated.
    rows_dirty: bool,
}

impl WidgetView for ForceOrderWidget {
    type Config = ForceOrderConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.force_order");
    const DISPLAY_NAME: &'static str = "widget.names.force_order";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.force_order");
    const ICON: Option<&'static str> = Some("widgets/force_order.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // A host without a directory has nothing to ask and nothing to start; the pane
        // reports the neutral notice and waits. Every application supplies one.
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(None, None, status, recovery, cx));
        };
        let source = connector.market();
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory(connector));
        let service = ForceOrderService::new(source, directory.catalog(), MAX_FORCE_ORDERS);
        let binding = WidgetBinding::spawn(cx, service);
        Ok(Self::assemble(
            Some(binding),
            Some(directory),
            status,
            recovery,
            cx,
        ))
    }

    /// The table is the content, and it is ready as soon as a snapshot has been
    /// accepted — including an empty one. An empty table is what "no force orders right
    /// now" looks like; it is not a state the widget reports.
    ///
    /// A pane that reads no route at all has nothing to draw however much it once
    /// accepted: the market it was reading is gone, and the neutral notice stands in
    /// its place.
    fn has_content(&self) -> bool {
        !self.routes.is_empty() && self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The markets this pane reads have to be loaded before anything else happens
        // here, and the check is first on purpose: with no route to read there is no
        // stream to start, and the pane waits for one instead of reporting the failure
        // the service would raise about a route nobody serves.
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let routes = directory.liquidation_routes();
        if routes.is_empty() || !routes.iter().all(|route| directory.is_loaded(route)) {
            self.wait_for_market(cx);
            return;
        }
        // The pane reached here only past its own gate, so the markets it was waiting
        // for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        // Read past the gate: a new catalogue generation is what brings this pane back
        // after a shrinking commit, and the stream it still holds resolved against the
        // retired generation.
        let market_reloaded = mem::take(&mut self.market_reloaded);
        // A route set that changed is a different stream rather than a repeat of this
        // one, and replacing it keeps the rows a reader is looking at.
        let retry = self.status.is_failed(cx.app())
            || (!self.routes.is_empty() && routes_changed(&self.routes, &routes));
        if !retry {
            self.clear_orders();
        }
        let routes: Arc<[VenueRoute]> = routes.into();
        self.routes = Arc::clone(&routes);
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        // The two decisions are separate: a reload still drops the rows — they were
        // written at the generation behind the commit — while the request is the
        // replacing one.
        let request = if retry || market_reloaded {
            ForceOrderRequest::Retry { routes }
        } else {
            ForceOrderRequest::Load { routes }
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            self.pending.track(request_id, PendingCommand::Start);
        }
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        let name_keys = market_venue_name_keys();
        // The body is the content slot's whole surface: the kit table draws the header
        // and the rows, and the frame exists for the caption the pane states over them.
        let frame = NodeBuilder::boxed()
            .id("ForceOrderTableFrame")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |read| {
                let _language = *read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(
                        translate,
                        "widget.force_order.table",
                        markets = joined_names(&translate, &name_keys)
                    )),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        let rows = self.snapshot.as_ref().map_or(0, ForceOrderSnapshot::len);
        let table = self.build_table(cx, rows)?;
        let window = cx.window();
        table.mount(cx.app_mut(), window, frame)?;
        self.table = Some(table);
        self.table_slot = Some(frame);
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The directory generation is the one signal the framework's surface root also
        // reads, so a write schedules the frame that runs this hook — including for a
        // pane that is only waiting and has no body. Nothing else may poll the
        // directory (AGENTS §8.3).
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                // A new generation may have replaced the one the live stream resolved
                // its listings against, so the load that follows asks for a new stream.
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        // The binding delivers messages instead of calling back into the widget; this
        // is the one place they are applied, where the widget has a context again.
        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        // A language switch redraws the cells through their `bind_text` closures; the
        // table's own copy (headers and the empty sentence) is resolved when the table
        // is built, so that is the one thing rebuilt.
        let language = *cx.read(&self.language);
        if language != self.seen_language {
            self.seen_language = language;
            self.rebuild_table(cx)?;
        }

        // Structural row changes go to the table, whose row factory has no application
        // context; every dynamic value inside a row is a `bind_*` closure instead.
        if self.rows_dirty {
            self.rows_dirty = false;
            let snapshot = self.snapshot.clone().unwrap_or_default();
            let count = snapshot.len();
            cx.write(&self.rows, snapshot);
            if let Some(table) = self.table.as_ref() {
                table.set_rows(cx.app_mut(), count)?;
                table.sync(cx.app_mut())?;
            }
            cx.request_frame()?;
        } else if let Some(table) = self.table.as_ref() {
            table.sync(cx.app_mut())?;
        }

        Ok(())
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // The table's nodes are removed by the framework after this hook, so the
        // component is released here while its nodes are still alive. The service is
        // cancelled the same way: stopping it is what ends the supervised stream.
        if let Some(table) = self.table.take() {
            table.release(cx.app_mut());
        }
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.table_slot = None;
    }
}

/// The options the force-order dataset needs from the kit table: a fixed row height
/// (the dataset declares one) and **no row selection**.
///
/// The old `TableDataSet` drew rows to read, not rows to pick: it had no selection
/// state, no click handling and no selected-row styling. The kit table defaults to
/// [`SelectionMode::Multi`], so the pane turns selection off explicitly here — the one
/// place the decision is made, which is what
/// `tests::the_table_should_not_offer_row_selection` pins.
fn table_options(row_height: f32) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

impl ForceOrderWidget {
    /// Builds a widget over the collaborators a caller hands it.
    fn assemble(
        binding: Option<WidgetBinding<ForceOrderProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let rows = cx.signal(ForceOrderSnapshot::default());
        let config = cx.services().config;
        let language = cx.services().language;
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let seen_language = *cx.read(&language);
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        Self {
            binding,
            pending: Pending::new(),
            table: None,
            table_slot: None,
            status,
            recovery,
            directory,
            rows,
            config,
            language,
            generation,
            translate,
            theme,
            snapshot: None,
            pending_snapshot: None,
            ready_received: false,
            awaiting_snapshot: true,
            routes: Arc::from([]),
            market_reloaded: false,
            awaiting_market: false,
            seen_generation,
            seen_language,
            rows_dirty: false,
        }
    }

    /// Builds a widget over a test's own source and directory.
    ///
    /// The framework's headless host composes the services without a directory (the
    /// connector still needs the old toolkit's runtime to build), so an interface test
    /// constructs the widget directly with the fixture directory it wants, and drives
    /// the widget hooks itself.
    #[cfg(test)]
    fn with_source<S: ForceOrderSource>(
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let display = directory.catalog();
        let service = ForceOrderService::new(source, display, MAX_FORCE_ORDERS);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(Some(binding), Some(directory), status, recovery, cx)
    }

    /// Builds the table into the body slot, over the rows accepted so far.
    fn build_table(&self, cx: &mut WidgetCx<'_>, rows: usize) -> Result<Table, EngineError> {
        let theme = self.theme.clone();
        let cell_theme = self.theme.clone();
        let font_size = self.theme.tokens().medium.font_size;
        let row_height = self.theme.tokens().medium.height;
        let cell_translate = self.translate.clone();
        let rows_signal = self.rows;
        let config = self.config;
        let language = self.language;

        let cells = move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
            build_cell(
                tree,
                parent,
                row,
                column,
                rows_signal,
                config,
                language,
                &cell_translate,
                &cell_theme,
                font_size,
            );
        };
        let table = Table::new(
            cx.app_mut(),
            theme,
            "ForceOrderTable",
            columns(&self.translate),
            table_options(row_height),
            cells,
            t_smol!(self.translate, "common.no_data"),
        );
        table.set_initial_rows(cx.app_mut(), rows);
        Ok(table)
    }

    /// Rebuilds the table after the language moved.
    ///
    /// The kit table's headers and empty sentence are resolved text rather than
    /// bindings, so this is the one part of the body a language switch has to rebuild.
    fn rebuild_table(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let (Some(slot), Some(previous)) = (self.table_slot, self.table.take()) else {
            return Ok(());
        };
        let window = cx.window();
        if let Some(outer) = previous.outer_node() {
            cx.app_mut().remove_node(window, outer)?;
        }
        previous.release(cx.app_mut());

        let rows = self.snapshot.as_ref().map_or(0, ForceOrderSnapshot::len);
        let table = self.build_table(cx, rows)?;
        table.mount(cx.app_mut(), window, slot)?;
        self.table = Some(table);
        Ok(())
    }

    /// Reports the neutral notice for the markets this pane reads, and drops what it
    /// was showing.
    ///
    /// The routes behind those rows are gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane reports
    /// no content — which is why the pane also stops accepting messages: the stream it
    /// was reading may still deliver a snapshot that was already in flight.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.routes = Arc::from([]);
        self.awaiting_market = true;
        self.clear_orders();
        self.report_markets_not_loaded(cx);
    }

    /// Reports the notice naming the markets that report liquidations.
    ///
    /// The sentence is the framework's to resolve, in the language showing on the
    /// frame that draws it; the pane hands over a catalogue reference only.
    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let venues = market_venues();
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
        );
    }

    /// Reports the neutral notice for a read the directory answered "not loaded" to.
    ///
    /// The pane's own gate asked the directory before it started a stream, so this is
    /// the answer to a question the gate could not have seen the answer to: the route
    /// was revoked between the two. That is a wait rather than a failure. Returns
    /// whether it reported, so a caller keeps its own failure path for everything else.
    fn report_waiting_for_directory(&mut self, error: &MarketError, cx: &mut WidgetCx<'_>) -> bool {
        if !is_catalog_not_loaded(error) {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    /// Applies one message the binding delivered.
    fn apply(&mut self, inbound: Inbound<ForceOrderProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => {
                self.apply_rpc(request_id, result, cx);
            }
        }
    }

    /// Applies one RPC reply, once its identity says it answers the command in flight.
    ///
    /// The binding replaces its command slot on every load, but a reply can already be
    /// queued when that happens: only the identity [`WidgetBinding::call`] returned may
    /// reach the pane, and a reply this table does not know is dropped and counted.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<ForceOrderProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_rpc_result(result, cx),
            // Late or unknown: the reply answers a read this pane replaced, and the
            // table has counted it.
            None => {}
        }
    }

    fn apply_reliable(
        &mut self,
        key: ForceOrderStreamKey,
        message: &ForceOrderStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if key != ForceOrderStreamKey::AllMarkets
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }

        match message {
            ForceOrderStreamMessage::Status(status) => match status {
                ForceOrderStatus::Connecting => {
                    if self.snapshot.is_none() {
                        self.status.report(cx.app_mut(), WidgetState::Connecting);
                    }
                }
                ForceOrderStatus::Ready => {
                    self.ready_received = true;
                    if let Some(snapshot) = self.pending_snapshot.take() {
                        self.apply_snapshot(snapshot, cx);
                    } else if !self.awaiting_snapshot {
                        self.report_snapshot_state(cx);
                    }
                }
                ForceOrderStatus::Error(error) => {
                    if self.report_waiting_for_directory(error, cx) {
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(Arc::new(error.clone()), recovery),
                    );
                }
            },
            ForceOrderStreamMessage::Snapshot(_) => {}
        }
    }

    fn apply_latest(
        &mut self,
        key: ForceOrderStreamKey,
        message: &ForceOrderStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if key != ForceOrderStreamKey::AllMarkets
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }

        if let ForceOrderStreamMessage::Snapshot(snapshot) = message {
            if self.ready_received {
                self.apply_snapshot(snapshot.clone(), cx);
            } else {
                self.pending_snapshot = Some(snapshot.clone());
            }
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<ForceOrderProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A failure is terminal and a pane waiting for its market has already reported
        // the notice the framework draws: neither may be repainted by a late answer
        // (AGENTS §4.4).
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        match result {
            Ok(ForceOrderResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status
                    .report(cx.app_mut(), WidgetState::failed(Arc::new(error), recovery));
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: ForceOrderSnapshot, cx: &mut WidgetCx<'_>) {
        if self.snapshot.as_ref() != Some(&snapshot) {
            self.rows_dirty = true;
        }
        self.snapshot = Some(snapshot);
        self.awaiting_snapshot = false;
        self.report_snapshot_state(cx);
    }

    fn clear_orders(&mut self) {
        if self.snapshot.take().is_some() {
            self.rows_dirty = true;
        }
    }

    /// Reports that the widget is showing its table.
    ///
    /// An accepted snapshot means the widget has content even when it holds no orders:
    /// connecting successfully is enough to show the table, and an empty table is the
    /// honest rendering of "nothing to show yet". There is no separate empty state.
    fn report_snapshot_state(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(cx.app_mut(), WidgetState::Ready);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
    };

    use app_config::time_zone::DataTimeZone;
    use base_ui::{
        kit::{layer::Layer, list::RowHeight, overlay::AnimationRouter, table::ColumnWidth},
        theme::appearance::Appearance,
    };
    use market_participants_connector::{
        catalog::{batch::CatalogEntry, error::CatalogError, test_support},
        market::domain::DisconnectReason,
        routing::PERPETUAL_SCOPE,
    };
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::{route::VenueListingKey, rules::ListingPrecision},
        market::{
            book::ListingForceOrder,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use ui_core::{
        app::App,
        event::Action,
        node::{signal::SignalScope, tree::NodeKind},
        paint::color::Color,
        window::{WindowId, WindowOptions},
    };
    use widget::{
        error::TranslatedError, frame::FrameRouter, rpc::RpcCallError, services::WidgetServices,
        test_support::WidgetTestApp,
    };

    use super::*;
    use crate::{
        model::{ForceOrderEntry, ForceOrderListing, ForceOrderState},
        service::{ForceOrderEventSubscription, ForceOrderSource},
    };

    // ---------------------------------------------------------------- vocabulary

    /// The route the fixture market reports liquidations on.
    fn route() -> VenueRoute {
        test_support::route("binance", PERPETUAL_SCOPE)
    }

    fn okx_route() -> VenueRoute {
        test_support::route("okx", "swap")
    }

    /// The translator the product ships, in the language a test asks for.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn entry(timestamp: i64) -> ForceOrderEntry {
        ForceOrderEntry {
            order: ListingForceOrder {
                listing: test_support::listing_key(&route(), "BTCUSDT"),
                price: MarketPrice {
                    value: Price::from(100_u64),
                    quote_asset: test_support::token("fixture", "USDT"),
                    basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
                },
                quantity: MarketQuantity::traded(Quantity::from(2_u64), QuantityUnit::Contracts)
                    .expect("a contract count is a traded quantity"),
                position_side: PositionSide::Long,
                timestamp,
            },
            listing: ForceOrderListing {
                label: Some(SmolStr::new_static("BTCUSDT")),
                precision: Some(precision()),
            },
        }
    }

    /// The same liquidation on the other side of the book.
    fn short_entry(timestamp: i64) -> ForceOrderEntry {
        let mut entry = entry(timestamp);
        entry.order.position_side = PositionSide::Short;
        entry
    }

    /// A price tick of one cent and a quantity step of one thousandth, so a label
    /// that keeps the listing's precision is told apart from the canonical form.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    fn snapshot(timestamp: i64) -> ForceOrderSnapshot {
        let mut state = ForceOrderState::new(1);
        state.merge(&[entry(timestamp)]);
        state.snapshot()
    }

    /// Two liquidations, one per side: display row 0 is the short (newest) one.
    fn both_sides_snapshot() -> ForceOrderSnapshot {
        let mut state = ForceOrderState::new(2);
        state.merge(&[entry(1), short_entry(2)]);
        state.snapshot()
    }

    fn empty_snapshot() -> ForceOrderSnapshot {
        ForceOrderState::new(1).snapshot()
    }

    fn disconnect() -> MarketError {
        MarketError::Disconnected(DisconnectReason::Normal)
    }

    /// A listing the fixture publishes at `price_tick`.
    fn published_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = test_support::entry(
            &route(),
            test_support::spot_token("binance", "BTC", "USDT"),
            "BTCUSDT",
        );
        restated.snapshot.rules.common.price.tick = price_tick
            .parse()
            .expect("the restated price tick should parse");
        restated
    }

    // --------------------------------------------------------------------- sources

    #[derive(Clone, Copy)]
    struct PendingForceOrderSource;

    struct PendingForceOrderSubscription;

    impl ForceOrderSource for PendingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            std::future::pending().await
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            std::future::pending()
        }
    }

    impl ForceOrderEventSubscription for PendingForceOrderSubscription {
        async fn recv(&mut self) -> Option<Result<Arc<Vec<ListingForceOrder>>, MarketError>> {
            std::future::pending().await
        }
    }

    /// A source that counts what the pane asks it for, so a test can tell "the pane
    /// started over" from "the pane re-checked its precondition".
    #[derive(Clone, Default)]
    struct CountingForceOrderSource {
        subscriptions: Arc<AtomicUsize>,
        listings: Arc<AtomicUsize>,
    }

    impl ForceOrderSource for CountingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            self.listings.fetch_add(1, Ordering::AcqRel);
            Ok(vec![test_support::listing_key(route, "BTCUSDT")])
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            self.subscriptions.fetch_add(1, Ordering::AcqRel);
            std::future::pending()
        }
    }

    /// A source that resolves a route's listings through the catalogue it was handed, at
    /// the moment a stream is opened for it, and records the increments they were
    /// published at then.
    #[derive(Clone)]
    struct ResolvingForceOrderSource {
        catalog: Arc<Catalog>,
        resolutions: Arc<std::sync::Mutex<Vec<Vec<ListingPrecision>>>>,
    }

    fn resolutions_of(
        resolutions: &Arc<std::sync::Mutex<Vec<Vec<ListingPrecision>>>>,
    ) -> Vec<Vec<ListingPrecision>> {
        resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    impl ForceOrderSource for ResolvingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            let published = self.catalog.cached_listings(route).unwrap_or_default();
            self.resolutions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(
                    published
                        .iter()
                        .map(|entry| ListingPrecision::of(&entry.snapshot.rules))
                        .collect(),
                );
            Ok(published
                .iter()
                .map(|entry| entry.listing.key.clone())
                .collect())
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            std::future::pending()
        }
    }

    // ------------------------------------------------------------------- directory

    /// A directory a test drives: the routes it publishes, the ones it has loaded, and
    /// the catalogue a new stream resolves its listings against.
    ///
    /// It is the fixture stand-in for the injected connector: the framework's headless
    /// host cannot build one (the connector still needs the old toolkit's runtime), so
    /// a test constructs the widget directly with this directory.
    #[derive(Clone)]
    struct FixtureDirectory {
        routes: Rc<RefCell<Vec<VenueRoute>>>,
        loaded: Rc<RefCell<Vec<VenueRoute>>>,
        catalog: Arc<Catalog>,
    }

    impl FixtureDirectory {
        fn new(catalog: Arc<Catalog>) -> Self {
            Self {
                routes: Rc::new(RefCell::new(Vec::new())),
                loaded: Rc::new(RefCell::new(Vec::new())),
                catalog,
            }
        }

        fn load(&self, route: VenueRoute) {
            let mut routes = self.routes.borrow_mut();
            if !routes.contains(&route) {
                routes.push(route.clone());
            }
            let mut loaded = self.loaded.borrow_mut();
            if !loaded.contains(&route) {
                loaded.push(route);
            }
        }

        fn unload(&self, route: &VenueRoute) {
            self.routes.borrow_mut().retain(|held| held != route);
            self.loaded.borrow_mut().retain(|held| held != route);
        }
    }

    impl MarketDirectory for FixtureDirectory {
        fn liquidation_routes(&self) -> Vec<VenueRoute> {
            self.routes.borrow().clone()
        }

        fn is_loaded(&self, route: &VenueRoute) -> bool {
            self.loaded.borrow().iter().any(|held| held == route)
        }

        fn catalog(&self) -> Arc<Catalog> {
            Arc::clone(&self.catalog)
        }
    }

    // --------------------------------------------------------------------- harness

    /// A widget under test: the framework's headless host, the widget built over the
    /// fixture directory, and the runtime turns needed to let the service answer.
    struct Fixture {
        harness: WidgetTestApp,
        widget: ForceOrderWidget,
        directory: FixtureDirectory,
        recovery: WidgetAction,
        retries: Rc<Cell<usize>>,
    }

    impl Fixture {
        /// Builds the fixture over `source`, with the fixture route loaded or not.
        fn new<S: ForceOrderSource>(source: S, loaded: bool) -> Self {
            Self::with_catalog(move |_| source, loaded, Vec::new())
        }

        /// The same, with the source built from the catalogue the fixture publishes.
        fn with_catalog<S: ForceOrderSource>(
            build: impl FnOnce(Arc<Catalog>) -> S,
            loaded: bool,
            entries: Vec<CatalogEntry>,
        ) -> Self {
            let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
            let fixture = test_support::Fixture::with_source(
                route(),
                "binance-usd-m-perp",
                test_support::pending(),
            );
            test_support::publish_fixture_route(fixture.catalog(), &route(), entries)
                .expect("the fixture route publishes its listings");
            let catalog = Arc::clone(fixture.catalog_arc());
            let directory = FixtureDirectory::new(Arc::clone(&catalog));
            if loaded {
                directory.load(route());
            }
            let source = build(catalog);
            let status = harness.status();
            let retries: Rc<Cell<usize>> = Rc::new(Cell::new(0));
            let counted = Rc::clone(&retries);
            let recovery = WidgetAction::new("common.retry", move |_| {
                counted.set(counted.get() + 1);
            });
            let directory_handle: Rc<dyn MarketDirectory> = Rc::new(directory.clone());
            let recovery_handle = recovery.clone();
            let widget = harness.with_cx(|cx| {
                ForceOrderWidget::with_source(source, directory_handle, status, recovery_handle, cx)
            });
            let mut fixture = Self {
                harness,
                widget,
                directory,
                recovery,
                retries,
            };
            fixture.load();
            fixture
        }

        fn with_cx<R>(
            &mut self,
            body: impl FnOnce(&mut ForceOrderWidget, &mut WidgetCx<'_>) -> R,
        ) -> R {
            let widget = &mut self.widget;
            self.harness.with_cx(|cx| body(widget, cx))
        }

        fn load(&mut self) {
            self.with_cx(|widget, cx| widget.load(cx));
        }

        fn sync(&mut self) {
            self.with_cx(|widget, cx| widget.sync(cx).expect("the widget syncs"));
        }

        fn drain(&mut self) {
            self.harness.drain();
        }

        fn status(&self) -> WidgetState {
            self.harness.status().state(self.harness.app())
        }

        fn has_content(&self) -> bool {
            self.widget.has_content()
        }

        fn snapshot_len(&self) -> Option<usize> {
            self.widget.snapshot.as_ref().map(ForceOrderSnapshot::len)
        }

        /// Loads one more market and announces the catalogue generation that goes with
        /// it, the way the composition root does when a venue commit lands.
        fn load_market(&mut self, route: VenueRoute) {
            self.directory.load(route);
            self.bump_generation();
        }

        fn unload_market(&mut self, route: &VenueRoute) {
            self.directory.unload(route);
            self.bump_generation();
        }

        fn bump_generation(&mut self) {
            let generation = self.harness.services().catalog_generation;
            let next = *self.harness.app().read(&generation) + 1;
            assert!(
                self.harness.app_mut().write(&generation, next),
                "the generation moves"
            );
            self.sync();
        }

        fn accept_snapshot(&mut self, snapshot: ForceOrderSnapshot) {
            self.with_cx(|widget, cx| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot, cx);
                widget.sync(cx).expect("the widget syncs");
            });
        }

        fn apply_latest(&mut self, snapshot: ForceOrderSnapshot) {
            self.with_cx(|widget, cx| {
                widget.apply_latest(
                    ForceOrderStreamKey::AllMarkets,
                    &ForceOrderStreamMessage::Snapshot(snapshot),
                    cx,
                );
            });
        }

        fn apply_reliable(&mut self, status: ForceOrderStatus) {
            self.with_cx(|widget, cx| {
                widget.apply_reliable(
                    ForceOrderStreamKey::AllMarkets,
                    &ForceOrderStreamMessage::Status(status),
                    cx,
                );
            });
        }

        fn apply_rpc(
            &mut self,
            request_id: RequestId,
            result: WidgetRpcResult<ForceOrderProtocol>,
        ) {
            self.with_cx(|widget, cx| widget.apply_rpc(request_id, result, cx));
        }

        fn clear_orders(&mut self) {
            self.with_cx(|widget, cx| {
                widget.clear_orders();
                widget.sync(cx).expect("the widget syncs");
            });
        }

        fn report_failure(&mut self) {
            let recovery = self.recovery.clone();
            let state = WidgetState::failed(Arc::new(disconnect()), recovery);
            assert!(self.harness.status().report(self.harness.app_mut(), state));
        }

        /// Builds the body the framework would build on the first frame with content.
        fn build_body(&mut self) {
            self.with_cx(|widget, cx| {
                let root = cx.tree().root();
                widget.body(root, cx).expect("the body builds");
            });
            for _ in 0..4 {
                self.sync();
                self.drain();
            }
        }

        fn table(&self) -> &Table {
            self.widget
                .table
                .as_ref()
                .expect("the body built the table")
        }

        fn table_rows(&self) -> usize {
            self.widget.table.as_ref().map_or(0, Table::rows)
        }

        fn body_node(&self) -> NodeId {
            self.widget
                .table_slot
                .expect("the body built the frame node")
        }

        fn settled_cell_text(&mut self, row: usize, column: usize) -> String {
            for _ in 0..16 {
                self.sync();
                self.drain();
                let node = self
                    .widget
                    .table
                    .as_ref()
                    .and_then(|table| table.cell_node(self.harness.app(), row, column));
                if let Some(node) = node {
                    return self.harness.text_of(node);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("row {row} column {column} should be mounted");
        }

        fn header_text(&mut self, column: usize) -> String {
            self.drain();
            let node = self
                .widget
                .table
                .as_ref()
                .and_then(|table| table.header_node(self.harness.app(), column));
            self.harness
                .text_of(node.expect("the header cell is mounted"))
        }

        /// The foreground colour the cell's text node draws in.
        ///
        /// The style pass writes the patch, so the frame that draws the cell has to have
        /// run: `drain` is what makes the patch readable.
        fn cell_foreground(&mut self, row: usize, column: usize) -> Color {
            self.drain();
            let cell = self
                .widget
                .table
                .as_ref()
                .and_then(|table| table.cell_node(self.harness.app(), row, column))
                .unwrap_or_else(|| panic!("row {row} column {column} is mounted"));
            let tree = self.harness.tree();
            let mut stack = vec![cell];
            while let Some(node) = stack.pop() {
                let target = tree.get(node).expect("a cell node");
                if matches!(target.kind(), NodeKind::Text(_)) {
                    return target
                        .style_patch()
                        .foreground
                        .expect("the cell text binds a foreground colour");
                }
                stack.extend(target.children().iter().copied());
            }
            panic!("row {row} column {column} has a text node");
        }
    }

    /// A widget whose service has to actually answer.
    ///
    /// The framework's [`WidgetTestApp`] owns a **current-thread** runtime and exposes no
    /// way to drive it, so a service task spawned there never runs. This host composes
    /// the same engine application and the same [`WidgetServices`] over a runtime the
    /// test owns, which is what lets `settle_until` pump the stream work.
    struct ServiceFixture {
        runtime: tokio::runtime::Runtime,
        app: App,
        window: WindowId,
        services: Rc<WidgetServices>,
        scope: SignalScope,
        status: WidgetStatus,
        widget: ForceOrderWidget,
        directory: FixtureDirectory,
        recovery: WidgetAction,
    }

    impl ServiceFixture {
        fn new<S: ForceOrderSource>(source: S, loaded: bool) -> Self {
            Self::with_catalog(move |_| source, loaded, Vec::new())
        }

        fn with_catalog<S: ForceOrderSource>(
            build: impl FnOnce(Arc<Catalog>) -> S,
            loaded: bool,
            entries: Vec<CatalogEntry>,
        ) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("the service runtime builds");
            let mut app = App::new(runtime.handle().clone());
            let window = app.create_window(WindowOptions::default());
            let scope = app.signal_scope();
            let theme = ThemeHandle::new(&mut app, Appearance::Light);
            // The composition root's three singletons, composed exactly as the
            // application root composes them; this host keeps no handles of its own.
            let layer = Rc::new(Layer::new());
            let animation = Rc::new(AnimationRouter::new());
            let router =
                Rc::new(FrameRouter::new(&mut app).expect("the host owns the frame hook slot"));
            let services = Rc::new(
                WidgetServices::builder(
                    &mut app,
                    runtime.handle().clone(),
                    product_translate(LanguageCode::En),
                    theme,
                    layer,
                    router,
                    animation,
                )
                .build(),
            );

            let fixture = test_support::Fixture::with_source(
                route(),
                "binance-usd-m-perp",
                test_support::pending(),
            );
            test_support::publish_fixture_route(fixture.catalog(), &route(), entries)
                .expect("the fixture route publishes its listings");
            let catalog = Arc::clone(fixture.catalog_arc());
            let directory = FixtureDirectory::new(Arc::clone(&catalog));
            if loaded {
                directory.load(route());
            }
            let source = build(catalog);
            let status = WidgetStatus::new(&mut app, scope);
            let recovery = WidgetAction::new("common.retry", |_| {});
            let directory_handle: Rc<dyn MarketDirectory> = Rc::new(directory.clone());
            let mut cx = WidgetCx::new(&mut app, window, &services, scope);
            let widget = ForceOrderWidget::with_source(
                source,
                directory_handle,
                status,
                recovery.clone(),
                &mut cx,
            );
            let mut fixture = Self {
                runtime,
                app,
                window,
                services,
                scope,
                status,
                widget,
                directory,
                recovery,
            };
            fixture.load();
            fixture
        }

        fn with_cx<R>(
            &mut self,
            body: impl FnOnce(&mut ForceOrderWidget, &mut WidgetCx<'_>) -> R,
        ) -> R {
            let mut cx = WidgetCx::new(&mut self.app, self.window, &self.services, self.scope);
            body(&mut self.widget, &mut cx)
        }

        fn load(&mut self) {
            self.with_cx(|widget, cx| widget.load(cx));
        }

        fn sync(&mut self) {
            self.with_cx(|widget, cx| widget.sync(cx).expect("the widget syncs"));
        }

        fn status(&self) -> WidgetState {
            self.status.state(&self.app)
        }

        fn settle_once(&mut self) {
            self.runtime.block_on(async {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            });
            self.sync();
        }

        fn settle_until(&mut self, what: &str, ready: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.settle_once();
                if ready(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "{what} should happen");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn load_market(&mut self, route: VenueRoute) {
            self.directory.load(route);
            self.bump_generation();
        }

        fn unload_market(&mut self, route: &VenueRoute) {
            self.directory.unload(route);
            self.bump_generation();
        }

        fn bump_generation(&mut self) {
            let generation = self.services.catalog_generation;
            let next = *self.app.read(&generation) + 1;
            assert!(self.app.write(&generation, next), "the generation moves");
            self.sync();
        }

        fn republish(&self, entries: Vec<CatalogEntry>) {
            test_support::publish_fixture_route(self.directory.catalog.as_ref(), &route(), entries)
                .expect("the fixture route publishes again");
        }

        fn accept_snapshot(&mut self, snapshot: ForceOrderSnapshot) {
            self.with_cx(|widget, cx| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot, cx);
            });
        }

        fn report_failure(&mut self) {
            let state = WidgetState::failed(Arc::new(disconnect()), self.recovery.clone());
            assert!(self.status.report(&mut self.app, state));
        }

        fn has_content(&self) -> bool {
            self.widget.has_content()
        }
    }

    // -------------------------------------------------------------- table dataset

    /// The table keeps the five-column dataset semantics: five columns, the declared
    /// weights and alignments, and the accepted rows.
    #[test]
    fn the_table_should_keep_the_five_column_dataset_semantics() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        let mut state = ForceOrderState::new(2);
        state.merge(&[entry(1), entry(2)]);
        fixture.accept_snapshot(state.snapshot());
        fixture.build_body();

        let table = fixture.table();
        assert_eq!(table.columns().len(), COLUMN_COUNT, "five columns");
        for (index, column) in table.columns().iter().enumerate() {
            assert_eq!(
                column.width,
                ColumnWidth::Flex(COLUMN_WEIGHTS[index]),
                "column {index} keeps its declared width weight"
            );
            assert_eq!(column.align, COLUMN_ALIGNS[index]);
            assert!(!column.sortable, "the dataset never sorted its columns");
        }
        assert_eq!(
            table.columns()[0].header.as_str(),
            "Route",
            "the headers are the localized dataset's"
        );
        assert_eq!(table.rows(), 2, "the accepted row count reaches the table");
    }

    /// The old dataset was a table to read, so the pane keeps row selection off: the
    /// kit table's own default is multi-select, which would be a new interaction (and
    /// a new a11y/selection state) rather than a migration of the old one.
    #[test]
    fn the_table_should_not_offer_row_selection() {
        let options = table_options(30.0);

        assert_eq!(
            options.selection,
            SelectionMode::None,
            "the old dataset had no selection: rows are display-only"
        );
        assert_eq!(options.row_height, RowHeight::Fixed(30.0));
    }

    /// Every cached column of a row is filled, and the route is the user-visible one
    /// rather than the canonical identity that belongs in logs.
    #[test]
    fn force_order_display_should_fill_every_cached_column() {
        let state_snapshot = snapshot(1);
        let translate = product_translate(LanguageCode::En);
        let row = displayed_row(&state_snapshot, 0).expect("the newest row is display row 0");
        let cells = (0..COLUMN_COUNT)
            .map(|column| cell_text(Some(row), column, UtcOffset::UTC, &translate).to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            cells,
            // The numbers keep the increments of the listing the liquidation was
            // reported on: a whole price is still read with the decimals its tick
            // states.
            [
                "Binance/USD-M-PERP",
                "BTCUSDT",
                "100.00",
                "2.000",
                "00:00:00.001"
            ]
        );
        assert_eq!(
            cell_text(Some(row), 0, UtcOffset::UTC, &translate),
            "Binance/USD-M-PERP",
            "the route column is the user-visible route"
        );
        assert_ne!(
            cell_text(Some(row), 0, UtcOffset::UTC, &translate),
            SmolStr::new(route().to_string()),
            "the canonical lower-case route is a machine identity"
        );
    }

    /// The rows are drawn newest first: display row 0 is the last accepted order.
    #[test]
    fn the_table_should_draw_the_newest_accepted_order_first() {
        let mut state = ForceOrderState::new(4);
        state.merge(&[entry(1), entry(2), entry(3)]);
        let state_snapshot = state.snapshot();
        let translate = product_translate(LanguageCode::En);

        assert_eq!(
            displayed_row(&state_snapshot, 0)
                .map(|row| row.order().timestamp)
                .map(|timestamp| timestamp.to_string()),
            Some(
                state_snapshot
                    .orders()
                    .last()
                    .map(|row| row.order().timestamp.to_string())
                    .expect("the snapshot holds rows")
            )
        );
        assert!(
            displayed_row(&state_snapshot, 3).is_none(),
            "a row past the snapshot draws nothing"
        );
        assert_eq!(
            cell_text(None, 0, UtcOffset::UTC, &translate),
            SmolStr::default()
        );
    }

    // ------------------------------------------------------------------ state

    #[test]
    fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content() {
        let fixture = Fixture::new(PendingForceOrderSource, true);

        assert!(!fixture.has_content());
        assert_eq!(fixture.status(), WidgetState::Connecting);
    }

    /// A market the user has not loaded is a wait rather than a failure: the pane names
    /// what to load and has nothing to draw — not even for a snapshot it once accepted,
    /// because nothing on screen is backed by a market any more.
    #[test]
    fn an_unloaded_market_should_leave_the_pane_waiting() {
        let mut fixture = Fixture::new(PendingForceOrderSource, false);

        let status = fixture.status();
        assert!(
            status.failure().is_none(),
            "an unloaded market is not a failure"
        );
        assert_eq!(
            status,
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: market_venues(),
                supported: market_venues(),
            }),
            "waiting is not working"
        );
        assert!(!fixture.has_content());

        fixture.accept_snapshot(snapshot(1));
        assert!(
            !fixture.has_content(),
            "a route the directory does not hold is no market to draw"
        );
    }

    /// The notice is a reference into the catalogue rather than a sentence, so the
    /// language showing when it is drawn is the language it is read in.
    #[test]
    fn a_language_change_should_re_report_the_market_notice() {
        let notice = Notice::MarketNotLoaded {
            venues: market_venues(),
            supported: market_venues(),
        };
        let translate = product_translate(LanguageCode::En);

        let english = notice.resolve(&translate).to_string();
        assert!(
            english.contains("Binance") && english.contains("OKX"),
            "the notice names the markets that report liquidations: {english}"
        );
        assert!(
            !english.contains("Hyperliquid"),
            "the notice names the venues whose loader can report liquidations: {english}"
        );

        assert!(translate.set_language(LanguageCode::ZhCn));
        let chinese = notice.resolve(&translate);
        assert!(
            chinese.contains("币安") && chinese.contains("欧易"),
            "the same reference resolves in the language showing now: {chinese}"
        );
        assert_ne!(
            chinese.as_str(),
            english,
            "and it is not the copy of the language the interface left"
        );
    }

    /// Loading the venue is what a waiting pane recovers from, by itself, when the
    /// directory generation moves.
    #[test]
    fn loading_the_venue_should_let_the_waiting_pane_start() {
        let mut fixture = Fixture::new(PendingForceOrderSource, false);
        assert!(fixture.status().notice().is_some());

        fixture.load_market(route());

        let status = fixture.status();
        assert!(
            status.notice().is_none(),
            "the pane left the notice when the generation announced the load"
        );
        assert!(status.failure().is_none());
        assert_eq!(status, WidgetState::Connecting);
    }

    /// The directory answering "this route's catalog is not in memory" is a wait, not
    /// a failure: the pane's own gate asked the same question a moment earlier, so this
    /// is the answer changing underneath the request.
    #[test]
    fn a_directory_that_has_not_loaded_a_catalog_should_leave_the_pane_waiting() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.apply_reliable(ForceOrderStatus::Error(MarketError::Catalog(Box::new(
            CatalogError::CatalogNotLoaded { route: route() },
        ))));

        let status = fixture.status();
        assert!(
            status.failure().is_none(),
            "a catalog that has not loaded is a wait, not a failure"
        );
        assert_eq!(
            status,
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: market_venues(),
                supported: market_venues(),
            })
        );
        assert!(!fixture.has_content());
    }

    /// A stream that is still alive after its markets went away must not write a status
    /// over the notice: a message already in flight belongs to markets the pane no
    /// longer reports reading.
    #[test]
    fn a_late_message_after_the_market_went_away_should_not_clear_the_notice() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.accept_snapshot(snapshot(1));
        assert!(fixture.has_content(), "the rows are what arrived");

        fixture.unload_market(&route());
        assert!(
            fixture.status().notice().is_some(),
            "the pane reports the neutral notice"
        );
        assert!(
            fixture.status().failure().is_none(),
            "waiting is not a failure"
        );

        fixture.apply_latest(snapshot(9));
        assert!(
            !fixture.has_content(),
            "a snapshot from the market that went away must not become content again"
        );
        assert!(
            fixture.status().notice().is_some(),
            "the notice is what the pane reports after a late snapshot"
        );

        fixture.apply_reliable(ForceOrderStatus::Ready);
        assert!(
            fixture.status().notice().is_some(),
            "the notice is what the pane reports after a late status"
        );
    }

    #[test]
    fn an_accepted_snapshot_should_become_content_and_clear_the_status() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.accept_snapshot(snapshot(1));

        assert!(fixture.has_content());
        assert_eq!(fixture.status(), WidgetState::Ready);
        assert!(fixture.status().failure().is_none());
    }

    /// A successful connection is enough to show the table. An empty snapshot is
    /// content — an empty table — and the widget reports nothing about it: the table
    /// itself is what says it holds no orders.
    #[test]
    fn an_empty_snapshot_should_be_content_and_report_nothing() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(empty_snapshot());
        fixture.build_body();

        assert!(fixture.has_content());
        assert_eq!(fixture.table_rows(), 0);
        assert_eq!(fixture.table().rows(), 0);
        let empty = fixture
            .table()
            .empty_node()
            .expect("an empty table has to say it is empty");
        assert!(
            fixture.harness.text_of(empty).contains("No data"),
            "the table's own empty sentence stands in: {:?}",
            fixture.harness.text_of(empty)
        );
        let status = fixture.status();
        assert_eq!(status, WidgetState::Ready);
        assert!(status.failure().is_none());
        assert!(
            status.notice().is_none(),
            "the empty state belongs to the table, not to the framework's placeholder"
        );
    }

    #[test]
    fn a_failure_with_content_should_be_reported_and_keep_the_content() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.accept_snapshot(snapshot(1));
        fixture.report_failure();

        assert!(
            fixture.has_content(),
            "a failure must not throw away data the widget already has"
        );
        let status = fixture.status();
        assert!(status.is_failed());
        let expected = disconnect().translated_message(&fixture.harness.services().translate);
        assert_eq!(
            status
                .failure()
                .map(|failure| failure.text(&fixture.harness.services().translate)),
            Some(expected)
        );
    }

    #[test]
    fn a_failure_without_content_should_be_reported_with_no_content() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.report_failure();

        assert!(!fixture.has_content());
        assert!(fixture.status().is_failed());
    }

    /// Every failure path republishes the recovery the framework gave the widget. The
    /// widget never builds its own: the label and the capability are the framework's.
    #[test]
    fn every_failure_should_offer_the_recovery_that_reloads_the_widget() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.report_failure();

        let failure = fixture
            .status()
            .failure()
            .cloned()
            .expect("a failure should offer a way out");
        assert_eq!(failure.action(), &fixture.recovery);
        assert_eq!(
            failure
                .action()
                .label(&fixture.harness.services().translate),
            "Retry"
        );
        assert!(failure.action().is_enabled());

        failure.action().run(fixture.harness.app_mut());
        assert_eq!(
            fixture.retries.get(),
            1,
            "the failure carries the framework's own recovery handle"
        );
    }

    #[test]
    fn a_late_stream_message_after_a_failure_should_not_change_the_status() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.report_failure();
        let failed = fixture.status();

        fixture.apply_reliable(ForceOrderStatus::Ready);
        fixture.apply_latest(snapshot(1));

        assert_eq!(fixture.status(), failed);
        assert!(fixture.status().is_failed());
        assert!(fixture.snapshot_len().is_none());
    }

    /// The answer to the read the pane sent is reported: the binding returned the
    /// identity before the request went out, and the reply carrying it is the pane's.
    #[test]
    fn the_answer_to_the_read_it_sent_should_be_reported() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
        );

        assert!(
            fixture.status().is_failed(),
            "the read's refusal is the pane's failure"
        );
    }

    /// A read the pane replaced answers a command no longer waiting: the reply is
    /// dropped and counted, and it does not fail the read that replaced it.
    #[test]
    fn the_answer_to_a_replaced_read_should_be_dropped() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.load();

        fixture.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
        );

        assert!(
            !fixture.status().is_failed(),
            "the replaced read's refusal is not this pane's failure"
        );
        assert_eq!(
            fixture.widget.pending.dropped(),
            1,
            "the late answer is dropped and counted"
        );
    }

    /// A failed pane is terminal: even the reply to the read it was still waiting for
    /// may not rewrite the failure sentence.
    #[test]
    fn a_failed_pane_should_not_be_repainted_by_a_late_reply() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.report_failure();
        let failed = fixture.status();

        fixture.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
        );

        assert_eq!(fixture.status(), failed);
    }

    #[test]
    fn a_snapshot_before_the_ready_boundary_should_await_it() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.apply_latest(snapshot(1));
        assert!(fixture.snapshot_len().is_none());
        assert!(!fixture.has_content());

        fixture.apply_reliable(ForceOrderStatus::Ready);

        assert_eq!(fixture.snapshot_len(), Some(1));
        assert!(fixture.has_content());
        assert_eq!(fixture.status(), WidgetState::Ready);
    }

    #[test]
    fn a_stream_failure_should_become_the_reported_state() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.apply_reliable(ForceOrderStatus::Error(disconnect()));

        assert!(fixture.status().is_failed());
        assert!(!fixture.has_content());
    }

    /// An explicit reload drops what it had and starts over from connecting.
    #[test]
    fn an_explicit_reload_should_start_over_from_connecting() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.accept_snapshot(snapshot(1));
        assert!(fixture.has_content());

        fixture.load();

        assert!(!fixture.has_content());
        assert_eq!(fixture.status(), WidgetState::Connecting);
        assert!(fixture.status().failure().is_none());
    }

    /// A failure is terminal: a reload the user asks for keeps the snapshot the widget
    /// still shows, because the user is looking at it.
    #[test]
    fn a_retry_after_a_failure_should_keep_the_snapshot_the_widget_still_shows() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);

        fixture.accept_snapshot(snapshot(1));
        fixture.report_failure();
        fixture.load();

        assert_eq!(
            fixture.snapshot_len(),
            Some(1),
            "a retry must not blank data the user is still reading"
        );
    }

    // ------------------------------------------------------------------ routes

    /// This pane reads the whole market, so a venue loaded while it already has content
    /// is a *different* route set rather than a publication of the one it is reading.
    #[test]
    fn a_venue_loaded_after_the_rows_arrived_should_be_read_too() {
        let source = CountingForceOrderSource::default();
        let subscriptions = Arc::clone(&source.subscriptions);
        let mut fixture = ServiceFixture::new(source, true);
        fixture.settle_until("the first stream", |_| {
            subscriptions.load(Ordering::Acquire) == 1
        });
        fixture.accept_snapshot(snapshot(1));
        assert!(fixture.has_content());
        assert_eq!(fixture.widget.routes.as_ref(), [route()].as_slice());

        fixture.load_market(okx_route());

        assert_eq!(
            fixture.widget.routes.len(),
            2,
            "the venue loaded after the rows arrived has to be read as well"
        );
        assert!(
            fixture
                .widget
                .routes
                .iter()
                .any(|held| held.venue.as_str() == "okx")
        );
        fixture.settle_until("the growth to start a stream over both routes", |_| {
            subscriptions.load(Ordering::Acquire) > 1
        });
    }

    /// A set that grew starts a stream that covers it, and a shrinking commit starts
    /// over on what is left rather than reading a market nobody loaded.
    #[test]
    fn a_whole_market_pane_wakes_when_its_routes_change() {
        let source = CountingForceOrderSource::default();
        let subscriptions = Arc::clone(&source.subscriptions);
        let mut fixture = ServiceFixture::new(source, true);
        fixture.settle_until("the first stream", |_| {
            subscriptions.load(Ordering::Acquire) == 1
        });
        let reading = subscriptions.load(Ordering::Acquire);

        fixture.load_market(okx_route());
        fixture.settle_until("the grown route set to start a stream", |_| {
            subscriptions.load(Ordering::Acquire) > reading
        });
        assert_eq!(fixture.widget.routes.len(), 2);
        let grown = subscriptions.load(Ordering::Acquire);

        fixture.unload_market(&okx_route());
        assert_eq!(
            fixture.widget.routes.as_ref(),
            [route()].as_slice(),
            "the withdrawn venue is no longer a market this pane reads"
        );
        fixture.settle_until("the shrinking route set to start over", |_| {
            subscriptions.load(Ordering::Acquire) > grown
        });
    }

    /// A shrinking commit takes the pane's markets away, and the flag it leaves has to
    /// survive the wait: the load the returning market unblocks opens a new stream
    /// rather than being answered idempotently for the stream that is still running.
    #[test]
    fn a_market_that_comes_back_after_a_shrinking_commit_should_reopen_the_stream() {
        let source = CountingForceOrderSource::default();
        let subscriptions = Arc::clone(&source.subscriptions);
        let mut fixture = ServiceFixture::new(source, true);
        fixture.settle_until("the first stream", |_| {
            subscriptions.load(Ordering::Acquire) == 1
        });

        fixture.unload_market(&route());
        assert!(
            fixture.widget.awaiting_market,
            "the pane waits for the market it lost"
        );
        assert!(
            fixture.widget.routes.is_empty(),
            "the markets the pane was reading are gone, so a commit can no longer land on one"
        );

        fixture.load_market(route());
        fixture.settle_until("the returning market to reopen the stream", |_| {
            subscriptions.load(Ordering::Acquire) == 2
        });
        assert!(
            !fixture.widget.awaiting_market,
            "the pane is reading its markets again"
        );
    }

    /// A refresh of a venue the pane reads replaces the catalogue generation its live
    /// stream resolved those markets' listings against, so the pane starts over: the
    /// rows go and the request opens a new stream, which resolves at the new generation.
    #[test]
    fn a_refresh_of_a_market_the_pane_reads_should_reopen_its_stream() {
        let resolutions: Arc<std::sync::Mutex<Vec<Vec<ListingPrecision>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut fixture = ServiceFixture::with_catalog(
            {
                let resolutions = Arc::clone(&resolutions);
                move |catalog| ResolvingForceOrderSource {
                    catalog,
                    resolutions,
                }
            },
            true,
            vec![published_entry("0.001")],
        );
        fixture.settle_until("the market to be read", |_| {
            resolutions_of(&resolutions).len() == 1
        });
        assert_eq!(
            resolutions_of(&resolutions)[0][0].price_tick.to_string(),
            "0.001",
            "the stream resolved at the generation this load published"
        );
        fixture.accept_snapshot(snapshot(1));
        assert!(fixture.has_content(), "the pane is showing its rows");
        let routes = Arc::clone(&fixture.widget.routes);

        // The same venue loads again and the catalogue it republishes restates the
        // listing's price tick, which is the shape a production load has.
        fixture.republish(vec![published_entry("0.01")]);
        fixture.bump_generation();
        fixture.settle_until("the market to be read again", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        assert_eq!(
            fixture.widget.routes, routes,
            "a refresh of a market the pane reads leaves it reading the same ones"
        );
        assert!(
            !fixture.has_content(),
            "the rows are gone: they were written at the generation that was replaced"
        );
        assert_eq!(
            resolutions_of(&resolutions)[1][0].price_tick.to_string(),
            "0.01",
            "the stream that was opened resolved at the generation this load published"
        );
    }

    /// A generation is one number for the whole directory, not a per-venue version, so
    /// a commit that only touched a venue this pane does not read still reloads it: the
    /// rows go and the stream reopens. That is the accepted cost of the coarse signal
    /// (the module documentation states it); this test pins the behaviour so it stays a
    /// decision rather than an accident.
    #[test]
    fn an_unrelated_directory_generation_should_reload_the_pane() {
        let source = CountingForceOrderSource::default();
        let subscriptions = Arc::clone(&source.subscriptions);
        let mut fixture = ServiceFixture::new(source, true);
        fixture.settle_until("the first stream", |_| {
            subscriptions.load(Ordering::Acquire) == 1
        });
        fixture.accept_snapshot(snapshot(1));
        assert!(fixture.has_content(), "the pane is showing its rows");
        let routes = Arc::clone(&fixture.widget.routes);
        let reading = subscriptions.load(Ordering::Acquire);

        // Nothing in the directory moved for this pane; only the published generation
        // did, which is all a commit to an unrelated venue leaves behind here.
        fixture.bump_generation();

        assert!(
            !fixture.has_content(),
            "the rows were written at the generation that moved, so they are dropped"
        );
        assert_eq!(fixture.status(), WidgetState::Connecting);
        assert_eq!(
            fixture.widget.routes, routes,
            "the routes this pane reads did not change"
        );
        fixture.settle_until("the reload to open a new stream", |_| {
            subscriptions.load(Ordering::Acquire) > reading
        });
    }

    /// A failure is terminal: a directory generation write does not revive a failed pane.
    #[test]
    fn a_venue_commit_should_not_revive_a_failed_pane() {
        let source = CountingForceOrderSource::default();
        let subscriptions = Arc::clone(&source.subscriptions);
        let mut fixture = ServiceFixture::new(source, true);
        fixture.settle_until("the first stream", |_| {
            subscriptions.load(Ordering::Acquire) == 1
        });
        let reading = subscriptions.load(Ordering::Acquire);

        fixture.report_failure();
        assert!(fixture.status().failure().is_some());

        fixture.load_market(okx_route());

        assert!(
            fixture.status().failure().is_some(),
            "a directory generation does not revive a failed pane"
        );
        assert_eq!(
            fixture.widget.routes.as_ref(),
            [route()].as_slice(),
            "the failed pane reads nothing new"
        );
        assert_eq!(
            subscriptions.load(Ordering::Acquire),
            reading,
            "the failed pane started no stream"
        );
    }

    // ------------------------------------------------------------------- table

    /// An accepted snapshot reaches the table region, and clearing it reaches it too.
    #[test]
    fn an_accepted_snapshot_should_reach_the_table_region() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.build_body();
        assert_eq!(fixture.table_rows(), 0);

        fixture.accept_snapshot(snapshot(1));
        assert_eq!(fixture.table_rows(), 1);

        fixture.clear_orders();
        assert_eq!(fixture.table_rows(), 0);
    }

    /// The body's cells are the accepted rows: every column of the first display row is
    /// drawn, with the route in its user-visible form and the numbers at the listing's
    /// own precision.
    #[test]
    fn the_body_should_draw_the_accepted_rows() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();

        assert_eq!(fixture.settled_cell_text(0, 0), "Binance/USD-M-PERP");
        assert_eq!(fixture.settled_cell_text(0, 1), "BTCUSDT");
        assert_eq!(fixture.settled_cell_text(0, 2), "100.00");
        assert_eq!(fixture.settled_cell_text(0, 3), "2.000");
        assert_eq!(fixture.settled_cell_text(0, 4), "00:00:00.001");
    }

    /// Every cell draws its text in a theme foreground, and the listing cell draws in
    /// the direction colour of the liquidation's side.
    ///
    /// A cell that binds no colour draws in the engine's default `TextStyle` foreground
    /// (black): the kit cell box's `FgDefault` does not inherit into the text node, so
    /// four of the five columns would be unreadable on a dark appearance and differ
    /// from the old table, which resolved `theme.roles.fg.default` for every cell.
    #[test]
    fn the_cells_should_draw_in_the_theme_colours() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(both_sides_snapshot());
        fixture.build_body();

        let theme = fixture.harness.services().theme.clone();
        let light = theme.theme_for(Appearance::Light);
        let foreground = light.role(Role::FgDefault);
        let up = light.role(Role::MarketUp);
        let down = light.role(Role::MarketDown);
        assert_ne!(up, down, "the two sides are drawn in different colours");

        for row in 0..2 {
            for column in [0, 2, 3, 4] {
                assert_eq!(
                    fixture.cell_foreground(row, column),
                    foreground,
                    "row {row} column {column} draws in the theme's default foreground"
                );
            }
        }
        assert_eq!(
            fixture.cell_foreground(0, 1),
            down,
            "display row 0 is the newest liquidation, and it is short"
        );
        assert_eq!(
            fixture.cell_foreground(1, 1),
            up,
            "display row 1 is the older liquidation, and it is long"
        );
    }

    /// The colour is read through the theme on the frame that draws the cell, so an
    /// appearance switch redraws it in the new appearance's foreground (AGENTS §7/G3).
    #[test]
    fn the_cells_should_follow_the_theme_appearance() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(both_sides_snapshot());
        fixture.build_body();

        let theme = fixture.harness.services().theme.clone();
        let light = theme.theme_for(Appearance::Light).role(Role::FgDefault);
        let dark = theme.theme_for(Appearance::Dark).role(Role::FgDefault);
        assert_ne!(
            light, dark,
            "the two appearances draw different default foregrounds"
        );
        assert_eq!(fixture.cell_foreground(0, 0), light);

        assert!(theme.set_appearance(fixture.harness.app_mut(), Appearance::Dark));
        fixture.sync();

        for column in [0, 2, 3, 4] {
            assert_eq!(
                fixture.cell_foreground(0, column),
                dark,
                "column {column} redraws in the dark appearance's foreground"
            );
        }
        assert_eq!(
            fixture.cell_foreground(0, 1),
            theme.theme_for(Appearance::Dark).role(Role::MarketDown)
        );
    }

    /// The numbers are read in the display time zone, which the cell resolves inside
    /// `bind_text`: moving the setting redraws the time column without touching the
    /// data the stream delivered.
    #[test]
    fn moving_the_display_zone_should_redraw_the_time_column() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();
        assert_eq!(fixture.settled_cell_text(0, 4), "00:00:00.001");

        let config_signal = fixture.harness.services().config;
        let mut config = (*fixture.harness.app().read(&config_signal)).clone();
        config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
        assert!(
            fixture.harness.app_mut().write(&config_signal, config),
            "the setting move is a change"
        );
        fixture.sync();

        assert_eq!(
            fixture.settled_cell_text(0, 4),
            "08:00:00.001",
            "the time column redraws in the zone the settings now name"
        );
        assert_eq!(
            fixture.settled_cell_text(0, 2),
            "100.00",
            "the price is a number rather than a clock"
        );
    }

    /// A language switch rebuilds the table's static copy — its headers and its empty
    /// sentence are resolved text rather than bindings — while the accepted rows stay.
    #[test]
    fn a_language_change_should_rebuild_the_tables_static_copy() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();
        assert_eq!(fixture.header_text(4), "Time");

        fixture.harness.switch_language(LanguageCode::ZhCn);
        fixture.sync();

        assert_eq!(
            fixture
                .widget
                .table
                .as_ref()
                .expect("the rebuilt table is mounted")
                .columns()[4]
                .header
                .as_str(),
            "时间"
        );
        assert_eq!(fixture.table_rows(), 1, "the rebuilt table keeps the rows");

        let empty = fixture
            .widget
            .table
            .as_ref()
            .expect("the rebuilt table is mounted")
            .empty_node()
            .expect("the rebuilt table keeps its empty overlay");
        fixture.drain();
        assert!(
            fixture.harness.text_of(empty).contains("暂无数据"),
            "the empty sentence is read in the language showing now: {:?}",
            fixture.harness.text_of(empty)
        );
    }

    /// A language switch replaces the table's subtree: the old table's nodes are
    /// removed from the tree before the new one is mounted, so the switch does not
    /// leave a second table behind.
    #[test]
    fn a_language_change_should_not_leak_the_old_table() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();
        let nodes = fixture.harness.node_count();
        let previous = fixture
            .table()
            .outer_node()
            .expect("the table mounted its nodes");

        fixture.harness.switch_language(LanguageCode::ZhCn);
        fixture.sync();

        assert_eq!(
            fixture.harness.node_count(),
            nodes,
            "the rebuilt table replaces the old one instead of adding to it"
        );
        assert!(
            fixture.harness.tree().get(previous).is_err(),
            "the old table's subtree was removed from the tree"
        );
    }

    /// `release` hands the table back to the kit component: the handlers the table
    /// registered are removed with it, which is what keeps the application from holding
    /// the table (and its row contents) after the surface nodes are gone.
    #[test]
    fn releasing_the_widget_should_release_the_table() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();
        let select = Action::Custom(SmolStr::new_static("ForceOrderTable:body.select"));
        let sort = Action::Custom(SmolStr::new_static("ForceOrderTable.sort"));
        assert!(
            fixture.harness.app().has_action_handler(&select),
            "the mounted table registers its row handler"
        );
        assert!(
            fixture.harness.app().has_action_handler(&sort),
            "the mounted table registers its sort handler"
        );

        fixture.with_cx(|widget, cx| widget.release(cx));

        assert!(
            fixture.widget.table.is_none(),
            "release drops the widget's table"
        );
        assert!(
            !fixture.harness.app().has_action_handler(&select),
            "a table left un-released keeps its row handler registered"
        );
        assert!(
            !fixture.harness.app().has_action_handler(&sort),
            "a table left un-released keeps its sort handler registered"
        );
    }

    /// The body builds one frame and mounts the table under it; the frame is what
    /// carries the caption over the table.
    #[test]
    fn the_body_should_mount_the_table_under_its_frame() {
        let mut fixture = Fixture::new(PendingForceOrderSource, true);
        fixture.accept_snapshot(snapshot(1));
        fixture.build_body();

        let frame = fixture.body_node();
        let outer = fixture
            .table()
            .outer_node()
            .expect("the table mounted its nodes");
        assert_eq!(
            fixture
                .harness
                .tree()
                .get(outer)
                .expect("the table exists")
                .parent(),
            Some(frame),
            "the table is built into the frame"
        );
        assert_eq!(
            fixture.harness.tree().debug_id(frame),
            Some("ForceOrderTableFrame"),
            "the frame keeps its own selector"
        );
    }

    /// `common.market_not_loaded` is authored as markdown/HTML: the surface draws it
    /// through the markdown component, so a reader gets the sentence, the `<kbd>` icon
    /// and the venue list — never the source markup.
    #[test]
    fn a_rich_market_notice_is_drawn_as_rich_text() {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let widget = harness.mount::<ForceOrderWidget>(ForceOrderConfig);
        harness.drain();

        let placeholder = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_PLACEHOLDER")
            .expect("the placeholder stands in for the missing content");
        assert!(
            harness
                .find_by_debug_prefix(placeholder, "MARKDOWN")
                .is_some(),
            "the rich notice goes through the markdown component"
        );
        let text = harness.text_of(placeholder);
        assert!(
            !text.contains("<p ") && !text.contains("<kbd>") && !text.contains("![]("),
            "the notice's source markup is not drawn as text: {text:?}"
        );
        assert!(
            text.contains("Binance") && text.contains("OKX"),
            "the venues are listed by name: {text:?}"
        );
        assert!(
            !text.contains("Hyperliquid"),
            "the list names the venues whose loader can report liquidations: {text:?}"
        );
    }

    /// A write to the directory generation is what schedules the frame that runs every
    /// mounted widget's `sync`, including one that is only waiting and has no body: the
    /// framework's surface root carries the dependency.
    #[test]
    fn a_directory_generation_write_should_request_a_frame_for_a_waiting_pane() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<ForceOrderWidget>(ForceOrderConfig);
        harness.drain();
        assert!(
            widget.status().state(harness.app()).notice().is_some(),
            "the pane waits for a directory the host does not supply"
        );
        let generation = harness.services().catalog_generation;
        let before = harness.app().frame_requests();

        assert!(harness.app_mut().write(&generation, 1));

        assert!(
            harness.app().frame_requests() > before,
            "the surface root reads the generation, so the write schedules a frame"
        );
        harness.drain();
    }
}
