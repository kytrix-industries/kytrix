use std::{mem, rc::Rc, sync::Arc};

use app_config::{
    settings::Config,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
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
use ui_kit::{
    kit::{
        list::SelectionMode,
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
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

const COLUMN_COUNT: usize = 5;

const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.15, 0.35, 0.175, 0.175, 0.15];

const COLUMN_ALIGNS: [ColumnAlign; COLUMN_COUNT] = [
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceOrderConfig;

impl WidgetConfig for ForceOrderConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

trait MarketDirectory {
    fn liquidation_routes(&self) -> Vec<VenueRoute>;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn catalog(&self) -> Arc<Catalog>;
}

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

fn routes_changed(held: &[VenueRoute], needed: &[VenueRoute]) -> bool {
    held.len() != needed.len() || !needed.iter().all(|route| held.contains(route))
}

fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::ForceOrder)
}

fn market_venue_name_keys() -> Vec<&'static str> {
    market_venues().iter().map(VenueDisplay::name_key).collect()
}

fn displayed_row(snapshot: &ForceOrderSnapshot, row: usize) -> Option<&ForceOrderRow> {
    let index = snapshot.len().checked_sub(row.checked_add(1)?)?;
    snapshot.orders().get(index).map(|row| &**row)
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
}

pub struct ForceOrderWidget {
    binding: Option<WidgetBinding<ForceOrderProtocol>>,
    pending: Pending<PendingCommand>,
    table: Option<Table>,
    table_slot: Option<NodeId>,
    status: WidgetStatus,
    recovery: WidgetAction,
    directory: Option<Rc<dyn MarketDirectory>>,
    rows: Signal<ForceOrderSnapshot>,
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    snapshot: Option<ForceOrderSnapshot>,
    pending_snapshot: Option<ForceOrderSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    routes: Arc<[VenueRoute]>,
    market_reloaded: bool,
    awaiting_market: bool,
    seen_generation: u64,
    seen_language: LanguageCode,
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

    fn has_content(&self) -> bool {
        !self.routes.is_empty() && self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let routes = directory.liquidation_routes();
        if routes.is_empty() || !routes.iter().all(|route| directory.is_loaded(route)) {
            self.wait_for_market(cx);
            return;
        }
        self.awaiting_market = false;
        let market_reloaded = mem::take(&mut self.market_reloaded);
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

        let language = *cx.read(&self.language);
        if language != self.seen_language {
            self.seen_language = language;
            self.rebuild_table(cx)?;
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

        Ok(())
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(table) = self.table.take() {
            table.release(cx.app_mut());
        }
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.table_slot = None;
    }
}

fn table_options(row_height: f32) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

impl ForceOrderWidget {
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

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.routes = Arc::from([]);
        self.awaiting_market = true;
        self.clear_orders();
        self.report_markets_not_loaded(cx);
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

    fn report_waiting_for_directory(&mut self, error: &MarketError, cx: &mut WidgetCx<'_>) -> bool {
        if !is_catalog_not_loaded(error) {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    fn apply(&mut self, inbound: Inbound<ForceOrderProtocol>, cx: &mut WidgetCx<'_>) {
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
        result: WidgetRpcResult<ForceOrderProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_rpc_result(result, cx),
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
    use ui_kit::{
        kit::{layer::Layer, list::RowHeight, overlay::AnimationRouter, table::ColumnWidth},
        theme::appearance::Appearance,
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

    fn route() -> VenueRoute {
        test_support::route("binance", PERPETUAL_SCOPE)
    }

    fn okx_route() -> VenueRoute {
        test_support::route("okx", "swap")
    }

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

    fn short_entry(timestamp: i64) -> ForceOrderEntry {
        let mut entry = entry(timestamp);
        entry.order.position_side = PositionSide::Short;
        entry
    }

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

    struct Fixture {
        harness: WidgetTestApp,
        widget: ForceOrderWidget,
        directory: FixtureDirectory,
        recovery: WidgetAction,
        retries: Rc<Cell<usize>>,
    }

    impl Fixture {
        fn new<S: ForceOrderSource>(source: S, loaded: bool) -> Self {
            Self::with_catalog(move |_| source, loaded, Vec::new())
        }

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

    #[test]
    fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content() {
        let fixture = Fixture::new(PendingForceOrderSource, true);

        assert!(!fixture.has_content());
        assert_eq!(fixture.status(), WidgetState::Connecting);
    }

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
