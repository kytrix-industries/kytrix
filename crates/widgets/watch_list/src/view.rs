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

const CONTINUOUS_TABLE_KEY: &str = "watch_list.table.continuous";
const SESSION_TABLE_KEY: &str = "watch_list.table.session";

const HEADER_KEY_SUFFIX: &str = "watch_list.header.add";

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

enum PendingCommand {
    Load,
    Add(VenueListingKey),
    Remove(VenueListingKey, Option<Box<WatchListEntry>>),
    Retry {
        key: VenueListingKey,
        error: WatchListStreamError,
    },
}

impl PendingCommand {
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

#[derive(Debug)]
enum WatchListFailure {
    Rpc(RpcCallError<WatchListServiceError>),
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

struct ListingAnswer {
    key: VenueListingKey,
    support: ListingSupport,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowRequest {
    Retry(VenueListingKey),
    Remove(VenueListingKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderCopy {
    language: LanguageCode,
    generation: u64,
    has_directory: bool,
}

struct HeaderControl {
    button: Button,
    node: NodeId,
    parent: NodeId,
    built: HeaderCopy,
}

struct RowControls {
    slot: NodeId,
    listing: Arc<VenueListingKey>,
    language: LanguageCode,
    disabled: bool,
    retry: bool,
    remove_label: SmolStr,
    buttons: Vec<Button>,
}

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
    binding: Option<WidgetBinding<WatchListProtocol>>,
    pending: Pending<PendingCommand>,
    directory: Option<Rc<dyn WatchListDirectory>>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    translate: Translate,
    theme: ThemeHandle,
    language: Signal<LanguageCode>,
    generation: Signal<u64>,
    catalog: Rc<RefCell<Arc<Catalog>>>,
    catalog_revision: Signal<u64>,
    selection_requests: Signal<Option<InstrumentSelection>>,
    row_requests: Signal<Option<RowRequest>>,
    add_enabled: Signal<bool>,
    entries: HashMap<VenueListingKey, WatchListEntry>,
    confirmed: bool,
    market_reloaded: bool,
    dropped: HashSet<VenueListingKey>,
    awaiting_market: bool,
    seen_generation: u64,
    seen_language: LanguageCode,
    published_catalog_revision: u64,
    catalog_moved: bool,
    rows_dirty: bool,
    controls_dirty: bool,
    body_frame: Option<NodeId>,
    continuous_slot: Option<NodeId>,
    session_slot: Option<NodeId>,
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

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn has_content(&self) -> bool {
        !self.config.entries.is_empty() || self.confirmed
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        let retry = self.status.is_failed(cx.app());
        if retry {
            self.market_reloaded = true;
        } else {
            self.entries.clear();
            self.confirmed = false;
        }
        let readable = self.readable(cx);
        for key in &self.config.entries {
            if !readable.contains(key) {
                self.entries.remove(key);
                continue;
            }
            if self.dropped.contains(key) {
                self.market_reloaded = true;
            }
            self.entries
                .entry(key.clone())
                .or_insert_with(|| WatchListEntry::connecting(key));
        }
        self.dropped.clear();
        self.pending.clear();
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        self.rows_dirty = true;
        self.start(cx);
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
        self.sync_catalog(cx);

        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

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

    fn table_signals(&self) -> TableSignals {
        TableSignals {
            catalog: Rc::clone(&self.catalog),
            catalog_revision: self.catalog_revision,
            language: self.language,
            translate: self.translate.clone(),
        }
    }

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

    fn served(&self, cx: &WidgetCx<'_>) -> HashSet<VenueListingKey> {
        let readable = self.readable(cx);
        self.entries
            .keys()
            .filter(|key| readable.contains(key))
            .cloned()
            .collect()
    }

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
        directory.any_market_loaded()
    }

    fn start(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = false;
        let asked = self.config.entries.clone();
        let served = self.served(cx);
        if !asked.is_empty() {
            let Some(directory) = self.directory.clone() else {
                return;
            };
            let routes = directory.loaded_scopes();
            let answered = asked
                .iter()
                .map(|key| {
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

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.entries.clear();
        self.pending.clear();
        self.report_waiting(cx);
        self.rows_dirty = true;
        self.controls_dirty = true;
    }

    fn report_waiting(&mut self, cx: &mut WidgetCx<'_>) {
        self.confirmed = false;
        if !self.has_content() {
            self.report_markets_not_loaded(cx);
        } else {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
    }

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

    fn publish_rows(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.rows_dirty = false;
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

    fn controls_disabled(&self, app: &App) -> bool {
        !self.pending.is_empty() || self.status.is_failed(app)
    }

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
            self.pending.clear();
            self.pending.track(request_id, command);
        }
        self.controls_dirty = true;
    }

    fn add(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) {
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
        if self.readable(cx).contains(&key) {
            self.entries
                .insert(key.clone(), WatchListEntry::connecting(&key));
        }
        self.rows_dirty = true;
        let served = self.served(cx);
        self.replace(&served, PendingCommand::Add(key), cx);
    }

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

    fn fail(&mut self, failure: WatchListFailure, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.pending.clear();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
        self.controls_dirty = true;
    }

    fn apply_rpc_result(
        &mut self,
        command: PendingCommand,
        result: WidgetRpcResult<WatchListProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match result {
            Ok(WatchListResponse::Applied) => {
                if self.status.is_failed(cx.app()) {
                    return;
                }
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
                self.config_change.publish(cx.app_mut());
                self.rows_dirty = true;
                self.fail(WatchListFailure::Rpc(error), cx);
            }
        }
    }

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

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<WatchListProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if let Some(command) = self.pending.take(request_id) {
            self.apply_rpc_result(command, result, cx);
        }
    }

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

    fn add_enabled(&self, app: &App) -> bool {
        !self.awaiting_market && !self.controls_disabled(app)
    }

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

struct ControlsContext {
    language: LanguageCode,
    disabled: bool,
    catalog: Arc<Catalog>,
    translate: Translate,
    theme: ThemeHandle,
    requests: Signal<Option<RowRequest>>,
    window: WindowId,
}

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

fn table_name(market: WatchListMarket) -> &'static str {
    match market {
        WatchListMarket::Continuous => "continuous",
        WatchListMarket::Session => "session",
    }
}

fn remove_label(row: &WatchListRow, catalog: &Catalog, translate: &Translate) -> SmolStr {
    format_smolstr!(
        "{} {}",
        t_smol!(translate, "widget.watch_list.remove"),
        listing_name(row, catalog, translate)
    )
}

fn listing_name(row: &WatchListRow, catalog: &Catalog, translate: &Translate) -> SmolStr {
    widget::error::ListingErrorPresentation::of(catalog.cached_listing_instrument(row.key()))
        .translated_message(translate)
}

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
