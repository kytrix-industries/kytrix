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
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::market_is_catalog_not_loaded,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
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

const HISTORY_LIMIT: u16 = 200;

const HEADER_KEY_SUFFIX: &str = "market_trade.header.instrument";

const TABLE_KEY_SUFFIX: &str = "market_trade.table";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MarketTradeConfig {
    selection: Option<InstrumentSelection>,
}

impl WidgetConfig for MarketTradeConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
}

pub struct MarketTradeWidget {
    config: MarketTradeConfig,
    binding: Option<WidgetBinding<MarketTradeProtocol>>,
    pending: Pending<PendingCommand>,
    table: Option<Table>,
    table_slot: Option<NodeId>,
    table_built: Option<TableCopy>,
    table_built_snapshot: Option<MarketTradeSnapshot>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    directory: Option<Rc<dyn MarketDirectory>>,
    rows: Signal<MarketTradeSnapshot>,
    config_signal: Signal<Config>,
    language: Signal<LanguageCode>,
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    snapshot: Option<MarketTradeSnapshot>,
    pending_snapshot: Option<MarketTradeSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    awaiting_market: bool,
    market_reloaded: bool,
    seen_generation: u64,
    rows_dirty: bool,
    selection_requests: Signal<Option<InstrumentSelection>>,
    header: Option<HeaderControl>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableCopy {
    language: LanguageCode,
    headers: [SmolStr; COLUMN_COUNT],
    empty: SmolStr,
}

struct HeaderControl {
    control: InstrumentButtonHandle,
    parent: NodeId,
    built: HeaderState,
}

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

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        if !self.market_ready(directory.as_ref()) {
            self.wait_for_market(cx);
            return;
        }
        self.awaiting_market = false;
        let market_reloaded = mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry {
            self.clear_trades();
        }
        self.start(retry || market_reloaded, cx);
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
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

        let requested = (*cx.read(&self.selection_requests)).clone();
        if let Some(selection) = requested {
            cx.write(&self.selection_requests, None);
            self.select_selection(selection, cx);
        }

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
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.release_header(cx, false);
        if let Some(table) = self.table.take() {
            let _ = table.release(cx.app_mut());
        }
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.table_slot = None;
        self.table_built = None;
        self.table_built_snapshot = None;
    }
}

impl MarketTradeWidget {
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
            pending: Pending::new(),
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

    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        if !directory.any_market_loaded() {
            return false;
        }

        match self.config.selection.as_ref() {
            Some(selection) => directory.is_loaded(&selection.listing.route),
            None => true,
        }
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_trades();
        self.report_markets_not_loaded(cx);
    }

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

    fn report_nothing_chosen(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message(
                "widget.market_trade.select_instrument_prompt",
            )),
        );
    }

    fn report_waiting_for_directory(&mut self, error: &MarketError, cx: &mut WidgetCx<'_>) -> bool {
        if !market_is_catalog_not_loaded(error) {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    fn current_key(&self) -> Option<MarketTradeStreamKey> {
        self.config
            .selection
            .as_ref()
            .map(|selection| MarketTradeStreamKey::new(selection.listing.clone()))
    }

    fn withdraw(&mut self, listing: &VenueListingKey, cause: &str, cx: &mut WidgetCx<'_>) {
        log::warn!(
            "`{}` {cause}; the tape went back to having nothing chosen",
            listing.listing_id
        );
        self.config.selection = None;
        self.config_change.publish(cx.app_mut());
        self.nothing_chosen(cx);
    }

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

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
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
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let routes = directory.loaded_scopes();
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

    fn load_trades(&mut self, selection: InstrumentSelection, retry: bool, cx: &mut WidgetCx<'_>) {
        let listing = selection.listing;
        let request = if retry {
            MarketTradeRequest::Retry { listing }
        } else {
            MarketTradeRequest::Load { listing }
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            self.pending.track(request_id, PendingCommand::Start);
        }
    }

    fn nothing_chosen(&mut self, cx: &mut WidgetCx<'_>) {
        self.clear_trades();
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        self.report_nothing_chosen(cx);
    }

    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        self.clear_trades();
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
            Inbound::Rpc { request_id, result } => {
                self.apply_rpc(request_id, result, cx);
            }
        }
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<MarketTradeProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_rpc_result(result, cx),
            None => {}
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

    fn table_copy(&self, cx: &WidgetCx<'_>) -> TableCopy {
        let units = self.units();
        TableCopy {
            language: *cx.read(&self.language),
            headers: headers(units.as_ref(), &self.translate),
            empty: t_smol!(self.translate, "common.no_data"),
        }
    }

    fn units(&self) -> Option<MarketTradeUnits> {
        let directory = self.directory.as_ref()?;
        let snapshot = self.snapshot.as_ref()?;
        let catalog = directory.catalog();
        units_of(snapshot, catalog.as_ref(), &self.translate)
    }

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

    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        match &self.directory {
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
            self.release_header(cx, false);
            return Ok(());
        }
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

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
