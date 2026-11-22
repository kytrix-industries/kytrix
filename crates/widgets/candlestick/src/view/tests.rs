//! The pane's own behaviour: what it reports, what the header offers, what the chart draws,
//! and what it leaves behind when it goes.
//!
//! The contract layer is driven directly — `load`, `sync`, `body`, `header`, `dispatch`,
//! `release` are the hooks the framework calls — over the engine's headless host, because
//! these tests are about this pane's decisions rather than about the framework's dispatch.
//! The stream messages a test needs are injected through the pane's `test_inbox` (or through
//! the `apply_*` hooks directly), and the candle source is a fixture rather than a live
//! connection: a headless host has no market directory, so the pane is constructed with
//! [`CandlestickWidget::with_source`] and every transition past `load`'s market gate is
//! driven by its own hook.
//!
//! # What the headless host cannot reach
//!
//! `market_ready` reads `cx.services().market_participants`, and `WidgetTestApp` composes the
//! services with that slot empty (a `MarketParticipantsConnector` needs a
//! the toolkit runtime type, which is not a dependency of this crate). Every
//! behaviour that the directory itself decides — a loaded venue unblocking the pane, a
//! shrinking commit sending it back to waiting, the instrument control opening over loaded
//! routes — is therefore asserted on the branch the pane takes with no directory, and the
//! gaps are named in the delivery report rather than papered over.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use app_config::{time_zone::DataTimeZone, widget::VersionedWidgetConfig};
use base_ui::style_builder::StyleBuilder;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::{
    catalog::{
        batch::CatalogEntry,
        display::catalog::DisplaySnapshot,
        error::{CatalogError, SourceError},
        store::Catalog,
        test_support,
    },
    error::MarketParticipantError,
    market::{domain::DisconnectReason, error::MarketError, timeframe::TimeFrame},
};
use market_participants_model::{
    decimal::{Price, Quantity, Tick},
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::PriceBasis,
    },
    market::{
        book::ListingCandlestick,
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit},
    },
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::{Action, Event, InputEvent, Modifiers, PointerButton},
    math::{Point, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope},
    test_support::record::PrimitiveKind,
};
use widget::{
    binding::Inbound,
    config::WidgetConfigChange,
    cx::WidgetCx,
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    surface::WidgetSurface,
    test_support::{CanvasPrimitive, WidgetTestApp},
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::{
    directory::{listing_answer, proves_absent},
    indicator::IndicatorId,
    model::CandlestickSeries,
    service::{CandlestickEventSubscription, CandlestickPageQuery},
    time::{TIMEFRAMES, remaining_time_label},
};

// -------------------------------------------------------------------- vocabulary

/// The symbol the fixture route publishes the charted listing under.
const SYMBOL: &str = "BTCUSDT";
/// The size the pane's body is laid out in. Everything drawn is measured from it.
const PAGE: (f32, f32) = (640.0, 400.0);
/// A page wide enough that a whole candle buffer fits inside the plot, which is what the
/// prefetch threshold tests need: they are about the buffer's room, not about the window.
const WIDE_PAGE: (f32, f32) = (7_800.0, 400.0);
/// The host window a wide page has to fit in: the root lays its children out inside the
/// window, so a pane wider than the window is clamped to the window's width.
const WINDOW_WIDE: (f32, f32) = (7_900.0, 600.0);

/// The translator the product ships: what these tests assert is the catalogue's copy.
fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn harness() -> WidgetTestApp {
    WidgetTestApp::with_translate(product_translate(LanguageCode::En))
}

fn route() -> VenueRoute {
    test_support::spot_route()
}

fn listing(symbol: &str) -> VenueListingKey {
    test_support::listing_key(&route(), symbol)
}

fn selection(symbol: &str) -> InstrumentSelection {
    InstrumentSelection {
        listing: listing(symbol),
    }
}

fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the fixture venue is canonical")
}

/// A config that already follows the fixture route's published listing.
fn config() -> CandlestickConfig {
    CandlestickConfig {
        selection: Some(selection(SYMBOL)),
        timeframe: TimeFrame::Minute1,
        adjustment: Adjustment::None,
        indicator: CandlestickIndicatorConfig::default(),
    }
}

fn test_precision() -> ListingPrecision {
    ListingPrecision {
        price_tick: Tick::from_str("0.01").expect("the fixture price tick parses"),
        quantity_tick: Tick::from_str("0.001").expect("the fixture quantity tick parses"),
    }
}

fn market_price(close: u64) -> MarketPrice {
    MarketPrice {
        value: Price::from(close),
        quote_asset: test_support::currency("USD"),
        basis: PriceBasis::PerAssetUnit,
    }
}

/// One candle of the fixture listing, with every price at `close`.
fn candle(timestamp: i64, close: u64) -> ListingCandlestick {
    let price = market_price(close);
    ListingCandlestick {
        listing: listing(SYMBOL),
        open: price.clone(),
        high: price.clone(),
        low: price.clone(),
        close: price,
        traded_quantity: Some(MarketQuantity {
            value: Quantity::from(1_u64),
            unit: QuantityUnit::AssetUnits {
                asset: test_support::token(test_support::VENUE, "BTC"),
            },
        }),
        traded_notional: None,
        timestamp,
    }
}

/// The two channels a fixture delivers, in the order an inbox holds them.
fn snapshot_message(snapshot: CandlestickSeriesSnapshot) -> Arc<CandlestickStreamMessage> {
    Arc::new(CandlestickStreamMessage::Snapshot(snapshot))
}

fn status_message(status: CandlestickStatus) -> Arc<CandlestickStreamMessage> {
    Arc::new(CandlestickStreamMessage::Status(status))
}

fn disconnect() -> MarketError {
    MarketError::Disconnected(DisconnectReason::Normal)
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

fn snapshot_of(candles: impl IntoIterator<Item = ListingCandlestick>) -> CandlestickSeriesSnapshot {
    let mut series = CandlestickSeries::new(MAXIMUM_CANDLES, test_precision());
    series.merge_initial(candles);
    series.snapshot()
}

/// A live-edge snapshot of `len` candles whose newest one is at `newest`.
fn live_edge_snapshot(len: usize, newest: i64) -> CandlestickSeriesSnapshot {
    let oldest = newest - i64::try_from(len).expect("the fixture length fits an i64") + 1;
    snapshot_of((oldest..=newest).map(|timestamp| candle(timestamp, 100)))
}

/// A live-edge snapshot of `len` candles whose venue has no more history.
fn exhausted_snapshot(len: usize) -> CandlestickSeriesSnapshot {
    let newest = i64::try_from(len).expect("the fixture length fits an i64");
    let mut series = CandlestickSeries::new(MAXIMUM_CANDLES, test_precision());
    series.merge_initial((1..=newest).map(|timestamp| candle(timestamp, 100)));
    series.mark_older_exhausted();
    series.snapshot()
}

/// A snapshot of a window wheeled away from the live edge: a full buffer with one older
/// candle merged in with [`NewestCandle::Evicted`], which is what leaves the edge.
fn panned_snapshot() -> CandlestickSeriesSnapshot {
    let mut series = CandlestickSeries::new(MAXIMUM_CANDLES, test_precision());
    let newest = i64::try_from(MAXIMUM_CANDLES).expect("the capacity fits an i64");
    series.merge_initial((1..=newest).map(|index| candle(index, 100)));
    series.merge_older(std::iter::once(candle(0, 100)), NewestCandle::Evicted);
    series.snapshot()
}

// ------------------------------------------------------------------------ source

/// The subscription the fixture source opens: it never delivers and never ends.
struct PendingSubscription;

impl CandlestickEventSubscription for PendingSubscription {
    fn precision(&self) -> ListingPrecision {
        test_precision()
    }

    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Option<Result<Arc<ListingCandlestick>, MarketError>>> + Send
    {
        std::future::pending()
    }
}

/// A source that answers what a route publishes and never delivers a candle.
///
/// The pane's stream messages are injected by the test rather than produced by this source,
/// so a test is about what the pane does with a push. A stream that is opened is never
/// answered, which is also what keeps a test from depending on its own timing.
#[derive(Clone)]
struct FixtureSource {
    timeframes: Vec<TimeFrame>,
    adjustments: Vec<Adjustment>,
}

impl FixtureSource {
    fn new(timeframes: Vec<TimeFrame>, adjustments: Vec<Adjustment>) -> Self {
        Self {
            timeframes,
            adjustments,
        }
    }

    /// The source a fixture route with the default scope publishes.
    fn publishing_all_intervals() -> Self {
        Self::new(TIMEFRAMES.to_vec(), vec![Adjustment::None])
    }
}

impl CandlestickSource for FixtureSource {
    type Subscription = PendingSubscription;

    fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
        Ok(self.timeframes.clone())
    }

    fn supported_adjustments(&self, _route: &VenueRoute) -> Result<Vec<Adjustment>, MarketError> {
        Ok(self.adjustments.clone())
    }

    fn fetch_candlesticks(
        &self,
        _query: CandlestickPageQuery,
    ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
    {
        std::future::pending()
    }

    fn subscribe_candlesticks(
        &self,
        _listing: VenueListingKey,
        _timeframe: TimeFrame,
        _adjustment: Adjustment,
    ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send {
        std::future::pending()
    }
}

// ------------------------------------------------------------------------ harness

/// Runs `body` with a [`WidgetCx`] over one pane instance's own scope, which is what a pane
/// hands its widget and what makes `cx.unique_key` per instance.
fn with_scope<R>(
    harness: &mut WidgetTestApp,
    scope: SignalScope,
    body: impl FnOnce(&mut WidgetCx<'_>) -> R,
) -> R {
    let window = harness.window();
    let services = Rc::clone(harness.services_rc());
    let app = harness.app_mut();
    let mut cx = WidgetCx::new(app, window, &services, scope);
    body(&mut cx)
}

/// A fixed-size box the pane's body and its header rows are built into.
/// Resizes the host window, which is the available space the root lays its children out in.
fn resize_window(harness: &mut WidgetTestApp, size: (f32, f32)) {
    let window = harness.window();
    harness
        .app_mut()
        .window_mut(window)
        .expect("the host window exists")
        .set_size(Size::new(size.0, size.1));
    harness.drain();
}

fn page(harness: &mut WidgetTestApp, id: &'static str, size: (f32, f32)) -> NodeId {
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
            .expect("the page builds")
    })
}

/// A header row group, as a pane builds its two groups.
fn header_group(harness: &mut WidgetTestApp, parent: NodeId, id: &'static str) -> NodeId {
    harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .id(id)
            .style(StyleBuilder::new().flex_row().height(32.0).build())
            .build(cx.tree_mut(), parent)
            .expect("the header group builds")
    })
}

/// A pane the framework would have built, driven by a fixture source.
struct Pane {
    widget: CandlestickWidget,
    status: WidgetStatus,
    /// The pane's own signal scope, which the framework releases after `release`.
    scope: SignalScope,
    chart_page: NodeId,
    left: NodeId,
    right: NodeId,
    /// How often the widget wrote its persistent configuration back.
    publishes: Rc<Cell<usize>>,
}

impl Pane {
    /// Every hook a test drives goes through the pane's own scope, as the framework's cx
    /// does for one widget instance.
    fn with<R>(
        &mut self,
        harness: &mut WidgetTestApp,
        body: impl FnOnce(&mut CandlestickWidget, &mut WidgetCx<'_>) -> R,
    ) -> R {
        let widget = &mut self.widget;
        with_scope(harness, self.scope, |cx| body(widget, cx))
    }

    fn load(&mut self, harness: &mut WidgetTestApp) {
        self.with(harness, |widget, cx| widget.load(cx));
    }

    /// Starts the stream the way `load` does once its market gate is past.
    fn start_reading(&mut self, harness: &mut WidgetTestApp) {
        self.with(harness, |widget, cx| {
            widget.start_current(false, false, cx);
        });
    }

    fn sync(&mut self, harness: &mut WidgetTestApp) {
        self.with(harness, |widget, cx| {
            widget.sync(cx).expect("the widget syncs");
        });
    }

    /// Builds the body the framework builds once the pane has content.
    fn body(&mut self, harness: &mut WidgetTestApp) {
        let page = self.chart_page;
        self.with(harness, |widget, cx| {
            widget.body(page, cx).expect("the body builds");
        });
    }

    /// Builds both header groups, as a pane builds its header row.
    fn headers(&mut self, harness: &mut WidgetTestApp) {
        let (left, right) = (self.left, self.right);
        self.with(harness, |widget, cx| {
            widget
                .header(HeaderGroup::Left, left, cx)
                .expect("the left group builds");
            widget
                .header(HeaderGroup::Right, right, cx)
                .expect("the right group builds");
        });
    }

    fn release(&mut self, harness: &mut WidgetTestApp) {
        self.with(harness, |widget, cx| widget.release(cx));
        // The framework drops the widget's scope after the surface's nodes are removed
        // (`AnyWidget::release`); the widget itself never owns the scope's release.
        harness.app_mut().release_scope(self.scope);
    }

    /// One production-shaped round: the widget's hook, then the pipeline that paints it.
    fn frame(&mut self, harness: &mut WidgetTestApp) {
        self.sync(harness);
        harness.drain();
    }

    /// Builds the body, lets the layout settle it, and draws one frame.
    fn draw(&mut self, harness: &mut WidgetTestApp) {
        self.body(harness);
        harness.drain();
        self.frame(harness);
    }

    fn status(&self, harness: &WidgetTestApp) -> WidgetState {
        self.status.state(harness.app())
    }

    fn key(&self) -> CandlestickStreamKey {
        let config = &self.widget.config;
        CandlestickStreamKey::new(
            config
                .selection
                .as_ref()
                .expect("the fixture pane follows a listing")
                .listing
                .clone(),
            config.timeframe,
            config.adjustment,
        )
    }

    fn push(&mut self, message: Inbound<CandlestickProtocol>) {
        self.widget.test_inbox.push(message);
    }

    fn push_snapshot(&mut self, snapshot: CandlestickSeriesSnapshot) {
        let key = self.key();
        self.push(Inbound::Latest {
            key,
            message: snapshot_message(snapshot),
        });
    }

    fn push_status(&mut self, status: CandlestickStatus) {
        let key = self.key();
        self.push(Inbound::Reliable {
            key,
            message: status_message(status),
        });
    }

    fn push_failure(&mut self, error: MarketError) {
        self.push_status(CandlestickStatus::Error(error));
    }

    fn push_page_settled(&mut self, direction: PageDirection) {
        let key = self.key();
        self.push(Inbound::Reliable {
            key,
            message: Arc::new(CandlestickStreamMessage::PageSettled { direction }),
        });
    }

    /// Accepts one snapshot as the latest stream would, then reports the ready status.
    fn accept(&mut self, harness: &mut WidgetTestApp, snapshot: CandlestickSeriesSnapshot) {
        self.push_snapshot(snapshot);
        self.push_status(CandlestickStatus::Ready);
        self.frame(harness);
    }

    /// The one canvas node the chart draws into.
    fn canvas(&self, harness: &WidgetTestApp) -> NodeId {
        harness
            .find_by_debug_prefix(self.chart_page, ChartCanvas::selector())
            .expect("the ready chart built its canvas")
    }

    fn canvas_primitives(&self, harness: &WidgetTestApp) -> Vec<CanvasPrimitive> {
        harness.canvas_primitives(self.canvas(harness))
    }

    /// The canvas the node tree currently holds, for `Arc::ptr_eq` steady-state assertions.
    fn canvas_handle(&self, harness: &WidgetTestApp) -> Arc<ui_core::paint::canvas::Canvas> {
        harness
            .canvas_handle(self.canvas(harness))
            .expect("the chart is a canvas node")
    }

    fn layout_width(&self) -> Option<f32> {
        self.widget.chart.layout_width()
    }

    /// The centre of the plot's data area, which is what the chart hit-tests against.
    fn data_center(&self) -> Point {
        let bounds = self
            .widget
            .chart
            .layout()
            .expect("the chart has been laid out")
            .data_bounds();
        Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn dispatch(&mut self, harness: &mut WidgetTestApp, event: Event) -> bool {
        self.with(harness, |widget, cx| widget.dispatch(&event, cx))
    }

    fn write(&self, harness: &mut WidgetTestApp, signal: Signal<Option<i64>>, value: Option<i64>) {
        harness.app_mut().write(&signal, value);
    }
}

/// Builds a pane over `source`, with its own scope and header groups, and no work started.
fn pane_with_source(
    harness: &mut WidgetTestApp,
    config: CandlestickConfig,
    source: FixtureSource,
    size: (f32, f32),
) -> Pane {
    let scope = harness.app_mut().signal_scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let chart_page = page(harness, "TEST_CANDLESTICK_PAGE", size);
    let left = header_group(harness, chart_page, "TEST_CANDLESTICK_HEADER_LEFT");
    let right = header_group(harness, chart_page, "TEST_CANDLESTICK_HEADER_RIGHT");
    let publishes = Rc::new(Cell::new(0));
    let counted = Rc::clone(&publishes);
    let change = WidgetConfigChange::new(move |_| counted.set(counted.get() + 1));
    let widget = with_scope(harness, scope, |cx| {
        CandlestickWidget::with_source(config, source, status, change, recovery(), cx)
    });
    Pane {
        widget,
        status,
        scope,
        chart_page,
        left,
        right,
        publishes,
    }
}

/// A pane that follows the fixture listing, with nothing started yet.
fn default_pane(harness: &mut WidgetTestApp) -> Pane {
    pane_with_source(
        harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
    )
}

/// A pane over a wide page, for the prefetch thresholds.
fn wide_pane(harness: &mut WidgetTestApp) -> Pane {
    let pane = pane_with_source(
        harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        WIDE_PAGE,
    );
    resize_window(harness, WINDOW_WIDE);
    pane
}

/// A pane that has been given a snapshot and drawn once: the shape most assertions start
/// from.
fn drawn_pane(harness: &mut WidgetTestApp, snapshot: CandlestickSeriesSnapshot) -> Pane {
    let mut pane = default_pane(harness);
    pane.start_reading(harness);
    pane.accept(harness, snapshot);
    pane.draw(harness);
    pane
}

// ------------------------------------------------------------------ the directory seam

/// A market directory a test loads and unloads venues on.
///
/// The headless host can be handed a `MarketParticipantsConnector`, but nothing in this
/// crate can *load* one (loading needs a live transport, and the loaded-venue table's write
/// entry is crate-private to the connector). This fixture answers the same questions a loaded
/// venue would, so the directory's branches are reachable from a view test.
struct FixtureDirectory {
    catalog: Arc<Catalog>,
    /// The routes this directory answers as loaded, and the capabilities each one carries.
    loaded: RefCell<Vec<(VenueRoute, Vec<CapabilityId>)>>,
}

impl FixtureDirectory {
    /// A directory over `catalog` with nothing loaded: what a host whose directory load never
    /// committed looks like.
    fn new(catalog: Arc<Catalog>) -> Rc<Self> {
        Rc::new(Self {
            catalog,
            loaded: RefCell::new(Vec::new()),
        })
    }

    /// The same directory with the fixture spot route loaded for candlesticks.
    fn with_spot_loaded(catalog: Arc<Catalog>) -> Rc<Self> {
        let directory = Self::new(catalog);
        directory.load(route(), vec![CapabilityId::Candlesticks]);
        directory
    }

    fn directory(self: &Rc<Self>) -> Rc<dyn MarketDirectory> {
        Rc::clone(self) as Rc<dyn MarketDirectory>
    }

    /// Publishes `route` as loaded, with `capabilities`.
    fn load(&self, route: VenueRoute, capabilities: Vec<CapabilityId>) {
        let mut loaded = self.loaded.borrow_mut();
        loaded.retain(|(held, _)| held != &route);
        loaded.push((route, capabilities));
    }

    /// Takes `route` back out of the loaded venues, as a shrinking commit does.
    fn unload(&self, route: &VenueRoute) {
        self.loaded.borrow_mut().retain(|(held, _)| held != route);
    }
}

impl MarketDirectory for FixtureDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        self.loaded
            .borrow()
            .iter()
            .map(|(route, capabilities)| LoadedScope {
                route: route.clone(),
                capabilities: capabilities.clone(),
                provides_catalog: true,
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn any_market_loaded(&self) -> bool {
        self.loaded
            .borrow()
            .iter()
            .any(|(route, _)| market_venues().contains(&route.venue))
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.loaded.borrow().iter().any(|(held, _)| held == route)
    }

    fn offers_candlesticks(&self, route: &VenueRoute) -> bool {
        self.loaded.borrow().iter().any(|(held, capabilities)| {
            held == route && capabilities.contains(&CapabilityId::Candlesticks)
        })
    }

    fn listing(&self, listing: &VenueListingKey) -> ListingAnswer {
        listing_answer(&self.catalog, listing)
    }

    fn display(&self) -> DisplaySnapshot {
        self.catalog.cached_display()
    }
}

/// A published fixture catalogue listing `symbols` on the fixture spot route.
fn fixture_catalog(symbols: &[&str]) -> Arc<Catalog> {
    let entries = fixture_entries(symbols);
    let fixture = test_support::Fixture::new(entries.clone());
    test_support::publish_fixture_route(fixture.catalog(), &route(), entries)
        .expect("the fixture route publishes its listings");
    Arc::clone(fixture.catalog_arc())
}

/// The same catalogue registered but never published: a read of it is answered with
/// [`CatalogError::CatalogNotLoaded`], the directory's "not loaded yet" state.
fn fixture_catalog_unpublished(symbols: &[&str]) -> Arc<Catalog> {
    let fixture = test_support::Fixture::new(fixture_entries(symbols));
    Arc::clone(fixture.catalog_arc())
}

/// The catalogue entries a fixture route lists `symbols` under.
fn fixture_entries(symbols: &[&str]) -> Vec<CatalogEntry> {
    symbols
        .iter()
        .map(|symbol| {
            test_support::entry(
                &route(),
                test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                symbol,
            )
        })
        .collect()
}

/// A pane over `directory`, with its own scope and header groups, and no work started.
fn pane_with_directory(
    harness: &mut WidgetTestApp,
    config: CandlestickConfig,
    source: FixtureSource,
    size: (f32, f32),
    directory: Rc<dyn MarketDirectory>,
) -> Pane {
    let scope = harness.app_mut().signal_scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let chart_page = page(harness, "TEST_CANDLESTICK_PAGE", size);
    let left = header_group(harness, chart_page, "TEST_CANDLESTICK_HEADER_LEFT");
    let right = header_group(harness, chart_page, "TEST_CANDLESTICK_HEADER_RIGHT");
    let publishes = Rc::new(Cell::new(0));
    let counted = Rc::clone(&publishes);
    let change = WidgetConfigChange::new(move |_| counted.set(counted.get() + 1));
    let widget = with_scope(harness, scope, |cx| {
        CandlestickWidget::with_source_and_directory(
            config,
            source,
            Some(directory),
            status,
            change,
            recovery(),
            cx,
        )
    });
    Pane {
        widget,
        status,
        scope,
        chart_page,
        left,
        right,
        publishes,
    }
}

/// Moves the directory generation the host publishes, which is what wakes a waiting pane.
fn publish_generation(harness: &mut WidgetTestApp, generation: u64) {
    let signal = harness.services().catalog_generation;
    harness.app_mut().write(&signal, generation);
}

/// One frame of the surface the framework draws around the body, over the pane's own status.
fn synced_surface(harness: &mut WidgetTestApp, pane: &Pane, has_content: bool) -> WidgetSurface {
    let mut surface = with_scope(harness, pane.scope, |cx| {
        WidgetSurface::build(cx, pane.chart_page, pane.status).expect("the surface builds")
    });
    sync_surface(harness, pane.scope, &mut surface, has_content);
    surface
}

fn sync_surface(
    harness: &mut WidgetTestApp,
    scope: SignalScope,
    surface: &mut WidgetSurface,
    has_content: bool,
) {
    with_scope(harness, scope, |cx| {
        surface.sync(cx, has_content).expect("the surface syncs");
    });
    harness.drain();
}

fn pointer_move(position: Point) -> Event {
    Event::Input(InputEvent::PointerMove {
        position,
        modifiers: Modifiers::default(),
        timestamp: std::time::Instant::now(),
    })
}

fn pointer_down(position: Point) -> Event {
    Event::Input(InputEvent::PointerDown {
        position,
        button: PointerButton::Primary,
        modifiers: Modifiers::default(),
        timestamp: std::time::Instant::now(),
    })
}

fn pointer_up(position: Point) -> Event {
    Event::Input(InputEvent::PointerUp {
        position,
        button: PointerButton::Primary,
        modifiers: Modifiers::default(),
        timestamp: std::time::Instant::now(),
    })
}

fn scroll(position: Point, delta: Point) -> Event {
    Event::Input(InputEvent::Scroll {
        position,
        delta,
        unit: ui_core::event::ScrollUnit::Line,
        phase: ui_core::event::ScrollPhase::Discrete,
        modifiers: Modifiers::default(),
        timestamp: std::time::Instant::now(),
    })
}

/// The point the settings panel occupies: the chart's surface is the whole pane, and the
/// panel is offered the events over its own column first, so a chart never sees them.
fn over_the_panel(pane: &Pane) -> Point {
    let bounds = pane.widget.chart.bounds().expect("the chart has a box");
    Point::new(
        bounds.max().x + 8.0,
        bounds.origin.y + bounds.size.height / 2.0,
    )
}

// --------------------------------------------------------------- config & reading

#[test]
fn the_default_config_should_follow_nothing() {
    let config = CandlestickConfig::default();

    assert_eq!(
        config.selection, None,
        "a new chart waits for the user to choose instead of charting a default"
    );
    assert_eq!(config.timeframe, TimeFrame::Minute1);
    assert_eq!(config.adjustment, Adjustment::None);
}

#[test]
fn a_chart_that_follows_nothing_should_describe_no_venue() {
    // Nothing is assumed for a chart nobody has pointed anywhere: no venue for the
    // countdown, and the picker opens wherever its own filters admit.
    assert_eq!(venue_of(&CandlestickConfig::default()), None);
    assert_eq!(venue_of(&config()), Some(&venue("binance")));
}

#[test]
fn a_config_should_round_trip_through_its_persisted_form() {
    let config = config();
    let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
    let decoded = envelope.decode::<CandlestickConfig>();

    assert_eq!(decoded.selection, config.selection);
    assert_eq!(decoded.timeframe, config.timeframe);
    assert_eq!(decoded.adjustment, config.adjustment);
}

#[test]
fn an_unselected_config_should_round_trip_through_its_persisted_form() {
    let config = CandlestickConfig::default();
    let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
    let decoded = envelope.decode::<CandlestickConfig>();

    assert_eq!(decoded.selection, None);
    assert_eq!(decoded.timeframe, config.timeframe);
}

#[test]
fn an_old_payload_should_decode_to_the_default_config() {
    // A payload written before the listing shape existed names a route and an instrument,
    // which is not a listing: it cannot decode, so the chart reads the defaults.
    let legacy = concat!(
        r#"{"selection":{"route":{"venue":"binance","scope":"spot"},"#,
        r#""instrument":{"symbol":"BTCUSDT"}},"timeframe":"1m"}"#,
    );
    let stored = serde_json::from_value::<VersionedWidgetConfig>(serde_json::json!({
        "schema_version": 1,
        "payload": legacy,
    }))
    .expect("the envelope should decode");

    let decoded = stored.decode::<CandlestickConfig>();

    assert_eq!(decoded.selection, None);
    assert_eq!(decoded.timeframe, CandlestickConfig::default().timeframe);
}

#[test]
fn two_listings_of_one_instrument_should_persist_apart() {
    let mut first = config();
    first.selection = Some(selection("BTCUSDT"));
    let mut second = config();
    second.selection = Some(selection("BTCUSDC"));
    let first_envelope = VersionedWidgetConfig::encode(&first).expect("config should encode");
    let second_envelope = VersionedWidgetConfig::encode(&second).expect("config should encode");

    assert_ne!(
        serde_json::to_string(&first_envelope).expect("the envelope encodes"),
        serde_json::to_string(&second_envelope).expect("the envelope encodes"),
        "two listings of one instrument are two scopes, not one"
    );
    assert_eq!(
        first_envelope.decode::<CandlestickConfig>().selection,
        first.selection
    );
}

#[test]
fn the_default_interval_should_be_one_the_picker_offers() {
    assert!(
        TIMEFRAMES.contains(&CandlestickConfig::default().timeframe),
        "the interval the chart falls back to is not one it can ask for"
    );
}

#[test]
fn a_rejection_should_be_read_as_the_scope_it_names() {
    let interval = MarketError::from(MarketParticipantError::UnsupportedTimeFrame {
        route: route(),
        timeframe: format_smolstr!("1s"),
    });
    let capability = MarketError::UnsupportedCapability {
        route: route(),
        capability: CapabilityId::Candlesticks,
    };
    let other = MarketError::UnsupportedCapability {
        route: route(),
        capability: CapabilityId::Tickers,
    };

    assert!(is_unsupported_timeframe(&interval));
    assert!(!is_unsupported_capability(&interval));
    assert!(is_unsupported_capability(&capability));
    assert!(!is_unsupported_timeframe(&capability));
    assert!(!is_unsupported_capability(&other));
    assert!(!is_unsupported_timeframe(&disconnect()));
    assert!(!is_unsupported_capability(&disconnect()));
}

#[test]
fn only_a_directory_answer_should_count_as_unsupported() {
    let missing = CatalogError::ListingNotFound {
        listing: Box::new(listing(SYMBOL)),
    };
    let revoked = CatalogError::RouteNotRegistered { route: route() };
    let failed = CatalogError::Source(Box::new(SourceError::Network {
        reason: SmolStr::new_static("the fixture catalog is unreachable"),
    }));

    assert!(proves_absent(&missing));
    assert!(proves_absent(&revoked));
    assert!(
        !proves_absent(&failed),
        "a fetch that failed proves nothing about the listing"
    );
}

#[test]
fn the_adjustment_refusal_is_read_as_an_unsupported_adjustment() {
    let refused = MarketError::from(MarketParticipantError::UnsupportedAdjustment {
        route: route(),
        adjustment: SmolStr::new(Adjustment::Forward.label()),
    });
    let no_candles = MarketError::UnsupportedCapability {
        route: route(),
        capability: CapabilityId::Candlesticks,
    };

    assert!(
        is_unsupported_adjustment(&refused),
        "the refusal the market layer wraps is the adjustment refusal"
    );
    assert!(
        !is_unsupported_timeframe(&refused),
        "one refusal is not read as the other dimension's"
    );
    assert!(
        !is_unsupported_capability(&refused),
        "a route that refuses one convention still serves candles"
    );
    assert!(is_unsupported_capability(&no_candles));
    assert!(!is_unsupported_adjustment(&no_candles));
}

#[test]
fn countdown_state_should_ignore_price_updates_for_the_same_candle() {
    let mut countdown = CountdownState::default();
    countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

    let changed = countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 30_000);

    assert_eq!((changed, countdown.label.as_ref()), (false, "01:00"));
}

#[test]
fn countdown_state_tick_should_advance_the_same_candle() {
    let mut countdown = CountdownState::default();
    countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

    let changed = countdown.tick(0, TimeFrame::Minute1, &venue("binance"), 1_000);

    assert_eq!((changed, countdown.label.as_ref()), (true, "00:59"));
}

#[test]
fn countdown_state_should_reset_for_a_new_candle() {
    let mut countdown = CountdownState::default();
    countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 59_000);

    let changed = countdown.observe_candle(60_000, TimeFrame::Minute1, &venue("binance"), 60_000);

    assert_eq!((changed, countdown.label.as_ref()), (true, "01:00"));
}

#[test]
fn clearing_the_countdown_should_restore_the_initial_label() {
    let mut countdown = CountdownState::default();
    countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

    countdown.clear();

    assert_eq!(
        (countdown.candle_timestamp, countdown.label.as_ref()),
        (None, "00:00")
    );
}

// ------------------------------------------------------------------ status & waits

#[test]
fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.load(&mut harness);

    assert!(!pane.widget.has_content(), "no candle has arrived yet");
    assert_eq!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded {
            venues: vec![route().venue],
            supported: market_venues(),
        }),
        "load reports the notice for the market the chart follows"
    );

    pane.start_reading(&mut harness);

    assert_eq!(pane.status(&harness), WidgetState::Connecting);
    assert!(!pane.widget.has_content());
    assert!(
        !pane.widget.pending.is_empty(),
        "the load's answer is being waited for"
    );
}

#[test]
fn an_unloaded_market_should_leave_the_pane_waiting() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);

    pane.load(&mut harness);

    assert!(!pane.widget.has_content());
    match pane.status(&harness) {
        WidgetState::Waiting(Notice::MarketNotLoaded { venues, supported }) => {
            assert_eq!(venues, vec![route().venue], "the pane names its own venue");
            assert_eq!(
                supported,
                market_venues(),
                "the sentence lists every market this widget can read"
            );
        }
        other => panic!("an unloaded market is a wait, got {other:?}"),
    }
    assert!(
        pane.widget.pending.is_empty(),
        "a pane waiting for its market starts no stream"
    );
    assert!(
        pane.widget.snapshot.is_none(),
        "and it holds no chart from before"
    );
}

#[test]
fn a_widget_without_a_selection_should_ask_for_one() {
    let mut harness = harness();
    let mut pane = pane_with_source(
        &mut harness,
        CandlestickConfig::default(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
    );

    // The state `load` reports once its market gate is past: nothing chosen is the pane's
    // own prompt rather than a failure.
    pane.start_reading(&mut harness);

    assert_eq!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::message(
            "widget.candlestick.select_instrument_prompt"
        )),
        "a chart with nothing chosen asks for an instrument"
    );
    assert!(!pane.widget.has_content());
    assert!(
        pane.widget.pending.is_empty(),
        "nothing chosen starts no stream"
    );
}

#[test]
fn a_language_change_should_relabel_the_notice_a_waiting_pane_shows() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.load(&mut harness);
    let mut surface = synced_surface(&mut harness, &pane, false);

    let english = harness.text_of(surface.root());
    assert!(
        !english.is_empty(),
        "the waiting notice is drawn as resolved copy"
    );
    assert!(
        !english.contains("<p") && !english.contains("<kbd"),
        "the surface draws the sentence rather than its markup: {english}"
    );

    harness.switch_language(LanguageCode::ZhCn);
    sync_surface(&mut harness, pane.scope, &mut surface, false);

    let chinese = harness.text_of(surface.root());
    assert_ne!(
        chinese, english,
        "the catalogue's other language names the same wait differently"
    );
}

#[test]
fn an_accepted_snapshot_should_become_content_and_clear_the_status() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let snapshot = snapshot_of([candle(1, 100)]);

    pane.push_snapshot(snapshot);
    pane.frame(&mut harness);

    assert!(pane.widget.has_content(), "the snapshot is the body");
    assert_eq!(
        pane.widget.countdown.candle_timestamp,
        Some(1),
        "the chart counts down to the newest candle it was given"
    );

    pane.push_status(CandlestickStatus::Ready);
    pane.frame(&mut harness);

    assert_eq!(pane.status(&harness), WidgetState::Ready);
    assert!(pane.status(&harness).failure().is_none());
    let surface = synced_surface(&mut harness, &pane, true);
    assert!(
        harness
            .find_by_debug_prefix(surface.root(), "WIDGET_BANNER")
            .is_none(),
        "a ready chart with content has no banner"
    );
}

#[test]
fn an_empty_snapshot_should_be_content_and_report_nothing() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);

    pane.push_snapshot(snapshot_of(std::iter::empty()));
    pane.push_status(CandlestickStatus::Ready);
    pane.draw(&mut harness);

    assert!(
        pane.widget.has_content(),
        "an instrument publishing no candles is content, not a state"
    );
    assert_eq!(pane.status(&harness), WidgetState::Ready);
    assert!(
        *harness.app().read(&pane.widget.empty_visible),
        "the overlay that says the plot holds nothing is on"
    );
    let empty = harness
        .find_by_debug_prefix(pane.chart_page, EMPTY_SELECTOR)
        .expect("the body built the no-data overlay");
    assert!(
        harness.visible(empty),
        "the plot says it holds no candles instead of staying blank"
    );
}

#[test]
fn a_failure_with_content_should_be_reported_and_keep_the_chart() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(20, 20));
    assert!(pane.widget.has_content());

    let requested = pane.widget.history_requests;
    pane.push_failure(disconnect());
    pane.frame(&mut harness);

    let state = pane.status(&harness);
    assert!(state.is_failed(), "a stream failure is a failure");
    assert!(
        pane.widget.has_content(),
        "the candles stay on the chart under the banner"
    );
    assert!(
        pane.widget.countdown_timer.is_none(),
        "and the countdown a dead stream can no longer advance is stopped"
    );
    assert_eq!(
        pane.widget.history_requests, requested,
        "a failed chart asks for no history: the stream that would answer is gone"
    );
    let surface = synced_surface(&mut harness, &pane, true);
    assert!(
        harness
            .find_by_debug_prefix(surface.root(), "WIDGET_BANNER")
            .is_some(),
        "a failed chart with content draws its failure over the body"
    );
}

#[test]
fn a_failure_without_content_should_be_reported_with_no_content() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);

    pane.push_failure(disconnect());
    pane.frame(&mut harness);

    assert!(pane.status(&harness).is_failed());
    assert!(!pane.widget.has_content(), "nothing was drawn to keep");
    let surface = synced_surface(&mut harness, &pane, false);
    assert!(
        harness
            .find_by_debug_prefix(surface.root(), "WIDGET_PLACEHOLDER")
            .is_some(),
        "a failed pane with no content stands in for it"
    );
    assert!(
        harness
            .find_by_debug_prefix(surface.root(), "WIDGET_BANNER")
            .is_none(),
        "there is no body for a banner to sit over"
    );
}

#[test]
fn every_failure_should_offer_the_recovery_that_reloads_the_widget() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);

    pane.push_failure(disconnect());
    pane.frame(&mut harness);

    let state = pane.status(&harness);
    let failure = state.failure().expect("the pane reports a failure");
    assert_eq!(
        failure.action().label_key(),
        "common.retry",
        "every failure offers the same recovery control"
    );
    assert!(failure.action().is_enabled());

    // The rpc path reports the same recovery, so a page that failed offers it too.
    let request_id = RequestId::new(1);
    pane.push(Inbound::Rpc {
        request_id,
        result: Err(RpcCallError::ServiceClosed { request_id }),
    });
    pane.frame(&mut harness);

    let state = pane.status(&harness);
    assert!(state.is_failed(), "an unanswered load is a failure");
    assert_eq!(
        state
            .failure()
            .expect("the pane reports a failure")
            .action()
            .label_key(),
        "common.retry"
    );
}

#[test]
fn a_late_stream_message_after_a_failure_should_not_change_the_status() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    pane.push_failure(disconnect());
    pane.frame(&mut harness);
    assert!(pane.status(&harness).is_failed());

    let painted = pane.widget.snapshot.clone().expect("the chart was kept");
    pane.push_snapshot(live_edge_snapshot(20, 40));
    pane.frame(&mut harness);

    assert!(
        pane.status(&harness).is_failed(),
        "a failure is terminal until the user recovers"
    );
    assert!(
        pane.widget
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.shares_chart_data_with(&painted)),
        "a message for the dead stream does not repaint the chart"
    );
}

#[test]
fn an_explicit_reload_should_start_over_from_connecting() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    assert!(pane.widget.has_content());

    // What `load` does once its market gate is past, for a pane the user reloaded: it
    // cannot vouch for what it was showing, so the chart goes.
    pane.with(&mut harness, |widget, cx| {
        widget.start_current(false, false, cx);
    });

    assert_eq!(pane.status(&harness), WidgetState::Connecting);
    assert!(!pane.widget.has_content(), "a reload drops the old chart");
    assert!(
        !pane.widget.pending.is_empty(),
        "and waits for the new stream's answer"
    );
}

#[test]
fn a_retry_after_a_failure_should_keep_the_chart_and_drop_the_pages_in_flight() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(30, 30));
    pane.widget.fetching_history = true;
    pane.widget.history_requested_before = Some(1);
    pane.widget.fetching_newer = true;
    pane.widget.newer_requested_after = Some(30);
    pane.push_failure(disconnect());
    pane.frame(&mut harness);
    assert!(pane.status(&harness).is_failed());

    // A retry is the framework's recovery: it reopens the stream for the scope the chart
    // was reading and keeps the candles the user is still looking at.
    pane.with(&mut harness, |widget, cx| {
        widget.start_current(true, false, cx);
    });

    assert!(
        pane.widget.has_content(),
        "a retry does not blank the chart"
    );
    assert!(!pane.widget.fetching_history);
    assert!(!pane.widget.fetching_newer);
    assert_eq!(pane.widget.history_requested_before, None);
    assert_eq!(pane.widget.newer_requested_after, None);
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
}

// ------------------------------------------------------------------------- scope

#[test]
fn a_snapshot_for_another_scope_should_be_ignored() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));

    // A snapshot for the same instrument on another interval belongs to the scope the
    // chart replaced; the stream of the old generation may still deliver it.
    let other = CandlestickStreamKey::new(
        listing(SYMBOL),
        TimeFrame::Minute5,
        pane.widget.config.adjustment,
    );
    pane.push(Inbound::Latest {
        key: other,
        message: snapshot_message(live_edge_snapshot(40, 40)),
    });
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget
            .snapshot
            .as_ref()
            .expect("the chart kept its snapshot")
            .latest_timestamp(),
        Some(10),
        "a message for another scope is not this chart's"
    );
}

#[test]
fn selecting_another_listing_should_switch_the_scope_and_drop_the_chart() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    let generation = pane.widget.generation;
    let publishes = pane.publishes.get();

    let next = selection("BTCUSDC");
    pane.with(&mut harness, |widget, cx| {
        cx.write(&widget.selection_requests, Some(next.clone()));
    });
    pane.frame(&mut harness);

    assert_eq!(pane.widget.config.selection, Some(next));
    assert!(
        pane.publishes.get() > publishes,
        "the new scope is written back to the persisted configuration"
    );
    assert!(
        pane.widget.snapshot.is_none(),
        "the candles of the previous listing are not the new listing's"
    );
    assert_ne!(pane.widget.generation, generation, "the scope starts over");
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
}

#[test]
fn selecting_a_scope_that_is_already_shown_should_be_rejected() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    let generation = pane.widget.generation;
    let publishes = pane.publishes.get();

    let same = pane
        .widget
        .config
        .selection
        .clone()
        .expect("the pane follows a listing");
    pane.with(&mut harness, |widget, cx| {
        cx.write(&widget.selection_requests, Some(same));
    });
    pane.frame(&mut harness);

    assert_eq!(
        pane.publishes.get(),
        publishes,
        "nothing changed, so nothing was written back"
    );
    assert_eq!(
        pane.widget.generation, generation,
        "the stream the chart is already reading is not replaced"
    );
    assert!(pane.widget.has_content(), "and the chart stays");
}

#[test]
fn a_market_taken_away_mid_flight_should_leave_the_pane_waiting() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));

    // The transition a shrinking commit performs: the venue the chart reads is gone, so the
    // pane reports the neutral notice and drops what it drew.
    pane.with(&mut harness, |widget, cx| widget.wait_for_market(cx));
    pane.frame(&mut harness);

    assert!(!pane.widget.has_content(), "the chart goes with the market");
    assert!(pane.widget.countdown_timer.is_none());
    match pane.status(&harness) {
        WidgetState::Waiting(Notice::MarketNotLoaded { venues, .. }) => {
            assert_eq!(venues, vec![route().venue]);
        }
        other => panic!("a market taken away is a wait, got {other:?}"),
    }

    // The stream the retired generation opened may still push a snapshot; it is dropped.
    pane.push_snapshot(live_edge_snapshot(20, 40));
    pane.frame(&mut harness);

    assert!(
        pane.widget.snapshot.is_none(),
        "a late snapshot does not repaint over the notice"
    );
    assert!(matches!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded { .. })
    ));
}

#[test]
fn a_catalog_generation_change_should_not_revive_a_failed_pane() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.push_failure(disconnect());
    pane.frame(&mut harness);
    assert!(pane.status(&harness).is_failed());
    let generation = pane.widget.generation;

    let signal = harness.services().catalog_generation;
    harness.app_mut().write(&signal, 1);
    pane.frame(&mut harness);

    assert!(
        pane.status(&harness).is_failed(),
        "the directory moving does not revive a failed pane"
    );
    assert_eq!(
        pane.widget.generation, generation,
        "and it starts no stream either"
    );
}

#[test]
fn a_catalog_generation_change_should_resettle_the_scope_a_pane_was_reading() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));

    // The catalogue generation the stream resolved its rules against was replaced. With no
    // directory holding the chart's market any more, the pane goes back to waiting for it
    // rather than keeping a stream built on a generation nothing publishes.
    let signal = harness.services().catalog_generation;
    harness.app_mut().write(&signal, 1);
    pane.frame(&mut harness);

    assert!(
        pane.widget.awaiting_market,
        "the scope is re-settled against the directory"
    );
    assert!(pane.widget.snapshot.is_none());
    assert!(matches!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded { .. })
    ));
}

#[test]
fn a_route_rejection_should_be_read_as_the_scope_it_names() {
    let mut harness = harness();
    let mut interval = default_pane(&mut harness);
    interval.with(&mut harness, |widget, cx| {
        widget.select_timeframe(TimeFrame::Hour1, cx);
    });
    interval.start_reading(&mut harness);
    let publishes = interval.publishes.get();

    // The route refuses the interval the chart subscribed to: the chart falls back to the
    // default rather than reporting a scope it can still chart.
    interval.push_failure(MarketError::from(
        MarketParticipantError::UnsupportedTimeFrame {
            route: route(),
            timeframe: SmolStr::new_static("1h"),
        },
    ));
    interval.frame(&mut harness);

    assert_eq!(
        interval.widget.config.timeframe,
        CandlestickConfig::default().timeframe
    );
    assert!(
        interval.publishes.get() > publishes,
        "the fallback interval is written back"
    );
    assert!(!interval.status(&harness).is_failed());

    // The route serving no candles at all is not a failure either: nothing is chosen any
    // more, and the pane asks for an instrument.
    let mut capability = default_pane(&mut harness);
    capability.start_reading(&mut harness);
    capability.push_failure(MarketError::UnsupportedCapability {
        route: route(),
        capability: CapabilityId::Candlesticks,
    });
    capability.frame(&mut harness);

    assert_eq!(capability.widget.config.selection, None);
    assert_eq!(
        capability.status(&harness),
        WidgetState::Waiting(Notice::message(
            "widget.candlestick.select_instrument_prompt"
        ))
    );
}

#[test]
fn changing_the_adjustment_should_subscribe_a_new_scope() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    let generation = pane.widget.generation;
    let publishes = pane.publishes.get();

    // What the adjustment picker's handler asks the pane to do.
    pane.with(&mut harness, |widget, cx| {
        widget.apply_header_request(HeaderRequest::Adjustment(Adjustment::Forward), cx);
    });
    pane.frame(&mut harness);

    assert_eq!(pane.widget.config.adjustment, Adjustment::Forward);
    assert!(
        pane.publishes.get() > publishes,
        "the convention is written back to the persisted configuration"
    );
    assert_ne!(
        pane.widget.generation, generation,
        "another convention is another stream"
    );
    assert!(
        pane.widget.snapshot.is_none(),
        "the new convention's prices are not the previous one's"
    );
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
}

#[test]
fn the_scope_should_be_settled_against_what_the_route_publishes() {
    let mut harness = harness();
    let source = FixtureSource::new(vec![TimeFrame::Minute1], vec![Adjustment::None]);
    let mut stored = config();
    stored.timeframe = TimeFrame::Hour1;
    stored.adjustment = Adjustment::Forward;
    let mut pane = pane_with_source(&mut harness, stored, source, PAGE);
    let publishes = pane.publishes.get();

    // The two lookups `load` performs before it subscribes: an interval or a convention the
    // route stopped publishing goes back to the default instead of being subscribed.
    pane.with(&mut harness, |widget, cx| {
        widget.resolve_timeframe(cx);
        widget.resolve_adjustment(cx);
    });

    assert_eq!(pane.widget.config.timeframe, TimeFrame::Minute1);
    assert_eq!(pane.widget.config.adjustment, Adjustment::None);
    assert!(
        pane.publishes.get() > publishes,
        "a settled scope is written back to the persisted configuration"
    );
}

// -------------------------------------------------------------- zones & countdown

#[test]
fn moving_the_data_time_zone_should_repaint_the_chart() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(20, 20));
    let before = pane.canvas_handle(&harness);

    let signal = harness.services().config;
    let mut settings = (*harness.app().read(&signal)).clone();
    settings.data_time_zone =
        DataTimeZone::from_hours(9).expect("the offered offset is one the setting takes");
    harness.app_mut().write(&signal, settings);
    pane.frame(&mut harness);

    assert!(
        !Arc::ptr_eq(&before, &pane.canvas_handle(&harness)),
        "the time axis labels follow the zone they are formatted in"
    );
}

#[test]
fn a_live_edge_window_should_arm_the_countdown_and_a_historical_one_should_not() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let now = current_timestamp_millis();
    pane.accept(&mut harness, live_edge_snapshot(10, now));
    pane.draw(&mut harness);

    assert!(
        pane.widget.should_refresh_countdown(),
        "a live-edge candle is the one the countdown counts down to"
    );
    assert!(
        pane.widget.countdown_timer.is_some(),
        "the countdown arms a one-shot timer for its next label"
    );

    pane.push_snapshot(panned_snapshot());
    pane.frame(&mut harness);

    assert!(
        !pane
            .widget
            .snapshot
            .as_ref()
            .expect("a chart")
            .at_live_edge()
    );
    assert!(
        pane.widget.countdown_timer.is_none(),
        "a window wheeled away from the live edge has nothing to count down to"
    );
}

#[test]
fn the_countdown_should_repaint_only_when_its_label_moves() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let newest = current_timestamp_millis();
    pane.accept(&mut harness, live_edge_snapshot(10, newest));
    pane.draw(&mut harness);
    let wake = pane.widget.countdown_wake;
    let current = pane.widget.countdown.label.clone();
    // A timestamp whose label differs: the label moves once a second, so one of the next
    // minute's seconds reads differently from the one already drawn.
    let moved = (0..60)
        .map(|second| newest + second * 1_000)
        .find(|timestamp| {
            remaining_time_label(newest, TimeFrame::Minute1, &route().venue, *timestamp) != current
        })
        .expect("a label that differs exists within one candle");
    let before = pane.canvas_handle(&harness);

    pane.write(&mut harness, wake, Some(moved));
    let requests = harness.frame_requests();
    pane.frame(&mut harness);

    assert_ne!(pane.widget.countdown.label, current, "the label moved");
    assert!(
        harness.frame_requests() > requests,
        "a moved countdown asks for the frame that draws it"
    );
    assert!(!Arc::ptr_eq(&before, &pane.canvas_handle(&harness)));

    // The same wake again recomputes the same label: nothing to draw, nothing asked for.
    pane.write(&mut harness, wake, Some(moved));
    let settled = pane.canvas_handle(&harness);
    let requests = harness.frame_requests();
    pane.sync(&mut harness);
    harness.drain();

    assert!(!harness.rendered(), "an unchanged label draws nothing (I3)");
    assert_eq!(harness.frame_requests(), requests);
    assert!(
        Arc::ptr_eq(&settled, &pane.canvas_handle(&harness)),
        "and the canvas is not rebuilt"
    );
}

#[test]
fn advancing_the_clock_should_fire_the_armed_countdown() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(
        &mut harness,
        live_edge_snapshot(10, current_timestamp_millis()),
    );
    pane.draw(&mut harness);
    assert!(pane.widget.countdown_timer.is_some());

    // The label moves at least once a second, so a second and a bit fires the one-shot.
    assert!(
        harness.advance(Duration::from_millis(1_100)),
        "the timer's callback asks for the frame that presents its write"
    );
    // A due timer is dispatched at a frame's entry, which is after that round's frame
    // routes: the sync that consumes the wake it wrote is the next round's (AGENTS §11).
    pane.frame(&mut harness);

    assert_eq!(
        *harness.app().read(&pane.widget.countdown_wake),
        None,
        "the sync consumed the wake the timer wrote"
    );
    assert!(
        pane.widget.countdown_timer.is_some(),
        "and re-armed the next one"
    );
}

// ----------------------------------------------------------------- canvas & input

#[test]
fn the_chart_should_draw_its_candles_on_the_canvas() {
    let mut harness = harness();
    let pane = drawn_pane(&mut harness, live_edge_snapshot(40, 40));

    assert!(
        harness.has_canvas(pane.canvas(&harness)),
        "the body's node is a canvas"
    );
    let primitives = pane.canvas_primitives(&harness);
    assert!(
        primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::Quad),
        "the candles are drawn"
    );
    assert!(
        primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::GlyphRun),
        "the axes and the readout are labelled"
    );
    let pushed = primitives
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::ClipPush)
        .count();
    let popped = primitives
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::ClipPop)
        .count();
    assert_eq!(pushed, popped, "every clip the chart opened is closed");
}

#[test]
fn a_settled_chart_should_not_rewrite_its_canvas_or_ask_for_a_frame() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(30, 30));
    let before = pane.canvas_handle(&harness);
    let requests = harness.frame_requests();

    pane.sync(&mut harness);
    harness.drain();

    assert!(!harness.rendered(), "a steady frame draws nothing (I3)");
    assert_eq!(harness.frame_requests(), requests);
    assert!(
        Arc::ptr_eq(&before, &pane.canvas_handle(&harness)),
        "a sync that changed nothing must not replace the canvas"
    );
    assert!(
        harness.paint_entries(pane.canvas(&harness)).is_empty(),
        "an idle round paints nothing, so the chart was not redrawn"
    );
}

#[test]
fn panning_the_viewport_should_replace_the_canvas() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let before = pane.canvas_handle(&harness);

    pane.with(&mut harness, |widget, _cx| {
        assert!(widget.pan_by(-10), "panning back moves the viewport");
    });
    pane.frame(&mut harness);

    assert!(
        !Arc::ptr_eq(&before, &pane.canvas_handle(&harness)),
        "a moved viewport is a different drawing"
    );
    assert!(pane.widget.viewport.right_timestamp.is_some());
}

#[test]
fn dragging_should_pan_the_viewport_and_stop_at_the_live_edge() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(120, 120));
    let center = pane.data_center();

    assert!(
        pane.dispatch(&mut harness, pointer_down(center)),
        "the press lands on the chart"
    );
    assert!(pane.widget.dragging);

    // Dragging right pulls older candles into view: the chart stops following the edge.
    let right = Point::new(center.x + 120.0, center.y);
    assert!(pane.dispatch(&mut harness, pointer_move(right)));
    assert!(
        pane.widget.viewport.right_timestamp.is_some(),
        "a drag away from the edge anchors the viewport"
    );

    // Dragging back past the edge clamps at it, and following is expressed as no anchor.
    // The drag stays inside the canvas, which is where the chart accepts a pointer at all.
    let bounds = pane.widget.chart.bounds().expect("the chart has a box");
    let left = Point::new(bounds.origin.x + 1.0, center.y);
    assert!(pane.dispatch(&mut harness, pointer_move(left)));
    assert_eq!(
        pane.widget.viewport.right_timestamp, None,
        "the live edge is the furthest the viewport goes"
    );

    assert!(pane.dispatch(&mut harness, pointer_up(center)));
    assert!(!pane.widget.dragging);
    assert_eq!(pane.widget.viewport.cursor, Some(center));
}

#[test]
fn scrolling_should_zoom_around_the_pointer() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let center = pane.data_center();
    let width = pane.widget.viewport.candle_width;

    assert!(
        pane.dispatch(&mut harness, scroll(center, Point::new(0.0, 1.0))),
        "the plot consumes the wheel rather than letting the page scroll"
    );

    let zoomed = pane.widget.viewport.candle_width;
    assert!(zoomed > width, "a positive step widens one candle");
    assert!(
        (MIN_CANDLE_WIDTH..=MAX_CANDLE_WIDTH).contains(&zoomed),
        "the candle width stays inside the zoom range"
    );
    assert_eq!(
        pane.widget.viewport.cursor,
        Some(center),
        "the candle under the pointer is anchored"
    );
}

/// A wheel with no vertical component is not a zoom step: a trackpad's sideways swipe
/// arrives as `delta.y == 0`, and reading that as "zoom out" shrinks the chart on every
/// horizontal event.
#[test]
fn a_wheel_without_a_vertical_delta_should_not_zoom() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let center = pane.data_center();
    let width = pane.widget.viewport.candle_width;

    assert!(
        pane.dispatch(&mut harness, scroll(center, Point::new(0.0, 0.0))),
        "the chart still consumes the wheel over its plot"
    );

    assert_eq!(
        pane.widget.viewport.candle_width, width,
        "a wheel with no steps leaves the zoom alone"
    );
    assert_eq!(
        pane.widget.viewport.cursor, None,
        "and it anchors no crosshair"
    );
}

/// The same for a horizontal scroll, which is what a trackpad sends while it pans
/// sideways: no vertical delta means no zoom, in either direction.
#[test]
fn a_horizontal_wheel_should_not_zoom_the_chart() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let center = pane.data_center();
    let width = pane.widget.viewport.candle_width;
    let anchored = pane.widget.viewport.right_timestamp;

    assert!(
        pane.dispatch(&mut harness, scroll(center, Point::new(24.0, 0.0))),
        "the chart consumes both axes of a two-finger pan over its plot"
    );

    assert_eq!(
        pane.widget.viewport.candle_width, width,
        "a sideways wheel does not zoom"
    );
    assert_eq!(
        pane.widget.viewport.right_timestamp, anchored,
        "and it does not re-anchor the viewport"
    );
    assert_eq!(pane.widget.viewport.cursor, None);
}

/// A press that started on the chart is released wherever the pointer is. The settings
/// panel is offered the up over its own column first, so the chart has to end the drag
/// without the position gate: otherwise it is left dragging for good.
#[test]
fn a_release_outside_the_canvas_should_end_the_drag() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(120, 120));
    let center = pane.data_center();

    assert!(pane.dispatch(&mut harness, pointer_down(center)));
    assert!(pane.widget.dragging, "the press started a drag");
    assert!(
        pane.dispatch(
            &mut harness,
            pointer_move(Point::new(center.x + 120.0, center.y))
        ),
        "the drag pans"
    );

    let outside = over_the_panel(&pane);
    assert!(
        pane.dispatch(&mut harness, pointer_up(outside)),
        "the release belongs to the widget whose press started the drag"
    );
    assert!(
        !pane.widget.dragging,
        "a release over the panel ends the drag"
    );
    assert_eq!(
        pane.widget.viewport.cursor, None,
        "no crosshair is placed off the canvas"
    );
}

/// Coming back over the plot after a release over the panel moves the crosshair; it must
/// not pan from the stale drag position the canvas never got to end.
#[test]
fn a_pointer_that_comes_back_to_the_plot_should_not_jump() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(120, 120));
    let center = pane.data_center();

    assert!(pane.dispatch(&mut harness, pointer_down(center)));
    let outside = over_the_panel(&pane);
    assert!(
        !pane.dispatch(&mut harness, pointer_move(outside)),
        "a move over the panel is not the chart's to consume"
    );
    assert!(pane.dispatch(&mut harness, pointer_up(outside)));
    let anchored = pane.widget.viewport.right_timestamp;

    let back = Point::new(center.x + 40.0, center.y);
    assert!(
        pane.dispatch(&mut harness, pointer_move(back)),
        "the chart takes the pointer again"
    );
    assert_eq!(
        pane.widget.viewport.cursor,
        Some(back),
        "the pointer came back to hover, not to drag"
    );
    assert_eq!(
        pane.widget.viewport.right_timestamp, anchored,
        "a hovering move pans nothing"
    );
    assert!(!pane.widget.dragging);
}

#[test]
fn events_outside_the_chart_should_not_be_consumed_and_leaving_should_clear_the_cursor() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let center = pane.data_center();

    assert!(pane.dispatch(&mut harness, pointer_move(center)));
    assert_eq!(pane.widget.viewport.cursor, Some(center));

    let outside = Point::new(9_000.0, 9_000.0);
    assert!(
        !pane.dispatch(&mut harness, pointer_move(outside)),
        "the panel and the header are not the chart's to consume"
    );
    assert_eq!(
        pane.widget.viewport.cursor,
        Some(center),
        "a move outside the canvas leaves the crosshair where it was"
    );

    let leave = Event::Input(InputEvent::PointerLeave {
        position: outside,
        modifiers: Modifiers::default(),
        timestamp: std::time::Instant::now(),
    });
    assert!(
        pane.dispatch(&mut harness, leave),
        "the leave ends the drag"
    );
    assert_eq!(pane.widget.viewport.cursor, None);
}

// ---------------------------------------------------------------------- layout

/// Opening the settings panel has to take width away from the plot, and closing it has to
/// give the width back.
#[test]
fn opening_the_panel_should_narrow_the_plot() {
    let mut harness = harness();
    let mut pane = drawn_pane(&mut harness, live_edge_snapshot(60, 60));
    let wide = pane.layout_width().expect("the chart was laid out");

    // The transition the settings control asks the pane for: the panel's column is built
    // by the sync that opens it, and the frame after it lays the column out.
    pane.with(&mut harness, |widget, cx| {
        widget.toggle_indicator_panel(cx);
    });
    pane.frame(&mut harness);
    harness.drain();
    pane.frame(&mut harness);

    let narrow = pane.layout_width().expect("the chart was laid out again");
    assert!(
        narrow < wide,
        "the open panel takes the plot's width: {narrow} vs {wide}"
    );

    pane.with(&mut harness, |widget, cx| {
        widget.toggle_indicator_panel(cx);
    });
    pane.frame(&mut harness);
    harness.drain();
    pane.frame(&mut harness);

    assert_eq!(
        pane.layout_width(),
        Some(wide),
        "closing the panel gives the plot its width back"
    );
}

#[test]
fn a_widened_chart_should_ask_for_the_history_it_now_needs() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let page = usize::from(HISTORY_PAGE_SIZE);
    pane.accept(&mut harness, live_edge_snapshot(page, 300));
    pane.draw(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 0,
        "a narrow plot shows nothing the loaded page does not hold"
    );

    // Widening the pane from the outside makes room for candles the buffer does not hold.
    let page_node = pane.chart_page;
    let wide = StyleBuilder::new()
        .width(WIDE_PAGE.0)
        .height(WIDE_PAGE.1)
        .flex_col()
        .build();
    harness.with_cx(|cx| {
        cx.tree_mut()
            .set_style(page_node, wide)
            .expect("the page is alive")
    });
    resize_window(&mut harness, WINDOW_WIDE);
    pane.frame(&mut harness);

    assert!(
        pane.layout_width().expect("the chart was laid out again") > PAGE.0,
        "the plot grew with the pane"
    );
    assert_eq!(
        pane.widget.history_requests, 1,
        "the widened chart asks for the page its window now needs"
    );
    assert_eq!(
        pane.widget.prefetched_history_newest,
        Some(NewestCandle::Kept),
        "the newest candle is on screen, so the page may not evict it"
    );
}

#[test]
fn the_header_should_offer_the_instrument_control_and_the_settings_button() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.load(&mut harness);
    pane.headers(&mut harness);
    harness.drain();

    assert!(
        harness
            .find_by_debug_prefix(pane.left, "candlestick.header.instrument@")
            .is_some(),
        "the left group carries the instrument control"
    );
    assert!(
        harness
            .find_by_debug_prefix(pane.right, "candlestick.header.settings@")
            .is_some(),
        "the right group carries the settings control"
    );
    assert!(
        harness
            .find_by_debug_prefix(pane.right, "candlestick.header.timeframe@")
            .is_some(),
        "the right group carries the interval picker"
    );

    // With no loaded route the instrument control is disabled, and a disabled control is
    // built without a click handler, so nothing can dispatch an action it does not carry.
    let key = format_smolstr!("candlestick.header.instrument@{}.click", pane.scope.index());
    assert!(
        !harness.app().has_action_handler(&Action::Custom(key)),
        "the instrument control is dead until a route is loaded"
    );
}

#[test]
fn the_settings_button_should_be_dead_until_the_market_is_loaded() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.load(&mut harness);
    pane.headers(&mut harness);
    harness.drain();

    let key = format_smolstr!("candlestick.header.settings@{}.click", pane.scope.index());
    assert!(
        !harness.app().has_action_handler(&Action::Custom(key)),
        "a control whose market is not loaded draws disabled and registers no handler"
    );

    // The request signal is never written either: the gate is at the action's entry too.
    assert!(
        harness
            .app()
            .read(&pane.widget.requests)
            .to_owned()
            .is_none(),
        "no header request was made"
    );
}

#[test]
fn the_adjustment_control_should_follow_what_the_route_publishes() {
    let mut harness = harness();
    let mut single = pane_with_source(
        &mut harness,
        config(),
        FixtureSource::new(TIMEFRAMES.to_vec(), vec![Adjustment::None]),
        PAGE,
    );
    single.start_reading(&mut harness);
    single.headers(&mut harness);
    let mut both = pane_with_source(
        &mut harness,
        config(),
        FixtureSource::new(
            TIMEFRAMES.to_vec(),
            vec![Adjustment::None, Adjustment::Forward],
        ),
        PAGE,
    );
    both.start_reading(&mut harness);
    both.headers(&mut harness);
    harness.drain();

    assert!(
        harness
            .find_by_debug_prefix(single.right, "candlestick.header.adjustment@")
            .is_none(),
        "a route publishing one convention offers no adjustment control"
    );
    assert!(
        harness
            .find_by_debug_prefix(both.right, "candlestick.header.adjustment@")
            .is_some(),
        "a route publishing two draws the control"
    );
}

// -------------------------------------------------------------------- pagination

#[test]
fn a_live_edge_chart_at_capacity_should_not_ask_for_history() {
    let mut harness = harness();
    let mut pane = wide_pane(&mut harness);
    pane.start_reading(&mut harness);
    let capacity = MAXIMUM_CANDLES;
    let snapshot = live_edge_snapshot(
        capacity,
        i64::try_from(capacity).expect("the capacity fits an i64"),
    );
    assert!(snapshot.at_live_edge());
    pane.widget.viewport.candle_width = MIN_CANDLE_WIDTH;
    pane.accept(&mut harness, snapshot);
    pane.draw(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 0,
        "a page merged at capacity would evict the newest candle the chart follows"
    );
    assert!(!pane.widget.fetching_history);
    assert_eq!(pane.widget.history_requested_before, None);
    assert_eq!(pane.widget.prefetched_history_newest, None);
}

#[test]
fn the_history_prefetch_should_need_a_whole_page_of_room() {
    let mut harness = harness();
    let mut pane = wide_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.widget.viewport.candle_width = MIN_CANDLE_WIDTH;
    let almost = MAXIMUM_CANDLES - usize::from(HISTORY_PAGE_SIZE) + 1;
    let newest = i64::try_from(almost).expect("the fixture length fits an i64");
    pane.accept(&mut harness, live_edge_snapshot(almost, newest));
    pane.draw(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 0,
        "a page the buffer cannot take whole is not asked for"
    );

    // One candle of room more, and the page fits: the chart asks.
    let fitting = MAXIMUM_CANDLES - usize::from(HISTORY_PAGE_SIZE);
    let newest = i64::try_from(fitting).expect("the fixture length fits an i64");
    pane.accept(&mut harness, live_edge_snapshot(fitting, newest));
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 1,
        "with a whole page of room the prefetch goes out"
    );
}

#[test]
fn an_exhausted_series_should_not_be_asked_for_history() {
    let mut harness = harness();
    let mut pane = wide_pane(&mut harness);
    pane.start_reading(&mut harness);
    let page = usize::from(HISTORY_PAGE_SIZE);
    pane.accept(&mut harness, exhausted_snapshot(page));
    pane.draw(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 0,
        "a venue with no more history is not asked for it"
    );

    // The same page without the flag: nothing but the flag can keep the prefetch quiet.
    let mut series = CandlestickSeries::new(MAXIMUM_CANDLES, test_precision());
    series
        .merge_initial((1..=i64::from(HISTORY_PAGE_SIZE)).map(|timestamp| candle(timestamp, 100)));
    pane.accept(&mut harness, series.snapshot());
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 1,
        "the same window without the exhausted flag asks"
    );
}

#[test]
fn a_deeper_indicator_warmup_should_widen_the_history_prefetch_threshold() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let snapshot = panned_snapshot();
    assert!(!snapshot.at_live_edge());
    pane.widget.viewport.right_timestamp = Some(300);
    pane.accept(&mut harness, snapshot);
    pane.draw(&mut harness);
    let range_start = visible_range(
        pane.widget
            .snapshot
            .as_ref()
            .expect("the chart holds the fixture")
            .candles(),
        pane.widget.viewport,
        pane.widget
            .chart
            .layout()
            .expect("the chart was laid out")
            .maximum_plot_width(),
    )
    .start;

    assert_eq!(pane.widget.config.indicator.warmup_depth(), 19);
    assert!(
        range_start > PREFETCH_MARGIN,
        "the default threshold leaves nothing to ask for at this width: {range_start}"
    );
    assert_eq!(pane.widget.history_requests, 0);

    let mut deeper = pane.widget.config.indicator.clone();
    assert!(
        deeper.set_line_period(IndicatorId::Ma, 0, 500),
        "the fixture configuration holds MA's first line"
    );
    pane.with(&mut harness, |widget, cx| {
        widget.set_indicator_config(deeper, cx);
    });
    assert_eq!(pane.widget.config.indicator.warmup_depth(), 499);
    assert!(range_start <= pane.widget.config.indicator.warmup_depth());
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 1,
        "the warmer indicator is what moved the threshold past this window"
    );
}

#[test]
fn starting_over_should_drop_the_last_requested_pages() {
    let mut harness = harness();
    let mut pane = wide_pane(&mut harness);
    pane.start_reading(&mut harness);
    let page = usize::from(HISTORY_PAGE_SIZE);
    pane.accept(&mut harness, live_edge_snapshot(page, 300));
    pane.draw(&mut harness);
    assert!(pane.widget.fetching_history);
    let asked = pane.widget.history_requested_before;
    assert!(asked.is_some());
    let requests = pane.widget.history_requests;

    // An explicit start over cannot vouch for the pages the previous generation asked for.
    pane.with(&mut harness, |widget, cx| {
        widget.start_current(false, false, cx);
    });

    assert!(!pane.widget.fetching_history);
    assert_eq!(pane.widget.history_requested_before, None);
    assert!(pane.widget.snapshot.is_none());

    pane.accept(&mut harness, live_edge_snapshot(page, 300));
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.history_requests,
        requests + 1,
        "the page the old generation asked for is asked for again"
    );
}

#[test]
fn a_panned_chart_near_the_end_should_ask_for_newer_candles() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    // A buffer has to be full before an older page evicts the newest candle, which is what
    // takes the series off the live edge.
    let mut series = CandlestickSeries::new(300, test_precision());
    series.merge_initial((1..=300).map(|index| candle(index, 100)));
    series.merge_older(std::iter::once(candle(0, 100)), NewestCandle::Evicted);
    let snapshot = series.snapshot();
    assert!(!snapshot.at_live_edge());
    pane.widget.viewport.right_timestamp = Some(300);
    pane.accept(&mut harness, snapshot);
    pane.draw(&mut harness);

    assert!(
        pane.widget.fetching_newer,
        "the right edge of the window has no candles after it"
    );
    assert_eq!(
        pane.widget.newer_requested_after,
        pane.widget
            .snapshot
            .as_ref()
            .expect("a chart")
            .latest_timestamp()
    );
}

#[test]
fn an_anchored_chart_should_ask_for_history_with_the_newest_candle_evicted() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    // A window wheeled back from the live edge: the newest candle is off screen, so the
    // page the chart asks for may evict it.
    pane.widget.viewport.right_timestamp = Some(30);
    let snapshot = live_edge_snapshot(60, 60);
    let earliest = snapshot.earliest_timestamp();
    pane.accept(&mut harness, snapshot.clone());
    pane.draw(&mut harness);

    assert_eq!(
        pane.widget.prefetched_history_newest,
        Some(NewestCandle::Evicted),
        "only the chart knows the live-edge candle is off screen"
    );
    assert_eq!(pane.widget.history_requested_before, earliest);
    assert_eq!(pane.widget.history_requests, 1);

    // A settle ends the page; a snapshot that does not move the oldest candle keeps the
    // marker, so the same page is not asked for twice.
    pane.push_page_settled(PageDirection::History);
    pane.frame(&mut harness);
    assert!(!pane.widget.fetching_history);
    pane.accept(&mut harness, snapshot);
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.history_requests, 1,
        "a page the chart already asked for is not asked for again"
    );
    assert_eq!(pane.widget.history_requested_before, earliest);
}

#[test]
fn a_page_in_flight_should_not_be_replaced_by_the_other_direction() {
    let mut harness = harness();
    let mut pane = pane_with_source(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        (1_800.0, 400.0),
    );
    resize_window(&mut harness, (1_900.0, 600.0));
    pane.start_reading(&mut harness);
    let mut series = CandlestickSeries::new(300, test_precision());
    series.merge_initial((1..=300).map(|index| candle(index, 100)));
    series.merge_older(std::iter::once(candle(0, 100)), NewestCandle::Evicted);
    pane.widget.viewport.right_timestamp = Some(300);
    // Zoomed in far enough that only the right edge is near the margin: the newer page is
    // the one this window needs.
    pane.widget.viewport.candle_width = MAX_CANDLE_WIDTH;
    pane.accept(&mut harness, series.snapshot());
    pane.draw(&mut harness);
    assert!(pane.widget.fetching_newer, "the newer page went out");
    assert_eq!(pane.widget.history_requests, 0);

    // Zoomed out the window now wants the older page too. The binding has one RPC slot, so
    // the page in flight must not be replaced: a cancelled pagination request is one whose
    // settle never comes.
    pane.widget.viewport.candle_width = MIN_CANDLE_WIDTH;
    pane.frame(&mut harness);

    assert!(!pane.widget.fetching_history);
    assert_eq!(pane.widget.history_requests, 0);
    assert!(
        pane.widget.fetching_newer,
        "the page in flight is still the newer one"
    );

    // The settle the stream publishes is what ends the page: only then does the other
    // direction become askable.
    pane.push_page_settled(PageDirection::Newer);
    pane.frame(&mut harness);

    assert!(!pane.widget.fetching_newer, "the page in flight is over");
    assert_eq!(
        pane.widget.history_requests, 1,
        "with no page in flight, the direction the window now needs is asked for"
    );
}

// ------------------------------------------------------------------ rpc attribution

#[test]
fn the_answer_to_the_load_should_apply_and_an_unknown_one_should_be_dropped() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let in_flight = RequestId::new(1);

    // A reply the pane is waiting for is applied: this load failed.
    pane.push(Inbound::Rpc {
        request_id: in_flight,
        result: Err(RpcCallError::ServiceClosed {
            request_id: in_flight,
        }),
    });
    pane.frame(&mut harness);

    assert!(
        pane.status(&harness).is_failed(),
        "the answer to the command in flight is the pane's failure"
    );
    assert_eq!(pane.widget.pending.dropped(), 0);

    // A reply for a command this pane never issued is counted and dropped.
    let mut recovered = default_pane(&mut harness);
    recovered.start_reading(&mut harness);
    recovered.push(Inbound::Rpc {
        request_id: RequestId::new(99),
        result: Err(RpcCallError::ServiceClosed {
            request_id: RequestId::new(99),
        }),
    });
    recovered.frame(&mut harness);

    assert!(!recovered.status(&harness).is_failed());
    assert_eq!(
        recovered.widget.pending.dropped(),
        1,
        "a late reply answers no command in flight"
    );
}

#[test]
fn a_replaced_command_s_answer_should_not_overwrite_a_failure() {
    let mut harness = harness();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    let replaced = RequestId::new(1);
    // The stream failed for the scope the load opened; the load's own answer is still out.
    pane.push_failure(disconnect());
    pane.frame(&mut harness);
    assert!(pane.status(&harness).is_failed());

    // A new scope replaces the command; the replaced answer arrives afterwards.
    pane.with(&mut harness, |widget, cx| {
        widget.select_timeframe(TimeFrame::Minute5, cx);
    });
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
    pane.push(Inbound::Rpc {
        request_id: replaced,
        result: Err(RpcCallError::ServiceClosed {
            request_id: replaced,
        }),
    });
    pane.frame(&mut harness);

    assert!(
        !pane.status(&harness).is_failed(),
        "the answer to the replaced command is not the new one's"
    );
    assert_eq!(
        pane.widget.pending.dropped(),
        1,
        "and it is counted as a drop"
    );
}

// ------------------------------------------------------------- panes & release

#[test]
fn two_panes_should_live_side_by_side() {
    let mut harness = harness();
    let mut first = default_pane(&mut harness);
    let mut second = default_pane(&mut harness);
    first.load(&mut harness);
    second.load(&mut harness);
    first.headers(&mut harness);
    second.headers(&mut harness);
    harness.drain();

    let first_key = with_scope(&mut harness, first.scope, |cx| {
        cx.unique_key(INSTRUMENT_KEY_SUFFIX)
    });
    let second_key = with_scope(&mut harness, second.scope, |cx| {
        cx.unique_key(INSTRUMENT_KEY_SUFFIX)
    });
    assert_ne!(
        first_key, second_key,
        "two panes of this widget never share one action identity"
    );

    let first_instrument = harness
        .find_by_debug_prefix(first.left, "candlestick.header.instrument@")
        .expect("the first pane built its control");
    let second_instrument = harness
        .find_by_debug_prefix(second.left, "candlestick.header.instrument@")
        .expect("the second pane built its control");
    assert_ne!(
        first_instrument, second_instrument,
        "two panes never share one control node"
    );
    let first_id = harness
        .tree()
        .debug_id(first_instrument)
        .expect("the control carries its debug id")
        .to_owned();
    let second_id = harness
        .tree()
        .debug_id(second_instrument)
        .expect("the control carries its debug id")
        .to_owned();
    assert_ne!(
        first_id, second_id,
        "each pane's action identity is its own, so neither can unhook the other"
    );

    first.start_reading(&mut harness);
    second.start_reading(&mut harness);
    first.accept(&mut harness, live_edge_snapshot(10, 10));
    second.accept(&mut harness, live_edge_snapshot(10, 20));
    first.draw(&mut harness);
    second.draw(&mut harness);

    assert_ne!(
        first.canvas(&harness),
        second.canvas(&harness),
        "each pane draws into its own canvas"
    );
    assert!(first.widget.has_content());
    assert!(second.widget.has_content());
}

#[test]
fn releasing_one_pane_should_leave_the_other_s_header_controls_alive() {
    let mut harness = harness();
    let mut first = default_pane(&mut harness);
    let mut second = default_pane(&mut harness);
    first.load(&mut harness);
    second.load(&mut harness);
    first.headers(&mut harness);
    second.headers(&mut harness);
    harness.drain();
    let surviving = harness
        .find_by_debug_prefix(second.left, "candlestick.header.instrument@")
        .expect("the second pane built its control");
    let surviving_id = harness
        .tree()
        .debug_id(surviving)
        .expect("the control carries its debug id")
        .to_owned();

    first.release(&mut harness);

    assert!(
        harness.tree().get(surviving).is_ok(),
        "releasing one pane takes down only its own control"
    );
    assert_eq!(
        harness.tree().debug_id(surviving),
        Some(surviving_id.as_str()),
        "and the survivor keeps its own identity"
    );
    // The surviving pane still works: its stream and its hooks are untouched.
    second.start_reading(&mut harness);
    second.accept(&mut harness, live_edge_snapshot(10, 10));
    assert!(second.widget.has_content());
}

#[test]
fn releasing_the_pane_should_return_its_value_slots() {
    let mut harness = harness();
    let baseline = harness.signal_slot_stats();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(20, 20));
    pane.draw(&mut harness);
    pane.with(&mut harness, |widget, cx| {
        widget.toggle_indicator_panel(cx);
    });
    pane.frame(&mut harness);
    assert!(
        harness.signal_slot_stats().live > baseline.live,
        "the pane minted signals of its own"
    );

    pane.release(&mut harness);

    assert_eq!(
        harness.signal_slot_stats().live,
        baseline.live,
        "releasing the pane reclaims every value slot it minted"
    );

    // The slot table is a high-water mark: what a release has to make sure is that the next
    // mount fits in it instead of allocating more.
    let reused = harness.signal_slot_stats().slots;
    for _ in 0..8 {
        let mut pane = default_pane(&mut harness);
        pane.start_reading(&mut harness);
        pane.draw(&mut harness);
        pane.release(&mut harness);
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "each mount hands its slots back"
        );
    }
    assert_eq!(
        harness.signal_slot_stats().slots,
        reused,
        "the slot table is reused rather than grown"
    );
}

#[test]
fn releasing_the_pane_should_remove_its_own_nodes_and_stop_its_timer() {
    let mut harness = harness();
    let baseline = harness.app().geometry().len();
    let mut pane = default_pane(&mut harness);
    pane.start_reading(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(40, 40));
    pane.draw(&mut harness);
    pane.headers(&mut harness);
    pane.with(&mut harness, |widget, cx| {
        widget.toggle_indicator_panel(cx);
    });
    pane.frame(&mut harness);
    let canvas = pane.canvas(&harness);
    let panel = harness
        .find_by_debug_prefix(pane.chart_page, PANEL_PARENT_SELECTOR)
        .expect("the body built the panel's column");
    let panel_child = harness
        .tree()
        .get(panel)
        .expect("the column is alive")
        .children()
        .first()
        .copied();
    assert!(panel_child.is_some(), "the open panel built its rows");
    assert!(pane.widget.countdown_timer.is_some());
    assert!(harness.app().geometry().len() > baseline);

    pane.release(&mut harness);

    assert!(
        harness.tree().get(canvas).is_err(),
        "the canvas node is the widget's own and goes with it"
    );
    assert!(
        panel_child.is_some_and(|child| harness.tree().get(child).is_err()),
        "the panel's subtree is removed"
    );
    assert_eq!(
        harness.app().geometry().len(),
        baseline,
        "every mesh the chart registered is returned"
    );
    assert!(pane.widget.countdown_timer.is_none());
    assert!(
        pane.widget.binding.is_none(),
        "the released pane cancelled the binding its stream lived on"
    );
    assert!(
        pane.widget.instrument.is_none() && pane.widget.controls.is_none(),
        "the header controls are released with the pane"
    );
    assert!(pane.widget.panel.is_none(), "and so is the settings panel");

    // The countdown was the timer's only reason to wake: nothing is left to fire, so the
    // round that moves the clock past its deadline asks for no frame.
    let requests = harness.frame_requests();
    harness.advance(Duration::from_millis(1_100));
    assert_eq!(
        harness.frame_requests(),
        requests,
        "a released pane asks for no frame"
    );
}

// --------------------------------------------------------------------- the directory

/// A waiting pane recovers from a loaded venue by itself: the published generation moves,
/// the pane re-runs its load, and the market it was waiting for starts it.
#[test]
fn loading_the_venue_should_let_the_waiting_pane_start() {
    let mut harness = harness();
    let catalog = fixture_catalog(&[SYMBOL]);
    let directory = FixtureDirectory::new(Arc::clone(&catalog));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);
    assert!(pane.widget.awaiting_market, "the pane waits for its market");
    assert_eq!(pane.widget.generation, 0, "and starts no stream");
    assert!(matches!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded { .. })
    ));

    // The directory load commits: the venue is loaded and its catalogue is published.
    directory.load(route(), vec![CapabilityId::Candlesticks]);
    publish_generation(&mut harness, 1);
    pane.frame(&mut harness);

    assert!(
        !pane.widget.awaiting_market,
        "the pane left the notice without being reopened"
    );
    assert_eq!(
        pane.widget.generation, 1,
        "and it started the stream it was waiting for"
    );
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "the choice it was holding is still the one it reads"
    );
}

/// A listing the published catalogue provably does not list is a scope that cannot be
/// charted: the pane goes back to having nothing chosen and writes the reset back.
#[test]
fn a_selection_missing_from_the_directory_should_be_cleared_and_reported() {
    let mut harness = harness();
    let catalog = fixture_catalog(&["ETHUSDT"]);
    let directory = FixtureDirectory::with_spot_loaded(catalog);
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);

    assert_eq!(pane.widget.config.selection, None, "the selection is reset");
    assert!(
        pane.publishes.get() >= 1,
        "the reset has to be written back, or the next start resets it again"
    );
    assert_eq!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::message(
            "widget.candlestick.select_instrument_prompt"
        )),
        "a chart with nothing chosen asks for one"
    );
}

/// A listing the catalogue publishes but has settled is one no market call reads: the pane
/// resets it the way it resets a missing one.
#[test]
fn a_selection_for_a_terminal_listing_should_be_cleared_and_reported() {
    let mut harness = harness();
    let settled = test_support::with_status(
        test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            SYMBOL,
        ),
        ListingStatus::Settled,
    );
    let fixture = test_support::Fixture::new(vec![settled.clone()]);
    test_support::publish_fixture_route(fixture.catalog(), &route(), vec![settled])
        .expect("the fixture route publishes its settled listing");
    let directory = FixtureDirectory::with_spot_loaded(Arc::clone(fixture.catalog_arc()));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);

    assert_eq!(
        pane.widget.config.selection, None,
        "a settled listing is no longer chartable"
    );
    assert!(pane.publishes.get() >= 1, "and the reset is written back");
    assert!(!pane.status(&harness).is_failed(), "it is not a failure");
    assert_eq!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::message(
            "widget.candlestick.select_instrument_prompt"
        )),
        "a chart whose listing stopped trading asks for another one"
    );
    assert!(
        pane.widget.snapshot.is_none(),
        "nothing is drawn for a listing no market call reads"
    );
}

/// A lookup that failed proves nothing: the stored selection stays and the stream is asked
/// for anyway, so a directory that could not answer does not throw the user's choice away.
#[test]
fn a_selection_should_survive_a_lookup_that_proves_nothing() {
    let mut harness = harness();
    // The venue is loaded, but its catalogue is registered and never published: the lookup
    // is answered with "not loaded yet", which proves nothing about the listing.
    let catalog = fixture_catalog_unpublished(&[SYMBOL]);
    let directory = FixtureDirectory::with_spot_loaded(catalog);
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);

    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "a lookup that proved nothing keeps the selection"
    );
    assert_eq!(
        pane.widget.generation, 1,
        "and the chart asks for the stream anyway"
    );
    assert_eq!(pane.publishes.get(), 0, "nothing is written back");
}

/// A route the loaded venues answer with but the catalogue does not list is a market the
/// chart waits for, not a scope that stopped serving candles: the selection the user made is
/// not the pane's to throw away.
#[test]
fn a_revoked_route_should_leave_the_selection_alone_and_be_reported() {
    let mut harness = harness();
    let directory = FixtureDirectory::with_spot_loaded(fixture_catalog(&[SYMBOL]));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );
    pane.load(&mut harness);
    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "a listed scope starts the chart"
    );
    let publishes = pane.publishes.get();

    // A shrinking commit takes the market away, but the user's choice is theirs.
    directory.unload(&route());
    publish_generation(&mut harness, 1);
    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "a market that went away does not unmake the user's choice"
    );
    assert_eq!(
        pane.publishes.get(),
        publishes,
        "nothing about the choice changed, so nothing is written back"
    );
    assert!(pane.widget.awaiting_market);
    assert!(!pane.status(&harness).is_failed(), "waiting is not working");
    assert!(matches!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded { .. })
    ));
}

/// A market whose directory load never committed is a market the pane waits for: the
/// directory is empty, and the selection it was given is kept untouched.
#[test]
fn a_directory_failure_should_preserve_the_selection() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(fixture_catalog(&[SYMBOL]));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);

    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "a market that is not in memory must not change the persisted config"
    );
    assert_eq!(pane.publishes.get(), 0, "nothing is written back");
    assert_eq!(
        pane.status(&harness),
        WidgetState::Waiting(Notice::MarketNotLoaded {
            venues: vec![route().venue],
            supported: market_venues(),
        }),
        "the pane waits for its market rather than reading nothing"
    );
}

/// The lookup is advisory and the loaded venue is the prerequisite: a chart whose market was
/// never loaded starts no stream at all rather than subscribing to a route nobody has loaded.
#[test]
fn a_market_that_is_not_in_memory_should_not_start_a_stream() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(fixture_catalog(&[SYMBOL]));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );

    pane.load(&mut harness);

    assert_eq!(
        pane.widget.generation, 0,
        "an unloaded market starts no stream"
    );
    assert_eq!(
        pane.widget.pending.dropped(),
        0,
        "and nothing was asked for it either"
    );
    assert!(pane.widget.snapshot.is_none());
    assert!(pane.widget.awaiting_market);
}

/// A committed refresh of the chart's own venue reopens its stream: the catalogue generation
/// the live stream resolved its rules against was replaced, so the same scope is asked for
/// again — and the selection the user made is not what changed.
#[test]
fn a_refresh_of_the_charts_own_venue_should_reopen_its_stream() {
    let mut harness = harness();
    let directory = FixtureDirectory::with_spot_loaded(fixture_catalog(&[SYMBOL]));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );
    pane.load(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    assert_eq!(pane.widget.generation, 1, "the chart started one stream");
    let publishes = pane.publishes.get();

    // The venue is loaded and refreshed: its catalogue publishes a new generation.
    publish_generation(&mut harness, 1);
    pane.frame(&mut harness);

    assert!(
        !pane.widget.awaiting_market,
        "a market that is still loaded is not a wait"
    );
    assert_eq!(
        pane.widget.generation, 2,
        "the chart reopened the stream it was reading"
    );
    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "the user's choice is not what the refresh changed"
    );
    assert_eq!(
        pane.publishes.get(),
        publishes,
        "and nothing about it is written back"
    );
    assert_eq!(pane.status(&harness), WidgetState::Connecting);
}

/// A market taken away while the chart was reading goes back to waiting on the generation
/// change, and the stream it was reading is dropped with it.
#[test]
fn a_market_taken_away_after_a_generation_change_should_leave_the_pane_waiting() {
    let mut harness = harness();
    let directory = FixtureDirectory::with_spot_loaded(fixture_catalog(&[SYMBOL]));
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );
    pane.load(&mut harness);
    pane.accept(&mut harness, live_edge_snapshot(10, 10));
    assert!(pane.widget.snapshot.is_some(), "the chart is reading");

    directory.unload(&route());
    publish_generation(&mut harness, 2);
    pane.frame(&mut harness);

    assert!(pane.widget.awaiting_market, "the pane waits again");
    assert!(pane.widget.snapshot.is_none(), "the chart it drew is gone");
    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "and the choice it was reading stays"
    );
}

/// A route whose loaded scope does not offer candlesticks is a scope this chart cannot
/// follow: the selection the selector offered is refused rather than carried into a stream
/// nothing will publish.
#[test]
fn a_scope_without_candlesticks_should_be_refused() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(fixture_catalog(&["ETHUSDT"]));
    directory.load(route(), vec![CapabilityId::MarketCatalog]);
    let mut pane = pane_with_directory(
        &mut harness,
        config(),
        FixtureSource::publishing_all_intervals(),
        PAGE,
        directory.directory(),
    );
    let offered = selection("ETHUSDT");
    let signal = pane.widget.selection_requests;
    harness.app_mut().write(&signal, Some(offered));

    pane.frame(&mut harness);

    assert_eq!(
        pane.widget.config.selection,
        config().selection,
        "a scope that does not serve candles is not followed"
    );
    assert_eq!(pane.publishes.get(), 0, "and nothing is written back");
}
