//! The watch-list widget.
//!
//! The widget follows a set of listings the user picked, one stream each, and
//! draws one row per listing. Everything the widget *does* around a frame —
//! connecting, loading, failing — is reported through the framework's status
//! handle; the body only ever draws the listings it follows.
//!
//! # The followed set is applied before the service confirms it
//!
//! Adding or removing a listing changes the widget's own list immediately, and
//! then asks the service for the new set. The user sees the row they asked for
//! without waiting for a round trip, and a request the service refuses is undone
//! by the command recorded in the in-flight table, which remembers what the
//! operation replaced. The service only changes the streams it follows once the
//! caller has the response, so the two sides cannot settle on different sets
//! without one of them failing — and a failure is what the recovery
//! re-synchronizes.
//!
//! # The tables are built once and kept up to date
//!
//! [`WidgetView::body`] builds one kit table per market once and keeps them for the
//! life of the pane; the rows reach them through [`Table::set_rows`], and every
//! dynamic value in a row is a `bind_text` / `bind_style` closure (see `table`).
//! The row controls are the one part a cell cannot build (the kit table's row
//! factory has no application context), so the factory builds their slot and this
//! module builds the buttons into it from [`WidgetView::sync`].
//!
//! # Every value is drawn with the unit it came with
//!
//! A listing publishes the traded quantity of its session in one unit and, when it
//! publishes one at all, a traded notional in an asset. Both are the listing's own,
//! so both are named where the row is drawn, and a value the listing does not
//! publish is drawn as missing rather than as zero or as the other unit.
//!
//! # The directory
//!
//! Which markets are loaded, whether a followed listing is still published and what
//! the catalogue calls it are the injected directory's answers — the connector in
//! the application, a fixture in a test. The pane never loads the directory and
//! never polls the catalogue: it re-reads the answers its [`WatchListDirectory`]
//! gives on every load, and re-runs that load when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3).
//!
//! # A generation is one number for the whole directory
//!
//! The signal says *that* the directory published a new generation, not *which
//! venue* changed (§8.3 has no per-venue version). So any write reloads this pane,
//! even a commit that only touched a venue it does not follow: the rows were written
//! with names and units of the retired generation, so the entries are rebuilt and
//! the streams are reopened against the new catalogue. Narrowing it needs the
//! composition root to publish the scope of a change, which the frozen contract does
//! not offer. The test `an_unrelated_generation_should_reload_the_pane` pins the
//! current behaviour.

use std::{cell::RefCell, collections::BTreeMap, mem, rc::Rc, sync::Arc};

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentSelector, InstrumentStatusFilter,
        LoadedScope, offers_route,
    },
};
use market_participants_connector::catalog::store::Catalog;
use market_participants_model::{
    listing::route::VenueListingKey, market::selection::InstrumentSelection, routing::CapabilityId,
};
use serde::{Deserialize, Deserializer, de};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

#[cfg(test)]
use super::service::TickerSource;
use super::{
    directory::{
        ConnectorDirectory, ListingSupport, WatchListDirectory, listing_support, market_venues,
    },
    model::{
        MAX_WATCH_LIST_ENTRIES, WatchListEntry, WatchListMarket, WatchListRow, WatchListRowState,
        WatchListStatus, WatchListStreamError, WatchListStreamEvent, sort_entries, split_rows,
    },
    service::{
        WatchListProtocol, WatchListRequest, WatchListResponse, WatchListService,
        WatchListServiceError, validate_replacement,
    },
    table::{TableHandle, TablePane, TableRows, TableSignals},
};

/// What a table frame is drawn under, and the key suffix of its kit table.
const CONTINUOUS_TABLE_KEY: &str = "watch_list.table.continuous";
const SESSION_TABLE_KEY: &str = "watch_list.table.session";

/// The header control's key suffix.
const HEADER_KEY_SUFFIX: &str = "watch_list.header.add";

/// The listings one watch list follows.
///
/// The entries are decoded through the same validation the service applies, so a
/// persisted list is either a list the service can follow or a configuration
/// error the framework reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct WatchListConfig {
    entries: Vec<VenueListingKey>,
}

#[derive(Deserialize)]
struct WatchListConfigWire {
    entries: Vec<VenueListingKey>,
}

impl<'de> Deserialize<'de> for WatchListConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WatchListConfigWire::deserialize(deserializer)?;
        validate_replacement(&wire.entries).map_err(de::Error::custom)?;

        Ok(Self {
            entries: wire.entries,
        })
    }
}

impl WidgetConfig for WatchListConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// One command this pane sent and has not seen the answer to.
///
/// A reply whose identity is not in the table answers a call the pane already
/// replaced; it is dropped and counted rather than applied to the command that
/// replaced it (AGENTS §8.2).
enum PendingCommand {
    /// The followed set is being replaced by an explicit or recovery load — or by the
    /// market behind it being loaded again, which asks for the same streams to be
    /// resolved once more — and none of those changes anything locally.
    Load,
    /// A listing was added before the service confirmed the new set.
    Add(VenueListingKey),
    /// A listing was removed before the service confirmed the new set; the entry it had is
    /// kept until the removal is, and is `None` for a listing whose market was not loaded —
    /// there was nothing to keep.
    Remove(VenueListingKey, Option<Box<WatchListEntry>>),
    /// A listing's stream is being reopened; its failure is kept until the retry is
    /// confirmed.
    Retry {
        key: VenueListingKey,
        error: WatchListStreamError,
    },
}

impl PendingCommand {
    /// Puts back what this operation changed, now that the service refused it.
    fn undo(
        self,
        config: &mut WatchListConfig,
        entries: &mut HashMap<VenueListingKey, WatchListEntry>,
    ) {
        match self {
            Self::Load => {}
            Self::Add(key) => {
                config.entries.retain(|entry| entry != &key);
                entries.remove(&key);
            }
            Self::Remove(key, entry) => {
                config.entries.push(key.clone());
                sort_entries(&mut config.entries);
                if let Some(entry) = entry {
                    entries.insert(key, *entry);
                }
            }
            Self::Retry { key, error } => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.apply_event(&WatchListStreamEvent::Error(error));
                }
            }
        }
    }
}

/// A failure of the widget itself, rather than of one followed listing.
///
/// A row reports its own stream failure where the row is. Only what the widget as
/// a whole cannot do is reported through its status, because that is what the
/// framework turns into a banner or a placeholder.
#[derive(Debug)]
enum WatchListFailure {
    /// The service refused an operation.
    Rpc(RpcCallError<WatchListServiceError>),
    /// The list already follows as many listings as it can.
    Capacity,
}

impl TranslatedError for WatchListFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Rpc(error) => error.translated_message(translate),
            Self::Capacity => t_smol!(
                translate,
                "error.watch_list.capacity",
                maximum = MAX_WATCH_LIST_ENTRIES
            ),
        }
    }
}

/// What the directory answered about one followed listing.
struct ListingAnswer {
    key: VenueListingKey,
    support: ListingSupport,
}
///
/// A button handler runs with an application context and no widget context, so it
/// writes the request into a signal and asks for a frame; the pane applies it in
/// [`WidgetView::sync`], where it has its own state again.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowRequest {
    Retry(VenueListingKey),
    Remove(VenueListingKey),
}

/// The static copy a header control was built with.
///
/// A kit control resolves its label and its filters when it is built, so the pane
/// rebuilds it when any of these move and disables it in place otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderCopy {
    language: LanguageCode,
    generation: u64,
    has_directory: bool,
}

/// The header control and the copy it was built with.
struct HeaderControl {
    button: Button,
    node: NodeId,
    parent: NodeId,
    built: HeaderCopy,
}

/// The per-row controls of one table row, with the copy they were built with.
struct RowControls {
    slot: NodeId,
    listing: Arc<VenueListingKey>,
    language: LanguageCode,
    disabled: bool,
    retry: bool,
    remove_label: SmolStr,
    buttons: Vec<Button>,
}

/// The parts of one row's controls a build depends on.
#[derive(PartialEq, Eq)]
struct RowCopy {
    listing: Arc<VenueListingKey>,
    language: LanguageCode,
    disabled: bool,
    retry: bool,
    remove_label: SmolStr,
}

impl RowCopy {
    fn of(
        row: &WatchListRow,
        language: LanguageCode,
        disabled: bool,
        catalog: &Catalog,
        translate: &Translate,
    ) -> Self {
        Self {
            listing: Arc::clone(row.key()),
            language,
            disabled,
            retry: row.state().is_read() && row.status() == WatchListStatus::Error,
            remove_label: remove_label(row, catalog, translate),
        }
    }

    fn matches(&self, controls: &RowControls, slot: NodeId) -> bool {
        controls.slot == slot
            && controls.language == self.language
            && controls.disabled == self.disabled
            && controls.retry == self.retry
            && Arc::ptr_eq(&controls.listing, &self.listing)
            && controls.remove_label == self.remove_label
    }
}

pub struct WatchListWidget {
    config: WatchListConfig,
    /// The executor bridge, when this host has a directory to drive a service from.
    binding: Option<WidgetBinding<WatchListProtocol>>,
    /// The commands in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    /// The loaded directory, when the host has one.
    directory: Option<Rc<dyn WatchListDirectory>>,
    status: WidgetStatus,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    translate: Translate,
    theme: ThemeHandle,
    language: Signal<LanguageCode>,
    /// The directory generation the composition root publishes.
    generation: Signal<u64>,
    /// The catalogue mirror the cells read, and the revision that says it moved.
    catalog: Rc<RefCell<Arc<Catalog>>>,
    catalog_revision: Signal<u64>,
    /// Selections confirmed in the header control, waiting for the next `sync`.
    selection_requests: Signal<Option<InstrumentSelection>>,
    /// Requests from the per-row controls, waiting for the next `sync`.
    row_requests: Signal<Option<RowRequest>>,
    /// Whether the header control accepts input, as the header control reads it.
    add_enabled: Signal<bool>,
    entries: HashMap<VenueListingKey, WatchListEntry>,
    /// Whether the service has confirmed the followed set at least once.
    ///
    /// It is what an empty list has to say: a list the user emptied is the empty table, while
    /// a list that has not been agreed to yet is a list still being asked about.
    confirmed: bool,
    /// Whether a market this list follows was loaded or refreshed again.
    ///
    /// Every stream resolves the increments its rows are written at when it is opened, so a
    /// commit that lands on one of the followed listings' markets replaces the generation
    /// behind that stream while the stream keeps what it resolved. The list therefore starts
    /// over, and the request it sends has to be the one that reopens those streams.
    market_reloaded: bool,
    /// The listings whose streams stopped being readable on their own.
    dropped: HashSet<VenueListingKey>,
    /// Whether the list is waiting for its markets rather than reading them.
    awaiting_market: bool,
    seen_generation: u64,
    seen_language: LanguageCode,
    published_catalog_revision: u64,
    catalog_moved: bool,
    /// Whether the rows drawn changed since the tables were last updated.
    rows_dirty: bool,
    /// Whether what the row controls offer changed since they were last built.
    controls_dirty: bool,
    /// The body root and the two table frames.
    body_frame: Option<NodeId>,
    continuous_slot: Option<NodeId>,
    session_slot: Option<NodeId>,
    /// The handles the two tables read through, minted before the tables exist.
    continuous_handle: TableHandle,
    session_handle: TableHandle,
    continuous: Option<TablePane>,
    session: Option<TablePane>,
    continuous_structure: Vec<(VenueListingKey, bool)>,
    session_structure: Vec<(VenueListingKey, bool)>,
    continuous_controls: BTreeMap<usize, RowControls>,
    session_controls: BTreeMap<usize, RowControls>,
    header: Option<HeaderControl>,
}

impl WidgetView for WatchListWidget {
    type Config = WatchListConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.watch_list");
    const DISPLAY_NAME: &'static str = "widget.names.watch_list";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.watch_list");
    const ICON: Option<&'static str> = Some("widgets/watch_list.png");

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
        let (binding, directory) = match cx.services().market_participants.clone() {
            Some(connector) => {
                let source = connector.market();
                let service = WatchListService::new(source);
                let binding = WidgetBinding::spawn(cx, service);
                let directory: Rc<dyn WatchListDirectory> =
                    Rc::new(ConnectorDirectory::new(connector));
                (Some(binding), Some(directory))
            }
            None => (None, None),
        };
        let catalog = directory
            .as_ref()
            .map_or_else(|| Arc::new(Catalog::new()), |directory| directory.catalog());
        Ok(Self::assemble(
            config,
            binding,
            directory,
            catalog,
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    /// The options this widget persists.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The list is the content, and it is ready as soon as the service has
    /// confirmed it — including an empty list. A list with nothing in it is what
    /// "no listings followed" looks like; it is not a state the widget reports.
    ///
    /// A pane whose market is not loaded has content whenever the user follows
    /// something: those listings are the user's list, and what the body draws for them
    /// is a table that says which market participant has to be loaded. A list that
    /// follows nothing has no such row to draw, so it is left to the framework's
    /// placeholder — the sentence that names every market participant this widget can
    /// read at all, and the control that loads one.
    fn has_content(&self) -> bool {
        !self.config.entries.is_empty() || self.confirmed
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The markets this list follows have to be loaded before anything else happens
        // here, and the check is first on purpose: a route the user never loaded, or one
        // a shrinking commit withdrew, leaves the pane waiting for it instead of reaching
        // the branches below, which settle the listings the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on a market this list follows that took it away
        // leaves this set while the streams that resolved against the retired generation
        // are still alive, and the load that brings the market back has to reopen them.
        let retry = self.status.is_failed(cx.app());
        if retry {
            // A recovery keeps the rows the user is looking at, so a load that was not
            // allowed to draft a request when the commit landed — every load a failed pane
            // starts is a recovery — has to reopen their streams itself.
            self.market_reloaded = true;
        } else {
            // An explicit load starts over, because the widget cannot vouch for
            // what it was showing.
            self.entries.clear();
            self.confirmed = false;
        }
        let readable = self.readable(cx);
        for key in &self.config.entries {
            if !readable.contains(key) {
                // The market behind this listing is not in memory: it has no stream to
                // follow and no row of its own, and what the table draws for it says so.
                // The entry goes, so a message that was already in flight for it cannot
                // fill a row nothing backs any more.
                self.entries.remove(key);
                continue;
            }
            // A listing whose stream went dead on its own is opened again rather than
            // handed the stream it left behind.
            if self.dropped.contains(key) {
                self.market_reloaded = true;
            }
            self.entries
                .entry(key.clone())
                .or_insert_with(|| WatchListEntry::connecting(key));
        }
        // Every listing that was written down has been read again, whether it was readable
        // or not: one whose market is still gone is a row that waits.
        self.dropped.clear();
        self.pending.clear();
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        self.rows_dirty = true;
        self.start(cx);
    }

    /// The only control this widget puts in its header: adding a listing.
    ///
    /// It refuses input while the pane waits for its markets, like every other control
    /// here: a listing can only be added from a market that is in memory, and the
    /// sentence the pane is waiting with is what tells the user to load one.
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
        self.release_header(cx, false);
        self.build_header(parent, cx)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        let body = NodeBuilder::boxed()
            .id("WatchListBody")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_frame = Some(body);

        // Each table takes half the height when both are drawn. A table with no rows of
        // its own is not drawn, and the one case where both are empty draws the
        // continuous table so that the empty list says it is empty instead of leaving
        // the body blank.
        let continuous_rows = self.continuous_handle.rows;
        let session_rows = self.session_handle.rows;
        let continuous_frame = NodeBuilder::boxed()
            .id("WatchListContinuousFrame")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |read| {
                let continuous = read.read(&continuous_rows);
                let session = read.read(&session_rows);
                StylePatch {
                    visible: Some(continuous.len() > 0 || session.len() == 0),
                    ..StylePatch::default()
                }
            })
            .bind_a11y({
                let translate = translate.clone();
                move |read| {
                    let _language = *read.read(&language);
                    A11yPatch {
                        label: Some(t_smol!(translate, "widget.watch_list.table")),
                        ..A11yPatch::default()
                    }
                }
            })
            .build(cx.tree_mut(), body)?;
        let session_frame = NodeBuilder::boxed()
            .id("WatchListSessionFrame")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |read| {
                let session = read.read(&session_rows);
                StylePatch {
                    visible: Some(session.len() > 0),
                    ..StylePatch::default()
                }
            })
            .bind_a11y(move |read| {
                let _language = *read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.watch_list.table")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), body)?;
        self.continuous_slot = Some(continuous_frame);
        self.session_slot = Some(session_frame);

        self.continuous = Some(TablePane::build(
            cx,
            continuous_frame,
            WatchListMarket::Continuous,
            CONTINUOUS_TABLE_KEY,
            self.table_signals(),
            self.continuous_handle.clone(),
        )?);
        self.session = Some(TablePane::build(
            cx,
            session_frame,
            WatchListMarket::Session,
            SESSION_TABLE_KEY,
            self.table_signals(),
            self.session_handle.clone(),
        )?);

        self.rows_dirty = true;
        self.publish_rows(cx)?;
        self.sync_row_controls(cx)?;
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The catalogue the cells read is replaced before anything else looks at it, so a
        // generation that moved is never drawn with the names of the retired one.
        self.sync_catalog(cx);

        // The directory generation is the one signal the framework's surface root also
        // reads, so a write schedules the frame that runs this hook — including for a
        // pane that is only waiting and has no body. Nothing else may poll the directory
        // (AGENTS §8.3).
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                // A new generation replaced the one the live streams resolved their
                // listings against, so the load that follows reopens them.
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
        // no widget context (see [`RowRequest`] and the module documentation).
        let request = (*cx.read(&self.row_requests)).clone();
        if let Some(request) = request {
            cx.write(&self.row_requests, None);
            match request {
                RowRequest::Retry(key) => self.retry(&key, cx),
                RowRequest::Remove(key) => self.remove(&key, cx),
            }
        }
        let selection = (*cx.read(&self.selection_requests)).clone();
        if let Some(selection) = selection {
            cx.write(&self.selection_requests, None);
            self.add(selection, cx);
        }

        // A language switch changes the table's static copy (headers, empty sentence,
        // the sentence a waiting row states) and the header control's label, so both are
        // rebuilt. The cells themselves follow their own language signal.
        let language = *cx.read(&self.language);
        if language != self.seen_language {
            self.seen_language = language;
            self.rebuild_tables(cx)?;
            self.rows_dirty = true;
        }
        self.sync_header(cx)?;

        let rows_changed = self.rows_dirty;
        if rows_changed {
            self.publish_rows(cx)?;
            cx.request_frame()?;
        }
        // The tables recycle their mounted row window as the list scrolls or the pane
        // resizes, and a row's controls are built into the slot its row factory built.
        // No dirty flag of this widget moves for that — the rows themselves are the same
        // — so the pairing is rebuilt whenever the tables say the window they draw moved
        // (they fill the contents of newly mounted rows in `sync`, which is also when
        // the slot a control belongs in exists for the first time).
        let mut window_moved = false;
        if let Some(pane) = self.continuous.as_ref() {
            window_moved |= pane.table.sync(cx.app_mut())?;
        }
        if let Some(pane) = self.session.as_ref() {
            window_moved |= pane.table.sync(cx.app_mut())?;
        }
        if rows_changed || self.controls_dirty || self.catalog_moved || window_moved {
            self.sync_row_controls(cx)?;
            self.controls_dirty = false;
            self.catalog_moved = false;
        }
        Ok(())
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // The tables' and the header row's nodes are removed by the framework after this
        // hook (or by the pane that owns the row), so only the components are released
        // here: each controls one click handler and one signal scope.
        for controls in [
            mem::take(&mut self.continuous_controls),
            mem::take(&mut self.session_controls),
        ] {
            for (_, controls) in controls {
                for button in controls.buttons {
                    button.release(cx.app_mut());
                }
            }
        }
        self.release_header(cx, false);
        if let Some(mut pane) = self.continuous.take() {
            pane.release(cx.app_mut());
        }
        if let Some(mut pane) = self.session.take() {
            pane.release(cx.app_mut());
        }
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.body_frame = None;
        self.continuous_slot = None;
        self.session_slot = None;
        self.rows_dirty = false;
    }
}

impl WatchListWidget {
    /// Builds a widget over the collaborators a caller hands it.
    fn assemble(
        config: WatchListConfig,
        binding: Option<WidgetBinding<WatchListProtocol>>,
        directory: Option<Rc<dyn WatchListDirectory>>,
        catalog: Arc<Catalog>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let capacity = config.entries.len();
        let language = cx.services().language;
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let seen_language = *cx.read(&language);
        let catalog_revision = cx.signal(0_u64);
        let selection_requests = cx.signal(None::<InstrumentSelection>);
        let row_requests = cx.signal(None::<RowRequest>);
        let add_enabled = cx.signal(false);
        let continuous_handle = TableHandle::new(cx);
        let session_handle = TableHandle::new(cx);
        Self {
            config,
            binding,
            pending: Pending::new(),
            directory,
            status,
            recovery,
            config_change,
            translate: cx.translate().clone(),
            theme: cx.theme().clone(),
            language,
            generation,
            catalog: Rc::new(RefCell::new(catalog)),
            catalog_revision,
            selection_requests,
            row_requests,
            add_enabled,
            entries: HashMap::with_capacity(capacity),
            confirmed: false,
            market_reloaded: false,
            dropped: HashSet::with_capacity(capacity),
            awaiting_market: true,
            seen_generation,
            seen_language,
            published_catalog_revision: 0,
            catalog_moved: false,
            rows_dirty: true,
            controls_dirty: true,
            body_frame: None,
            continuous_slot: None,
            session_slot: None,
            continuous_handle,
            session_handle,
            continuous: None,
            session: None,
            continuous_structure: Vec::new(),
            session_structure: Vec::new(),
            continuous_controls: BTreeMap::new(),
            session_controls: BTreeMap::new(),
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
    fn with_source<S: TickerSource>(
        config: WatchListConfig,
        source: S,
        directory: Rc<dyn WatchListDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = WatchListService::new(source);
        let binding = WidgetBinding::spawn(cx, service);
        let catalog = directory.catalog();
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            catalog,
            status,
            config_change,
            recovery,
            cx,
        )
    }

    /// The signals and handles both tables read.
    fn table_signals(&self) -> TableSignals {
        TableSignals {
            catalog: Rc::clone(&self.catalog),
            catalog_revision: self.catalog_revision,
            language: self.language,
            translate: self.translate.clone(),
        }
    }

    /// The listings this pane can read right now: the followed ones whose market is loaded.
    fn readable(&self, _cx: &WidgetCx<'_>) -> HashSet<VenueListingKey> {
        let Some(directory) = self.directory.as_ref() else {
            return HashSet::default();
        };
        self.config
            .entries
            .iter()
            .filter(|key| directory.is_loaded(&key.route))
            .cloned()
            .collect()
    }

    /// The set the service is asked for: the listings this pane reads.
    fn served(&self, cx: &WidgetCx<'_>) -> HashSet<VenueListingKey> {
        let readable = self.readable(cx);
        self.entries
            .keys()
            .filter(|key| readable.contains(key))
            .cloned()
            .collect()
    }

    /// Whether anything this list follows is in memory.
    fn market_ready(&self, _cx: &WidgetCx<'_>) -> bool {
        let Some(directory) = self.directory.as_ref() else {
            return false;
        };
        if self
            .config
            .entries
            .iter()
            .any(|key| directory.is_loaded(&key.route))
        {
            return true;
        }
        if !self.config.entries.is_empty() {
            return false;
        }
        // A list that follows nothing is read through the markets themselves: the picker
        // that adds a listing is drawn from the loaded venues, so with one of them loaded
        // there is something to add and the empty list is the empty table's own business.
        directory.any_market_loaded()
    }

    /// Settles the followed set against what the venues still carry, and follows it.
    fn start(&mut self, cx: &mut WidgetCx<'_>) {
        // The pane reached here only past its own gate, so the markets it was waiting
        // for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        let asked = self.config.entries.clone();
        let served = self.served(cx);
        if !asked.is_empty() {
            let Some(directory) = self.directory.clone() else {
                return;
            };
            // Every lookup is started in one synchronous pass, and every answer is applied
            // in one settlement: each question was asked about the same set, so settling
            // them one by one would let a later answer judge a set an earlier one changed.
            let routes = directory.loaded_scopes();
            let answered = asked
                .iter()
                .map(|key| {
                    // A route the reading does not carry is not asked: no question is put
                    // to a catalog about a scope this pane holds no evidence for. That is
                    // *not* a withdrawal — see `listing_support`: the venue may have loaded
                    // since the reading, so the listing stays.
                    let loaded = offers_route(&routes, &key.route, CapabilityId::Tickers);
                    let offered = loaded.then(|| directory.contains(key));
                    (key.clone(), offered)
                })
                .collect::<Vec<_>>();
            let answers: Vec<ListingAnswer> = answered
                .iter()
                .map(|(key, offered)| ListingAnswer {
                    key: key.clone(),
                    support: listing_support(key, offered.clone(), directory.as_ref()),
                })
                .collect();
            self.settle_listings(&asked, &served, &answers, cx);
        }
        let served = self.served(cx);
        self.replace(&served, PendingCommand::Load, cx);
    }

    /// Drops the followed listings the routes no longer offer.
    fn settle_listings(
        &mut self,
        asked: &[VenueListingKey],
        served: &HashSet<VenueListingKey>,
        answers: &[ListingAnswer],
        cx: &mut WidgetCx<'_>,
    ) {
        if self.config.entries != asked {
            return;
        }
        if served.iter().any(|key| !self.entries.contains_key(key)) {
            return;
        }
        let mut withdrawn = HashSet::with_capacity(answers.len());
        withdrawn.extend(
            answers
                .iter()
                .filter(|answer| answer.support == ListingSupport::Withdrawn)
                .map(|answer| answer.key.clone()),
        );
        if withdrawn.is_empty() {
            return;
        }
        let before = self.config.entries.len();
        self.config.entries.retain(|key| !withdrawn.contains(key));
        log::warn!(
            "{} followed listing(s) are no longer offered; they left the list",
            before - self.config.entries.len()
        );
        for key in &withdrawn {
            self.entries.remove(key);
        }
        self.config_change.publish(cx.app_mut());
        self.rows_dirty = true;
    }

    /// Reports the state a pane with nothing to read is in.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.entries.clear();
        self.pending.clear();
        self.report_waiting(cx);
        self.rows_dirty = true;
        self.controls_dirty = true;
    }

    /// Reports the state a pane with nothing to read is in.
    ///
    /// Two panes are in it: the one that waits for the markets behind the listings it
    /// follows, whose rows say what has to be loaded, and the one that follows nothing at
    /// all, whose only way of saying it is the framework's placeholder. Which of the two
    /// this is depends on what the user follows rather than on why the pane stopped
    /// reading, so every path that leaves the pane with nothing to read reports it the
    /// same way.
    fn report_waiting(&mut self, cx: &mut WidgetCx<'_>) {
        self.confirmed = false;
        if !self.has_content() {
            self.report_markets_not_loaded(cx);
        } else {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
    }

    /// Reports the notice naming every market this widget could read at all.
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

    /// Publishes the followed set to the two tables.
    fn publish_rows(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.rows_dirty = false;
        // The body is what mounts the tables; before it exists there is nothing to
        // publish to, and the dirty flag stays set so the first body publishes the rows
        // it was built with.
        if self.continuous.is_none() && self.session.is_none() {
            self.rows_dirty = true;
            return Ok(());
        }
        let (continuous, session) = split_rows(&self.config.entries, &self.entries);
        let continuous = TableRows::new(continuous);
        let session = TableRows::new(session);
        let continuous_structure = structure_of(&continuous);
        let session_structure = structure_of(&session);
        let continuous_rebuild = continuous_structure != self.continuous_structure
            || self
                .continuous
                .as_ref()
                .is_none_or(|pane| pane.len() != continuous.len());
        let session_rebuild = session_structure != self.session_structure
            || self
                .session
                .as_ref()
                .is_none_or(|pane| pane.len() != session.len());
        self.continuous_structure = continuous_structure;
        self.session_structure = session_structure;
        if let Some(pane) = self.continuous.as_mut() {
            pane.publish(cx, continuous, continuous_rebuild)?;
        }
        if let Some(pane) = self.session.as_mut() {
            pane.publish(cx, session, session_rebuild)?;
        }
        if continuous_rebuild || session_rebuild {
            self.controls_dirty = true;
        }
        Ok(())
    }

    /// The tables' static copy moved; rebuild both in place.
    fn rebuild_tables(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let (Some(continuous_slot), Some(session_slot)) = (self.continuous_slot, self.session_slot)
        else {
            return Ok(());
        };
        let window = cx.window();
        if let Some(mut pane) = self.continuous.take() {
            if cx.tree().get(pane.node).is_ok() {
                let _ = cx.app_mut().remove_node(window, pane.node);
            }
            pane.release(cx.app_mut());
        }
        if let Some(mut pane) = self.session.take() {
            if cx.tree().get(pane.node).is_ok() {
                let _ = cx.app_mut().remove_node(window, pane.node);
            }
            pane.release(cx.app_mut());
        }
        self.continuous = Some(TablePane::build(
            cx,
            continuous_slot,
            WatchListMarket::Continuous,
            CONTINUOUS_TABLE_KEY,
            self.table_signals(),
            self.continuous_handle.clone(),
        )?);
        self.session = Some(TablePane::build(
            cx,
            session_slot,
            WatchListMarket::Session,
            SESSION_TABLE_KEY,
            self.table_signals(),
            self.session_handle.clone(),
        )?);
        self.continuous_structure.clear();
        self.session_structure.clear();
        Ok(())
    }

    /// Whether the row controls have to refuse input right now.
    fn controls_disabled(&self, app: &App) -> bool {
        !self.pending.is_empty() || self.status.is_failed(app)
    }

    /// Asks the service to follow exactly `served`, the part of the followed set this pane is
    /// reading.
    fn replace(
        &mut self,
        served: &HashSet<VenueListingKey>,
        command: PendingCommand,
        cx: &mut WidgetCx<'_>,
    ) {
        let mut entries = served.iter().cloned().collect::<Vec<_>>();
        sort_entries(&mut entries);
        let request = if self.market_reloaded {
            WatchListRequest::MarketReloaded { entries }
        } else {
            WatchListRequest::Replace { entries }
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            // The call replaced whatever was in flight; the table follows it so a late
            // answer to the replaced one is unknown and counted as dropped.
            self.pending.clear();
            self.pending.track(request_id, command);
        }
        self.controls_dirty = true;
    }

    /// Starts following a listing the user picked.
    fn add(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) {
        // The control refuses input in both of these states, and a confirmation that got
        // through anyway — the dialog was confirmed after the pane started waiting, say —
        // must not subscribe to a listing the pane has no market for.
        if self.awaiting_market || self.controls_disabled(cx.app()) {
            return;
        }
        let key = selection.listing;
        if self.config.entries.contains(&key) {
            return;
        }
        if self.config.entries.len() == MAX_WATCH_LIST_ENTRIES {
            self.fail(WatchListFailure::Capacity, cx);
            return;
        }

        self.config.entries.push(key.clone());
        sort_entries(&mut self.config.entries);
        self.config_change.publish(cx.app_mut());
        // What this pane reads is decided by the markets in memory and not by what the
        // picker offered: a venue that shrank in between leaves the listing it named on a
        // route nobody loaded. Such a listing is followed all the same — the row says
        // which market participant has to be loaded — but it has no stream.
        if self.readable(cx).contains(&key) {
            self.entries
                .insert(key.clone(), WatchListEntry::connecting(&key));
        }
        self.rows_dirty = true;
        let served = self.served(cx);
        self.replace(&served, PendingCommand::Add(key), cx);
    }

    /// Stops following one listing.
    fn remove(&mut self, key: &VenueListingKey, cx: &mut WidgetCx<'_>) {
        if self.controls_disabled(cx.app()) {
            return;
        }
        let Some(index) = self.config.entries.iter().position(|entry| entry == key) else {
            return;
        };
        let entry = self.entries.remove(key);
        let key = self.config.entries.remove(index);
        self.config_change.publish(cx.app_mut());

        if self.awaiting_market {
            // Taking the last followed listing out of a list that is waiting for a market
            // leaves a list with no row to say anything with, so the pane goes back to the
            // placeholder it was waiting with.
            self.rows_dirty = true;
            self.report_waiting(cx);
            return;
        }
        self.rows_dirty = true;
        let served = self.served(cx);
        self.replace(
            &served,
            PendingCommand::Remove(key, entry.map(Box::new)),
            cx,
        );
    }

    /// Reopens the stream of one listing the user is retrying.
    fn retry(&mut self, key: &VenueListingKey, cx: &mut WidgetCx<'_>) {
        if self.controls_disabled(cx.app()) {
            return;
        }
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        if entry.status() != WatchListStatus::Error {
            return;
        }
        let Some(error) = entry.error().cloned() else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(key) {
            entry.begin_retry();
        }
        self.rows_dirty = true;
        self.controls_dirty = true;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(cx, WatchListRequest::Retry { key: key.clone() });
        self.pending.clear();
        self.pending.track(
            request_id,
            PendingCommand::Retry {
                key: key.clone(),
                error,
            },
        );
    }

    /// Reports a failure of the widget itself, with the framework's recovery.
    fn fail(&mut self, failure: WatchListFailure, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.pending.clear();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
        self.controls_dirty = true;
    }

    /// Applies the service's answer to the one operation that was in flight.
    fn apply_rpc_result(
        &mut self,
        command: PendingCommand,
        result: WidgetRpcResult<WatchListProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match result {
            Ok(WatchListResponse::Applied) => {
                // A failure is terminal (§4.4): the answer to a request that was already in
                // flight when the list failed is not a reason to report it as working again.
                if self.status.is_failed(cx.app()) {
                    return;
                }
                // The request that was answered is the one the reload flag was waiting
                // for: the streams it reopened were resolved at the retired generation.
                self.market_reloaded = false;
                self.controls_dirty = true;
                if self.awaiting_market {
                    return;
                }
                self.confirmed = true;
                self.status.report(cx.app_mut(), WidgetState::Ready);
            }
            Err(error) => {
                if self.status.is_failed(cx.app()) {
                    return;
                }
                command.undo(&mut self.config, &mut self.entries);
                // The followed set is the config, so putting a refused change back is a
                // config change of its own.
                self.config_change.publish(cx.app_mut());
                self.rows_dirty = true;
                self.fail(WatchListFailure::Rpc(error), cx);
            }
        }
    }

    /// Applies one message from one listing's stream.
    fn apply_event(
        &mut self,
        key: &VenueListingKey,
        event: &WatchListStreamEvent,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        if let WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(error)) = event
            && market_is_catalog_not_loaded(error)
        {
            // The listing stops being read: the route cannot answer for it, so it has no
            // row of its own values any more. Nothing is asked of the service here — the
            // key is written down instead, because the stream it was opened for is parked
            // on this error and the service still holds it.
            self.entries.remove(key);
            self.dropped.insert(key.clone());
            self.rows_dirty = true;
            if !self.market_ready(cx) {
                self.wait_for_market(cx);
            }
            return;
        }
        let changed = self
            .entries
            .get_mut(key)
            .is_some_and(|entry| entry.apply_event(event));
        if changed {
            self.rows_dirty = true;
        }
    }

    fn apply(&mut self, inbound: Inbound<WatchListProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_event(&key, &message, cx),
            Inbound::Latest { key, message } => self.apply_event(&key, &message, cx),
            Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
        }
    }

    /// Applies one RPC reply, once its identity says it answers the command in flight.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<WatchListProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A late or unknown identity answers a request this pane replaced, and the
        // table has counted it.
        if let Some(command) = self.pending.take(request_id) {
            self.apply_rpc_result(command, result, cx);
        }
    }

    /// Keeps the catalogue mirror the cells read in step with the directory.
    fn sync_catalog(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        let catalog = directory.catalog();
        if Arc::ptr_eq(&self.catalog.borrow(), &catalog) {
            return;
        }
        *self.catalog.borrow_mut() = catalog;
        self.published_catalog_revision = self.published_catalog_revision.wrapping_add(1);
        let revision = self.published_catalog_revision;
        cx.write(&self.catalog_revision, revision);
        self.catalog_moved = true;
    }

    /// The filters the picker this control opens is built with.
    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Tickers,
            entry_predicate: None,
            routes: self.directory.as_ref().map_or_else(
                || Arc::from(Vec::<LoadedScope>::new()),
                |directory| directory.loaded_scopes(),
            ),
        }
    }

    /// Whether the add control accepts input.
    fn add_enabled(&self, app: &App) -> bool {
        !self.awaiting_market && !self.controls_disabled(app)
    }

    /// Builds the add control into the header group's container.
    fn build_header(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let filters = self.instrument_filters();
        let enabled = self.add_enabled(cx.app());
        let key = cx.unique_key(HEADER_KEY_SUFFIX);
        let label = t_smol!(self.translate, "widget.watch_list.add");
        let requests = self.selection_requests;
        let enabled_signal = self.add_enabled;
        let host = SelectorHost::new(Rc::clone(cx.services_rc()))
            .with_catalog(self.directory.as_ref().map(|directory| directory.catalog()));
        let confirm: Rc<dyn Fn(InstrumentSelection, &mut App, WindowId)> =
            Rc::new(move |selection, app, window| {
                app.write(&requests, Some(selection));
                let _ = app.request_repaint(window);
            });
        let button = Button::new(cx.app_mut(), self.theme.clone(), key)
            .primary()
            .with_unified_size(UnifiedSize::Small)
            .icon(IconKind::Plus)
            .a11y_label(label)
            .disabled(!enabled)
            .on_click(move |ctx| {
                // A disabled button still dispatches its handler in this kit: the guard is
                // the real refusal, and the dialog of a pane with no market must not open.
                if !*ctx.read(&enabled_signal) {
                    return;
                }
                let window = ctx.window();
                let app = ctx.app_mut();
                InstrumentSelector::show_with_filters(
                    None,
                    filters.clone(),
                    Rc::clone(&confirm),
                    &host,
                    app,
                    window,
                );
            });
        let node = {
            let window = cx.window();
            button.build(cx.app_mut(), window, parent)?
        };
        self.header = Some(HeaderControl {
            button,
            node,
            parent,
            built: HeaderCopy {
                language: *cx.read(&self.language),
                generation: self.seen_generation,
                has_directory: self.directory.is_some(),
            },
        });
        Ok(())
    }

    /// Keeps the header control in step with the copy it was built with.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let enabled = self.add_enabled(cx.app());
        cx.write(&self.add_enabled, enabled);
        let Some(header) = self.header.as_ref() else {
            return Ok(());
        };
        let language = *cx.read(&self.language);
        let moved = header.built.language != language
            || header.built.generation != self.seen_generation
            || header.built.has_directory != self.directory.is_some();
        let parent = header.parent;
        if moved || cx.tree().get(parent).is_err() {
            self.release_header(cx, moved && cx.tree().get(parent).is_ok());
            if cx.tree().get(parent).is_ok() {
                return self.build_header(parent, cx);
            }
            return Ok(());
        }
        if let Some(header) = self.header.as_ref() {
            let _ = header.button.set_disabled(cx.app_mut(), !enabled);
        }
        Ok(())
    }

    /// Takes down the header control's component.
    fn release_header(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header.take() else {
            return;
        };
        if remove_node && cx.tree().get(header.node).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, header.node);
        }
        header.button.release(cx.app_mut());
    }

    /// Pairs every mounted row with its controls.
    ///
    /// A row's slot is built by the table's row factory and filled here: the factory has
    /// no application context, so the kit buttons are the widget's own to build, release
    /// and rebuild. The pairing is keyed by row index and the built copy, so a row whose
    /// text moved keeps its controls and a row window the table rebuilt gets fresh ones.
    fn sync_row_controls(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let context = ControlsContext {
            language: *cx.read(&self.language),
            disabled: self.controls_disabled(cx.app()),
            catalog: Arc::clone(&self.catalog.borrow()),
            translate: self.translate.clone(),
            theme: self.theme.clone(),
            requests: self.row_requests,
            window: cx.window(),
        };
        sync_controls_for(
            WatchListMarket::Continuous,
            &mut self.continuous,
            &mut self.continuous_controls,
            &context,
            cx,
        )?;
        sync_controls_for(
            WatchListMarket::Session,
            &mut self.session,
            &mut self.session_controls,
            &context,
            cx,
        )?;
        Ok(())
    }
}

/// What one row-control build reads.
struct ControlsContext {
    language: LanguageCode,
    disabled: bool,
    catalog: Arc<Catalog>,
    translate: Translate,
    theme: ThemeHandle,
    requests: Signal<Option<RowRequest>>,
    window: WindowId,
}

/// Pairs one table's mounted rows with their controls.
fn sync_controls_for(
    market: WatchListMarket,
    pane: &mut Option<TablePane>,
    controls: &mut BTreeMap<usize, RowControls>,
    context: &ControlsContext,
    cx: &mut WidgetCx<'_>,
) -> Result<(), EngineError> {
    let Some(pane) = pane.as_mut() else {
        controls.clear();
        return Ok(());
    };
    let rows = pane.rows.clone();
    // The mounted rows are the slots whose nodes are still in the tree: the virtual list
    // recycles its row window, and the slot map keeps the rows it built until they are
    // pruned here.
    let mut mounted: Vec<(usize, NodeId)> = Vec::new();
    let mut gone: Vec<usize> = Vec::new();
    for (&row, &node) in pane.slots.borrow().iter() {
        if cx.tree().get(node).is_ok() {
            mounted.push((row, node));
        } else {
            gone.push(row);
        }
    }
    for row in gone {
        pane.slots.borrow_mut().remove(&row);
    }
    mounted.sort_unstable_by_key(|(row, _)| *row);

    let mounted_rows: Vec<usize> = mounted.iter().map(|(row, _)| *row).collect();
    let stale: Vec<usize> = controls
        .keys()
        .copied()
        .filter(|row| !mounted_rows.contains(row))
        .collect();
    for row in stale {
        if let Some(entry) = controls.remove(&row) {
            release_controls(cx, entry, context.window)?;
        }
    }

    for (row, slot) in mounted {
        let Some(current) = rows.row(row) else {
            continue;
        };
        let copy = RowCopy::of(
            current,
            context.language,
            context.disabled,
            &context.catalog,
            &context.translate,
        );
        let keep = controls
            .get(&row)
            .is_some_and(|entry| copy.matches(entry, slot));
        if keep {
            continue;
        }
        if let Some(entry) = controls.remove(&row) {
            release_controls(cx, entry, context.window)?;
        }
        let mut buttons = Vec::with_capacity(2);
        if copy.retry {
            let key = cx.unique_key(&format_smolstr!(
                "watch_list.{}.row.{row}.retry",
                table_name(market)
            ));
            let label = t_smol!(context.translate, "common.retry");
            let request = RowRequest::Retry((*copy.listing).clone());
            let requests = context.requests;
            let button = Button::new(cx.app_mut(), context.theme.clone(), key)
                .ghost()
                .with_unified_size(UnifiedSize::Small)
                .icon(IconKind::ArrowsClockwise)
                .a11y_label(label)
                .disabled(context.disabled)
                .on_click(move |ctx| {
                    ctx.write(&requests, Some(request.clone()));
                    let window = ctx.window();
                    let _ = ctx.app_mut().request_repaint(window);
                });
            button.build(cx.app_mut(), context.window, slot)?;
            buttons.push(button);
        }
        let key = cx.unique_key(&format_smolstr!(
            "watch_list.{}.row.{row}.remove",
            table_name(market)
        ));
        let request = RowRequest::Remove((*copy.listing).clone());
        let requests = context.requests;
        let button = Button::new(cx.app_mut(), context.theme.clone(), key)
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .icon(IconKind::X)
            .a11y_label(copy.remove_label.clone())
            .disabled(context.disabled)
            .on_click(move |ctx| {
                ctx.write(&requests, Some(request.clone()));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), context.window, slot)?;
        buttons.push(button);
        controls.insert(
            row,
            RowControls {
                slot,
                listing: Arc::clone(&copy.listing),
                language: copy.language,
                disabled: copy.disabled,
                retry: copy.retry,
                remove_label: copy.remove_label,
                buttons,
            },
        );
    }
    Ok(())
}

/// Releases one row's controls; their nodes go with the row window that held them.
fn release_controls(
    cx: &mut WidgetCx<'_>,
    controls: RowControls,
    window: WindowId,
) -> Result<(), EngineError> {
    for button in controls.buttons {
        if let Some(node) = button.node()
            && cx.tree().get(node).is_ok()
        {
            cx.app_mut().remove_node(window, node)?;
        }
        button.release(cx.app_mut());
    }
    Ok(())
}

/// The local name of a table, for the row controls' kit keys.
fn table_name(market: WatchListMarket) -> &'static str {
    match market {
        WatchListMarket::Continuous => "continuous",
        WatchListMarket::Session => "session",
    }
}

/// What the row controls of one row depend on.
fn remove_label(row: &WatchListRow, catalog: &Catalog, translate: &Translate) -> SmolStr {
    // The listing is named beside the verb rather than through the translation: the
    // catalogue's own remove message has no placeholder for it, and both languages read
    // verb-first, so the two are joined here — name after verb, the way a reader says it.
    format_smolstr!(
        "{} {}",
        t_smol!(translate, "widget.watch_list.remove"),
        listing_name(row, catalog, translate)
    )
}

/// The published name of one listing, or the localized unknown.
fn listing_name(row: &WatchListRow, catalog: &Catalog, translate: &Translate) -> SmolStr {
    widget::error::ListingErrorPresentation::of(catalog.cached_listing_instrument(row.key()))
        .translated_message(translate)
}

/// The structural identity of a table's rows: what has to be rebuilt when it moves.
fn structure_of(rows: &TableRows) -> Vec<(VenueListingKey, bool)> {
    rows.as_slice()
        .iter()
        .map(|row| {
            (
                row.key().as_ref().clone(),
                matches!(row.state(), WatchListRowState::AwaitingMarket),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
