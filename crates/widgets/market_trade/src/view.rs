//! The market-trade tape widget.
//!
//! The widget owns one listing, and draws whatever the live trade stream publishes
//! for it. Everything the tape *does* around a frame — connecting, loading the opening
//! page, failing — is reported through the framework's status cell; the body only ever
//! draws the trades it has.
//!
//! # The table is built once and kept up to date
//!
//! [`WidgetView::body`] builds the kit table once and keeps it for the life of the
//! pane. The rows are updated through [`Table::set_rows`], and every dynamic value in
//! a row (the accepted trades, the display time zone, the theme colours) is a
//! `bind_text` / `bind_style` closure over the signals the pane owns — the kit table's
//! row factory runs **without an application context**, so a cell cannot read the
//! settings or the theme while it is built (AGENTS §9).
//!
//! The table's **static copy** is the other half: the column headers name the units of
//! the newest retained trade, and the empty-state sentence is a catalogue message, so
//! both are resolved when the table is built. A language switch, or a snapshot that
//! changes the units the headers name, is what makes the pane rebuild the table — the
//! one place this pane cannot express a dependency as a binding (AGENTS §15, gap 1).
//!
//! # One listing, one stream
//!
//! The stored selection is a whole `VenueListingKey`: two listings of one instrument
//! are two scopes, and neither is translated into the other. The service keys its
//! stream the same way, so a message that belongs to a listing the user has already
//! left is recognized and dropped.
//!
//! # The directory
//!
//! The pane never loads the directory and never polls the catalogue: it re-reads the
//! answers its [`MarketDirectory`] gives on every load, and re-runs that load when the
//! injected [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3). A listing the route provably dropped — it left the
//! catalogue, or its venue settled or delisted it — is reset to "nothing chosen"; a
//! lookup that failed proves nothing and leaves the stored value alone.
//!
//! # A generation is one number for the whole directory
//!
//! The generation signal says *that* the directory published a new generation, not
//! *which* venue changed (§8.3 has no per-venue version). So any write reloads this
//! pane, even a commit that only touched a venue it does not read: the trades are
//! dropped and the stream is reopened. That is the accepted trade — a row's labels
//! were written at the precision the stream resolved against the retired generation —
//! and the price is a visible reload on unrelated venue commits. Narrowing it needs the
//! composition root to publish the scope of a change, which the frozen contract does
//! not offer. The test `an_unrelated_directory_generation_should_reopen_the_stream`
//! pins the current behaviour; `force_order` took the same decision (W1b2 F2).
//!
//! # The header control
//!
//! The header holds the instrument picker, and the picker is
//! [`InstrumentButton`](instrument_selector_ui::instrument_button::InstrumentButton) —
//! the one control every pane that follows a listing draws. Its label and its filters
//! are chosen when it is built, so the pane rebuilds it when its static copy moves (a
//! language switch, the selection, or a directory generation that may have widened what
//! the picker offers). The control opens
//! [`InstrumentSelector`](instrument_selector_ui::instrument_selector::InstrumentSelector)
//! through [`SelectorHost`](instrument_selector_ui::host::SelectorHost), the host the
//! selector crate froze with the widget contract; this pane does not build its own
//! dialogs or reach the selector's globals.
//!
//! The control is built **once per pane header row** by [`WidgetView::header`] and
//! rebuilt in place by [`WidgetView::sync`] when its copy changed; the header row
//! itself belongs to the pane. What the widget keeps is the handle the control hands
//! back, and releasing that handle is what takes the component's click handler and its
//! signal scope down — a header rebuilt on every language switch would otherwise leak
//! one component scope per rebuild (W2a review F2).
//!
//! # Confirmation is a signal, not a re-entry
//!
//! The selector's confirm handler runs with an application context and no widget
//! context, so it cannot call into the pane: it writes the confirmed selection into
//! [`MarketTradeWidget::selection_requests`] and asks for a frame, and the next `sync`
//! applies it. The same split the news pane uses for its header control (AGENTS §4.2:
//! a control asks for the next `sync` instead of re-entering the widget).

use std::{mem, rc::Rc, sync::Arc};

use app_config::{
    settings::Config,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use base_ui::{kit::table::Table, style_builder::StyleBuilder, theme::handle::ThemeHandle};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::market::error::MarketError;
use market_participants_model::{
    listing::route::VenueListingKey, market::selection::InstrumentSelection, routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    window::WindowId,
};
use widget::{
    binding::{Inbound, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::market_is_catalog_not_loaded,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    columns::{
        COLUMN_COUNT, MarketTradeUnits, build_cell, columns, headers, table_options, units_of,
    },
    directory::{
        ConnectorDirectory, ListingSupport, MarketDirectory, listing_support, market_venues,
    },
    model::{MAX_TRADES, MarketTradeSnapshot},
    service::{
        MarketTradeProtocol, MarketTradeRequest, MarketTradeResponse, MarketTradeService,
        MarketTradeStatus, MarketTradeStreamKey, MarketTradeStreamMessage,
    },
};

/// How many trades the opening page asks for.
const HISTORY_LIMIT: u16 = 200;

/// The widget-local name of the header control's kit key.
///
/// A kit key is **application-level** — the button derives its click action from it
/// (`"<key>.click"`, the kit button's documented convention) and two panes that build the
/// same constant key share one handler — so the pane does not spell it out:
/// [`WidgetCx::unique_key`] stamps this suffix with the pane's instance scope. The same
/// suffix in the same instance always mints the same key, which is what lets a control
/// rebuilt on a later frame reclaim the identity it registered.
const HEADER_KEY_SUFFIX: &str = "market_trade.header.instrument";

/// The widget-local name of the table's kit key (its sort action is `"<key>.sort"`).
///
/// Unique per instance for the same reason as [`HEADER_KEY_SUFFIX`]: two panes that build a
/// table under one constant key would register one sort handler and the first release would
/// take it away from the second.
const TABLE_KEY_SUFFIX: &str = "market_trade.table";

/// The listing a tape follows, persisted with the widget.
///
/// The selection is optional because a tape can exist before the user has chosen
/// anything: it then asks for a listing instead of streaming a default nobody asked for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MarketTradeConfig {
    selection: Option<InstrumentSelection>,
}

impl WidgetConfig for MarketTradeConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

pub struct MarketTradeWidget {
    /// The options this widget persists.
    config: MarketTradeConfig,
    /// The executor bridge, when this host has a directory to drive a service from.
    ///
    /// A headless host without a connector has nothing to start: the pane waits for the
    /// directory instead of spawning a service it could never ask a question.
    binding: Option<WidgetBinding<MarketTradeProtocol>>,
    /// The table, once the body has been built (the framework builds a body only once
    /// the pane has content).
    table: Option<Table>,
    /// The node the table was built into; a static-copy rebuild mounts it there again.
    table_slot: Option<NodeId>,
    /// The copy the table was built with: a difference is what makes the pane rebuild it.
    table_built: Option<TableCopy>,
    /// The snapshot the table's static copy was last validated against.
    ///
    /// The headers name the units of the newest retained trade, so a snapshot that did
    /// not move cannot have moved the copy: comparing the read against this is what keeps
    /// the steady-state `sync` from resolving the display catalogue every frame.
    table_built_snapshot: Option<MarketTradeSnapshot>,
    status: WidgetStatus,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    /// The loaded directory, when the host has one.
    directory: Option<Rc<dyn MarketDirectory>>,
    /// The accepted trades, as the cells read them.
    rows: Signal<MarketTradeSnapshot>,
    /// The settings snapshot: the display time zone the time column is read in.
    config_signal: Signal<Config>,
    /// The selected language, read where the table's static copy is built.
    language: Signal<LanguageCode>,
    /// The directory generation the composition root publishes.
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    snapshot: Option<MarketTradeSnapshot>,
    pending_snapshot: Option<MarketTradeSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    /// Whether the pane is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market again
    /// once the user loads it — so a trade that was in flight when the route went away
    /// can still arrive. Accepting it would put a snapshot back and make the pane report
    /// content over the notice the framework is drawing, so every message entry refuses
    /// while this is set.
    awaiting_market: bool,
    /// Whether a directory generation moved under the stream the pane is reading.
    ///
    /// The rows were named and written at the generation the stream resolved when it
    /// was opened; a new generation may have replaced it, so the next load asks for the
    /// request that opens a new stream (`Retry`) rather than the idempotent `Load`.
    market_reloaded: bool,
    /// The generation this pane has already reacted to.
    seen_generation: u64,
    /// Whether the accepted rows changed since the table was last updated.
    rows_dirty: bool,
    /// Selections confirmed in the header control, waiting for the next `sync`.
    ///
    /// A button handler owns no widget context, so it asks the next frame for the
    /// transition instead of performing it.
    selection_requests: Signal<Option<InstrumentSelection>>,
    /// The header control this pane built: the parent it lives in and the copy it was
    /// built with. The row belongs to the pane; only the component is this widget's.
    header: Option<HeaderControl>,
}

/// The static copy a table was built with.
///
/// The kit table takes its headers and its empty sentence as resolved text, so this is
/// what says whether the current table still states the truth. Comparing it is how the
/// pane decides to rebuild, and the language it carries is what the rows' `bind_text`
/// closures do not need to know (they follow their own signal).
#[derive(Clone, Debug, PartialEq, Eq)]
struct TableCopy {
    language: LanguageCode,
    headers: [SmolStr; COLUMN_COUNT],
    empty: SmolStr,
}

/// The header control and the copy it was built with.
///
/// The control is kept as the selector crate's handle rather than as a bare node: a kit
/// component owns a signal scope and (when it is live) the click handler its `on_click`
/// registered, so only the component can hand both back (`InstrumentButtonHandle::release`).
/// Holding the node alone would leave the widget able to unregister the handler but not to
/// reclaim the scope.
struct HeaderControl {
    control: InstrumentButtonHandle,
    parent: NodeId,
    built: HeaderState,
}

/// What the header control states, as one comparable value.
///
/// The generation is part of it because the filters snapshot the control opens its
/// selector with is read from the loaded venues when the control is built: a venue
/// loaded later is one the user must be able to pick on, and a control built before
/// that commit would keep opening a picker that does not offer it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderState {
    selection: Option<InstrumentSelection>,
    language: LanguageCode,
    generation: u64,
}

impl WidgetView for MarketTradeWidget {
    type Config = MarketTradeConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market_trade");
    const DISPLAY_NAME: &'static str = "widget.names.market_trade";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.market_trade");
    const ICON: Option<&'static str> = Some("widgets/market_trade.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // A host without a directory has nothing to ask and nothing to start; the pane
        // reports the neutral notice and waits. Every application supplies one.
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                config,
                None,
                None,
                status,
                config_change,
                recovery,
                cx,
            ));
        };
        let service = MarketTradeService::new(connector.market(), HISTORY_LIMIT, MAX_TRADES);
        let binding = WidgetBinding::spawn(cx, service);
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory::new(connector));
        Ok(Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The table is the content, and it is ready as soon as a snapshot has been
    /// accepted — including an empty one. An empty tape is what "nothing has printed
    /// yet" looks like; it is not a state the widget reports.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The market this tape follows has to be loaded before anything else happens
        // here, and the check is first on purpose: a route the user never loaded, or one
        // a shrinking commit withdrew, leaves the pane waiting for it instead of reaching
        // the branches below, which settle the selection the user stored.
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        if !self.market_ready(directory.as_ref()) {
            self.wait_for_market(cx);
            return;
        }
        // The pane reached here only past its own gate, so the markets it was waiting
        // for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        // Read past the gate: a commit on the tape's own venue that took its market away
        // leaves this set while the stream that resolved against the retired generation
        // is still alive, and the load that brings the market back has to reopen that
        // stream.
        let market_reloaded = mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry {
            // An explicit load starts over, because the widget cannot vouch for what it
            // was showing. A retry keeps the trades the user is still reading.
            self.clear_trades();
        }
        // The two decisions are separate here: the trades still go for a reload — nothing
        // on screen can be vouched for after the generation behind it was replaced —
        // while the request is the replacing one.
        self.start(retry || market_reloaded, cx);
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        // The body is the content slot's whole surface: the kit table draws the header
        // and the rows, and the frame exists for the caption the pane states over them.
        let frame = NodeBuilder::boxed()
            .id("MarketTradeTableFrame")
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
                    label: Some(t_smol!(translate, "widget.market_trade.table")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        let copy = self.table_copy(cx);
        let table = self.build_table(cx, copy.clone())?;
        let window = cx.window();
        table.mount(cx.app_mut(), window, frame)?;
        self.table = Some(table);
        self.table_slot = Some(frame);
        self.table_built = Some(copy);
        self.table_built_snapshot = self.snapshot.clone();
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The directory generation is the one signal the framework's surface root also
        // reads, so a write schedules the frame that runs this hook — including for a
        // pane that is only waiting and has no body. Nothing else may poll the directory
        // (AGENTS §8.3).
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                // A new generation may have replaced the one the live stream resolved its
                // listing against, so the load that follows asks for a new stream.
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        // The binding delivers messages instead of calling back into the widget; this is
        // the one place they are applied, where the widget has a context again.
        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        // A confirmation is applied here rather than in the control's handler, which owns
        // no widget context (see the module documentation).
        let requested = (*cx.read(&self.selection_requests)).clone();
        if let Some(selection) = requested {
            cx.write(&self.selection_requests, None);
            self.select_selection(selection, cx);
        }

        // The table's static copy (headers and the empty sentence) is resolved text, so a
        // language switch or a snapshot that changes the units it names rebuilds it. The
        // cheap comparisons come first: the display catalogue is only resolved when one of
        // them says the copy may have moved.
        let language = *cx.read(&self.language);
        let copy_moved = self.table_built.as_ref().is_none_or(|copy| {
            copy.language != language || self.snapshot != self.table_built_snapshot
        });
        if self.table.is_some() && copy_moved {
            let copy = self.table_copy(cx);
            self.table_built_snapshot = self.snapshot.clone();
            if self.table_built.as_ref() != Some(&copy) {
                self.rebuild_table(cx, copy)?;
            }
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

        self.sync_header(cx)?;

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Left {
            return Ok(());
        }
        // The pane is building its row; the control this widget built into the previous
        // one went with it. Only the component is the widget's to take down.
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // The table's nodes are removed by the framework after this hook, so the
        // component is released here while its nodes are still alive. The control in the
        // header row belongs to the pane; the component it stands on is the widget's.
        self.release_header(cx, false);
        if let Some(table) = self.table.take() {
            let _ = table.release(cx.app_mut());
        }
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.table_slot = None;
        self.table_built = None;
        self.table_built_snapshot = None;
    }
}

impl MarketTradeWidget {
    /// Builds a widget over the collaborators a caller hands it.
    fn assemble(
        config: MarketTradeConfig,
        binding: Option<WidgetBinding<MarketTradeProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let rows = cx.signal(MarketTradeSnapshot::default());
        let selection_requests = cx.signal(None::<InstrumentSelection>);
        let config_signal = cx.services().config;
        let language = cx.services().language;
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        Self {
            config,
            binding,
            table: None,
            table_slot: None,
            table_built: None,
            table_built_snapshot: None,
            status,
            recovery,
            config_change,
            directory,
            rows,
            config_signal,
            language,
            generation,
            translate,
            theme,
            snapshot: None,
            pending_snapshot: None,
            ready_received: false,
            awaiting_snapshot: true,
            awaiting_market: false,
            market_reloaded: false,
            seen_generation,
            rows_dirty: false,
            selection_requests,
            header: None,
        }
    }

    /// Builds a widget over a test's own source and directory.
    ///
    /// The framework's headless host composes the services without a directory (the
    /// connector still needs the old toolkit's runtime to build), so an interface test
    /// constructs the widget directly with the fixture directory it wants, and drives
    /// the widget hooks itself.
    #[cfg(test)]
    fn with_source<S: super::service::MarketTradeSource>(
        config: MarketTradeConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = MarketTradeService::new(source, HISTORY_LIMIT, MAX_TRADES);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        )
    }

    /// Whether what this tape needs is in memory: at least one market that answers with
    /// trades, and the route of the stored selection among them.
    ///
    /// A tape that follows nothing still needs a market: the picker is drawn from the
    /// loaded venues, so with none of them loaded there is nothing to choose from — the
    /// pane waits for the market participants metadata and says so, rather than asking for a
    /// choice that is not there. Once one is loaded, a tape that still follows nothing is
    /// the "select an instrument" notice's business and this answers `true` for it.
    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        if !directory.any_market_loaded() {
            return false;
        }

        match self.config.selection.as_ref() {
            Some(selection) => directory.is_loaded(&selection.listing.route),
            None => true,
        }
    }

    /// Reports the neutral notice for the market this tape follows, and drops the trades
    /// it was showing.
    ///
    /// The route behind that tape is gone, so nothing on screen is backed by a market any
    /// more, and the framework only draws the notice while the pane reports no content —
    /// which is why the pane also stops accepting messages: the stream it was reading is
    /// still alive, and a trade that was already in flight would otherwise put content
    /// back over the notice. The stored selection is left alone: it is the user's, and
    /// loading the market again is what brings it back.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_trades();
        self.report_markets_not_loaded(cx);
    }

    /// Reports the notice naming the market this tape follows.
    ///
    /// The list the sentence ends with is every venue that answers with trades — the
    /// capability this pane is defined by, read off the definition table rather than off
    /// what happens to be loaded — while the market it waits for is the one behind the
    /// selection, or that whole list while the user follows nothing and no single venue
    /// can be named. The sentence is the framework's to resolve, in the language showing
    /// on the frame that draws it; the pane hands over a catalogue reference only.
    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let supported = market_venues();
        let venues = self
            .config
            .selection
            .as_ref()
            .map(|selection| vec![selection.listing.route.venue])
            .unwrap_or_else(|| supported.clone());

        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }),
        );
    }

    /// Reports the prompt asking for a listing.
    fn report_nothing_chosen(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message(
                "widget.market_trade.select_instrument_prompt",
            )),
        );
    }

    /// Reports the neutral notice for a read the directory answered "not loaded" to, and
    /// drops the trades it was showing.
    ///
    /// The pane's own gate asks the directory before it starts a stream, so this is the
    /// answer to a question the gate could not have seen the answer to: the route was
    /// revoked between the two. That is a wait rather than a failure. Returns whether it
    /// reported, so a caller keeps its own failure path for everything else.
    fn report_waiting_for_directory(&mut self, error: &MarketError, cx: &mut WidgetCx<'_>) -> bool {
        if !market_is_catalog_not_loaded(error) {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    /// The stream scope this tape would accept messages from.
    ///
    /// Absent until a listing is chosen: nothing is streaming, so nothing is in scope.
    fn current_key(&self) -> Option<MarketTradeStreamKey> {
        self.config
            .selection
            .as_ref()
            .map(|selection| MarketTradeStreamKey::new(selection.listing.clone()))
    }

    /// Puts the stored selection back to "nothing chosen" when the listing cannot be
    /// followed any more.
    ///
    /// The value is one this build can no longer act on — the route left the registry,
    /// or the venue delisted the listing — so it goes back to the config default and the
    /// pane is told, or the next start would only reset it again.
    fn withdraw(&mut self, listing: &VenueListingKey, cause: &str, cx: &mut WidgetCx<'_>) {
        log::warn!(
            "`{}` {cause}; the tape went back to having nothing chosen",
            listing.listing_id
        );
        self.config.selection = None;
        self.config_change.publish(cx.app_mut());
        self.nothing_chosen(cx);
    }

    /// Applies what the directory answered about the stored listing.
    ///
    /// Returns whether this answer still describes the scope the tape follows, which is
    /// what decides whether the subscription for it starts. A choice made while the
    /// answer was being read is the user's, and the answer about the listing they left
    /// is not this tape's to apply.
    fn settle_selection(
        &mut self,
        selection: InstrumentSelection,
        support: ListingSupport,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.config.selection.as_ref() != Some(&selection) {
            return false;
        }
        if support == ListingSupport::Withdrawn {
            self.withdraw(&selection.listing, "is no longer offered by its route", cx);
        }
        true
    }

    /// Settles the stored scope against the route, and starts the tape on it.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        // The pane reached here only past its own gate, so the market it was waiting for
        // is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        let Some(directory) = self.directory.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        // A stored listing the route no longer offers is settled before the tape
        // subscribes to it: subscribing first would report a failure about a scope the
        // user cannot return to anyway. The lookup is work this tape is doing, so it
        // reports connecting while it is in flight — the request that follows is what
        // connects. That request is also what settles the scope for real.
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        // What the loaded venues answer is read here, where the settlement is decided,
        // rather than remembered: a venue the user loads after this tape started reading
        // carries a scope it may follow, and one a shrinking commit withdrew carries none.
        let routes = directory.loaded_scopes();
        // A route that is not loaded cannot be asked anything at all, so it is settled
        // without a lookup: the catalogue would answer with the same refusal.
        if !offers_route(&routes, &selection.listing.route, CapabilityId::MarketTrade) {
            self.withdraw(&selection.listing, "is no longer a loaded route", cx);
            return;
        }
        let offered = directory.contains(&selection.listing);
        let support = listing_support(&selection.listing, offered, directory.as_ref());
        if !self.settle_selection(selection, support, cx) {
            return;
        }
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        self.load_trades(selection, retry, cx);
    }

    /// Asks the service for the settled scope.
    fn load_trades(&mut self, selection: InstrumentSelection, retry: bool, cx: &mut WidgetCx<'_>) {
        let listing = selection.listing;
        let request = if retry {
            MarketTradeRequest::Retry { listing }
        } else {
            MarketTradeRequest::Load { listing }
        };
        if let Some(binding) = self.binding.as_mut() {
            binding.call(cx, request);
        }
    }

    /// Says what a tape with nothing to follow is doing: nothing, and it is not a failure.
    ///
    /// There is nothing to follow, so there is no market to read and nothing the tape can
    /// vouch for. It has no content of its own, so it says what it is waiting for instead
    /// of reporting work it is not doing.
    fn nothing_chosen(&mut self, cx: &mut WidgetCx<'_>) {
        self.clear_trades();
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        self.report_nothing_chosen(cx);
    }

    /// Applies a selector result as one configuration transition.
    ///
    /// Returns whether the selection was accepted. A listing key is the whole of a scope,
    /// so two listings of one instrument are two selections and neither is translated
    /// into the other.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        // The trades on screen belong to the scope the user just left.
        self.clear_trades();
        // A pane that is waiting for its market is a pane whose gate `start` would not run:
        // `start` reads the loaded scopes of a directory that has none and withdraws the
        // choice the user just made, writing the default back on top of it. `load` is the
        // path that runs the gate, and it keeps the choice while it waits (AGENTS §4.2).
        if self.awaiting_market {
            self.load(cx);
        } else {
            self.start(false, cx);
        }
        true
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<MarketTradeProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // An answer that arrives after the pane failed, or while it waits for its market, is
        // about a call that no longer belongs to what the pane is showing: the failure is
        // terminal until the user recovers, and a late error must not replace it (AGENTS §4.4).
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        match result {
            Ok(MarketTradeResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status
                    .report(cx.app_mut(), WidgetState::failed(Arc::new(error), recovery));
            }
        }
    }

    fn apply(&mut self, inbound: Inbound<MarketTradeProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { result, .. } => self.apply_rpc_result(result, cx),
        }
    }

    fn apply_reliable(
        &mut self,
        key: MarketTradeStreamKey,
        message: &MarketTradeStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }

        match message {
            MarketTradeStreamMessage::Status(status) => match status {
                MarketTradeStatus::Connecting => {
                    if self.snapshot.is_none() {
                        self.status.report(cx.app_mut(), WidgetState::Connecting);
                    }
                }
                MarketTradeStatus::LoadingHistory => {
                    if self.snapshot.is_none() {
                        self.status.report(cx.app_mut(), WidgetState::Loading);
                    }
                }
                MarketTradeStatus::Ready => {
                    self.ready_received = true;
                    if let Some(snapshot) = self.pending_snapshot.take() {
                        self.apply_snapshot(snapshot, cx);
                    } else if !self.awaiting_snapshot {
                        self.status.report(cx.app_mut(), WidgetState::Ready);
                    }
                }
                MarketTradeStatus::Error(error) => {
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
            MarketTradeStreamMessage::Snapshot(_) => {}
        }
    }

    fn apply_latest(
        &mut self,
        key: MarketTradeStreamKey,
        message: &MarketTradeStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }

        if let MarketTradeStreamMessage::Snapshot(snapshot) = message {
            if self.ready_received {
                self.apply_snapshot(snapshot.clone(), cx);
            } else {
                self.pending_snapshot = Some(snapshot.clone());
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: MarketTradeSnapshot, cx: &mut WidgetCx<'_>) {
        if self.snapshot.as_ref() != Some(&snapshot) {
            self.rows_dirty = true;
        }
        self.snapshot = Some(snapshot);
        self.awaiting_snapshot = false;
        self.status.report(cx.app_mut(), WidgetState::Ready);
    }

    fn clear_trades(&mut self) {
        if self.snapshot.take().is_some() {
            self.rows_dirty = true;
        }
    }

    /// The static copy the table would be built with now.
    fn table_copy(&self, cx: &WidgetCx<'_>) -> TableCopy {
        let units = self.units();
        TableCopy {
            language: *cx.read(&self.language),
            headers: headers(units.as_ref(), &self.translate),
            empty: t_smol!(self.translate, "common.no_data"),
        }
    }

    /// The units of the newest retained trade, as the published generation names them.
    fn units(&self) -> Option<MarketTradeUnits> {
        let directory = self.directory.as_ref()?;
        let snapshot = self.snapshot.as_ref()?;
        let catalog = directory.catalog();
        units_of(snapshot, catalog.as_ref(), &self.translate)
    }

    /// Builds the table into the body slot, over the accepted rows.
    fn build_table(&self, cx: &mut WidgetCx<'_>, copy: TableCopy) -> Result<Table, EngineError> {
        let theme = self.theme.clone();
        let cell_theme = self.theme.clone();
        let font_size = self.theme.tokens().medium.font_size;
        let row_height = self.theme.tokens().medium.height;
        let rows = self.rows;
        let config = self.config_signal;

        let cells = move |tree: &mut ui_core::node::tree::NodeTree,
                          parent: NodeId,
                          row: usize,
                          column: usize| {
            build_cell(
                tree,
                parent,
                row,
                column,
                rows,
                config,
                cell_theme.clone(),
                font_size,
            );
        };
        let key = cx.unique_key(TABLE_KEY_SUFFIX);
        let table = Table::new(
            cx.app_mut(),
            theme,
            key,
            columns(copy.headers),
            table_options(row_height),
            cells,
            copy.empty,
        );
        let rows = self.snapshot.as_ref().map_or(0, MarketTradeSnapshot::len);
        table.set_initial_rows(cx.app_mut(), rows);
        Ok(table)
    }

    /// Rebuilds the table after its static copy moved.
    ///
    /// The kit table's headers and empty sentence are resolved text rather than bindings,
    /// so this is the one part of the body a language switch (or a snapshot that changes
    /// the units the headers name) has to rebuild.
    fn rebuild_table(&mut self, cx: &mut WidgetCx<'_>, copy: TableCopy) -> Result<(), EngineError> {
        let (Some(slot), Some(previous)) = (self.table_slot, self.table.take()) else {
            return Ok(());
        };
        let window = cx.window();
        if let Some(outer) = previous.outer_node() {
            cx.app_mut().remove_node(window, outer)?;
        }
        previous.release(cx.app_mut());

        let table = self.build_table(cx, copy.clone())?;
        table.mount(cx.app_mut(), window, slot)?;
        self.table = Some(table);
        self.table_built = Some(copy);
        Ok(())
    }

    /// The filters the picker this control opens is built with.
    ///
    /// The loaded venues are read here, where the control is built, rather than kept: a
    /// scope loaded after this tape started reading is a scope the user may select, and
    /// one a shrinking commit withdrew is not.
    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::MarketTrade,
            entry_predicate: None,
            routes: self.directory.as_ref().map_or_else(
                || Arc::from(Vec::<LoadedScope>::new()),
                |directory| directory.loaded_scopes(),
            ),
        }
    }

    /// The selector host the header control opens its dialog through.
    ///
    /// The host is the one thing in this widget that owns the services rather than
    /// borrowing them, so it takes the shared handle the composition root handed over
    /// ([`WidgetCx::services_rc`]) — the same aggregate the widget works over, not a
    /// second one assembled from its fields.
    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        match &self.directory {
            // The host derives its catalogue from the injected directory root; the pane's
            // own directory is the same root in production, and the only source in a
            // headless test.
            Some(directory) => host.with_catalog(Some(directory.catalog())),
            None => host,
        }
    }

    fn header_state(&self, cx: &WidgetCx<'_>) -> HeaderState {
        HeaderState {
            selection: self.config.selection.clone(),
            language: *cx.read(&self.language),
            generation: self.seen_generation,
        }
    }

    /// Builds the instrument control into the header group's container.
    fn build_header(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let host = self.selector_host(cx);
        let filters = self.instrument_filters();
        let selection = self.config.selection.clone();
        let requests = self.selection_requests;
        let button = InstrumentButton::new(
            cx.unique_key(HEADER_KEY_SUFFIX),
            selection,
            filters,
            "widget.market_trade.select_instrument",
            move |selection: InstrumentSelection, app: &mut App, window: WindowId| {
                hand_over_selection(requests, selection, app, window);
            },
        );
        let window = cx.window();
        let control = button.build(&host, cx.app_mut(), window, parent)?;
        self.header = Some(HeaderControl {
            control,
            parent,
            built: self.header_state(cx),
        });
        Ok(())
    }

    /// Keeps the header control in step with the copy it was built with.
    ///
    /// The control's label, its filters and its action are chosen when it is built, so a
    /// language switch, a new selection or a directory generation that may have widened
    /// the picker is what rebuilds it. The parent is the pane's node: if it is gone, the
    /// control went with the row and there is nothing to rebuild until the pane asks for
    /// a header again.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(header) = self.header.as_ref() else {
            return Ok(());
        };
        let moved = header.built.language != *cx.read(&self.language)
            || header.built.generation != self.seen_generation
            || header.built.selection.as_ref() != self.config.selection.as_ref();
        if !moved {
            return Ok(());
        }
        let parent = header.parent;
        if cx.tree().get(parent).is_err() {
            // The pane took its row down; the control went with it, and only the
            // component (its click handler and signal scope) is left to release.
            self.release_header(cx, false);
            return Ok(());
        }
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    /// Takes down the header control's component.
    ///
    /// `remove_node` is true when the pane's row is still there and the widget is
    /// replacing its own control in it; a row the pane already took down needs no node
    /// removal (`StaleNode`). The node itself belongs to the pane's header row, but the
    /// **component** — its click handler and its signal scope — is this widget's in every
    /// case, which is why the handle the selector's control hands back is released here.
    fn release_header(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header.take() else {
            return;
        };
        let node = header.control.node();
        if remove_node && cx.tree().get(node).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        let _ = header.control.release(cx.app_mut());
    }
}

/// Hands a confirmed selection over to the next `sync`.
///
/// The control's confirm handler runs with an application context and no widget context, so
/// it cannot call into the pane: it writes the request signal and asks for a frame, and
/// [`WidgetView::sync`] applies the choice and publishes the configuration change. A
/// confirmation that wrote nothing would leave the pane following the listing it was already
/// on, however far the user browsed.
fn hand_over_selection(
    requests: Signal<Option<InstrumentSelection>>,
    selection: InstrumentSelection,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(selection));
    let _ = app.request_repaint(window);
}

#[cfg(test)]
mod tests;
