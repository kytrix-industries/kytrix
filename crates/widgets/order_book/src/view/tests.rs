//! Tests for the order-book pane.
//!
//! The framework's headless host composes the services without a market directory
//! (the connector still needs the old toolkit's runtime to build), so most
//! interface tests construct the widget directly over a fixture directory and
//! drive the `WidgetView` hooks — the same shape the market-trade and
//! cryptocurrency-ETF tests use. The mount-path tests at the end drive the real
//! framework host, which is what a pane without a directory is (a pane waiting
//! for the market participant's metadata).

use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::{
    catalog::{
        batch::CatalogEntry,
        display::catalog::DisplaySnapshot,
        error::{CatalogError, SourceError},
        store::Catalog,
        test_support,
    },
    market::{domain::DisconnectReason, error::MarketError},
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity, Tick},
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::PriceBasis,
    },
    market::{
        book::{ListingOrderBookLevel, MarketSequence},
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit},
    },
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::Action,
    node::{builder::NodeBuilder, id::NodeId},
};
use widget::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    protocol::RequestId,
    rpc::RpcCallError,
    status::{WidgetAction, WidgetState, WidgetStatus},
    test_support::WidgetTestApp,
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::service::{OrderBookEventSubscription, OrderBookSource};

// ------------------------------------------------------------------- vocabulary

const PAGE: (f32, f32) = (640.0, 360.0);

/// The depth the fixture route publishes: deep enough for the standard ladder.
const FIXTURE_DEPTH: u16 = 5_000;

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn route() -> VenueRoute {
    test_support::spot_route()
}

fn listing(base: &str) -> VenueListingKey {
    test_support::listing_key(&route(), &format!("{base}USDT"))
}

fn selection(base: &str) -> InstrumentSelection {
    InstrumentSelection {
        listing: listing(base),
    }
}

fn quote_asset() -> AssetId {
    test_support::token(test_support::VENUE, "USDT")
}

fn base_asset() -> AssetId {
    test_support::token(test_support::VENUE, "BTC")
}

fn tick(value: &str) -> Tick {
    value.parse().expect("a tick should parse")
}

fn price(value: &str) -> Price {
    value.parse().expect("a price should parse")
}

fn quantity(value: &str) -> Quantity {
    value.parse().expect("a quantity should parse")
}

fn metadata() -> OrderBookMetadata {
    OrderBookMetadata {
        precision: ListingPrecision {
            price_tick: tick("1"),
            quantity_tick: tick("1"),
        },
        quote_asset: quote_asset(),
        quantity_unit: QuantityUnit::AssetUnits {
            asset: base_asset(),
        },
        depth: FIXTURE_DEPTH,
    }
}

fn level(price_value: &str, quantity_value: &str) -> ListingOrderBookLevel {
    ListingOrderBookLevel {
        price: MarketPrice {
            value: price(price_value),
            quote_asset: quote_asset(),
            basis: PriceBasis::PerAssetUnit,
        },
        quantity: MarketQuantity {
            value: quantity(quantity_value),
            unit: QuantityUnit::AssetUnits {
                asset: base_asset(),
            },
        },
    }
}

fn levels(entries: &[(&str, &str)]) -> Vec<ListingOrderBookLevel> {
    entries
        .iter()
        .map(|(price, quantity)| level(price, quantity))
        .collect()
}

fn snapshot(sequence: u64, asks: &[(&str, &str)], bids: &[(&str, &str)]) -> OrderBookSnapshot {
    snapshot_with_metadata(sequence, asks, bids, metadata())
}

fn snapshot_with_metadata(
    sequence: u64,
    asks: &[(&str, &str)],
    bids: &[(&str, &str)],
    metadata: OrderBookMetadata,
) -> OrderBookSnapshot {
    OrderBookSnapshot {
        asks: levels(asks).into(),
        bids: levels(bids).into(),
        sequence: MarketSequence::Numeric(sequence),
        metadata,
    }
}

fn config(selected: Option<&str>) -> OrderBookConfig {
    OrderBookConfig {
        selection: selected.map(selection),
        ..OrderBookConfig::default()
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

fn disconnect() -> MarketError {
    MarketError::Disconnected(DisconnectReason::Normal)
}

fn no_answer() -> CatalogError {
    CatalogError::Source(Box::new(SourceError::Network {
        reason: SmolStr::new_static("the test venue did not answer"),
    }))
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

// ---------------------------------------------------------------------- sources

/// A source whose opening subscription never answers.
#[derive(Clone, Copy)]
struct PendingSource;

struct PendingSubscription;

impl OrderBookSource for PendingSource {
    type Subscription = PendingSubscription;

    fn subscribe_order_book(
        &self,
        _listing: VenueListingKey,
        _depth: u16,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        std::future::pending()
    }
}

impl OrderBookEventSubscription for PendingSubscription {
    fn metadata(&self) -> OrderBookMetadata {
        metadata()
    }

    fn snapshot(&self) -> Option<OrderBookSnapshot> {
        None
    }

    fn synchronize(&mut self) -> impl Future<Output = Result<(), OrderBookStreamError>> + Send {
        std::future::pending()
    }

    fn recv(&mut self) -> impl Future<Output = Option<Result<(), OrderBookStreamError>>> + Send {
        std::future::pending()
    }
}

// -------------------------------------------------------------------- directory

/// A directory a test drives: the scopes it has loaded and the catalogue they
/// answer from.
#[derive(Clone)]
struct FixtureDirectory {
    catalog: Arc<Catalog>,
    loaded: Rc<RefCell<Vec<VenueRoute>>>,
    depth: Option<u16>,
}

impl FixtureDirectory {
    fn new(catalog: Arc<Catalog>, loaded: bool, depth: Option<u16>) -> Self {
        let loaded = if loaded { vec![route()] } else { Vec::new() };
        Self {
            catalog,
            loaded: Rc::new(RefCell::new(loaded)),
            depth,
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
                capabilities: vec![CapabilityId::OrderBook],
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

    fn order_book_depth(&self, _route: &VenueRoute) -> Option<u16> {
        self.depth
    }

    fn display(&self) -> DisplaySnapshot {
        self.catalog.cached_display()
    }
}

// ---------------------------------------------------------------------- harness

/// A pane under test: the framework's headless host, the widget built over the
/// fixture directory, and the box its body and header are built into.
struct Fixture {
    harness: WidgetTestApp,
    widget: OrderBookWidget,
    directory: FixtureDirectory,
    status: WidgetStatus,
    page: NodeId,
    left: NodeId,
    right: NodeId,
    publishes: Rc<Cell<usize>>,
}

impl Fixture {
    fn new(config: OrderBookConfig, entries: Vec<CatalogEntry>, loaded: bool) -> Self {
        Self::with_depth(config, entries, loaded, Some(FIXTURE_DEPTH))
    }

    fn with_depth(
        config: OrderBookConfig,
        entries: Vec<CatalogEntry>,
        loaded: bool,
        depth: Option<u16>,
    ) -> Self {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let fixture = test_support::Fixture::new(entries.clone());
        if !entries.is_empty() {
            test_support::publish_fixture_route(fixture.catalog(), &route(), entries)
                .expect("the fixture route should publish its catalogue");
        }
        let catalog = Arc::clone(fixture.catalog_arc());
        let directory = FixtureDirectory::new(Arc::clone(&catalog), loaded, depth);
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let page = leaf(&mut harness, "TEST_ORDER_BOOK_PAGE", PAGE);
        let left = leaf(&mut harness, "TEST_ORDER_BOOK_HEADER_LEFT", (320.0, 32.0));
        let right = leaf(&mut harness, "TEST_ORDER_BOOK_HEADER_RIGHT", (320.0, 32.0));
        let publishes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counted = Rc::clone(&publishes);
        let change = WidgetConfigChange::new(move |_| {
            counted.set(counted.get() + 1);
        });
        let directory_handle: Rc<dyn MarketDirectory> = Rc::new(directory.clone());
        let widget = harness.with_cx(|cx| {
            OrderBookWidget::with_source(
                config,
                PendingSource,
                directory_handle,
                status,
                change,
                recovery(),
                cx,
            )
        });
        let mut fixture = Self {
            harness,
            widget,
            directory,
            status,
            page,
            left,
            right,
            publishes,
        };
        fixture.load();
        fixture
    }

    fn with_cx<R>(&mut self, body: impl FnOnce(&mut OrderBookWidget, &mut WidgetCx<'_>) -> R) -> R {
        let widget = &mut self.widget;
        self.harness.with_cx(|cx| body(widget, cx))
    }

    fn load(&mut self) {
        self.with_cx(|widget, cx| widget.load(cx));
    }

    fn sync(&mut self) {
        self.with_cx(|widget, cx| {
            widget.sync(cx).expect("the widget syncs");
        });
    }

    fn body(&mut self) {
        let parent = self.page;
        self.with_cx(|widget, cx| {
            widget.body(parent, cx).expect("the body builds");
        });
    }

    fn headers(&mut self) {
        let (left, right) = (self.left, self.right);
        self.with_cx(|widget, cx| {
            widget
                .header(HeaderGroup::Left, left, cx)
                .expect("the left group builds");
            widget
                .header(HeaderGroup::Right, right, cx)
                .expect("the right group builds");
        });
    }

    fn drain(&mut self) {
        self.harness.drain();
    }

    fn status(&self) -> WidgetState {
        self.status.state(self.harness.app())
    }

    fn app(&self) -> &App {
        self.harness.app()
    }

    fn tree(&self) -> &ui_core::node::tree::NodeTree {
        self.harness.tree()
    }

    fn key(&self) -> OrderBookStreamKey {
        OrderBookStreamKey::new(
            listing("BTC"),
            self.widget.config.price_multiplier,
            self.widget.config.window_size,
        )
    }

    fn apply_latest(&mut self, snapshot: OrderBookSnapshot) {
        let key = self.key();
        self.apply_latest_for(key, snapshot);
    }

    fn apply_latest_for(&mut self, key: OrderBookStreamKey, snapshot: OrderBookSnapshot) {
        let message = OrderBookStreamMessage::Snapshot(snapshot);
        self.with_cx(|widget, cx| widget.apply_latest(key, &message, cx));
    }

    fn apply_reliable(&mut self, status: OrderBookStatus) {
        let key = self.key();
        let message = OrderBookStreamMessage::Status(status);
        self.with_cx(|widget, cx| widget.apply_reliable(key, &message, cx));
    }

    fn apply_rpc(&mut self, request_id: RequestId, result: WidgetRpcResult<OrderBookProtocol>) {
        self.with_cx(|widget, cx| widget.apply_rpc(request_id, result, cx));
    }

    /// Accepts a book and reports the ready revision that describes it, in the
    /// order the two channels can deliver them.
    fn accept(&mut self, snapshot: OrderBookSnapshot) {
        let sequence = snapshot.sequence.clone();
        self.apply_latest(snapshot);
        self.apply_reliable(OrderBookStatus::Ready { revision: sequence });
    }

    /// One production-shaped round: the widget synchronizes, then the frame runs.
    fn frame(&mut self) {
        self.sync();
        self.drain();
    }

    /// Builds the body, lets the layout settle, then draws one frame.
    fn draw(&mut self) {
        self.body();
        self.drain();
        self.frame();
        self.frame();
    }

    /// The text one column of one mounted ladder row states.
    fn row_text(&self, side: RowSide, row: usize, column: usize) -> String {
        let list = match side {
            RowSide::Ask => self.widget.ask_list.as_ref(),
            RowSide::Bid => self.widget.bid_list.as_ref(),
        }
        .expect("the ladder list is built");
        let content = list
            .row_content_node(self.app(), row)
            .expect("the row is mounted");
        let container = self
            .tree()
            .get(content)
            .expect("the row content is alive")
            .children()
            .first()
            .copied()
            .expect("the row builds a container");
        let cell = self
            .tree()
            .get(container)
            .expect("the row container is alive")
            .children()
            .get(column)
            .copied()
            .expect("the row has its three cells");
        self.harness.text_of(cell)
    }

    fn node(&self, id: &str) -> Option<NodeId> {
        self.harness.find_by_debug_prefix(self.page, id)
    }

    fn text(&self, id: &str) -> String {
        self.harness
            .text_of(self.node(id).unwrap_or_else(|| panic!("`{id}` is built")))
    }

    fn write_header_request(&mut self, request: HeaderRequest) {
        let signal = self.widget.header_requests;
        self.harness.with_cx(|cx| cx.write(&signal, Some(request)));
    }

    fn write_selection(&mut self, selection: InstrumentSelection) {
        let signal = self.widget.selection_requests;
        self.harness
            .with_cx(|cx| cx.write(&signal, Some(selection)));
    }
}

// ----------------------------------------------------------------- status & waits

/// A pane over the fixture route's listing, loaded or not.
fn pane(loaded: bool) -> Fixture {
    Fixture::new(config(Some("BTC")), vec![default_entry()], loaded)
}

/// A box the pane's body or a header group is built into.
fn leaf(harness: &mut WidgetTestApp, id: &'static str, size: (f32, f32)) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id(id)
            .style(
                StyleBuilder::new()
                    .width(size.0)
                    .height(size.1)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the fixture box builds")
    })
}

#[test]
fn a_widget_without_a_book_should_report_connecting_and_have_no_content() {
    let fixture = pane(true);

    assert_eq!(fixture.status(), WidgetState::Connecting);
    assert!(!fixture.widget.has_content());
    assert_eq!(
        fixture.widget.config(),
        Some(&config(Some("BTC"))),
        "the pane persists the scope and the drawing choices"
    );
}

#[test]
fn an_unloaded_market_should_leave_the_pane_waiting() {
    let fixture = pane(false);

    assert!(
        matches!(fixture.status(), WidgetState::Waiting(_)),
        "a market that was never loaded is a wait, not a failure"
    );
    assert!(!fixture.widget.has_content());
}

#[test]
fn loading_the_venue_should_let_the_waiting_pane_start() {
    let mut fixture = pane(false);
    let generation = fixture.harness.services().catalog_generation;
    fixture.directory.load(route());
    let next = *fixture.app().read(&generation) + 1;
    assert!(fixture.harness.app_mut().write(&generation, next));

    fixture.sync();

    assert!(
        !matches!(fixture.status(), WidgetState::Waiting(_)),
        "the pane stops waiting once its market is in memory"
    );
    assert_eq!(fixture.status(), WidgetState::Connecting);
}

#[test]
fn a_widget_without_a_selection_should_ask_for_one() {
    let fixture = Fixture::new(config(None), vec![default_entry()], true);

    assert_eq!(
        fixture.status(),
        WidgetState::waiting(Notice::message(
            "widget.order_book.select_instrument_prompt"
        ))
    );
    assert!(!fixture.widget.has_content());
}

#[test]
fn a_selection_missing_from_the_catalog_should_be_cleared() {
    let mut fixture = Fixture::new(config(Some("BTC")), vec![eth_entry()], true);

    fixture.load();

    assert_eq!(
        fixture
            .widget
            .config()
            .and_then(|config| config.selection.clone()),
        None
    );
    assert_eq!(fixture.publishes.get(), 1, "the reset is written back");
    assert_eq!(
        fixture.status(),
        WidgetState::waiting(Notice::message(
            "widget.order_book.select_instrument_prompt"
        ))
    );
}

#[test]
fn a_catalog_lookup_failure_should_preserve_the_selection() {
    let mut fixture = pane(true);
    let failing = FailingDirectory::new(no_answer());
    fixture
        .harness
        .with_cx(|_cx| fixture.widget.directory = Some(Rc::new(failing)));
    fixture.load();

    assert_eq!(
        fixture
            .widget
            .config()
            .and_then(|config| config.selection.clone()),
        Some(selection("BTC")),
        "a question that could not be put proves nothing about the listing"
    );
    assert_eq!(fixture.publishes.get(), 0);
}

/// A directory whose catalogue cannot answer, which proves nothing.
struct FailingDirectory {
    failure: CatalogError,
    catalog: Arc<Catalog>,
}

impl FailingDirectory {
    fn new(failure: CatalogError) -> Self {
        let fixture = test_support::Fixture::new(vec![default_entry()]);
        Self {
            failure,
            catalog: Arc::clone(fixture.catalog_arc()),
        }
    }
}

impl MarketDirectory for FailingDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        vec![LoadedScope {
            route: route(),
            capabilities: vec![CapabilityId::OrderBook],
            provides_catalog: true,
        }]
        .into()
    }

    fn any_market_loaded(&self) -> bool {
        true
    }

    fn is_loaded(&self, _route: &VenueRoute) -> bool {
        true
    }

    fn contains(&self, _listing: &VenueListingKey) -> Result<bool, CatalogError> {
        Err(self.failure.clone())
    }

    fn listing(&self, _listing: &VenueListingKey) -> Option<Arc<CatalogEntry>> {
        None
    }

    fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog)
    }

    fn order_book_depth(&self, _route: &VenueRoute) -> Option<u16> {
        Some(FIXTURE_DEPTH)
    }

    fn display(&self) -> DisplaySnapshot {
        self.catalog.cached_display()
    }
}

// ---------------------------------------------------------------------- content

#[test]
fn an_accepted_book_should_become_content_and_report_ready() {
    let mut fixture = pane(true);
    assert!(!fixture.widget.has_content());

    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));

    assert!(fixture.widget.has_content());
    assert_eq!(fixture.status(), WidgetState::Ready);
}

#[test]
fn an_empty_book_should_be_content_and_report_nothing() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[], &[]));

    assert!(
        fixture.widget.has_content(),
        "a venue that published nothing is still a published book"
    );
    assert_eq!(fixture.status(), WidgetState::Ready);
}

#[test]
fn a_failure_with_content_should_keep_the_ladder() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));

    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::MarketParticipant(disconnect()),
    ));

    assert!(
        fixture.widget.has_content(),
        "the book the user is reading stays"
    );
    assert!(fixture.status().is_failed());
}

#[test]
fn a_failure_without_content_should_be_reported_as_a_failure() {
    let mut fixture = pane(true);
    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::SubscriptionClosed,
    ));

    assert!(!fixture.widget.has_content());
    assert!(fixture.status().is_failed());
    assert!(!fixture.status().is_busy(), "the failure is terminal");
}

#[test]
fn a_late_book_after_the_market_went_away_should_not_repaint_over_the_notice() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.directory.unload(&route());
    fixture.load();
    assert!(
        !fixture.widget.has_content(),
        "the notice is what is on screen"
    );

    fixture.accept(snapshot(2, &[("102", "1")], &[("99", "1")]));

    assert!(
        !fixture.widget.has_content(),
        "a book for the market that went away is not the notice's to show"
    );
    assert!(matches!(fixture.status(), WidgetState::Waiting(_)));
}

#[test]
fn a_book_the_stream_has_overtaken_should_not_roll_the_ladder_back() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(9, &[("101", "1")], &[("100", "1")]));

    fixture.apply_latest(snapshot(3, &[], &[]));

    assert_eq!(
        fixture
            .widget
            .snapshot
            .as_ref()
            .map(|book| book.sequence.clone()),
        Some(MarketSequence::Numeric(9)),
        "an older book was overtaken by the one on screen"
    );
}

#[test]
fn a_ready_status_that_overtakes_its_book_should_wait_for_it() {
    let mut fixture = pane(true);
    fixture.apply_reliable(OrderBookStatus::Ready {
        revision: MarketSequence::Numeric(5),
    });

    assert_eq!(fixture.status(), WidgetState::Loading);
    assert!(!fixture.widget.has_content());
}

#[test]
fn a_book_after_a_failure_should_not_change_the_status_or_the_ladder() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::SubscriptionClosed,
    ));
    let failed = fixture.status();
    let sequence = fixture
        .widget
        .snapshot
        .as_ref()
        .map(|book| book.sequence.clone());

    fixture.accept(snapshot(2, &[("102", "1")], &[("99", "1")]));

    assert_eq!(fixture.status(), failed, "the failure is terminal");
    assert_eq!(
        fixture
            .widget
            .snapshot
            .as_ref()
            .map(|book| book.sequence.clone()),
        sequence,
        "a book that arrives after the failure is not drawn"
    );
}

#[test]
fn a_book_for_another_scope_should_be_ignored() {
    let mut fixture = pane(true);
    let other = OrderBookStreamKey::new(
        listing("ETH"),
        fixture.widget.config.price_multiplier,
        fixture.widget.config.window_size,
    );

    fixture.apply_latest_for(other, snapshot(1, &[("101", "1")], &[("100", "1")]));

    assert!(!fixture.widget.has_content());
    assert!(
        fixture
            .harness
            .app()
            .read(&fixture.widget.book)
            .book
            .is_none(),
        "a message for another scope publishes no book"
    );
}

#[test]
fn an_explicit_reload_should_start_over_from_connecting() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));

    fixture.load();

    assert_eq!(fixture.status(), WidgetState::Connecting);
    assert!(
        !fixture.widget.has_content(),
        "an explicit load cannot vouch for what it was showing"
    );
}

#[test]
fn a_retry_after_a_failure_should_keep_the_ladder() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::SubscriptionClosed,
    ));

    fixture.load();

    assert_eq!(fixture.status(), WidgetState::Connecting);
    assert!(
        fixture.widget.has_content(),
        "the book the user is still reading survives the retry"
    );
    assert_eq!(
        fixture.widget.pending.dropped(),
        0,
        "the retry is the command in flight"
    );
}

// --------------------------------------------------------------------- directory

#[test]
fn a_directory_generation_change_should_reload_the_book() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    let generation = fixture.harness.services().catalog_generation;
    let next = *fixture.app().read(&generation) + 1;
    assert!(fixture.harness.app_mut().write(&generation, next));

    fixture.sync();

    assert_eq!(
        fixture.status(),
        WidgetState::Connecting,
        "the changed generation starts the book over"
    );
    assert_eq!(fixture.widget.seen_generation, next);
    assert_eq!(fixture.widget.config.price_multiplier, 1);
    assert!(
        !fixture.widget.pending.is_empty(),
        "the load that reopens the stream is in flight"
    );
}

#[test]
fn an_unchanged_directory_generation_should_not_reload_the_book() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));

    fixture.sync();

    assert!(!fixture.widget.market_reloaded);
}

#[test]
fn a_venue_that_is_not_loaded_should_not_ask_for_the_book() {
    let mut fixture = pane(true);
    fixture.directory.unload(&route());
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.load();

    assert!(!fixture.widget.has_content());
    assert!(matches!(fixture.status(), WidgetState::Waiting(_)));
}

#[test]
fn a_stream_that_reports_its_catalog_as_not_loaded_should_leave_the_pane_waiting() {
    let mut fixture = pane(true);
    let not_loaded =
        MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded { route: route() }));

    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::MarketParticipant(not_loaded),
    ));

    assert!(
        matches!(fixture.status(), WidgetState::Waiting(_)),
        "a catalog that is not in memory is a wait, not a failure"
    );
    assert!(!fixture.status().is_failed());
}

#[test]
fn a_stream_that_reports_its_venue_as_not_loaded_should_leave_the_pane_waiting() {
    let mut fixture = pane(true);
    let unloaded = MarketError::from(
        market_participants_connector::error::MarketParticipantError::VenueNotLoaded {
            venue: route().venue,
            scope: route().scope,
        },
    );
    assert!(unloaded.is_venue_not_loaded());

    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::MarketParticipant(unloaded),
    ));

    assert!(matches!(fixture.status(), WidgetState::Waiting(_)));
    assert!(!fixture.status().is_failed());
}

#[test]
fn a_directory_generation_should_not_revive_a_failed_pane() {
    let mut fixture = pane(true);
    fixture.apply_reliable(OrderBookStatus::Error(
        OrderBookStreamError::SubscriptionClosed,
    ));
    assert!(fixture.status().is_failed());
    let generation = fixture.harness.services().catalog_generation;
    let next = *fixture.app().read(&generation) + 1;
    assert!(fixture.harness.app_mut().write(&generation, next));

    fixture.sync();

    assert!(
        fixture.status().is_failed(),
        "only the user's recovery leaves the terminal state"
    );
}

// --------------------------------------------------------------- request identity

/// The directory withdraws a stored listing only on an answer that proves it gone.
#[test]
fn a_listing_its_route_still_publishes_should_be_offered() {
    let fixture = test_support::Fixture::new(vec![default_entry()]);
    test_support::publish_fixture_route(fixture.catalog(), &route(), vec![default_entry()])
        .expect("the fixture route publishes its catalogue");
    let directory = FixtureDirectory::new(Arc::clone(fixture.catalog_arc()), true, None);
    let routes = directory.loaded_scopes();

    assert_eq!(
        listing_support(&listing("BTC"), Ok(true), &routes, &directory),
        ListingSupport::Offered
    );
}

#[test]
fn a_listing_the_catalog_does_not_hold_should_be_withdrawn() {
    let fixture = test_support::Fixture::new(vec![default_entry()]);
    test_support::publish_fixture_route(fixture.catalog(), &route(), vec![default_entry()])
        .expect("the fixture route publishes its catalogue");
    let directory = FixtureDirectory::new(Arc::clone(fixture.catalog_arc()), true, None);
    let routes = directory.loaded_scopes();

    assert_eq!(
        listing_support(&listing("ETH"), Ok(false), &routes, &directory),
        ListingSupport::Withdrawn
    );
}

#[test]
fn a_listing_in_a_terminal_state_should_be_withdrawn() {
    let delisted = test_support::with_status(default_entry(), ListingStatus::Delisted);
    let fixture = test_support::Fixture::new(vec![delisted.clone()]);
    test_support::publish_fixture_route(fixture.catalog(), &route(), vec![delisted])
        .expect("the fixture route publishes its catalogue");
    let directory = FixtureDirectory::new(Arc::clone(fixture.catalog_arc()), true, None);
    let routes = directory.loaded_scopes();

    assert_eq!(
        listing_support(&listing("BTC"), Ok(true), &routes, &directory),
        ListingSupport::Withdrawn
    );
}

#[test]
fn a_route_that_left_the_loaded_venues_should_withdraw_the_stored_listing() {
    let fixture = test_support::Fixture::new(vec![default_entry()]);
    let directory = FixtureDirectory::new(Arc::clone(fixture.catalog_arc()), false, None);

    assert_eq!(
        listing_support(&listing("BTC"), Ok(true), &[], &directory),
        ListingSupport::Withdrawn
    );
}

#[test]
fn a_catalog_that_did_not_answer_should_leave_the_stored_listing_alone() {
    let fixture = test_support::Fixture::new(vec![default_entry()]);
    let directory = FixtureDirectory::new(Arc::clone(fixture.catalog_arc()), true, None);
    let routes = directory.loaded_scopes();

    assert_eq!(
        listing_support(&listing("BTC"), Err(no_answer()), &routes, &directory),
        ListingSupport::Unknown,
        "a question that could not be put proves nothing about the listing"
    );
}
#[test]
fn the_answer_to_a_replaced_command_should_be_dropped() {
    let mut fixture = Fixture::new(
        config(Some("BTC")),
        vec![default_entry(), eth_entry()],
        true,
    );
    // The first command: the load the fixture ran on construction.
    let replaced = RequestId::new(1);
    fixture.write_selection(selection("ETH"));
    fixture.sync();
    // The replacement is the new command; the old identity now belongs to nobody.
    assert_eq!(fixture.widget.pending.dropped(), 0);

    fixture.apply_rpc(
        replaced,
        Err(RpcCallError::ServiceClosed {
            request_id: replaced,
        }),
    );

    assert!(
        !fixture.status().is_failed(),
        "the old answer is not the new command's"
    );
    assert_eq!(fixture.widget.pending.dropped(), 1);
}

#[test]
fn the_answer_to_the_command_in_flight_should_be_reported() {
    let mut fixture = pane(true);
    let in_flight = RequestId::new(1);
    fixture.apply_rpc(
        in_flight,
        Err(RpcCallError::ServiceClosed {
            request_id: in_flight,
        }),
    );

    assert!(fixture.status().is_failed());
    assert_eq!(fixture.widget.pending.dropped(), 0);
}

#[test]
fn a_confirmed_selection_should_reach_the_next_sync() {
    let mut fixture = Fixture::new(
        config(Some("BTC")),
        vec![default_entry(), eth_entry()],
        true,
    );
    let signal = fixture.widget.selection_requests;
    let window = fixture.harness.window();
    let confirmed = selection("ETH");

    hand_over_selection(signal, confirmed.clone(), fixture.harness.app_mut(), window);

    assert_eq!(
        *fixture.harness.app().read(&signal),
        Some(confirmed.clone()),
        "the control's handler hands the choice over through the signal"
    );
    fixture.sync();
    assert_eq!(fixture.widget.config.selection, Some(confirmed));
    assert_eq!(
        fixture.publishes.get(),
        1,
        "the confirmed choice is written back"
    );
}

#[test]
fn selecting_the_scope_that_is_already_shown_should_not_restart_the_book() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    let sequence = fixture
        .widget
        .snapshot
        .as_ref()
        .map(|book| book.sequence.clone());

    fixture.write_selection(selection("BTC"));
    fixture.sync();

    assert_eq!(
        fixture.publishes.get(),
        0,
        "nothing was configured differently"
    );
    assert_eq!(
        fixture
            .widget
            .snapshot
            .as_ref()
            .map(|book| book.sequence.clone()),
        sequence,
        "the book on screen is still being read"
    );
    assert_eq!(
        fixture.widget.config.selection.as_ref(),
        Some(&selection("BTC"))
    );
}

// ---------------------------------------------------------------------- grouping

#[test]
fn changing_the_grouping_should_regroup_the_book_without_dropping_it() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(
        1,
        &[("101", "1"), ("109", "2"), ("110", "4")],
        &[("100", "1")],
    ));
    let before = fixture
        .widget
        .grouped
        .as_ref()
        .expect("the book is grouped")
        .asks
        .len();

    fixture.write_header_request(HeaderRequest::PriceStep(10));
    fixture.sync();

    let book = fixture.widget.grouped.as_ref().expect("the book stays");
    assert_eq!(book.price_tick, tick("1"));
    assert!(
        book.asks.len() < before,
        "the same levels grouped to a wider step are fewer rows"
    );
    assert!(
        fixture.widget.snapshot.is_some(),
        "nothing on screen was dropped"
    );
    assert_eq!(fixture.widget.config.price_multiplier, 10);
    assert_eq!(fixture.publishes.get(), 1, "the choice is written back");
}

#[test]
fn changing_the_row_limit_should_regroup_the_book() {
    let mut fixture = pane(true);
    let asks = (1..=30)
        .map(|value| (value.to_string(), "1".to_owned()))
        .collect::<Vec<_>>();
    let asks = asks
        .iter()
        .map(|(price, quantity)| (price.as_str(), quantity.as_str()))
        .collect::<Vec<_>>();
    fixture.accept(snapshot(1, &asks, &[]));

    fixture.write_header_request(HeaderRequest::WindowSize(50));
    fixture.sync();

    assert_eq!(fixture.widget.config.window_size, 50);
    assert_eq!(
        fixture
            .widget
            .grouped
            .as_ref()
            .map_or(0, |book| book.asks.len()),
        30,
        "the wider window keeps every level the venue published"
    );
}

#[test]
fn the_laid_out_height_should_limit_the_grouped_rows() {
    let mut fixture = Fixture::new(config(Some("BTC")), vec![default_entry()], true);
    let asks = (1..=60)
        .map(|value| (value.to_string(), "1".to_owned()))
        .collect::<Vec<_>>();
    let asks = asks
        .iter()
        .map(|(price, quantity)| (price.as_str(), quantity.as_str()))
        .collect::<Vec<_>>();
    fixture.accept(snapshot(1, &asks, &[]));
    fixture.body();
    fixture.drain();

    // The page is 360 px tall; the two ladders split what the headers and the
    // mid-price row leave, so each side fits far fewer than the 25 rows asked for.
    fixture.frame();
    fixture.frame();

    let drawable = fixture.widget.drawable_rows;
    assert!(
        drawable > 0 && drawable < 25,
        "a 360 px page fits {} rows",
        drawable
    );
    assert_eq!(
        fixture
            .widget
            .grouped
            .as_ref()
            .map_or(0, |book| book.asks.len()),
        drawable,
        "the grouping keeps the rows the pane can draw"
    );
}

#[test]
fn a_row_limit_the_route_cannot_fill_should_be_settled_before_the_stream_opens() {
    let shallow = Fixture::with_depth(
        OrderBookConfig {
            window_size: WINDOW_SIZES[2],
            ..config(Some("BTC"))
        },
        vec![default_entry()],
        true,
        Some(20),
    );

    assert_eq!(
        shallow.widget.config.window_size,
        crate::model::SHALLOW_WINDOW_SIZES[2],
        "a shallow route offers the shallow ladder's default"
    );
    assert_eq!(shallow.publishes.get(), 1);
}

#[test]
fn a_supported_choice_should_survive_settling() {
    let mut config = OrderBookConfig {
        price_multiplier: PRICE_MULTIPLIERS[4],
        window_size: WINDOW_SIZES[2],
        ..OrderBookConfig::default()
    };

    assert!(!config.settle(Some(FIXTURE_DEPTH)));
    assert_eq!(config.price_multiplier, PRICE_MULTIPLIERS[4]);
    assert_eq!(config.window_size, WINDOW_SIZES[2]);
}

// ------------------------------------------------------------------- animation

#[test]
fn reduce_motion_should_put_the_bars_where_they_belong() {
    let mut fixture = pane(true);
    fixture.body();
    fixture.drain();
    let signal = fixture.harness.services().reduce_motion;
    fixture.harness.app_mut().write(&signal, true);

    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.accept(snapshot(2, &[("101", "3")], &[("100", "3")]));

    assert_eq!(
        *fixture.harness.app().read(&fixture.widget.depth_progress),
        1.0
    );
    assert!(
        fixture
            .widget
            .depth_ticker
            .as_ref()
            .is_none_or(|state| !state.ticking.get()),
        "nothing is left to animate"
    );
}

#[test]
fn an_imperceptible_movement_should_not_start_an_animation() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));

    // The second book is the same drawing: no bars moved, so no timer is started.
    fixture.accept(snapshot(2, &[("101", "1")], &[("100", "1")]));

    assert!(fixture.widget.depth_ticker.is_none());
    assert!(!depth_is_active(
        *fixture.harness.app().read(&fixture.widget.depth_progress),
        0.0
    ));
}

#[test]
fn the_depth_animation_should_advance_on_a_tick_and_stop_when_it_arrives() {
    let mut fixture = pane(true);
    fixture.body();
    fixture.drain();
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.accept(snapshot(2, &[("101", "4")], &[("100", "4")]));
    let ticking = || {
        fixture
            .widget
            .depth_ticker
            .as_ref()
            .is_some_and(|state| state.ticking.get())
    };
    assert!(ticking(), "the movement started a timer");
    let start = *fixture.harness.app().read(&fixture.widget.depth_progress);
    assert_eq!(start, 0.0);

    let mut ticks = 0;
    while ticking() && ticks < 200 {
        fixture.harness.advance(DEPTH_ANIMATION_TICK);
        ticks += 1;
    }

    assert!(
        ticks > 0 && ticks < 200,
        "the chain settled after {ticks} ticks"
    );
    assert_eq!(
        *fixture.harness.app().read(&fixture.widget.depth_progress),
        1.0
    );
    assert!(
        !depth_is_active(1.0, *fixture.harness.app().read(&fixture.widget.depth_span)),
        "a settled movement asks for no more frames"
    );

    let settled = fixture.harness.frame_requests();
    fixture.harness.advance(Duration::from_millis(100));
    assert_eq!(fixture.harness.frame_requests(), settled);
}

#[test]
fn the_mid_price_should_take_the_direction_it_moved_in() {
    assert_eq!(
        mid_direction_of(Some(price("101")), Some(price("100"))),
        MidDirection::Up
    );
    assert_eq!(
        mid_direction_of(Some(price("100")), Some(price("101"))),
        MidDirection::Down
    );
    assert_eq!(
        mid_direction_of(Some(price("100")), Some(price("100"))),
        MidDirection::Flat
    );
    assert_eq!(
        mid_direction_of(Some(price("100")), None),
        MidDirection::Flat
    );
    assert_eq!(
        mid_direction_of(None, Some(price("100"))),
        MidDirection::Flat
    );
}

// ------------------------------------------------------------------------ body

/// A book that has not moved draws no frame: the rows, the bars and the lists all
/// write only what changed (I3).
#[test]
fn a_steady_book_should_not_ask_for_a_frame() {
    let mut fixture = pane(true);
    let reduce_motion = fixture.harness.services().reduce_motion;
    fixture.harness.app_mut().write(&reduce_motion, true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.draw();
    let requests = fixture.harness.frame_requests();

    fixture.frame();
    fixture.frame();

    assert_eq!(
        fixture.harness.frame_requests(),
        requests,
        "a steady ladder writes nothing and schedules no frame"
    );
}

#[test]
fn the_body_should_draw_the_ladder_it_was_given() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "7"), ("102", "3")], &[("100", "2")]));
    fixture.draw();

    assert!(fixture.node("TEST_ORDER_BOOK_LADDER").is_some());
    assert_eq!(fixture.row_text(RowSide::Ask, 0, 0), "101");
    assert_eq!(fixture.row_text(RowSide::Ask, 0, 1), "7");
    assert_eq!(fixture.row_text(RowSide::Ask, 0, 2), "7");
    assert_eq!(fixture.row_text(RowSide::Ask, 1, 0), "102");
    assert_eq!(fixture.row_text(RowSide::Ask, 1, 2), "10");
    assert_eq!(fixture.row_text(RowSide::Bid, 0, 0), "100");
    assert_eq!(
        fixture.text("TEST_ORDER_BOOK_MID_PRICE"),
        "101",
        "the mid price is rounded to the venue's own tick"
    );
    assert_eq!(fixture.text("TEST_ORDER_BOOK_SPREAD"), "Spread 1");
    assert_eq!(fixture.text("TEST_ORDER_BOOK_ASK_IMBALANCE"), "0.83");
    assert_eq!(fixture.text("TEST_ORDER_BOOK_BID_IMBALANCE"), "0.17");

    for selector in [
        ASK_BARS_SELECTOR,
        BID_BARS_SELECTOR,
        crate::paint::IMBALANCE_SELECTOR,
    ] {
        let node = fixture
            .harness
            .find_by_debug_prefix(fixture.page, selector)
            .unwrap_or_else(|| panic!("`{selector}` is drawn"));
        assert!(fixture.harness.has_canvas(node), "`{selector}` is a canvas");
    }
    assert!(fixture.widget.ask_list.is_some());
    assert!(fixture.widget.bid_list.is_some());
}

#[test]
fn an_empty_book_should_say_so_inside_the_ladder() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[], &[]));
    fixture.draw();

    let overlay = fixture
        .node("TEST_ORDER_BOOK_EMPTY_OVERLAY")
        .expect("the empty state is built");
    assert!(
        fixture.harness.visible(overlay),
        "an empty book says it is empty"
    );
    assert!(
        fixture
            .text("TEST_ORDER_BOOK_EMPTY_OVERLAY")
            .contains("No data")
    );
}

#[test]
fn a_book_with_levels_should_hide_the_empty_state() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.draw();

    let overlay = fixture
        .node("TEST_ORDER_BOOK_EMPTY_OVERLAY")
        .expect("the empty state is built");
    assert!(!fixture.harness.visible(overlay));
}

#[test]
fn the_ladder_should_name_the_published_codes() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.draw();

    assert_eq!(
        fixture.text("TEST_ORDER_BOOK_COLUMN_PRICE"),
        "Price (USDT)",
        "the price column is named by the code the generation published"
    );
    assert_eq!(
        fixture.text("TEST_ORDER_BOOK_COLUMN_AMOUNT"),
        "Amount (BTC)"
    );
    assert_eq!(fixture.text("TEST_ORDER_BOOK_COLUMN_TOTAL"), "Total (BTC)");
}

#[test]
fn an_unpublished_asset_should_be_named_as_unknown() {
    let mut fixture = pane(true);
    let unpublished = test_support::token(test_support::VENUE, "NOWHERE");
    let metadata = OrderBookMetadata {
        quote_asset: unpublished.clone(),
        quantity_unit: QuantityUnit::AssetUnits {
            asset: unpublished.clone(),
        },
        ..metadata()
    };
    fixture.accept(snapshot_with_metadata(
        1,
        &[("101", "1")],
        &[("100", "1")],
        metadata,
    ));
    fixture.draw();

    assert_eq!(
        fixture.text("TEST_ORDER_BOOK_COLUMN_PRICE"),
        "Price (Unknown asset)",
        "an asset the generation does not name is the localized unknown"
    );
    let identity = unpublished.to_string();
    assert!(
        !fixture
            .text("TEST_ORDER_BOOK_COLUMN_PRICE")
            .contains(&identity),
        "a canonical identity is not a name"
    );
}

#[test]
fn a_language_change_should_relabel_the_panes_own_copy() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.draw();

    fixture.harness.switch_language(LanguageCode::ZhCn);
    fixture.frame();
    fixture.frame();

    assert!(
        fixture
            .text("TEST_ORDER_BOOK_COLUMN_PRICE")
            .contains("USDT"),
        "the unit stays the published code"
    );
    assert_ne!(
        fixture.text("TEST_ORDER_BOOK_COLUMN_PRICE"),
        "Price (USDT)",
        "the sentence itself is read in the language showing now"
    );
    assert_eq!(
        *fixture.harness.app().read(&fixture.widget.empty_text),
        "暂无数据"
    );
}

// ----------------------------------------------------------------------- header

#[test]
fn the_header_should_offer_the_instrument_control_and_the_drawing_controls() {
    let mut fixture = pane(true);
    fixture.headers();

    let keys = [
        INSTRUMENT_KEY_SUFFIX,
        PRICE_STEP_KEY_SUFFIX,
        WINDOW_SIZE_KEY_SUFFIX,
    ];
    for suffix in keys {
        let key = fixture.harness.with_cx(|cx| cx.unique_key(suffix));
        let action = if suffix == INSTRUMENT_KEY_SUFFIX {
            Action::Custom(format_smolstr!("{key}.click"))
        } else {
            Action::Custom(format_smolstr!("{key}.trigger"))
        };
        assert!(
            fixture.harness.app().has_action_handler(&action),
            "the header control registers its handler: {suffix}"
        );
    }
}

#[test]
fn two_panes_should_not_share_a_single_component_key() {
    let mut fixture = pane(true);
    fixture.headers();
    let first_key = fixture
        .harness
        .with_cx(|cx| cx.unique_key(INSTRUMENT_KEY_SUFFIX));
    let first_action = Action::Custom(format_smolstr!("{first_key}.click"));
    let first_node = fixture
        .widget
        .header_left
        .as_ref()
        .expect("the first pane built its picker")
        .control
        .node();
    assert!(fixture.harness.app().has_action_handler(&first_action));

    // The second pane: one more pane of the same widget over the same directory, with
    // its own signal scope — what `AnyWidget::mount` mints for each pane.
    let right = fixture.right;
    let (peer, peer_key) = {
        let services = Rc::clone(fixture.harness.services_rc());
        let window = fixture.harness.window();
        let scope = fixture.harness.app_mut().signal_scope();
        let directory: Rc<dyn MarketDirectory> = Rc::new(fixture.directory.clone());
        let status = WidgetStatus::new(fixture.harness.app_mut(), scope);
        let mut cx = WidgetCx::new(fixture.harness.app_mut(), window, &services, scope);
        let key = cx.unique_key(INSTRUMENT_KEY_SUFFIX);
        let mut peer = OrderBookWidget::with_source(
            config(Some("ETH")),
            PendingSource,
            directory,
            status,
            WidgetConfigChange::new(|_| {}),
            WidgetAction::new("common.retry", |_| {}),
            &mut cx,
        );
        peer.header(HeaderGroup::Left, right, &mut cx)
            .expect("the second pane's picker builds");
        (peer, key)
    };
    let peer_action = Action::Custom(format_smolstr!("{peer_key}.click"));
    let peer_node = peer
        .header_left
        .as_ref()
        .expect("the second pane built its picker")
        .control
        .node();

    assert_ne!(
        first_key, peer_key,
        "each pane mints its own component identity"
    );
    assert_ne!(first_node, peer_node, "and builds its own control");
    assert!(
        fixture.harness.app().has_action_handler(&peer_action),
        "the second pane's control is live beside the first"
    );

    // Releasing the first pane takes down only its own control: the second pane's
    // handler and node stay standing.
    fixture.with_cx(|widget, cx| widget.release(cx));
    assert!(
        !fixture.harness.app().has_action_handler(&first_action),
        "the released pane takes its own component down"
    );
    assert!(
        fixture.harness.app().has_action_handler(&peer_action),
        "and leaves the other pane's component alone"
    );
    assert!(
        fixture.harness.tree().get(peer_node).is_ok(),
        "the other pane's node is untouched"
    );
}

#[test]
fn a_rebuilt_header_control_should_not_leak_its_component_scope() {
    let mut fixture = pane(true);
    fixture.headers();
    // One frame first, so the controls the first build minted are behind us.
    fixture.frame();
    let before = fixture.harness.signal_slot_stats().live;

    // Two language switches rebuild both groups in place; each rebuild releases the
    // component it replaces.
    for language in [LanguageCode::ZhCn, LanguageCode::En] {
        fixture.harness.switch_language(language);
        fixture.frame();
        fixture.frame();
    }

    let after = fixture.harness.signal_slot_stats().live;
    assert!(
        after <= before + SELECT_RELEASE_GAP,
        "a rebuilt header keeps its signal slots bounded: {before} -> {after}"
    );
}

/// The value slots two language switches leave behind, which is the kit's own known
/// gap rather than this widget's.
///
/// `kit::select` mints a signal scope of its own (three values per picker) and its
/// `release` does not hand that scope back yet: two switches rebuild two pickers
/// each, so twelve values stay. `InstrumentButton`'s component scope is released
/// with its handle. Registered with the kit batch (`test-ledger/w4-order-book.md`
/// §10), and the assertion above pins the count so a widget-side leak still fails.
const SELECT_RELEASE_GAP: usize = 3 * 2 * 2;

// ---------------------------------------------------------------------- release

#[test]
fn releasing_the_pane_should_reclaim_its_value_slots_and_canvases() {
    let mut fixture = pane(true);
    fixture.accept(snapshot(1, &[("101", "1")], &[("100", "1")]));
    fixture.draw();
    fixture.headers();
    let mounted = fixture.harness.signal_slot_stats().live;
    let nodes = fixture.harness.node_count();
    assert!(mounted > 0);

    let page = fixture.page;
    fixture.with_cx(|widget, cx| widget.release(cx));

    assert!(
        fixture.harness.signal_slot_stats().live < mounted,
        "the pane hands its value slots back"
    );
    assert_eq!(
        fixture.harness.node_count(),
        nodes,
        "the widget's own release does not remove the surface's nodes"
    );
    assert!(fixture.widget.ask_list.is_none());
    assert!(fixture.widget.bid_list.is_none());
    assert!(fixture.widget.depth_ticker.is_none());
    let _ = page;
}

// ------------------------------------------------------------------------ config

#[test]
fn a_config_should_round_trip_through_its_persisted_form() {
    let config = OrderBookConfig {
        price_multiplier: PRICE_MULTIPLIERS[3],
        window_size: WINDOW_SIZES[1],
        ..config(Some("BTC"))
    };
    let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
    let decoded = envelope.decode::<OrderBookConfig>();

    assert_eq!(decoded, config);
}

#[test]
fn an_unselected_config_should_round_trip_through_its_persisted_form() {
    let config = OrderBookConfig::default();
    let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
    let decoded = envelope.decode::<OrderBookConfig>();

    assert_eq!(decoded.selection, None);
    assert_eq!(decoded, config);
}

#[test]
fn an_old_payload_should_decode_to_the_defaults() {
    let stored = r#"{"schema_version":1,"payload":"{\"selection\":{\"route\":{\"venue\":\"binance\",\"market\":\"spot\"},\"instrument\":{\"kind\":\"spot\"}},\"price_multiplier\":10,\"window_size\":50}"}"#;
    let envelope: VersionedWidgetConfig =
        serde_json::from_str(stored).expect("the stored envelope should parse");

    let decoded = envelope.decode::<OrderBookConfig>();

    assert_eq!(decoded, OrderBookConfig::default());
}

#[test]
fn an_unsupported_grouping_should_reset_to_the_default() {
    let mut config = OrderBookConfig {
        price_multiplier: 7,
        ..OrderBookConfig::default()
    };

    assert!(
        config.settle(None),
        "a replaced choice is reported as a change"
    );
    assert_eq!(
        config.price_multiplier,
        OrderBookConfig::default().price_multiplier
    );
}

#[test]
fn a_row_limit_should_survive_settling_without_a_route() {
    let mut config = OrderBookConfig {
        window_size: crate::model::SHALLOW_WINDOW_SIZES[0],
        ..OrderBookConfig::default()
    };

    assert!(!config.settle(None));
    assert_eq!(config.window_size, crate::model::SHALLOW_WINDOW_SIZES[0]);
}

// ------------------------------------------------------------------------ mount

#[test]
fn a_host_without_a_directory_should_wait_for_its_market() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let pane = harness.mount::<OrderBookWidget>(config(Some("BTC")));

    assert!(matches!(
        pane.status().state(harness.app()),
        WidgetState::Waiting(_)
    ));
    assert_eq!(pane.widget_id(), OrderBookWidget::ID);
    let _ = harness.tree();
}

#[test]
fn a_widget_should_keep_its_identity_and_persisted_options() {
    assert_eq!(OrderBookWidget::ID.as_str(), "kytrix.order_book");
    assert_eq!(OrderBookWidget::CATEGORY, WidgetCategory::Market);
    assert_eq!(
        <OrderBookConfig as WidgetConfig>::SCHEMA_VERSION,
        WidgetConfigSchemaVersion::V1
    );
    assert!(
        OrderBookWidget::DESCRIPTION.is_some_and(|key| key.starts_with("widget.descriptions."))
    );
}
