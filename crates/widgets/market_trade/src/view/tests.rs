use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
};

use app_config::{time_zone::DataTimeZone, widget::VersionedWidgetConfig};
use i18n::{language::codes::LanguageCode, translate::Translate};
use instrument_selector_ui::instrument_selector::offers_route;
use market_participants_connector::{
    catalog::{
        batch::CatalogEntry,
        error::{CatalogError, SourceError},
        store::Catalog,
        test_support,
    },
    market::{domain::DisconnectReason, error::MarketError},
};
use market_participants_model::{
    decimal::{Price, Quantity, UnixMillis},
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::PriceBasis,
    },
    market::{
        book::ListingTrade,
        price::MarketPrice,
        units::{MarketQuantity, MarketTradeId, QuantityUnit, Side},
    },
    routing::CapabilityId,
};
use smol_str::format_smolstr;
use ui_core::{
    event::{Action, ActionSource},
    node::tree::NodeKind,
    paint::color::Color,
    test_support::diagnostics::signal_slot_stats,
};
use ui_kit::theme::{appearance::Appearance, role::Role};
use widget::{
    cx::WidgetCx, error::TranslatedError, protocol::RequestId, rpc::RpcCallError,
    test_support::WidgetTestApp, view::WidgetView,
};

use super::*;
use crate::{
    model::MarketTradeState,
    service::{MarketTradeEventSubscription, MarketTradeSource},
};

fn route() -> VenueRoute {
    test_support::spot_route()
}

fn okx_route() -> VenueRoute {
    test_support::route("okx", "spot")
}

fn listing() -> VenueListingKey {
    test_support::listing_key(&route(), "BTCUSDT")
}

fn other_listing() -> VenueListingKey {
    test_support::listing_key(&route(), "ETHUSDT")
}

fn selection(listing: VenueListingKey) -> InstrumentSelection {
    InstrumentSelection { listing }
}

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn market_price(value: u64) -> MarketPrice {
    MarketPrice {
        value: Price::from(value),
        quote_asset: test_support::token("binance", "USDT"),
        basis: PriceBasis::PerAssetUnit,
    }
}

fn traded_quantity(value: u64) -> MarketQuantity {
    MarketQuantity {
        value: Quantity::from(value),
        unit: QuantityUnit::AssetUnits {
            asset: test_support::token("binance", "BTC"),
        },
    }
}

fn trade(id: u64, timestamp: UnixMillis, value: u64) -> ListingTrade {
    ListingTrade {
        listing: listing(),
        id: MarketTradeId::Numeric(id),
        price: market_price(value),
        quantity: traded_quantity(2),
        side: Side::Buy,
        timestamp,
    }
}

fn precision() -> ListingPrecision {
    ListingPrecision {
        price_tick: "0.01".parse().expect("a price tick should parse"),
        quantity_tick: "0.001".parse().expect("a quantity step should parse"),
    }
}

fn default_entry() -> CatalogEntry {
    test_support::entry(
        &route(),
        test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
        "BTCUSDT",
    )
}

fn eth_entry() -> CatalogEntry {
    test_support::entry(
        &route(),
        test_support::spot_token(test_support::VENUE, "ETH", "USDT"),
        "ETHUSDT",
    )
}

fn listing_entry(price_tick: &str) -> CatalogEntry {
    let mut restated = default_entry();
    restated.snapshot.rules.common.price.tick = price_tick
        .parse()
        .expect("the restated price tick should parse");
    restated
}

fn delisted_entry() -> CatalogEntry {
    test_support::with_status(default_entry(), ListingStatus::Delisted)
}

fn disconnect() -> MarketError {
    MarketError::Disconnected(DisconnectReason::Normal)
}

fn no_answer() -> CatalogError {
    CatalogError::Source(Box::new(SourceError::Network {
        reason: SmolStr::new_static("the test venue did not answer"),
    }))
}

fn snapshot_of_listing(
    listing: VenueListingKey,
    trades: &[(u64, UnixMillis, u64)],
) -> MarketTradeSnapshot {
    let mut state = MarketTradeState::new(trades.len().max(1), precision());
    state.merge(trades.iter().map(|&(id, timestamp, price)| ListingTrade {
        listing: listing.clone(),
        id: MarketTradeId::Numeric(id),
        price: market_price(price),
        quantity: traded_quantity(2),
        side: Side::Buy,
        timestamp,
    }));
    state.snapshot()
}

fn snapshot(timestamp: UnixMillis) -> MarketTradeSnapshot {
    snapshot_of_listing(listing(), &[(1, timestamp, 100)])
}

fn empty_snapshot() -> MarketTradeSnapshot {
    MarketTradeSnapshot::default()
}

fn both_sides_snapshot() -> MarketTradeSnapshot {
    let mut state = MarketTradeState::new(2, precision());
    state.merge([trade(1, 1, 100)]);
    let mut newer = trade(2, 2, 101);
    newer.side = Side::Sell;
    state.merge([newer]);
    state.snapshot()
}

fn contract_snapshot() -> MarketTradeSnapshot {
    let mut state = MarketTradeState::new(2, precision());
    let mut newer = trade(2, 2, 101);
    newer.quantity = MarketQuantity {
        value: Quantity::from(3_u64),
        unit: QuantityUnit::Contracts,
    };
    state.merge([trade(1, 1, 100), newer]);
    state.snapshot()
}

fn resolutions_of(resolutions: &Arc<Mutex<Vec<ListingPrecision>>>) -> Vec<ListingPrecision> {
    resolutions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[derive(Clone, Copy)]
struct PendingSource;

struct PendingSubscription;

impl MarketTradeSource for PendingSource {
    type Subscription = PendingSubscription;

    fn fetch_market_trade(
        &self,
        _listing: VenueListingKey,
        _limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
        std::future::pending()
    }

    fn subscribe_market_trade(
        &self,
        _listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        std::future::pending()
    }
}

impl MarketTradeEventSubscription for PendingSubscription {
    fn precision(&self) -> ListingPrecision {
        precision()
    }

    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<ListingTrade>, MarketError>>> + Send {
        std::future::pending()
    }
}

#[derive(Clone, Default)]
struct RecordingSource {
    subscribed: Arc<Mutex<Vec<VenueListingKey>>>,
}

impl RecordingSource {
    fn subscribed(&self) -> Vec<VenueListingKey> {
        self.subscribed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl MarketTradeSource for RecordingSource {
    type Subscription = PendingSubscription;

    fn fetch_market_trade(
        &self,
        _listing: VenueListingKey,
        _limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
        std::future::pending()
    }

    fn subscribe_market_trade(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        self.subscribed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(listing);
        std::future::pending()
    }
}

#[derive(Clone)]
struct ResolvingSource {
    catalog: Arc<Catalog>,
    resolutions: Arc<Mutex<Vec<ListingPrecision>>>,
}

impl MarketTradeSource for ResolvingSource {
    type Subscription = PendingSubscription;

    fn fetch_market_trade(
        &self,
        _listing: VenueListingKey,
        _limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
        std::future::pending()
    }

    fn subscribe_market_trade(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let published = self
            .catalog
            .cached_listing(&listing)
            .expect("the fixture publishes the listing the tape follows");
        self.resolutions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ListingPrecision::of(&published.snapshot.rules));
        std::future::pending()
    }
}

#[derive(Clone)]
struct FixtureDirectory {
    loaded: Rc<RefCell<Vec<VenueRoute>>>,
    catalog: Arc<Catalog>,
}

impl FixtureDirectory {
    fn new(catalog: Arc<Catalog>, loaded: bool) -> Self {
        let loaded = if loaded { vec![route()] } else { Vec::new() };
        Self {
            loaded: Rc::new(RefCell::new(loaded)),
            catalog,
        }
    }

    fn load(&self, route: VenueRoute) {
        let mut loaded = self.loaded.borrow_mut();
        if !loaded.contains(&route) {
            loaded.push(route);
        }
    }

    fn unload(&self, route: &VenueRoute) {
        self.loaded.borrow_mut().retain(|held| held != route);
    }
}

impl MarketDirectory for FixtureDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        self.loaded
            .borrow()
            .iter()
            .map(|route| LoadedScope {
                route: route.clone(),
                capabilities: vec![CapabilityId::MarketTrade],
                provides_catalog: true,
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn any_market_loaded(&self) -> bool {
        self.loaded
            .borrow()
            .iter()
            .any(|route| market_venues().contains(&route.venue))
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.loaded.borrow().iter().any(|held| held == route)
    }

    fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError> {
        self.catalog.contains(listing)
    }

    fn listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>> {
        self.catalog.cached_listing(listing)
    }

    fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog)
    }
}

struct Fixture {
    harness: WidgetTestApp,
    widget: MarketTradeWidget,
    directory: FixtureDirectory,
    recovery: WidgetAction,
    reloads: Rc<Cell<usize>>,
    publishes: Rc<Cell<usize>>,
}

impl Fixture {
    fn new<S: MarketTradeSource>(
        config: MarketTradeConfig,
        source: S,
        loaded: bool,
        entries: Vec<CatalogEntry>,
    ) -> Self {
        Self::with_catalog(config, move |_| source, loaded, entries)
    }

    fn with_catalog<S: MarketTradeSource>(
        config: MarketTradeConfig,
        build: impl FnOnce(Arc<Catalog>) -> S,
        loaded: bool,
        entries: Vec<CatalogEntry>,
    ) -> Self {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let fixture = test_support::Fixture::new(entries.clone());
        test_support::publish_fixture_route(fixture.catalog(), &route(), entries)
            .expect("the fixture route should publish its catalogue");
        let catalog = Arc::clone(fixture.catalog_arc());
        let directory = FixtureDirectory::new(Arc::clone(&catalog), loaded);
        let source = build(Arc::clone(&catalog));
        let status = harness.status();

        let reloads: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counted_reloads = Rc::clone(&reloads);
        let recovery = WidgetAction::new("common.retry", move |_| {
            counted_reloads.set(counted_reloads.get() + 1);
        });
        let publishes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counted_publications = Rc::clone(&publishes);
        let change = WidgetConfigChange::new(move |_| {
            counted_publications.set(counted_publications.get() + 1);
        });

        let directory_handle: Rc<dyn MarketDirectory> = Rc::new(directory.clone());
        let widget_recovery = recovery.clone();
        let widget = harness.with_cx(|cx| {
            MarketTradeWidget::with_source(
                config,
                source,
                directory_handle,
                status,
                change,
                widget_recovery,
                cx,
            )
        });

        let mut fixture = Self {
            harness,
            widget,
            directory,
            recovery,
            reloads,
            publishes,
        };
        fixture.load();
        fixture
    }

    fn over_listing<S: MarketTradeSource>(source: S, loaded: bool) -> Self {
        Self::new(
            MarketTradeConfig {
                selection: Some(selection(listing())),
            },
            source,
            loaded,
            vec![default_entry()],
        )
    }

    fn with_cx<R>(
        &mut self,
        body: impl FnOnce(&mut MarketTradeWidget, &mut WidgetCx<'_>) -> R,
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
        self.widget.snapshot.as_ref().map(MarketTradeSnapshot::len)
    }

    fn selected(&self) -> Option<InstrumentSelection> {
        self.widget.config.selection.clone()
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

    fn apply_latest(&mut self, key: MarketTradeStreamKey, snapshot: MarketTradeSnapshot) {
        self.with_cx(|widget, cx| {
            widget.apply_latest(key, &MarketTradeStreamMessage::Snapshot(snapshot), cx);
        });
    }

    fn apply_reliable(&mut self, key: MarketTradeStreamKey, status: MarketTradeStatus) {
        self.with_cx(|widget, cx| {
            widget.apply_reliable(key, &MarketTradeStreamMessage::Status(status), cx);
        });
    }

    fn apply_rpc(&mut self, request_id: RequestId, result: WidgetRpcResult<MarketTradeProtocol>) {
        self.with_cx(|widget, cx| widget.apply_rpc(request_id, result, cx));
    }

    fn accept_snapshot(&mut self, snapshot: MarketTradeSnapshot) {
        self.with_cx(|widget, cx| {
            widget.ready_received = true;
            widget.apply_snapshot(snapshot, cx);
        });
        self.sync();
    }

    fn report_failure(&mut self) {
        let state = WidgetState::failed(Arc::new(disconnect()), self.recovery.clone());
        assert!(self.harness.status().report(self.harness.app_mut(), state));
    }

    fn request_selection(&mut self, selection: InstrumentSelection) {
        let requests = self.widget.selection_requests;
        assert!(
            self.harness.app_mut().write(&requests, Some(selection)),
            "the control asks for a selection"
        );
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

    fn build_header(&mut self) {
        self.with_cx(|widget, cx| {
            let root = cx.tree().root();
            widget
                .header(HeaderGroup::Left, root, cx)
                .expect("the header control builds");
        });
    }

    fn header_node(&self) -> Option<NodeId> {
        self.harness
            .find_by_debug_prefix(self.harness.root(), HEADER_KEY_SUFFIX)
    }

    fn kit_key(&mut self, suffix: &str) -> SmolStr {
        self.with_cx(|_, cx| cx.unique_key(suffix))
    }

    fn header_action(&mut self) -> Action {
        Action::Custom(format_smolstr!("{}.click", self.kit_key(HEADER_KEY_SUFFIX)))
    }

    fn table_sort_action(&mut self) -> Action {
        Action::Custom(format_smolstr!("{}.sort", self.kit_key(TABLE_KEY_SUFFIX)))
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

    fn settle_until(&mut self, what: &str, ready: impl Fn(&Self) -> bool) {
        for _ in 0..256 {
            self.harness.run_until_idle();
            self.sync();
            if ready(self) {
                return;
            }
        }
        panic!("{what} should happen");
    }
}

#[test]
fn a_widget_without_a_listing_should_ask_for_one() {
    let fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );

    assert!(!fixture.has_content());
    let status = fixture.status();
    assert!(
        !status.is_busy(),
        "a pane asking for a listing is not working"
    );
    assert!(status.failure().is_none());
    assert_eq!(
        status.notice(),
        Some(&Notice::message(
            "widget.market_trade.select_instrument_prompt"
        ))
    );
}

#[test]
fn a_tape_with_nothing_chosen_and_no_market_loaded_should_wait_for_one() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        false,
        Vec::new(),
    );

    assert!(!fixture.has_content(), "there are no trades to read");
    assert!(
        fixture.widget.awaiting_market,
        "and no market to read them from either"
    );
    assert_eq!(
        fixture.status(),
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: market_venues(),
            supported: market_venues(),
        }),
        "a tape nobody could choose from waits for the market participant metadata"
    );

    fixture.directory.load(route());
    fixture.bump_generation();
    assert!(!fixture.widget.awaiting_market);
    assert_eq!(
        fixture.status(),
        WidgetState::waiting(Notice::message(
            "widget.market_trade.select_instrument_prompt"
        )),
        "a market the user can choose from asks for a listing"
    );
}

#[test]
fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content() {
    let fixture = Fixture::over_listing(PendingSource, true);

    assert!(!fixture.has_content());
    assert_eq!(fixture.status(), WidgetState::Connecting);
}

#[test]
fn the_status_should_follow_the_stream_lifecycle() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    let key = MarketTradeStreamKey::new(listing());
    assert_eq!(fixture.status(), WidgetState::Connecting);

    fixture.apply_reliable(key.clone(), MarketTradeStatus::Connecting);
    assert_eq!(fixture.status(), WidgetState::Connecting);

    fixture.apply_reliable(key.clone(), MarketTradeStatus::LoadingHistory);
    assert_eq!(
        fixture.status(),
        WidgetState::Loading,
        "a stream that is opening its page is loading"
    );

    fixture.apply_latest(key.clone(), snapshot(1));
    assert_eq!(
        fixture.status(),
        WidgetState::Loading,
        "a snapshot before the ready boundary does not become content"
    );
    assert!(!fixture.has_content());

    fixture.apply_reliable(key, MarketTradeStatus::Ready);
    assert_eq!(fixture.status(), WidgetState::Ready);
    assert!(fixture.has_content());
    assert_eq!(fixture.snapshot_len(), Some(1));
}

#[test]
fn an_accepted_snapshot_should_become_content_and_clear_the_status() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.accept_snapshot(snapshot(1));

    assert!(fixture.has_content());
    assert_eq!(fixture.status(), WidgetState::Ready);
    assert!(fixture.status().failure().is_none());
}

#[test]
fn an_empty_snapshot_should_be_content_and_report_nothing() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.accept_snapshot(empty_snapshot());
    fixture.build_body();

    assert!(fixture.has_content());
    assert_eq!(fixture.table_rows(), 0);
    let empty = fixture
        .table()
        .empty_node()
        .expect("an empty tape has to say it is empty");
    fixture.drain();
    assert!(
        fixture.harness.text_of(empty).contains("No data"),
        "the empty sentence is the table's own: {:?}",
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
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.report_failure();

    assert!(
        fixture.has_content(),
        "a failure must not throw away data the widget already has"
    );
    let status = fixture.status();
    assert!(status.is_failed());
    let translate = product_translate(LanguageCode::En);
    assert_eq!(
        status.failure().map(|failure| failure.text(&translate)),
        Some(disconnect().translated_message(&translate))
    );
}

#[test]
fn a_failure_without_content_should_be_reported_with_no_content() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.report_failure();

    assert!(!fixture.has_content());
    assert!(fixture.status().is_failed());
}

#[test]
fn every_failure_should_offer_the_recovery_that_reloads_the_widget() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.report_failure();

    let translate = product_translate(LanguageCode::En);
    let recovery = fixture
        .status()
        .failure()
        .map(|failure| failure.action().clone())
        .expect("a failure should offer a way out");
    assert_eq!(
        &recovery, &fixture.recovery,
        "the widget reports the recovery the framework handed it"
    );
    assert_eq!(recovery.label(&translate).as_str(), "Retry");
    assert!(recovery.is_enabled());
    assert_eq!(fixture.reloads.get(), 0);

    recovery.run(fixture.harness.app_mut());

    assert_eq!(fixture.reloads.get(), 1, "the recovery runs the load");
}

#[test]
fn a_late_stream_message_after_a_failure_should_not_change_the_status() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    let key = MarketTradeStreamKey::new(listing());
    fixture.report_failure();
    let failed = fixture.status();

    fixture.apply_reliable(key.clone(), MarketTradeStatus::Ready);
    fixture.apply_latest(key, snapshot(1));

    assert_eq!(fixture.status(), failed);
    assert_eq!(fixture.snapshot_len(), None);
}

#[test]
fn a_late_rpc_result_should_not_replace_a_terminal_failure() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.report_failure();
    let failed = fixture.status();

    fixture.with_cx(|widget, cx| {
        widget.apply_rpc_result(
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
            cx,
        );
    });

    assert_eq!(
        fixture.status(),
        failed,
        "the failed pane keeps its failure"
    );
}

#[test]
fn an_rpc_result_while_waiting_for_the_market_should_not_replace_the_wait() {
    let mut fixture = Fixture::over_listing(PendingSource, false);
    assert!(
        fixture.widget.awaiting_market,
        "the fixture waits for its market"
    );
    let waiting = fixture.status();

    fixture.with_cx(|widget, cx| {
        widget.apply_rpc_result(
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
            cx,
        );
    });

    assert_eq!(
        fixture.status(),
        waiting,
        "the pane keeps waiting for its market"
    );
    assert!(!fixture.status().is_failed());
}

#[test]
fn the_answer_to_the_read_it_sent_should_be_reported() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.apply_rpc(
        RequestId::new(1),
        Err(RpcCallError::ServiceClosed {
            request_id: RequestId::new(1),
        }),
    );

    assert!(
        fixture.status().is_failed(),
        "the read's refusal is the tape's failure"
    );
}

#[test]
fn the_answer_to_a_replaced_read_should_be_dropped() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.load();

    fixture.apply_rpc(
        RequestId::new(1),
        Err(RpcCallError::ServiceClosed {
            request_id: RequestId::new(1),
        }),
    );

    assert!(
        !fixture.status().is_failed(),
        "the replaced read's refusal is not this tape's failure"
    );
    assert_eq!(
        fixture.widget.pending.dropped(),
        1,
        "the late answer is dropped and counted"
    );
}

#[test]
fn a_message_from_another_scope_should_be_ignored() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.with_cx(|widget, _| widget.ready_received = true);

    fixture.apply_latest(MarketTradeStreamKey::new(other_listing()), snapshot(1));

    assert_eq!(
        fixture
            .widget
            .snapshot
            .as_ref()
            .map(MarketTradeSnapshot::len),
        None
    );
    assert!(fixture.widget.pending_snapshot.is_none());
}

#[test]
fn two_listings_of_one_instrument_should_stay_independent() {
    let instrument = test_support::spot_token(test_support::VENUE, "BTC", "USDT");
    let first = test_support::entry(&route(), instrument.clone(), "BTCUSDT");
    let second = test_support::entry(&route(), instrument, "BTCUSDT-PERP");
    assert_eq!(
        first.instrument.id(),
        second.instrument.id(),
        "the fixture is one instrument listed twice"
    );
    assert_ne!(first.listing.key, second.listing.key);
    let fixture = Fixture::new(
        MarketTradeConfig {
            selection: Some(selection(first.listing.key.clone())),
        },
        PendingSource,
        true,
        vec![first.clone()],
    );

    assert_eq!(
        fixture.selected(),
        Some(selection(first.listing.key.clone()))
    );
    assert_eq!(
        fixture.widget.current_key(),
        Some(MarketTradeStreamKey::new(first.listing.key)),
        "the stream key names the exact listing, not the instrument"
    );
    assert_ne!(
        fixture.widget.current_key(),
        Some(MarketTradeStreamKey::new(second.listing.key))
    );
}

#[test]
fn a_snapshot_before_the_ready_boundary_should_await_it() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    let key = MarketTradeStreamKey::new(listing());

    fixture.apply_latest(key.clone(), snapshot(1));
    assert_eq!(fixture.snapshot_len(), None);
    assert!(!fixture.has_content());

    fixture.apply_reliable(key, MarketTradeStatus::Ready);

    assert_eq!(fixture.snapshot_len(), Some(1));
    assert!(fixture.has_content());
    assert_eq!(fixture.status(), WidgetState::Ready);
}

#[test]
fn a_stream_failure_should_become_the_reported_state() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.apply_reliable(
        MarketTradeStreamKey::new(listing()),
        MarketTradeStatus::Error(disconnect()),
    );

    assert!(fixture.status().is_failed());
    assert!(!fixture.has_content());
}

#[test]
fn an_explicit_reload_should_start_over_from_connecting() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    assert!(fixture.has_content());

    fixture.load();

    assert!(!fixture.has_content());
    assert_eq!(fixture.status(), WidgetState::Connecting);
    assert!(fixture.status().failure().is_none());
}

#[test]
fn a_retry_after_a_failure_should_keep_the_snapshot_the_widget_still_shows() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
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
fn an_accepted_snapshot_should_reach_the_table_region() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot_of_listing(listing(), &[(1, 1, 100), (2, 2, 101)]));
    fixture.build_body();

    assert_eq!(fixture.table_rows(), 2);

    fixture.with_cx(|widget, _| widget.clear_trades());
    fixture.sync();

    assert_eq!(fixture.table_rows(), 0);
}

#[test]
fn the_headers_should_name_the_units_of_the_followed_listing() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.build_body();

    assert_eq!(
        [
            fixture.header_text(0),
            fixture.header_text(1),
            fixture.header_text(2),
        ],
        ["Price (USDT)", "Amount (BTC)", "Time"]
    );
}

#[test]
fn the_body_should_draw_the_accepted_rows() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot_of_listing(
        listing(),
        &[(1, 2_000, 100), (2, 3_000, 101)],
    ));
    fixture.build_body();

    assert_eq!(
        [
            fixture.settled_cell_text(0, 0),
            fixture.settled_cell_text(0, 1),
            fixture.settled_cell_text(0, 2),
        ],
        ["101.00", "2.000", "00:00:03.000"],
        "display row 0 is the newer print"
    );
    assert_eq!(
        fixture.settled_cell_text(1, 0),
        "100.00",
        "display row 1 is the older print"
    );
}

#[test]
fn the_body_should_mount_the_table_under_its_frame() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
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
        Some("MarketTradeTableFrame"),
        "the frame keeps its own selector"
    );
}

#[test]
fn the_price_cell_should_draw_in_the_side_colour() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(both_sides_snapshot());
    fixture.build_body();

    let theme = fixture.harness.services().theme.clone();
    let light = theme.theme_for(Appearance::Light);
    let foreground = light.role(Role::FgDefault);
    let up = light.role(Role::MarketUp);
    let down = light.role(Role::MarketDown);
    assert_ne!(up, down, "the two sides are drawn in different colours");

    assert_eq!(
        fixture.cell_foreground(0, 0),
        down,
        "display row 0 is the newest print, and it is a sell"
    );
    assert_eq!(
        fixture.cell_foreground(1, 0),
        up,
        "display row 1 is the older print, and it is a buy"
    );
    for row in 0..2 {
        for column in [1, 2] {
            assert_eq!(
                fixture.cell_foreground(row, column),
                foreground,
                "row {row} column {column} draws in the theme's default foreground"
            );
        }
    }
}

#[test]
fn moving_the_display_zone_should_redraw_the_time_column() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.build_body();
    assert_eq!(fixture.settled_cell_text(0, 2), "00:00:00.001");

    let config_signal = fixture.harness.services().config;
    let mut config = (*fixture.harness.app().read(&config_signal)).clone();
    config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
    assert!(
        fixture.harness.app_mut().write(&config_signal, config),
        "the setting move is a change"
    );
    fixture.sync();

    assert_eq!(
        fixture.settled_cell_text(0, 2),
        "08:00:00.001",
        "the time column redraws in the zone the settings now name"
    );
    assert_eq!(
        fixture.settled_cell_text(0, 0),
        "100.00",
        "the price is a number rather than a clock"
    );
}

#[test]
fn a_language_change_should_rebuild_the_tables_static_copy() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.build_body();
    assert_eq!(fixture.header_text(0), "Price (USDT)");

    fixture.harness.switch_language(LanguageCode::ZhCn);
    fixture.sync();

    assert_eq!(fixture.header_text(0), "价格（USDT）");
    assert_eq!(fixture.header_text(2), "时间");
    assert_eq!(fixture.table_rows(), 1, "the rebuilt table keeps the rows");

    let empty = fixture
        .table()
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
    let mut fixture = Fixture::over_listing(PendingSource, true);
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
fn a_snapshot_that_changes_the_units_should_rebuild_the_table() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.build_body();
    assert_eq!(
        fixture.header_text(1),
        "Amount (BTC)",
        "the base asset is the unit of the trade the tape holds"
    );

    fixture.accept_snapshot(contract_snapshot());

    assert_eq!(
        fixture.header_text(1),
        "Amount (Contracts)",
        "the rebuilt table names the unit the newest trade carries"
    );
    assert_eq!(
        fixture.header_text(0),
        "Price (USDT)",
        "the quote unit did not move"
    );
    assert_eq!(
        fixture.table_rows(),
        2,
        "the rebuilt table keeps the rows it was given"
    );

    fixture.accept_snapshot(snapshot(3));

    assert_eq!(
        fixture.header_text(1),
        "Amount (BTC)",
        "a unit that moves back rebuilds the table back"
    );
}

#[test]
fn releasing_the_widget_should_release_the_table() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.build_body();
    fixture.build_header();
    let table_key = fixture.table().key().to_owned();
    let expected_table_key = fixture.kit_key(TABLE_KEY_SUFFIX);
    assert_eq!(
        table_key, expected_table_key,
        "the table's kit key is minted from the pane's instance scope, not a constant"
    );
    let sort = fixture.table_sort_action();
    let click = fixture.header_action();
    assert!(
        fixture.harness.app().has_action_handler(&sort),
        "the mounted table registers its sort handler"
    );
    assert!(
        fixture.harness.app().has_action_handler(&click),
        "the header control registers its click handler"
    );

    fixture.with_cx(|widget, cx| widget.release(cx));

    assert!(
        fixture.widget.table.is_none(),
        "release drops the widget's table"
    );
    assert!(
        !fixture.harness.app().has_action_handler(&sort),
        "a table left un-released keeps its sort handler registered"
    );
    assert!(
        !fixture.harness.app().has_action_handler(&click),
        "a header control left un-released keeps its click handler registered"
    );
}

#[test]
fn an_unrelated_directory_generation_should_reopen_the_stream() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::over_listing(source.clone(), true);
    fixture.settle_until("the first stream to be opened", |_| {
        !source.subscribed().is_empty()
    });
    fixture.accept_snapshot(snapshot(1));
    assert!(fixture.has_content(), "the tape is reading trades");
    let subscribed = source.subscribed().len();

    fixture.bump_generation();
    fixture.settle_until("the tape to open a new stream", |_| {
        source.subscribed().len() > subscribed
    });

    assert!(
        !fixture.has_content(),
        "the trades are gone: their labels were written at the generation that was replaced"
    );
    assert_eq!(fixture.status(), WidgetState::Connecting);
}

#[test]
fn a_refresh_of_the_tapes_own_venue_should_reopen_its_stream() {
    let resolutions: Arc<Mutex<Vec<ListingPrecision>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&resolutions);
    let mut fixture = Fixture::with_catalog(
        MarketTradeConfig {
            selection: Some(selection(listing())),
        },
        move |catalog| ResolvingSource {
            catalog,
            resolutions: recorded,
        },
        true,
        vec![listing_entry("1")],
    );
    fixture.settle_until("the tape's stream to be opened", |_| {
        resolutions_of(&resolutions).len() == 1
    });
    assert_eq!(resolutions_of(&resolutions)[0].price_tick.to_string(), "1");
    fixture.accept_snapshot(snapshot(1));

    test_support::publish_fixture_route(
        fixture.directory.catalog().as_ref(),
        &route(),
        vec![listing_entry("0.01")],
    )
    .expect("the venue republishes its catalogue");
    fixture.bump_generation();
    fixture.settle_until("the tape to open a new stream", |_| {
        resolutions_of(&resolutions).len() == 2
    });

    assert!(
        !fixture.has_content(),
        "the trades are gone: they were written at the generation that was replaced"
    );
    assert_eq!(
        resolutions_of(&resolutions)[1].price_tick.to_string(),
        "0.01",
        "the stream that was opened resolved at the generation this load published"
    );
}

#[test]
fn a_venue_loaded_after_the_tape_started_reading_should_be_offered_by_its_picker() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::over_listing(source.clone(), true);
    fixture.settle_until("the tape to follow its listing", |_| {
        !source.subscribed().is_empty()
    });
    fixture.accept_snapshot(snapshot(1));

    fixture.directory.load(okx_route());
    fixture.bump_generation();

    let filters = fixture.widget.instrument_filters();
    assert!(
        offers_route(&filters.routes, &okx_route(), CapabilityId::MarketTrade),
        "a scope loaded after the tape started reading is one its picker offers"
    );
}

#[test]
fn a_commit_that_takes_the_tapes_own_market_away_should_send_it_back_to_waiting() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    assert!(fixture.has_content(), "the tape is reading trades");

    fixture.directory.unload(&route());
    fixture.bump_generation();

    assert!(
        fixture.widget.awaiting_market,
        "the tape waits for the market it lost"
    );
    assert!(
        !fixture.has_content(),
        "the trades it was drawing are not backed by a market any more"
    );
    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "a market that went away does not unmake the user's choice"
    );
    assert_eq!(
        fixture.status(),
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![route().venue],
            supported: market_venues(),
        }),
        "waiting is not working"
    );
}

#[test]
fn a_venue_commit_should_not_revive_a_failed_pane() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    fixture.accept_snapshot(snapshot(1));
    fixture.report_failure();
    let failure = fixture.status();
    assert!(failure.is_failed(), "the tape is in its terminal failure");

    fixture.directory.unload(&route());
    fixture.bump_generation();
    fixture.directory.load(route());
    fixture.bump_generation();

    assert_eq!(
        fixture.status(),
        failure,
        "a directory commit must not revive a failed tape"
    );
    assert!(
        !fixture.widget.awaiting_market,
        "a tape in its terminal failure is not a tape waiting for a market"
    );
}

#[test]
fn choosing_a_listing_should_start_following_it() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        source.clone(),
        true,
        vec![default_entry(), eth_entry()],
    );

    fixture.request_selection(selection(listing()));
    fixture.sync();

    assert_eq!(fixture.selected(), Some(selection(listing())));
    assert_eq!(fixture.status(), WidgetState::Connecting);
    assert_eq!(fixture.publishes.get(), 1, "the choice is written back");
    fixture.settle_until("the tape to follow the chosen listing", |_| {
        source.subscribed().contains(&listing())
    });

    fixture.with_cx(|widget, cx| {
        assert!(
            !widget.select_selection(selection(listing()), cx),
            "selecting the same scope again is not a new transition"
        );
        assert!(
            widget.select_selection(selection(other_listing()), cx),
            "another listing of the same route is another scope"
        );
    });
    assert_eq!(fixture.selected(), Some(selection(other_listing())));
    assert_eq!(
        fixture.widget.current_key(),
        Some(MarketTradeStreamKey::new(other_listing()))
    );
    assert_eq!(fixture.publishes.get(), 2);
}

#[test]
fn a_selection_from_the_header_control_should_start_following_it() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        source.clone(),
        true,
        vec![default_entry()],
    );
    fixture.build_header();

    fixture.request_selection(selection(listing()));
    fixture.sync();

    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "the confirmed selection becomes the pane's selection"
    );
    assert_eq!(
        &*fixture
            .harness
            .app()
            .read(&fixture.widget.selection_requests),
        &None,
        "the request is consumed once"
    );
    fixture.settle_until("the tape to follow the confirmed listing", |_| {
        source.subscribed().contains(&listing())
    });
}

#[test]
fn a_confirmation_should_hand_the_choice_to_the_next_sync() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );
    fixture.build_header();
    let node = fixture.header_node().expect("the control is in the header");
    let click = fixture.header_action();
    let window = fixture.harness.window();

    assert!(
        fixture
            .harness
            .app_mut()
            .dispatch_action(window, node, click, ActionSource::Programmatic),
        "the live control handles its own click"
    );
    fixture.drain();
    assert!(
        !fixture.harness.app().overlays(window).is_empty(),
        "the control opened the selector it hosts"
    );

    let requests = fixture.widget.selection_requests;
    let before = fixture.harness.app().frame_requests();
    hand_over_selection(
        requests,
        selection(listing()),
        fixture.harness.app_mut(),
        window,
    );

    assert_eq!(
        &*fixture.harness.app().read(&requests),
        &Some(selection(listing())),
        "the confirmation writes the selection the next sync applies"
    );
    assert!(
        fixture.harness.app().frame_requests() > before,
        "the confirmation asks for the frame that applies it"
    );

    fixture.sync();

    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "the next sync follows the confirmed listing"
    );
    assert_eq!(
        &*fixture.harness.app().read(&requests),
        &None,
        "the request is consumed once"
    );
}

#[test]
fn a_confirmation_made_while_the_market_is_loading_should_keep_the_choice() {
    let mut fixture = Fixture::over_listing(PendingSource, false);
    assert!(
        fixture.widget.awaiting_market,
        "the fixture waits for its market"
    );

    fixture.request_selection(selection(other_listing()));
    fixture.sync();

    assert_eq!(
        fixture.selected(),
        Some(selection(other_listing())),
        "a choice made while the market is away is not withdrawn by a settle it never saw"
    );
    assert_eq!(
        fixture.publishes.get(),
        1,
        "the choice is written back once"
    );
    assert!(
        fixture.widget.awaiting_market,
        "the pane keeps waiting for the market"
    );
    let status = fixture.status();
    assert!(
        status.notice().is_some(),
        "the pane says which market it waits for"
    );
    assert!(status.failure().is_none(), "waiting is not working");
}

#[test]
fn the_header_should_build_the_instrument_control() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );
    fixture.build_header();

    let node = fixture.header_node().expect("the control is in the header");
    assert_eq!(
        fixture.harness.text_of(node),
        "Select instrument",
        "a control that follows nothing states the offer"
    );

    fixture.request_selection(selection(listing()));
    fixture.sync();

    let node = fixture.header_node().expect("the control is in the header");
    let label = fixture.harness.text_of(node);
    assert_ne!(
        label, "Select instrument",
        "the control names the listing now"
    );
    assert!(
        label.contains("Binance"),
        "the control states the route the listing is on: {label:?}"
    );
}

#[test]
fn a_control_with_no_market_loaded_should_carry_no_click() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        false,
        Vec::new(),
    );
    fixture.build_header();
    let click = fixture.header_action();

    let node = fixture.header_node().expect("the control is in the header");
    assert_eq!(
        fixture.harness.text_of(node),
        "Select instrument",
        "a control that follows nothing states the offer"
    );
    assert!(
        !fixture.harness.app().has_action_handler(&click),
        "a control with no market to offer must not answer a click with a selector"
    );
}

#[test]
fn a_language_change_should_rebuild_the_header_control() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );
    fixture.build_header();
    let previous = fixture.header_node().expect("the control is in the header");
    assert_eq!(fixture.harness.text_of(previous), "Select instrument");

    fixture.harness.switch_language(LanguageCode::ZhCn);
    fixture.sync();

    assert!(
        fixture.harness.tree().get(previous).is_err(),
        "the control built in the old language was removed"
    );
    let node = fixture.header_node().expect("the control is in the header");
    assert_eq!(
        fixture.harness.text_of(node),
        "选择标的",
        "the rebuilt control states the offer in the language showing now"
    );
}

#[test]
fn rebuilding_the_header_control_should_not_leak_signal_slots() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );
    fixture.build_header();
    fixture.drain();
    let baseline = signal_slot_stats(fixture.harness.app()).live;

    for cycle in 0..10 {
        let language = if cycle % 2 == 0 {
            LanguageCode::ZhCn
        } else {
            LanguageCode::En
        };
        fixture.harness.switch_language(language);
        fixture.sync();

        assert!(
            fixture.header_node().is_some(),
            "cycle {cycle}: the control is still in the header"
        );
        assert_eq!(
            signal_slot_stats(fixture.harness.app()).live,
            baseline,
            "cycle {cycle}: the control rebuilt for the language switch released the one it replaced"
        );
    }
}

#[test]
fn a_directory_generation_should_rebuild_the_header_control() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::over_listing(source.clone(), true);
    fixture.settle_until("the tape to follow its listing", |_| {
        !source.subscribed().is_empty()
    });
    fixture.accept_snapshot(snapshot(1));
    fixture.build_header();
    let previous = fixture.header_node().expect("the control is in the header");

    fixture.directory.load(okx_route());
    fixture.bump_generation();

    assert!(
        fixture.harness.tree().get(previous).is_err(),
        "the control built against the retired generation was removed"
    );
    let rebuilt = fixture
        .header_node()
        .expect("a control stands in the header again");
    assert_ne!(
        rebuilt, previous,
        "a new control was built for the generation that may have widened the picker"
    );
}

#[test]
fn two_panes_should_not_share_their_control_identities() {
    let mut fixture = Fixture::new(
        MarketTradeConfig::default(),
        PendingSource,
        true,
        vec![default_entry()],
    );
    let root = fixture.harness.root();
    fixture.build_header();
    let first_click = fixture.header_action();
    let first_node = fixture
        .header_node()
        .expect("the first pane's control is in its row");

    let directory: Rc<dyn MarketDirectory> = Rc::new(fixture.directory.clone());
    let status = fixture.harness.status();
    let (peer, peer_click) = {
        let services = Rc::clone(fixture.harness.services_rc());
        let window = fixture.harness.window();
        let scope = fixture.harness.app_mut().signal_scope();
        let mut cx = WidgetCx::new(fixture.harness.app_mut(), window, &services, scope);
        let key = cx.unique_key(HEADER_KEY_SUFFIX);
        let mut peer = MarketTradeWidget::with_source(
            MarketTradeConfig::default(),
            PendingSource,
            directory,
            status,
            WidgetConfigChange::new(|_| {}),
            WidgetAction::new("common.retry", |_| {}),
            &mut cx,
        );
        peer.header(HeaderGroup::Left, root, &mut cx)
            .expect("the second pane's header control builds");
        (peer, Action::Custom(format_smolstr!("{key}.click")))
    };
    let peer_node = peer
        .header
        .as_ref()
        .expect("the second pane built its control")
        .control
        .node();

    assert_ne!(
        first_click, peer_click,
        "each pane's control registered its own action identity"
    );
    assert!(
        fixture.harness.app().has_action_handler(&first_click),
        "the first pane's control is live"
    );
    assert!(
        fixture.harness.app().has_action_handler(&peer_click),
        "the second pane's control is live"
    );

    fixture.with_cx(|widget, cx| widget.release(cx));

    assert!(
        !fixture.harness.app().has_action_handler(&first_click),
        "the closed pane released the control it built"
    );
    assert!(
        fixture.harness.app().has_action_handler(&peer_click),
        "releasing one pane removed another pane's header handler"
    );
    assert!(
        fixture.harness.tree().get(peer_node).is_ok(),
        "the other pane's control node is still mounted"
    );
    assert!(
        fixture.harness.tree().get(first_node).is_ok(),
        "the closed pane's header row belongs to the pane, not the widget"
    );
}

#[test]
fn a_listing_the_route_no_longer_offers_should_go_back_to_nothing_chosen() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.with_cx(|widget, cx| {
        widget.settle_selection(selection(listing()), ListingSupport::Withdrawn, cx);
    });

    assert_eq!(fixture.selected(), None);
    assert_eq!(
        fixture.status().notice(),
        Some(&Notice::message(
            "widget.market_trade.select_instrument_prompt"
        ))
    );
    assert_eq!(
        fixture.publishes.get(),
        1,
        "the replacement is reported, or the next start resets it again"
    );
}

#[test]
fn a_lookup_that_failed_should_leave_the_selection_alone() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    let support = listing_support(&listing(), Err(no_answer()), &fixture.directory);

    assert_eq!(support, ListingSupport::Unknown);
    fixture.with_cx(|widget, cx| {
        widget.settle_selection(selection(listing()), support, cx);
    });

    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "the venue not answering is not the venue saying no"
    );
    assert_eq!(fixture.publishes.get(), 0);
}

#[test]
fn a_choice_made_while_the_lookup_was_in_flight_should_survive_it() {
    let mut fixture = Fixture::over_listing(PendingSource, true);

    fixture.with_cx(|widget, cx| {
        widget.config.selection = Some(selection(other_listing()));
        assert!(!widget.settle_selection(selection(listing()), ListingSupport::Withdrawn, cx));
    });

    assert_eq!(
        fixture.selected(),
        Some(selection(other_listing())),
        "a choice the user made is not the answer's to overwrite"
    );
    assert_eq!(fixture.publishes.get(), 0);
}

#[test]
fn a_listing_of_an_unloaded_market_should_leave_the_pane_waiting() {
    let fixture = Fixture::over_listing(PendingSource, false);

    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "a market that is not loaded does not unmake the user's choice"
    );
    let status = fixture.status();
    assert_eq!(
        status,
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![listing().route.venue],
            supported: market_venues(),
        }),
        "the pane waits"
    );
    assert!(
        status.failure().is_none(),
        "waiting for a market is not a failure"
    );
    let notice = status.notice().expect("the pane waits for its market");
    assert!(
        notice
            .resolve(&product_translate(LanguageCode::En))
            .contains("Binance"),
        "the notice names the market"
    );
    assert_eq!(
        fixture.publishes.get(),
        0,
        "waiting changes no configuration"
    );
}

#[test]
fn loading_the_venue_should_let_the_waiting_pane_subscribe() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::over_listing(source.clone(), false);
    assert!(
        fixture.status().notice().is_some(),
        "the pane waits for a market nobody loaded"
    );

    fixture.directory.load(route());
    fixture.bump_generation();
    fixture.settle_until("the tape to subscribe", |_| !source.subscribed().is_empty());

    let status = fixture.status();
    assert!(
        status.notice().is_none(),
        "the pane left the notice without being reopened"
    );
    assert!(status.failure().is_none(), "recovery is not a failure");
}

#[test]
fn a_listing_of_a_market_that_lists_nothing_should_go_back_to_nothing_chosen() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    test_support::publish_fixture_route(fixture.directory.catalog().as_ref(), &route(), Vec::new())
        .expect("the route publishes an empty catalogue");
    fixture.load();

    assert_eq!(fixture.selected(), None);
    assert!(
        fixture.status().failure().is_none(),
        "a market that lists nothing is not a failure: nothing is chosen"
    );
    assert_eq!(fixture.publishes.get(), 1);
}

#[test]
fn a_market_revoked_after_construction_should_leave_the_pane_waiting() {
    let mut fixture = Fixture::over_listing(PendingSource, true);
    assert!(
        fixture.status().notice().is_none(),
        "the market the fixture loaded is the one the pane reads"
    );

    fixture.directory.unload(&route());
    fixture.load();

    assert_eq!(
        fixture.selected(),
        Some(selection(listing())),
        "a market that is gone does not unmake the user's choice"
    );
    let status = fixture.status();
    assert!(
        status.failure().is_none(),
        "a market that is no longer registered is not a failure"
    );
    let notice = status.notice().expect("the pane waits for its market");
    assert!(
        notice
            .resolve(&product_translate(LanguageCode::En))
            .contains("Binance"),
        "the notice names the market"
    );
    assert_eq!(
        fixture.publishes.get(),
        0,
        "waiting changes no configuration"
    );
}

#[test]
fn a_pane_waiting_for_its_market_should_follow_the_language() {
    let notice = Notice::MarketNotLoaded {
        venues: vec![route().venue],
        supported: market_venues(),
    };
    let english = notice.resolve(&product_translate(LanguageCode::En));
    let chinese = notice.resolve(&product_translate(LanguageCode::ZhCn));

    assert!(
        english.contains("Binance"),
        "the notice names the market: {english:?}"
    );
    assert!(
        chinese.contains("币安"),
        "the same reference resolves in the language showing now: {chinese:?}"
    );
    assert_ne!(
        chinese, english,
        "the copy of the language the interface left is gone"
    );
}

#[test]
fn a_delisted_listing_should_go_back_to_nothing_chosen() {
    let mut fixture = Fixture::new(
        MarketTradeConfig {
            selection: Some(selection(listing())),
        },
        PendingSource,
        true,
        vec![delisted_entry()],
    );
    fixture.load();

    assert_eq!(fixture.selected(), None);
    assert!(
        fixture.status().failure().is_none(),
        "a delisted listing is not a failure: the tape has nothing chosen"
    );
    assert_eq!(
        fixture.status().notice(),
        Some(&Notice::message(
            "widget.market_trade.select_instrument_prompt"
        ))
    );
    assert_eq!(fixture.publishes.get(), 1);
}

#[test]
fn a_listing_the_route_dropped_should_never_be_subscribed_to() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::new(
        MarketTradeConfig {
            selection: Some(selection(listing())),
        },
        source.clone(),
        true,
        vec![eth_entry()],
    );
    fixture.load();

    assert!(
        fixture.status().failure().is_none(),
        "a listing the route dropped is not a failure: the tape has nothing chosen"
    );
    assert!(!fixture.has_content());
    assert_eq!(
        fixture.status().notice(),
        Some(&Notice::message(
            "widget.market_trade.select_instrument_prompt"
        ))
    );
    assert!(
        source.subscribed().is_empty(),
        "the tape subscribed to a listing the route no longer publishes"
    );
}

#[test]
fn a_listing_the_route_still_offers_should_be_subscribed_to() {
    let source = RecordingSource::default();
    let mut fixture = Fixture::over_listing(source.clone(), true);
    fixture.settle_until("the tape to subscribe", |_| !source.subscribed().is_empty());

    assert_eq!(fixture.selected(), Some(selection(listing())));
    assert_eq!(source.subscribed(), vec![listing()]);
}

#[test]
fn the_selected_listing_should_survive_a_config_round_trip() {
    let config = MarketTradeConfig {
        selection: Some(selection(listing())),
    };

    let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");
    let decoded = stored.decode::<MarketTradeConfig>();

    assert_eq!(decoded.selection, config.selection);
    assert!(MarketTradeConfig::default().selection.is_none());
}

#[test]
fn an_old_payload_that_names_a_route_and_an_instrument_should_be_reset() {
    let stored: VersionedWidgetConfig = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "payload": concat!(
            r#"{"selection":{"route":{"venue":"binance","scope":"spot"},"#,
            r#""instrument":{"symbol":"BTCUSDT"}}}"#,
        ),
    }))
    .expect("the stored envelope should decode");

    let decoded = stored.decode::<MarketTradeConfig>();

    assert!(
        decoded.selection.is_none(),
        "a selection without a listing key is not a listing this build can follow"
    );
}

#[test]
fn a_rich_market_notice_is_drawn_as_rich_text() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let widget = harness.mount::<MarketTradeWidget>(MarketTradeConfig::default());
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
        !text.contains("Longbridge"),
        "the list names the venues whose loader can report trades: {text:?}"
    );
}

#[test]
fn a_directory_generation_write_should_request_a_frame_for_a_waiting_pane() {
    let mut harness = WidgetTestApp::new();
    let widget = harness.mount::<MarketTradeWidget>(MarketTradeConfig::default());
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
