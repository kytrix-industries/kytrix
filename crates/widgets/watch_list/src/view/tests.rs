//! Interface tests for the watch-list widget.
//!
//! Every test drives the widget through the frozen contract: the headless host
//! ([`WidgetTestApp`]), a fixture directory standing in for the injected connector
//! (the headless host cannot build one), a fake ticker source, and the widget's own
//! lifecycle hooks. The service, the streams and the frames are driven by the
//! host's runtime turns, never by sleeping.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ahash::{HashMap, HashSet};
use base_ui::theme::components::InteractiveState;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
    catalog::{batch::CatalogEntry, error::CatalogError, store::Catalog, test_support},
    market::error::MarketError,
    routing::LONGBRIDGE_US_SCOPE,
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity},
    identifier::VenueId,
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::PriceBasis,
    },
    market::{
        price::MarketPrice,
        selection::{InstrumentSelection, ListingTicker},
        units::{MarketQuantity, QuantityUnit},
    },
    routing::CapabilityId,
};
use serde_json::json;
use smol_str::SmolStr;
use tokio::sync::{Mutex, mpsc};
use ui_core::node::{builder::NodeBuilder, id::NodeId};
use widget::{
    config::WidgetConfigChange,
    protocol::RequestId,
    rpc::RpcCallError,
    status::{WidgetAction, WidgetState},
    test_support::WidgetTestApp,
    venue::VenueDisplay,
};

use super::*;
use crate::{
    directory::WatchListDirectory,
    model::{WatchListEntry, WatchListStreamEvent},
    service::{TickerEventSubscription, TickerSource},
    table::MISSING_VALUE,
};

// ------------------------------------------------------------------- vocabulary

/// The translator the product ships, in the language a test asks for.
fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn route() -> VenueRoute {
    test_support::spot_route()
}

fn stock_route() -> VenueRoute {
    test_support::route("longbridge", LONGBRIDGE_US_SCOPE)
}

// The native symbol behind every listing key a test builds, so a catalogue entry
// can be published for a key a test only names by its key: a `VenueListingKey`
// carries the canonical identity of the listing, not the venue-native symbol.
thread_local! {
    static SYMBOLS: RefCell<HashMap<SmolStr, SmolStr>> = RefCell::new(HashMap::default());
}

fn register(listing: VenueListingKey, symbol: &str) -> VenueListingKey {
    SYMBOLS.with(|symbols| {
        symbols.borrow_mut().insert(
            SmolStr::new(listing.listing_id.as_str()),
            SmolStr::new(symbol),
        );
    });
    listing
}

fn symbol_of(listing: &VenueListingKey) -> SmolStr {
    let id = listing.listing_id.as_str();
    SYMBOLS
        .with(|symbols| symbols.borrow().get(id.as_str()).cloned())
        .unwrap_or_else(|| SmolStr::new(id.as_str()))
}

fn key(symbol: &str) -> VenueListingKey {
    register(test_support::listing_key(&route(), symbol), symbol)
}

fn stock(symbol: &str) -> VenueListingKey {
    register(test_support::listing_key(&stock_route(), symbol), symbol)
}

fn config(keys: Vec<VenueListingKey>) -> WatchListConfig {
    WatchListConfig { entries: keys }
}

fn precision() -> ListingPrecision {
    ListingPrecision {
        price_tick: "0.01".parse().expect("a price tick should parse"),
        quantity_tick: "0.001".parse().expect("a quantity step should parse"),
    }
}

fn quote_asset(_listing: &VenueListingKey) -> AssetId {
    test_support::token("binance", "USDT")
}

fn base_asset(_listing: &VenueListingKey) -> AssetId {
    test_support::token("binance", "ETH")
}

fn price(listing: &VenueListingKey, value: u64) -> MarketPrice {
    MarketPrice {
        value: Price::from(value),
        quote_asset: quote_asset(listing),
        basis: PriceBasis::PerAssetUnit,
    }
}

fn ticker(listing: &VenueListingKey, open: u64, close: u64) -> ListingTicker {
    ListingTicker {
        listing: listing.clone(),
        monotonic_id: Some(1),
        open: price(listing, open),
        high: Some(price(listing, close)),
        low: Some(price(listing, close)),
        close: price(listing, close),
        traded_quantity: Some(MarketQuantity {
            value: Quantity::from(10_u64),
            unit: QuantityUnit::AssetUnits {
                asset: base_asset(listing),
            },
        }),
        traded_notional: Some(MarketQuantity {
            value: Quantity::from(1_000_u64),
            unit: QuantityUnit::Notional {
                asset: quote_asset(listing),
            },
        }),
    }
}

fn instrument_for(listing: &VenueListingKey) -> market_participants_model::instrument::Instrument {
    if listing.route.venue == VenueId::Longbridge {
        test_support::spot_token("binance", "ETH", "USDT")
    } else {
        test_support::spot_token("binance", "BTC", "USDT")
    }
}

/// A catalogue publishing the listings a test follows.
fn catalog_over(keys: &[VenueListingKey]) -> Arc<Catalog> {
    let fixture = test_support::Fixture::with_source(
        route(),
        "watch-list-continuous",
        test_support::pending(),
    );
    let continuous = entries_for(keys, &route());
    let session = entries_for(keys, &stock_route());
    fixture.register_route(&stock_route(), "watch-list-session", &session);
    test_support::publish_fixture_route(fixture.catalog(), &route(), continuous)
        .expect("the continuous route publishes its listings");
    // The extra route publishes through its own source, which stages the canonical
    // terms of every asset the listings name.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the fixture runtime builds");
    let _ = runtime.block_on(fixture.load_route(&stock_route()));
    Arc::clone(fixture.catalog_arc())
}

fn entries_for(keys: &[VenueListingKey], route: &VenueRoute) -> Vec<CatalogEntry> {
    keys.iter()
        .filter(|listing| &listing.route == route)
        .map(|listing| {
            let symbol = symbol_of(listing);
            test_support::entry(route, instrument_for(listing), symbol.as_str())
        })
        .collect()
}

// -------------------------------------------------------------------- directory

/// A directory a test drives: the routes it has loaded, the ones that answer with
/// tickers, and the catalogue a stream resolves its listings against.
#[derive(Clone)]
struct FixtureDirectory {
    loaded: Rc<RefCell<Vec<VenueRoute>>>,
    ticker_routes: Rc<RefCell<HashSet<VenueRoute>>>,
    catalog: Arc<Catalog>,
}

impl FixtureDirectory {
    fn new(catalog: Arc<Catalog>, loaded: Vec<VenueRoute>) -> Self {
        let ticker_routes = loaded.iter().cloned().collect();
        Self {
            loaded: Rc::new(RefCell::new(loaded)),
            ticker_routes: Rc::new(RefCell::new(ticker_routes)),
            catalog,
        }
    }

    fn load(&self, route: VenueRoute) {
        let mut loaded = self.loaded.borrow_mut();
        if !loaded.contains(&route) {
            loaded.push(route.clone());
        }
        self.ticker_routes.borrow_mut().insert(route);
    }

    fn unload(&self, route: &VenueRoute) {
        self.loaded.borrow_mut().retain(|held| held != route);
        self.ticker_routes.borrow_mut().retain(|held| held != route);
    }

    /// Withdraws the ticker capability from a route's loaded scope without
    /// unloading it: a scope that is loaded and answers no tickers is the proof a
    /// stored listing left the list.
    fn revoke_tickers(&self, route: &VenueRoute) {
        self.ticker_routes.borrow_mut().remove(route);
    }

    fn capabilities(&self, route: &VenueRoute) -> Vec<CapabilityId> {
        let mut capabilities = vec![CapabilityId::MarketCatalog];
        if self.ticker_routes.borrow().contains(route) {
            capabilities.push(CapabilityId::Tickers);
        }
        capabilities
    }
}

impl WatchListDirectory for FixtureDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        self.loaded
            .borrow()
            .iter()
            .map(|route| LoadedScope {
                route: route.clone(),
                capabilities: self.capabilities(route),
                provides_catalog: true,
            })
            .collect()
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

    fn serves_tickers(&self, route: &VenueRoute) -> bool {
        self.ticker_routes.borrow().contains(route)
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

// ----------------------------------------------------------------------- source

type TickerEvent = Result<Arc<ListingTicker>, MarketError>;
type TickerSender = mpsc::UnboundedSender<TickerEvent>;

struct FakeSubscription {
    events: mpsc::UnboundedReceiver<TickerEvent>,
    dropped: Arc<AtomicUsize>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

impl TickerEventSubscription for FakeSubscription {
    fn precision(&self) -> ListingPrecision {
        precision()
    }

    async fn recv(&mut self) -> Option<TickerEvent> {
        self.events.recv().await
    }
}

/// A ticker source whose subscriptions a test hands out and whose opening fetches
/// are the tickers the test published.
///
/// The service resolves the opening snapshot through whichever reports first, so a
/// source that already has a subscription in hand and a fetch that answers at once
/// is a stream that reaches `Ready` on its own; a test that wants to drive one
/// listing further keeps the sender and pushes messages through it.
#[derive(Clone, Default)]
struct FakeSource {
    fetched: Arc<Mutex<HashMap<VenueListingKey, ListingTicker>>>,
    subscriptions: Arc<Mutex<HashMap<VenueListingKey, FakeSubscription>>>,
    /// One live sender per generation started for a listing: replacing a stream must
    /// not close the subscription the previous one is still reading (a closed channel
    /// is a disconnected stream, which the widget would report as a failure).
    senders: Arc<Mutex<HashMap<VenueListingKey, Vec<TickerSender>>>>,
    plans: Arc<Mutex<HashMap<VenueListingKey, VecDeque<Result<(), MarketError>>>>>,
    pending_fetches: Arc<Mutex<HashSet<VenueListingKey>>>,
    subscriptions_made: Arc<AtomicUsize>,
    fetched_calls: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl FakeSource {
    fn new() -> Self {
        Self::default()
    }

    /// Publishes a ticker and the subscription that will carry the listing's live
    /// messages.
    fn quote(&self, listing: &VenueListingKey, ticker: ListingTicker) -> TickerSender {
        self.fetched.blocking_lock().insert(listing.clone(), ticker);
        let (sender, receiver) = mpsc::unbounded_channel();
        self.subscriptions.blocking_lock().insert(
            listing.clone(),
            FakeSubscription {
                events: receiver,
                dropped: Arc::clone(&self.dropped),
            },
        );
        self.senders
            .blocking_lock()
            .entry(listing.clone())
            .or_default()
            .push(sender.clone());
        sender
    }

    /// Makes the next opening fetch of `listing` wait forever: the stream stays at
    /// `LoadingSnapshot` with whatever the row showed before.
    fn pend_fetch(&self, listing: &VenueListingKey) {
        self.pending_fetches.blocking_lock().insert(listing.clone());
    }

    /// Hands out one more subscription for `listing`, over a live channel.
    fn rearm(&self, listing: &VenueListingKey, ticker: ListingTicker) -> TickerSender {
        self.pending_fetches.blocking_lock().remove(listing);
        self.plans
            .blocking_lock()
            .entry(listing.clone())
            .or_default()
            .push_back(Ok(()));
        self.quote(listing, ticker)
    }

    fn send(&self, listing: &VenueListingKey, event: TickerEvent) {
        if let Some(sender) = self
            .senders
            .blocking_lock()
            .get(listing)
            .and_then(|senders| senders.last())
        {
            let _ = sender.send(event);
        }
    }

    fn subscriptions_made(&self) -> usize {
        self.subscriptions_made.load(Ordering::Acquire)
    }
}

impl TickerSource for FakeSource {
    type Subscription = FakeSubscription;

    async fn fetch_ticker(&self, listing: VenueListingKey) -> Result<ListingTicker, MarketError> {
        self.fetched_calls.fetch_add(1, Ordering::AcqRel);
        if self.pending_fetches.lock().await.contains(&listing) {
            std::future::pending().await
        }
        self.fetched
            .lock()
            .await
            .get(&listing)
            .cloned()
            .ok_or(MarketError::ServiceClosed)
    }

    async fn subscribe_ticker(
        &self,
        listing: VenueListingKey,
    ) -> Result<Self::Subscription, MarketError> {
        self.subscriptions_made.fetch_add(1, Ordering::AcqRel);
        let plan = self
            .plans
            .lock()
            .await
            .get_mut(&listing)
            .and_then(VecDeque::pop_front);
        match plan {
            Some(Err(error)) => Err(error),
            _ => self
                .subscriptions
                .lock()
                .await
                .remove(&listing)
                .ok_or(MarketError::ServiceClosed),
        }
    }
}

// ---------------------------------------------------------------------- harness

struct Pane {
    widget: WatchListWidget,
    parent: NodeId,
    header_parent: NodeId,
}

/// One widget under test: the host, the fixture directory, the source, and the
/// counters the assertions read.
struct Fixture {
    harness: WidgetTestApp,
    pane: Pane,
    directory: FixtureDirectory,
    source: FakeSource,
    config_changes: Rc<Cell<usize>>,
}

impl Fixture {
    /// A pane over `config`, with every route of the fixture loaded.
    fn new(config: WatchListConfig) -> Self {
        Self::with(config, vec![route()])
    }

    /// A pane over `config`, with `loaded` routes loaded in its directory.
    fn with(config: WatchListConfig, loaded: Vec<VenueRoute>) -> Self {
        Self::with_source(FakeSource::new(), config, loaded)
    }

    fn with_source(source: FakeSource, config: WatchListConfig, loaded: Vec<VenueRoute>) -> Self {
        let catalog = catalog_over(&config.entries);
        let directory = FixtureDirectory::new(Arc::clone(&catalog), loaded);
        Self::assemble(source, config, directory)
    }

    fn assemble(source: FakeSource, config: WatchListConfig, directory: FixtureDirectory) -> Self {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let status = harness.status();
        let parent = harness.placeholder_parent();
        let root = harness.root();
        let header_parent = harness.with_cx(|cx| {
            NodeBuilder::boxed()
                .id("TEST_HEADER_HOST")
                .build(cx.tree_mut(), root)
                .expect("the header host is alive")
        });
        let config_changes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let changes = Rc::clone(&config_changes);
        let config_change = WidgetConfigChange::new(move |_| changes.set(changes.get() + 1));
        let recovery = WidgetAction::new("common.retry", |_| {});
        let directory_handle: Rc<dyn WatchListDirectory> = Rc::new(directory.clone());
        let widget = harness.with_cx(|cx| {
            WatchListWidget::with_source(
                config,
                source.clone(),
                directory_handle,
                status,
                config_change,
                recovery,
                cx,
            )
        });
        Self {
            harness,
            pane: Pane {
                widget,
                parent,
                header_parent,
            },
            directory,
            source,
            config_changes,
        }
    }

    fn with_cx<R>(&mut self, body: impl FnOnce(&mut WatchListWidget, &mut WidgetCx<'_>) -> R) -> R {
        let widget = &mut self.pane.widget;
        self.harness.with_cx(|cx| body(widget, cx))
    }

    fn load(&mut self) {
        self.with_cx(|widget, cx| widget.load(cx));
    }

    fn sync(&mut self) {
        self.with_cx(|widget, cx| widget.sync(cx).expect("the widget syncs"));
    }

    fn build_body(&mut self) {
        let parent = self.pane.parent;
        self.with_cx(|widget, cx| widget.body(parent, cx).expect("the body builds"));
    }

    fn build_header(&mut self) {
        let parent = self.pane.header_parent;
        self.with_cx(|widget, cx| {
            widget
                .header(HeaderGroup::Left, parent, cx)
                .expect("the header builds")
        });
    }

    fn frame(&mut self) {
        self.sync();
        self.harness.drain();
        self.harness.drain();
    }

    /// Loads, settles the service, builds the body and runs a frame.
    fn start(&mut self) {
        self.load();
        self.settle();
        self.build_body();
        self.sync();
        self.harness.drain();
    }

    /// Drives runtime turns and frames until the widget's rows are current.
    fn settle(&mut self) {
        for _ in 0..32 {
            self.sync();
            self.harness.run_until_idle();
            self.harness.drain();
        }
        self.sync();
        self.harness.drain();
    }

    /// Drives runtime turns and frames until `ready` holds.
    fn drive_until(&mut self, ready: impl Fn(&Self) -> bool) {
        for _ in 0..40 {
            if ready(self) {
                return;
            }
            self.sync();
            self.harness.run_until_idle();
            self.harness.drain();
        }
        assert!(ready(self), "the pane did not reach the expected state");
    }

    fn status(&self) -> WidgetState {
        self.pane.widget.status.state(self.harness.app())
    }

    fn rows(&self, market: WatchListMarket) -> usize {
        let pane = match market {
            WatchListMarket::Continuous => self.pane.widget.continuous.as_ref(),
            WatchListMarket::Session => self.pane.widget.session.as_ref(),
        };
        pane.map_or(0, TablePane::len)
    }

    fn table_node(&self, market: WatchListMarket) -> Option<NodeId> {
        let pane = match market {
            WatchListMarket::Continuous => self.pane.widget.continuous.as_ref(),
            WatchListMarket::Session => self.pane.widget.session.as_ref(),
        };
        pane.map(|pane| pane.node)
    }

    fn table_text(&self, market: WatchListMarket) -> String {
        self.table_node(market)
            .map_or_else(String::new, |node| self.harness.text_of(node))
    }

    fn all_text(&self) -> String {
        let body = self.pane.widget.body_frame.expect("the body is built");
        self.harness.text_of(body)
    }

    fn row(&self, market: WatchListMarket, index: usize) -> Arc<WatchListRow> {
        let pane = match market {
            WatchListMarket::Continuous => self.pane.widget.continuous.as_ref(),
            WatchListMarket::Session => self.pane.widget.session.as_ref(),
        };
        pane.expect("the table exists")
            .rows
            .row(index)
            .cloned()
            .expect("the row exists")
    }

    fn table_pane(&self, market: WatchListMarket) -> &TablePane {
        match market {
            WatchListMarket::Continuous => self.pane.widget.continuous.as_ref(),
            WatchListMarket::Session => self.pane.widget.session.as_ref(),
        }
        .expect("the table exists")
    }

    /// Writes the directory generation the way the composition root does.
    fn publish_generation(&mut self, generation: u64) {
        let signal = self.harness.services().catalog_generation;
        self.harness.app_mut().write(&signal, generation);
    }

    fn switch_language(&mut self, language: LanguageCode) {
        self.harness.switch_language(language);
    }

    /// Reports a terminal failure of the pane, as one of its own paths would.
    fn fail(&mut self, failure: WatchListFailure) {
        self.with_cx(|widget, cx| widget.fail(failure, cx));
    }

    fn apply_rpc_result(
        &mut self,
        command: PendingCommand,
        result: WidgetRpcResult<WatchListProtocol>,
    ) {
        self.with_cx(|widget, cx| widget.apply_rpc_result(command, result, cx));
    }
}

fn refused() -> RpcCallError<WatchListServiceError> {
    RpcCallError::ServiceClosed {
        request_id: RequestId::new(1),
    }
}

fn service_refused() -> WidgetRpcResult<WatchListProtocol> {
    Err(refused())
}

// ------------------------------------------------------------------- lifecycle

#[test]
fn a_list_should_report_connecting_and_then_head_its_rows() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.load();
    assert!(matches!(fixture.status(), WidgetState::Connecting));

    fixture.settle();
    fixture.build_body();
    fixture.build_header();
    fixture.frame();

    assert!(
        matches!(fixture.status(), WidgetState::Ready),
        "the accepted set is content"
    );
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains("120.00 USDT"),
        "the price and its asset: {text}"
    );
    assert!(text.contains("20.00"), "the change: {text}");
    assert!(text.contains("20.00%"), "the change percentage: {text}");
}

#[test]
fn an_empty_list_over_a_loaded_market_should_be_ready_content_with_its_empty_table() {
    let mut fixture = Fixture::with(config(Vec::new()), vec![route()]);
    fixture.start();

    assert!(matches!(fixture.status(), WidgetState::Ready));
    assert!(
        fixture.pane.widget.has_content(),
        "the empty table is content"
    );
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 0);
    assert!(
        fixture
            .all_text()
            .contains(t_smol!(fixture.pane.widget.translate, "common.no_data").as_str()),
        "the empty list says it is empty"
    );
    assert!(
        fixture.harness.visible(
            fixture
                .pane
                .widget
                .continuous_slot
                .expect("the continuous frame")
        ),
        "the empty list draws one table"
    );
    assert!(
        !fixture
            .harness
            .visible(fixture.pane.widget.session_slot.expect("the session frame")),
        "and not the session table it has no rows for"
    );
}

#[test]
fn a_list_that_follows_nothing_should_wait_for_its_first_market() {
    let mut fixture = Fixture::with(config(Vec::new()), Vec::new());
    fixture.load();
    fixture.frame();

    assert!(!fixture.pane.widget.has_content());
    match fixture.status() {
        WidgetState::Waiting(notice) => {
            assert!(!notice.resolve(&fixture.pane.widget.translate).is_empty());
        }
        other => panic!("a list with no market waits, got {other:?}"),
    }
    assert_eq!(fixture.source.subscriptions_made(), 0);
}

#[test]
fn loading_the_venue_should_let_the_waiting_list_subscribe() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::with(config(following.clone()), Vec::new());
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 110));
    fixture.load();
    fixture.frame();
    assert!(
        fixture.pane.widget.has_content(),
        "the followed listings are the user list even while no market backs them"
    );
    assert_eq!(fixture.source.subscriptions_made(), 0);

    fixture.directory.load(route());
    fixture.publish_generation(1);
    fixture.settle();
    fixture.build_body();
    fixture.frame();

    assert!(fixture.pane.widget.has_content());
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert_eq!(fixture.source.subscriptions_made(), 1);
    assert!(matches!(fixture.status(), WidgetState::Ready));
}

#[test]
fn a_failed_pane_should_not_ask_the_service_for_anything() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 110));
    fixture.start();
    let subscriptions = fixture.source.subscriptions_made();
    let calls = fixture.source.fetched_calls.load(Ordering::Acquire);

    fixture.fail(WatchListFailure::Capacity);
    fixture.publish_generation(9);
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Failed(_)));
    assert_eq!(
        fixture.source.subscriptions_made(),
        subscriptions,
        "a failed pane starts no streams"
    );
    assert_eq!(
        fixture.source.fetched_calls.load(Ordering::Acquire),
        calls,
        "and asks for no tickers"
    );
}

#[test]
fn a_waiting_pane_should_not_ask_the_service_for_anything() {
    let mut fixture = Fixture::with(config(vec![key("BTCUSDT")]), Vec::new());
    fixture.load();
    fixture.settle();
    assert_eq!(fixture.source.subscriptions_made(), 0);
    assert_eq!(fixture.source.fetched_calls.load(Ordering::Acquire), 0);
}

#[test]
fn an_explicit_reload_should_start_over_and_keep_the_rows() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);

    fixture.load();
    fixture.sync();
    assert_eq!(
        fixture.rows(WatchListMarket::Continuous),
        1,
        "the followed listing is still a row while it starts over"
    );
    assert_eq!(fixture.rows(WatchListMarket::Session), 0);
    assert_eq!(
        fixture.row(WatchListMarket::Continuous, 0).status(),
        WatchListStatus::Connecting,
        "an explicit reload does not vouch for what it was showing"
    );
    assert!(
        fixture
            .row(WatchListMarket::Continuous, 0)
            .display()
            .is_none(),
        "the reload does not vouch for the price it was showing"
    );
}
#[test]
fn a_recovery_should_keep_the_rows_it_had_and_reopen_the_streams() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let subscriptions = fixture.source.subscriptions_made();
    fixture.fail(WatchListFailure::Capacity);

    fixture
        .source
        .rearm(&following[0], ticker(&following[0], 100, 130));
    fixture.load();
    fixture.settle();
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Ready));
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert!(
        fixture.source.subscriptions_made() > subscriptions,
        "a recovery reopens the streams"
    );
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains("130.00 USDT")
    );
}

// ------------------------------------------------------------------ the values

#[test]
fn a_failed_row_should_keep_its_ticker_and_leave_the_widget_alone() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.frame();
    fixture
        .source
        .send(&following[0], Err(MarketError::ServiceClosed));
    fixture.settle();
    fixture.frame();

    assert!(
        !matches!(fixture.status(), WidgetState::Failed(_)),
        "one stream failing is not the pane failing"
    );
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains("120.00 USDT"),
        "the failed row keeps its price: {text}"
    );
    assert!(
        text.contains(
            t_smol!(
                fixture.pane.widget.translate,
                "widget.watch_list.ticker_unavailable"
            )
            .as_str()
        ),
        "and says its ticker is unavailable"
    );
}

#[test]
fn two_listings_of_one_instrument_should_stay_independent() {
    let first = key("BTCUSDT");
    let second = key("BTCUSDTB");
    let mut fixture = Fixture::new(config(vec![first.clone(), second.clone()]));
    fixture.source.quote(&first, ticker(&first, 100, 120));
    fixture.source.quote(&second, ticker(&second, 100, 90));
    fixture.start();
    fixture.drive_until(|fixture| {
        fixture.row(WatchListMarket::Continuous, 0).status() == WatchListStatus::Ready
    });
    fixture.frame();

    assert_eq!(fixture.rows(WatchListMarket::Continuous), 2);
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(text.contains("120.00 USDT"));
    assert!(text.contains("90.00 USDT"));

    fixture.source.send(&first, Err(MarketError::ServiceClosed));
    fixture.settle();
    fixture.frame();
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains("120.00 USDT"),
        "the other row is untouched: {text}"
    );
    assert!(text.contains("90.00 USDT"));
}

#[test]
fn a_row_waiting_for_its_market_should_be_one_cell_where_its_values_would_be() {
    let unloaded = stock("AAPL");
    let mut fixture = Fixture::new(config(vec![key("BTCUSDT"), unloaded.clone()]));
    fixture
        .source
        .quote(&key("BTCUSDT"), ticker(&key("BTCUSDT"), 100, 110));
    fixture.start();
    fixture.frame();

    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert_eq!(fixture.rows(WatchListMarket::Session), 1);
    let session = fixture.table_pane(WatchListMarket::Session);
    let sentence = fixture.harness.text_of(session.node);
    let market_name = unloaded
        .route
        .venue
        .display_string(&fixture.pane.widget.translate);
    assert!(
        sentence.contains(market_name.as_str()),
        "the row names the market to load: {sentence}"
    );
    assert!(
        !sentence.contains("<p") && !sentence.contains("!["),
        "the catalogue's markup is not drawn: {sentence}"
    );
    assert!(
        session
            .table
            .cell_node(fixture.harness.app(), 0, 1)
            .is_some(),
        "the row draws its one cell where its values would be"
    );
    assert!(
        session
            .table
            .cell_node(fixture.harness.app(), 0, 2)
            .is_none(),
        "the covered columns have no cells"
    );
    assert!(
        session
            .table
            .cell_node(fixture.harness.app(), 0, 7)
            .is_some(),
        "the controls keep their own column"
    );
}

#[test]
fn a_listing_should_be_drawn_in_the_table_of_the_market_it_is_quoted_on() {
    let crypto = key("BTCUSDT");
    let stock_listing = stock("AAPL");
    let mut fixture = Fixture::with(
        config(vec![crypto.clone(), stock_listing.clone()]),
        vec![route(), stock_route()],
    );
    fixture.source.quote(&crypto, ticker(&crypto, 100, 120));
    fixture
        .source
        .quote(&stock_listing, ticker(&stock_listing, 10, 11));
    fixture.start();
    fixture.frame();

    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert_eq!(fixture.rows(WatchListMarket::Session), 1);
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains("120.00 USDT")
    );
    assert!(
        fixture
            .table_text(WatchListMarket::Session)
            .contains("11.00 USD")
    );
}

#[test]
fn the_table_should_offer_the_followed_columns() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let translate = fixture.pane.widget.translate.clone();
    let pane = fixture.table_pane(WatchListMarket::Continuous);
    let headers: Vec<String> = pane
        .table
        .columns()
        .iter()
        .map(|column| column.header.to_string())
        .collect();
    assert_eq!(
        headers,
        vec![
            t_smol!(translate, "common.route").to_string(),
            t_smol!(translate, "common.instrument").to_string(),
            t_smol!(translate, "common.price").to_string(),
            t_smol!(translate, "widget.watch_list.change_24h").to_string(),
            t_smol!(translate, "widget.watch_list.change_percent_24h").to_string(),
            t_smol!(translate, "widget.watch_list.traded_quantity_24h").to_string(),
            t_smol!(translate, "widget.watch_list.traded_notional_24h").to_string(),
            String::new(),
        ]
    );
}

#[test]
fn the_two_tables_should_offer_their_own_column_sets() {
    let following = vec![key("BTCUSDT"), stock("AAPL")];
    let mut fixture = Fixture::with(config(following), vec![route(), stock_route()]);
    fixture.start();
    let translate = fixture.pane.widget.translate.clone();
    let continuous = fixture.table_pane(WatchListMarket::Continuous);
    let session = fixture.table_pane(WatchListMarket::Session);
    assert_eq!(
        continuous.table.columns()[3].header,
        t_smol!(translate, "widget.watch_list.change_24h")
    );
    assert_eq!(
        session.table.columns()[3].header,
        t_smol!(translate, "widget.watch_list.us.change")
    );
    assert_eq!(
        continuous.table.columns()[6].header,
        t_smol!(translate, "widget.watch_list.traded_notional_24h")
    );
    assert_eq!(
        session.table.columns()[6].header,
        t_smol!(translate, "widget.watch_list.us.traded_notional")
    );
}

#[test]
fn the_first_column_should_state_the_route_the_row_follows() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following));
    fixture.start();
    let expected = widget::venue::route_display_string(&route(), &fixture.pane.widget.translate);
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains(expected.as_str()),
        "the row states the whole route it follows"
    );
}

#[test]
fn the_traded_columns_should_show_the_unit_their_listing_publishes() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(text.contains("BTC"), "the traded quantity's unit: {text}");
    assert!(text.contains("USDT"), "the traded notional's asset: {text}");
}

#[test]
fn the_traded_columns_should_show_missing_for_the_unit_not_published() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    let mut only_quantity = ticker(&following[0], 100, 120);
    only_quantity.traded_notional = None;
    fixture.source.quote(&following[0], only_quantity);
    fixture.start();
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains(MISSING_VALUE),
        "the missing value is drawn: {text}"
    );
}

#[test]
fn a_row_should_be_named_by_the_published_instrument() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains("BTC/USDT"),
        "the published name is the whole label: {text}"
    );
}

// ------------------------------------------------------------------- language

#[test]
fn switching_the_language_should_redraw_the_table_copy_and_keep_the_rows() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);

    fixture.switch_language(LanguageCode::ZhCn);
    fixture.sync();
    fixture.harness.drain();

    let translate = fixture.pane.widget.translate.clone();
    let pane = fixture.table_pane(WatchListMarket::Continuous);
    assert_eq!(
        pane.table.columns()[0].header,
        t_smol!(translate, "common.route"),
        "the headers are rebuilt in the language showing"
    );
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains("120.00 USDT"),
        "the row survives the rebuild"
    );
}

#[test]
fn a_row_should_name_the_market_to_load_in_the_language_showing() {
    let unloaded = stock("AAPL");
    let mut fixture = Fixture::new(config(vec![key("BTCUSDT"), unloaded.clone()]));
    fixture.start();
    let english = fixture.table_text(WatchListMarket::Session);
    let english_name = unloaded
        .route
        .venue
        .display_string(&fixture.pane.widget.translate);
    assert!(english.contains(english_name.as_str()));

    fixture.switch_language(LanguageCode::ZhCn);
    fixture.sync();
    fixture.harness.drain();

    let chinese = fixture.table_text(WatchListMarket::Session);
    let chinese_name = unloaded
        .route
        .venue
        .display_string(&fixture.pane.widget.translate);
    assert!(
        chinese.contains(chinese_name.as_str()),
        "the sentence is read again: {chinese}"
    );
    assert_ne!(english, chinese, "the language moved");
}

// ------------------------------------------------------------------- controls

#[test]
fn the_header_should_offer_adding_a_listing() {
    let mut fixture = Fixture::new(config(vec![key("BTCUSDT")]));
    fixture.build_header();
    let header = fixture
        .pane
        .widget
        .header
        .as_ref()
        .expect("the header control");
    assert_eq!(header.button.a11y_label_text(), Some("Add instrument"));
    assert!(fixture.harness.visible(header.node), "the control is drawn");
}

#[test]
fn the_add_control_should_refuse_input_while_the_list_is_waiting() {
    let mut fixture = Fixture::with(config(vec![key("BTCUSDT")]), Vec::new());
    fixture.load();
    fixture.build_header();
    fixture.frame();
    let header = fixture
        .pane
        .widget
        .header
        .as_ref()
        .expect("the header control");
    assert_eq!(
        header.button.interactive().state(fixture.harness.app()),
        InteractiveState::Disabled,
        "a pane waiting for its market offers nothing to add from"
    );
}

#[test]
fn the_add_control_should_be_available_while_the_list_is_reading() {
    let mut fixture = Fixture::new(config(vec![key("BTCUSDT")]));
    let key = fixture.pane.widget.config.entries[0].clone();
    fixture.source.quote(&key, ticker(&key, 100, 120));
    fixture.start();
    fixture.build_header();
    fixture.frame();
    let header = fixture
        .pane
        .widget
        .header
        .as_ref()
        .expect("the header control");
    assert_ne!(
        header.button.interactive().state(fixture.harness.app()),
        InteractiveState::Disabled,
        "a reading pane can add a listing"
    );
}

/// Cancels the bridge, so the tests below drive the request/answer path by hand.
fn quiet(fixture: &mut Fixture) {
    if let Some(binding) = fixture.pane.widget.binding.as_mut() {
        binding.cancel();
    }
}

#[test]
fn adding_a_listing_should_show_it_before_the_service_confirms_it() {
    let mut fixture = Fixture::new(config(Vec::new()));
    fixture.start();
    quiet(&mut fixture);
    let added = key("ETHUSDT");
    let requests = fixture.pane.widget.selection_requests;
    fixture.harness.app_mut().write(
        &requests,
        Some(InstrumentSelection {
            listing: added.clone(),
        }),
    );
    fixture.sync();

    assert!(fixture.pane.widget.config.entries.contains(&added));
    assert!(
        fixture.pane.widget.entries.contains_key(&added),
        "the row is read before the service confirms it"
    );
    assert!(
        !fixture.pane.widget.pending.is_empty(),
        "one operation is in flight"
    );
    assert_eq!(fixture.config_changes.get(), 1, "the change is reported");
    fixture.build_body();
    fixture.frame();
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
}

#[test]
fn adding_a_listing_the_list_already_follows_should_change_nothing() {
    let following = key("BTCUSDT");
    let mut fixture = Fixture::new(config(vec![following.clone()]));
    fixture.start();
    quiet(&mut fixture);
    let requests = fixture.pane.widget.selection_requests;
    fixture.harness.app_mut().write(
        &requests,
        Some(InstrumentSelection {
            listing: following.clone(),
        }),
    );
    fixture.sync();

    assert_eq!(fixture.pane.widget.config.entries, vec![following]);
    assert_eq!(fixture.pane.widget.pending.dropped(), 0);
}

#[test]
fn a_refused_add_should_take_the_row_back() {
    let mut fixture = Fixture::new(config(Vec::new()));
    fixture.start();
    quiet(&mut fixture);
    let added = key("ETHUSDT");
    let requests = fixture.pane.widget.selection_requests;
    fixture.harness.app_mut().write(
        &requests,
        Some(InstrumentSelection {
            listing: added.clone(),
        }),
    );
    fixture.sync();
    assert!(fixture.pane.widget.config.entries.contains(&added));

    fixture.apply_rpc_result(PendingCommand::Add(added.clone()), service_refused());
    fixture.sync();
    fixture.frame();

    assert!(
        !fixture.pane.widget.config.entries.contains(&added),
        "a refused add is undone"
    );
    assert!(!fixture.pane.widget.entries.contains_key(&added));
    assert!(matches!(fixture.status(), WidgetState::Failed(_)));
    assert_eq!(fixture.config_changes.get(), 2, "the undo is reported too");
}

#[test]
fn removing_a_listing_should_hide_it_before_the_service_confirms_it() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    quiet(&mut fixture);
    let requests = fixture.pane.widget.row_requests;
    fixture
        .harness
        .app_mut()
        .write(&requests, Some(RowRequest::Remove(following[0].clone())));
    fixture.sync();
    fixture.frame();

    assert!(fixture.pane.widget.config.entries.is_empty());
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 0);
    assert!(!fixture.pane.widget.pending.is_empty());
}

#[test]
fn a_refused_remove_should_put_the_row_back_with_what_it_showed() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.frame();
    let shown = fixture
        .row(WatchListMarket::Continuous, 0)
        .display()
        .map(|display| display.price.clone());
    quiet(&mut fixture);

    let requests = fixture.pane.widget.row_requests;
    fixture
        .harness
        .app_mut()
        .write(&requests, Some(RowRequest::Remove(following[0].clone())));
    fixture.sync();
    assert!(!fixture.pane.widget.config.entries.contains(&following[0]));

    // The refusal puts the entry the removal took back; the test rebuilds that entry
    // with the ticker the row showed, which is what the pending command held.
    fixture.pane.widget.pending.clear();
    let mut entry = WatchListEntry::connecting(&following[0]);
    entry.apply_event(&WatchListStreamEvent::Snapshot {
        ticker: Arc::new(ticker(&following[0], 100, 120)),
        precision: precision(),
    });
    entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready));
    fixture.apply_rpc_result(
        PendingCommand::Remove(following[0].clone(), Some(Box::new(entry))),
        service_refused(),
    );
    fixture.sync();
    fixture.frame();

    assert!(fixture.pane.widget.config.entries.contains(&following[0]));
    let restored = fixture
        .pane
        .widget
        .entries
        .get(&following[0])
        .expect("the entry is back");
    assert_eq!(
        restored
            .row()
            .display()
            .map(|display| display.price.clone()),
        shown,
        "the row that came back shows what it showed"
    );
}

#[test]
fn a_listing_should_be_removable_while_its_market_is_not_loaded() {
    let unloaded = stock("AAPL");
    let mut fixture = Fixture::with(config(vec![unloaded.clone()]), Vec::new());
    fixture.start();
    quiet(&mut fixture);
    assert!(fixture.pane.widget.awaiting_market);
    assert_eq!(fixture.rows(WatchListMarket::Session), 1);

    let requests = fixture.pane.widget.row_requests;
    fixture
        .harness
        .app_mut()
        .write(&requests, Some(RowRequest::Remove(unloaded.clone())));
    fixture.sync();
    fixture.frame();

    assert!(!fixture.pane.widget.config.entries.contains(&unloaded));
    assert_eq!(fixture.rows(WatchListMarket::Session), 0);
    assert!(
        fixture.pane.widget.pending.is_empty(),
        "a waiting pane sends no request for its own file"
    );
}

#[test]
fn the_row_controls_should_follow_the_row_state() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.drive_until(|fixture| {
        fixture.row(WatchListMarket::Continuous, 0).status() == WatchListStatus::Ready
    });
    fixture.build_body();
    fixture.frame();

    let controls = fixture
        .pane
        .widget
        .continuous_controls
        .get(&0)
        .expect("the row has controls");
    assert!(!controls.retry, "a ready row is not retried");
    assert_eq!(controls.buttons.len(), 1, "only removal is offered");

    fixture
        .source
        .send(&following[0], Err(MarketError::ServiceClosed));
    fixture.settle();
    fixture.frame();
    let controls = fixture
        .pane
        .widget
        .continuous_controls
        .get(&0)
        .expect("the row has controls");
    assert!(controls.retry, "a failed row is retried");
    assert_eq!(controls.buttons.len(), 2, "retry and removal are offered");
}

#[test]
fn retrying_a_failed_row_should_reopen_it_without_forgetting_the_price() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.frame();
    fixture
        .source
        .send(&following[0], Err(MarketError::ServiceClosed));
    fixture.settle();
    fixture.frame();
    assert_eq!(
        fixture.row(WatchListMarket::Continuous, 0).status(),
        WatchListStatus::Error
    );

    // The retried generation subscribes but its opening fetch never answers, so what
    // the row shows between the retry and the new snapshot is observable.
    fixture
        .source
        .rearm(&following[0], ticker(&following[0], 100, 130));
    fixture.source.pend_fetch(&following[0]);
    let requests = fixture.pane.widget.row_requests;
    fixture
        .harness
        .app_mut()
        .write(&requests, Some(RowRequest::Retry(following[0].clone())));
    fixture.sync();
    fixture.settle();
    fixture.frame();

    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    let text = fixture.table_text(WatchListMarket::Continuous);
    assert!(
        text.contains("120.00 USDT"),
        "the retried row keeps the price it showed: {text}"
    );
}

#[test]
fn a_refused_retry_should_restore_the_failure_it_replaced() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture
        .source
        .send(&following[0], Err(MarketError::ServiceClosed));
    fixture.settle();
    quiet(&mut fixture);

    let error = WatchListStreamError::MarketParticipant(MarketError::ServiceClosed);
    fixture.apply_rpc_result(
        PendingCommand::Retry {
            key: following[0].clone(),
            error,
        },
        service_refused(),
    );
    fixture.sync();
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Failed(_)));
    assert_eq!(fixture.pane.widget.pending.dropped(), 0);
}

#[test]
fn the_answer_to_a_replaced_command_should_be_dropped() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    quiet(&mut fixture);
    fixture.with_cx(|widget, cx| {
        widget.apply_rpc(
            RequestId::new(999),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(999),
            }),
            cx,
        );
    });

    assert!(
        !matches!(fixture.status(), WidgetState::Failed(_)),
        "an answer to a command this pane replaced is not its failure"
    );
    assert_eq!(fixture.pane.widget.pending.dropped(), 1);
}

#[test]
fn a_full_list_should_refuse_another_listing() {
    let following: Vec<VenueListingKey> = (0..MAX_WATCH_LIST_ENTRIES)
        .map(|index| key(&format!("SYM{index}")))
        .collect();
    let mut fixture = Fixture::new(config(following));
    fixture.start();
    quiet(&mut fixture);
    let requests = fixture.pane.widget.selection_requests;
    fixture.harness.app_mut().write(
        &requests,
        Some(InstrumentSelection {
            listing: key("EXTRA"),
        }),
    );
    fixture.sync();
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Failed(_)));
    assert_eq!(
        fixture.pane.widget.config.entries.len(),
        MAX_WATCH_LIST_ENTRIES,
        "the refused listing never enters the list"
    );
}

// ------------------------------------------------------------------- terminal

#[test]
fn a_message_that_arrives_after_a_failure_should_be_ignored() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.frame();
    fixture.fail(WatchListFailure::Capacity);
    let before = fixture
        .row(WatchListMarket::Continuous, 0)
        .display()
        .map(|display| display.price.clone());

    fixture
        .source
        .send(&following[0], Ok(Arc::new(ticker(&following[0], 100, 999))));
    fixture.settle();
    fixture.frame();

    let after = fixture
        .row(WatchListMarket::Continuous, 0)
        .display()
        .map(|display| display.price.clone());
    assert_eq!(before, after, "a terminal failure does not take messages");
}

#[test]
fn an_answer_that_arrives_after_a_failure_should_be_ignored() {
    let mut fixture = Fixture::new(config(Vec::new()));
    fixture.start();
    fixture.fail(WatchListFailure::Capacity);
    let failure = match fixture.status() {
        WidgetState::Failed(failure) => failure,
        other => panic!("the pane failed, got {other:?}"),
    };
    let text = failure.text(&fixture.pane.widget.translate);

    fixture.apply_rpc_result(PendingCommand::Load, service_refused());
    fixture.frame();

    match fixture.status() {
        WidgetState::Failed(current) => {
            assert_eq!(current.text(&fixture.pane.widget.translate), text);
        }
        other => panic!("a late answer does not revive a failed pane, got {other:?}"),
    }
}

#[test]
fn an_unchanged_steady_state_should_not_request_frames() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.build_body();
    fixture.frame();

    // Settle every frame the setup asked for; what follows is the steady state.
    fixture.harness.drain();
    fixture.harness.drain();
    let requests = fixture.harness.frame_requests();
    fixture.sync();
    fixture.harness.drain();

    assert_eq!(
        fixture.harness.frame_requests(),
        requests,
        "a steady sync asks for no frame (I3)"
    );
}

// ---------------------------------------------------------------- generation

#[test]
fn a_directory_generation_should_reload_the_pane_and_reopen_the_streams() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let subscriptions = fixture.source.subscriptions_made();
    fixture
        .source
        .rearm(&following[0], ticker(&following[0], 100, 130));

    fixture.publish_generation(1);
    fixture.settle();
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Ready));
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert!(
        fixture.source.subscriptions_made() > subscriptions,
        "the retired generation's streams are reopened"
    );
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains("130.00 USDT")
    );
}

#[test]
fn an_unrelated_generation_should_reload_the_pane() {
    // §8.3 has no per-venue version: every generation write reloads the pane, even a
    // commit on a venue it does not follow. The test pins that accepted trade.
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let subscriptions = fixture.source.subscriptions_made();
    fixture
        .source
        .rearm(&following[0], ticker(&following[0], 100, 121));

    fixture.publish_generation(2);
    fixture.settle();
    fixture.frame();

    assert!(fixture.source.subscriptions_made() > subscriptions);
    assert!(matches!(fixture.status(), WidgetState::Ready));
}

#[test]
fn a_failed_pane_should_ignore_a_venue_generation() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    let subscriptions = fixture.source.subscriptions_made();
    fixture.fail(WatchListFailure::Capacity);

    fixture.publish_generation(3);
    fixture.sync();
    fixture.harness.drain();

    assert!(matches!(fixture.status(), WidgetState::Failed(_)));
    assert_eq!(fixture.source.subscriptions_made(), subscriptions);
}

#[test]
fn a_row_whose_market_has_no_catalog_should_stop_being_read() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    quiet(&mut fixture);
    let error = MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded { route: route() }));
    fixture.with_cx(|widget, cx| {
        widget.apply_event(
            &following[0],
            &WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(error)),
            cx,
        );
    });
    fixture.sync();
    fixture.frame();

    assert!(
        fixture.pane.widget.config.entries.contains(&following[0]),
        "the user still follows the listing"
    );
    assert!(
        fixture.pane.widget.entries.is_empty(),
        "a row nothing can answer for stops being read"
    );
    assert_eq!(fixture.rows(WatchListMarket::Continuous), 1);
    assert_eq!(
        fixture.row(WatchListMarket::Continuous, 0).state(),
        &WatchListRowState::AwaitingMarket,
        "the row says which market has to be loaded"
    );
}

#[test]
fn a_recovery_with_no_market_loaded_should_wait_rather_than_stay_failed() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::with(config(following), Vec::new());
    fixture.start();
    assert!(
        !matches!(fixture.status(), WidgetState::Failed(_)),
        "a pane with rows says what is missing in them"
    );
    fixture.fail(WatchListFailure::Capacity);

    // The recovery asks the gate again; a gate that answers "nothing to read" is a
    // wait, not a failure that nothing clears.
    fixture.load();
    fixture.frame();
    assert!(
        !matches!(fixture.status(), WidgetState::Failed(_)),
        "a recovery with no market loaded waits"
    );
}

#[test]
fn a_market_that_comes_back_should_reopen_the_streams_the_wait_kept() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();

    fixture.directory.unload(&route());
    fixture.publish_generation(4);
    fixture.frame();
    assert_eq!(
        fixture.rows(WatchListMarket::Continuous),
        1,
        "the listing is still the user's row while its market is gone"
    );
    assert_eq!(
        fixture.row(WatchListMarket::Continuous, 0).status(),
        WatchListStatus::Connecting
    );

    fixture.directory.load(route());
    fixture
        .source
        .rearm(&following[0], ticker(&following[0], 100, 125));
    fixture.publish_generation(5);
    fixture.settle();
    fixture.frame();

    assert!(matches!(fixture.status(), WidgetState::Ready));
    assert!(
        fixture
            .table_text(WatchListMarket::Continuous)
            .contains("125.00 USDT")
    );
}

#[test]
fn a_withdrawn_listing_should_leave_the_list() {
    let following = vec![key("BTCUSDT"), key("ETHUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture
        .source
        .quote(&following[1], ticker(&following[1], 100, 120));
    fixture.start();
    let changes = fixture.config_changes.get();

    // Republish the route without the first listing: the catalogue now proves the
    // listing is not offered, which is what withdraws it from the followed set.
    test_support::publish_fixture_route(
        &fixture.directory.catalog,
        &route(),
        vec![test_support::entry(
            &route(),
            instrument_for(&following[1]),
            symbol_of(&following[1]).as_str(),
        )],
    )
    .expect("the route republishes");
    fixture.publish_generation(6);
    fixture.settle();
    fixture.frame();

    assert!(
        !fixture.pane.widget.config.entries.contains(&following[0]),
        "a listing the route no longer publishes leaves the list"
    );
    assert_eq!(
        fixture.pane.widget.config.entries,
        vec![following[1].clone()]
    );
    assert_eq!(fixture.config_changes.get(), changes + 1);
}

#[test]
fn a_route_that_stops_answering_tickers_should_withdraw_its_listings() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.directory.revoke_tickers(&route());

    fixture.publish_generation(7);
    fixture.settle();
    fixture.frame();

    assert!(fixture.pane.widget.config.entries.is_empty());
}

// --------------------------------------------------------------------- config

#[test]
fn the_followed_set_should_survive_a_config_round_trip() {
    let following = vec![key("BTCUSDT"), stock("AAPL")];
    let config = WatchListConfig {
        entries: following.clone(),
    };
    let payload = serde_json::to_value(&config).expect("the config serializes");
    let decoded: WatchListConfig = serde_json::from_value(payload).expect("the config decodes");
    assert_eq!(decoded.entries, following);
}

#[test]
fn the_payload_should_hold_listing_keys_and_no_display_string() {
    let config = WatchListConfig {
        entries: vec![key("BTCUSDT")],
    };
    let payload = serde_json::to_value(&config).expect("the config serializes");
    assert!(payload["entries"].is_array());
    let entry = &payload["entries"][0];
    assert!(
        entry.get("route").is_some(),
        "the listing key is stored whole: {entry}"
    );
    assert!(
        entry.get("price").is_none() && entry.get("display").is_none(),
        "no display string is persisted: {entry}"
    );
}

#[test]
fn a_config_beyond_the_capacity_should_be_reset() {
    let entries: Vec<_> = (0..=MAX_WATCH_LIST_ENTRIES)
        .map(|index| serde_json::to_value(key(&format!("SYM{index}"))).expect("a key serializes"))
        .collect();
    let payload = json!({ "entries": entries });
    assert!(
        serde_json::from_value::<WatchListConfig>(payload).is_err(),
        "a list over the capacity is a configuration error"
    );
}

#[test]
fn a_config_that_follows_one_listing_twice_should_be_reset() {
    let listing = key("BTCUSDT");
    let payload = json!({ "entries": [&listing, &listing] });
    assert!(serde_json::from_value::<WatchListConfig>(payload).is_err());
}

#[test]
fn an_old_payload_that_names_routes_and_instruments_should_be_reset() {
    let payload = json!({
        "entries": [{ "route": "binance/spot", "instrument": "BTC/USDT" }]
    });
    assert!(
        serde_json::from_value::<WatchListConfig>(payload).is_err(),
        "the old payload no longer decodes"
    );
}

// ---------------------------------------------------------------- two panes

#[test]
fn two_panes_should_keep_their_own_row_controls() {
    let listing = key("BTCUSDT");
    let catalog = catalog_over(std::slice::from_ref(&listing));
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let mut panes = Vec::new();
    for _ in 0..2 {
        let source = FakeSource::new();
        source.quote(&listing, ticker(&listing, 100, 120));
        let directory = FixtureDirectory::new(Arc::clone(&catalog), vec![route()]);
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let directory_handle: Rc<dyn WatchListDirectory> = Rc::new(directory);
        let mut widget = {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            WatchListWidget::with_source(
                config(vec![listing.clone()]),
                source.clone(),
                directory_handle,
                status,
                WidgetConfigChange::default(),
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        };
        {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            widget.load(cx);
        }
        panes.push((widget, parent, scope, source));
    }

    for (widget, parent, scope, _) in panes.iter_mut() {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, *scope);
        widget.body(*parent, cx).expect("the body builds");
    }
    for _ in 0..40 {
        for (widget, _, scope, _) in panes.iter_mut() {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, *scope);
            widget.sync(cx).expect("the widget syncs");
        }
        harness.run_until_idle();
        harness.drain();
    }

    let keys: Vec<SmolStr> = panes
        .iter()
        .map(|(widget, _, _, _)| {
            widget
                .continuous_controls
                .get(&0)
                .expect("a control")
                .buttons
                .last()
                .expect("a remove control")
                .key()
                .into()
        })
        .collect();
    assert_ne!(keys[0], keys[1], "each pane mints its own kit key");
    let actions: Vec<_> = panes
        .iter()
        .map(|(widget, _, _, _)| {
            widget
                .continuous_controls
                .get(&0)
                .expect("a control")
                .buttons
                .last()
                .expect("a remove control")
                .action_identity()
                .expect("the control carries its click")
        })
        .collect();
    assert!(harness.app().has_action_handler(&actions[0]));
    assert!(harness.app().has_action_handler(&actions[1]));

    {
        let (widget, _, scope, _) = &mut panes[0];
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, *scope);
        widget.release(cx);
    }
    assert!(
        !harness.app().has_action_handler(&actions[0]),
        "the released pane takes its handler down"
    );
    assert!(
        harness.app().has_action_handler(&actions[1]),
        "and leaves the other pane's handler alone"
    );
}
#[test]
fn releasing_a_pane_should_reclaim_every_value_slot() {
    let listing = key("BTCUSDT");
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let baseline = harness.signal_slot_stats();
    let mut first_slots = None;

    for _ in 0..4 {
        let source = FakeSource::new();
        source.quote(&listing, ticker(&listing, 100, 120));
        let catalog = catalog_over(std::slice::from_ref(&listing));
        let directory = FixtureDirectory::new(catalog, vec![route()]);
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let directory_handle: Rc<dyn WatchListDirectory> = Rc::new(directory);
        let mut widget = {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            WatchListWidget::with_source(
                config(vec![listing.clone()]),
                source,
                directory_handle,
                status,
                WidgetConfigChange::default(),
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        };
        for _ in 0..20 {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            widget.sync(cx).expect("the widget syncs");
            harness.run_until_idle();
            harness.drain();
        }
        {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            widget.body(parent, cx).expect("the body builds");
        }
        {
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let cx = &mut WidgetCx::new(harness.app_mut(), window, &services, scope);
            widget.sync(cx).expect("the widget syncs");
            widget.release(cx);
        }
        // The pane is not a mounted widget, so its node subtree and its signal scope
        // are this test's to hand back.
        let window = harness.window();
        if harness
            .app()
            .window(window)
            .is_some_and(|target| target.tree().get(parent).is_ok())
        {
            let _ = harness.app_mut().remove_node(window, parent);
        }
        harness.app_mut().release_scope(scope);
        let stats = harness.signal_slot_stats();
        if let Some(first) = first_slots {
            assert_eq!(
                stats, first,
                "the next pane reuses the slots the last one handed back"
            );
        } else {
            first_slots = Some(stats);
        }
        assert_eq!(
            stats.live, baseline.live,
            "releasing a pane returns its value slots"
        );
    }

    let after = harness.signal_slot_stats();
    assert_eq!(
        after,
        first_slots.expect("the panes ran"),
        "the slot table stopped growing after the first pane"
    );
}
#[test]
fn releasing_a_pane_twice_should_be_idempotent() {
    let following = vec![key("BTCUSDT")];
    let mut fixture = Fixture::new(config(following.clone()));
    fixture
        .source
        .quote(&following[0], ticker(&following[0], 100, 120));
    fixture.start();
    fixture.with_cx(|widget, cx| widget.release(cx));
    fixture.with_cx(|widget, cx| widget.release(cx));
}
