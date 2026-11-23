use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use app_config::widget::WidgetConfig;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::{catalog::error::CatalogError, market::error::MarketError};
use market_participants_model::{
    identifier::{IdentifierValue, VenueId, VenueScopeId},
    listing::route::VenueRoute,
    prediction::{
        PredictionEventRef, PredictionEventView, PredictionOrderBook, PredictionPricePoint,
        PredictionRange, PredictionSeries,
    },
};
use ui_core::{
    event::{ActionSource, Event, InputEvent, Modifiers, PointerButton},
    math::Point,
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope},
};
use widget::{
    config::WidgetConfigChange,
    cx::WidgetCx,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    test_support::WidgetTestApp,
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::{
    book::{PredictionOrderBookError, PredictionOrderBookStatus},
    directory::MarketDirectory,
    service::{
        PredictionMarketFetchError, PredictionMarketSource, PredictionMarketStatus,
        PredictionMarketStreamMessage, PredictionSeriesEventSubscription,
    },
    test_support::{charted_market, event_ref, event_with, order_book},
};

const PAGE: (f32, f32) = (900.0, 520.0);

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn harness() -> WidgetTestApp {
    WidgetTestApp::with_translate(product_translate(LanguageCode::En))
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

struct FixtureDirectory {
    venues: RefCell<Vec<VenueId>>,
    routes: RefCell<Vec<VenueRoute>>,
}

impl FixtureDirectory {
    fn empty() -> Rc<Self> {
        Rc::new(Self {
            venues: RefCell::new(Vec::new()),
            routes: RefCell::new(Vec::new()),
        })
    }

    fn loaded(venues: &[VenueId]) -> Rc<Self> {
        let directory = Self::empty();
        for venue in venues {
            directory.load(*venue);
        }
        directory
    }

    fn load(&self, venue: VenueId) {
        if !self.venues.borrow().contains(&venue) {
            self.venues.borrow_mut().push(venue);
        }
        let route = VenueRoute::new(venue, VenueScopeId::new("predict").expect("canonical"));
        if !self.routes.borrow().contains(&route) {
            self.routes.borrow_mut().push(route);
        }
    }

    fn unload(&self, venue: VenueId) {
        self.venues.borrow_mut().retain(|loaded| *loaded != venue);
        self.routes
            .borrow_mut()
            .retain(|route| route.venue != venue);
    }
}

impl MarketDirectory for FixtureDirectory {
    fn any_event_market_loaded(&self) -> bool {
        !self.venues.borrow().is_empty()
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.routes.borrow().iter().any(|loaded| loaded == route)
    }

    fn catalog(&self) -> Option<Arc<market_participants_connector::catalog::store::Catalog>> {
        None
    }
}

#[derive(Clone)]
struct PendingSource;

struct PendingSubscription;

impl PredictionSeriesEventSubscription for PendingSubscription {
    async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
        std::future::pending().await
    }
}

impl PredictionMarketSource for PendingSource {
    type Subscription = PendingSubscription;

    async fn event(&self, _event: PredictionEventRef) -> Result<PredictionEventView, MarketError> {
        std::future::pending().await
    }

    async fn series(
        &self,
        _event: PredictionEventRef,
        _legs: Vec<market_participants_model::prediction::PredictionLeg>,
        _range: PredictionRange,
    ) -> Result<Vec<PredictionSeries>, MarketError> {
        std::future::pending().await
    }

    async fn subscribe_series(
        &self,
        _event: PredictionEventRef,
        _legs: Vec<market_participants_model::prediction::PredictionLeg>,
    ) -> Result<Self::Subscription, MarketError> {
        Ok(PendingSubscription)
    }
}

struct PendingBookSubscription;

impl crate::book::PredictionOrderBookEventSubscription for PendingBookSubscription {
    async fn recv(
        &mut self,
    ) -> Option<
        Result<Vec<market_participants_model::prediction::PredictionOrderBookUpdate>, MarketError>,
    > {
        std::future::pending().await
    }
}

impl crate::book::PredictionOrderBookSource for PendingSource {
    type Subscription = PendingBookSubscription;

    async fn fetch_order_book(
        &self,
        _target: crate::model::PredictionOrderBookTarget,
    ) -> Result<PredictionOrderBook, MarketError> {
        std::future::pending().await
    }

    async fn subscribe_order_book(
        &self,
        _target: crate::model::PredictionOrderBookTarget,
    ) -> Result<Self::Subscription, MarketError> {
        Ok(PendingBookSubscription)
    }
}

struct Pane {
    widget: PredictionMarketWidget,
    status: WidgetStatus,
    scope: SignalScope,
    publishes: Rc<Cell<usize>>,
    page: NodeId,
    header_left: NodeId,
    header_right: NodeId,
}

fn with_pane_cx<R>(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    body: impl FnOnce(&mut WidgetCx<'_>, &mut PredictionMarketWidget) -> R,
) -> R {
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let scope = pane.scope;
    let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
    body(&mut cx, &mut pane.widget)
}

fn release_pane(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let scope = pane.scope;
    with_pane_cx(harness, pane, |cx, widget| widget.release(cx));
    harness.app_mut().release_scope(scope);
}

fn page(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_PAGE")
            .style(
                ui_core::paint::style_builder::StyleBuilder::new()
                    .width(PAGE.0)
                    .height(PAGE.1)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the page builds")
    })
}

fn group(harness: &mut WidgetTestApp, parent: NodeId) -> NodeId {
    harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_HEADER_GROUP")
            .style(
                ui_core::paint::style_builder::StyleBuilder::new()
                    .flex_row()
                    .build(),
            )
            .build(cx.tree_mut(), parent)
            .expect("the header group builds")
    })
}

fn pane_over(
    harness: &mut WidgetTestApp,
    directory: Rc<FixtureDirectory>,
    config: PredictionMarketConfig,
) -> Pane {
    let scope = harness.app_mut().signal_scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_left = group(harness, page);
    let header_right = group(harness, page);
    let publishes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let counted = Rc::clone(&publishes);
    let config_change = WidgetConfigChange::new(move |_| counted.set(counted.get() + 1));
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let directory_handle: Rc<dyn MarketDirectory> = directory;
    let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
    let widget = PredictionMarketWidget::with_source(
        config,
        PendingSource,
        directory_handle,
        status,
        config_change,
        recovery(),
        &mut cx,
    );
    Pane {
        widget,
        status,
        scope,
        publishes,
        page,
        header_left,
        header_right,
    }
}

fn pane(harness: &mut WidgetTestApp, directory: &Rc<FixtureDirectory>) -> Pane {
    pane_over(
        harness,
        Rc::clone(directory),
        PredictionMarketConfig::default(),
    )
}

fn peer_pane(harness: &mut WidgetTestApp, directory: &Rc<FixtureDirectory>) -> Pane {
    pane(harness, directory)
}

fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
    with_pane_cx(harness, pane, |cx, widget| widget.load(cx));
}

fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
    with_pane_cx(harness, pane, |cx, widget| widget.sync(cx)).expect("the widget syncs");
}

fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.page;
    with_pane_cx(harness, pane, |cx, widget| widget.body(parent, cx)).expect("the body builds");
    harness.drain();
}

fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let left = pane.header_left;
    let right = pane.header_right;
    with_pane_cx(harness, pane, |cx, widget| {
        widget
            .header(HeaderGroup::Left, left, cx)
            .expect("the left group builds");
        widget
            .header(HeaderGroup::Right, right, cx)
            .expect("the right group builds");
    });
    harness.drain();
}

fn state(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
    pane.status.state(harness.app())
}

fn failed_state(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
    let state = state(harness, pane);
    assert!(state.is_failed(), "the pane is failed: {state:?}");
    state
}

fn select_event(pane: &mut Pane) {
    pane.widget.selection = Some(event_ref());
    pane.widget.selection_title = Some("An event".into());
}

fn reading(
    range: PredictionRange,
    revision: u64,
    points: &[(i64, &str)],
) -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, legs) = charted_market(&event, "market");
    let view = event_with(vec![market.clone()]);
    let series: Vec<PredictionSeries> = legs
        .iter()
        .map(|leg| PredictionSeries {
            leg: leg.clone(),
            points: points
                .iter()
                .map(|(timestamp, probability)| PredictionPricePoint {
                    timestamp: *timestamp,
                    probability: probability.parse().expect("the fixture probability parses"),
                })
                .collect(),
        })
        .collect();
    Arc::new(PredictionMarketSnapshot::new(
        revision, range, view, market, series,
    ))
}

fn empty_reading(range: PredictionRange, revision: u64) -> Arc<PredictionMarketSnapshot> {
    reading(range, revision, &[])
}

fn report_snapshot(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    snapshot: Arc<PredictionMarketSnapshot>,
) {
    let message = Arc::new(PredictionMarketStreamMessage::Snapshot(snapshot));
    with_pane_cx(harness, pane, |cx, widget| {
        let key = widget.current_key().expect("the pane follows a selection");
        widget.apply_market_latest(key, &message, cx);
    });
}

fn report_market_status(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    status: PredictionMarketStatus,
) {
    let message = Arc::new(PredictionMarketStreamMessage::Status(status));
    with_pane_cx(harness, pane, |cx, widget| {
        let key = widget.current_key().expect("the pane follows a selection");
        widget.apply_market_reliable(key, &message, cx);
    });
}

fn book_status(status: PredictionOrderBookStatus) -> Arc<PredictionOrderBookStreamMessage> {
    Arc::new(PredictionOrderBookStreamMessage::Status(status))
}

fn report_book_status(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    status: PredictionOrderBookStatus,
) {
    let message = book_status(status);
    with_pane_cx(harness, pane, |cx, widget| {
        let key = widget.book_key.clone().expect("the book is requested");
        widget.apply_book_reliable(key, &message, cx);
    });
}

fn report_book_snapshot(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    book: Arc<PredictionOrderBook>,
) {
    let message = Arc::new(PredictionOrderBookStreamMessage::Snapshot(book));
    with_pane_cx(harness, pane, |cx, widget| {
        let key = widget.book_key.clone().expect("the book is requested");
        widget.apply_book_latest(key, &message, cx);
    });
}

fn report_book(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    bids: Vec<(&str, &str)>,
    asks: Vec<(&str, &str)>,
) {
    let book = book_of(pane, bids, asks);
    report_book_snapshot(harness, pane, book);
}

fn book_of(
    pane: &Pane,
    bids: Vec<(&str, &str)>,
    asks: Vec<(&str, &str)>,
) -> Arc<PredictionOrderBook> {
    let target = pane
        .widget
        .supported_book_target()
        .expect("this build reads the fixture venue's book");
    Arc::new(order_book(
        target.leg,
        "0.01",
        bids.iter()
            .map(|(p, s)| crate::test_support::level(p, s))
            .collect(),
        asks.iter()
            .map(|(p, s)| crate::test_support::level(p, s))
            .collect(),
    ))
}

fn request(harness: &mut WidgetTestApp, pane: &mut Pane, request: HeaderRequest) {
    with_pane_cx(harness, pane, |cx, widget| {
        cx.write(&widget.requests, Some(request));
    });
    sync(harness, pane);
}

fn advance(harness: &mut WidgetTestApp, pane: &mut Pane, by: Duration) -> bool {
    let painted = harness.advance(by);
    sync(harness, pane);
    painted
}

fn ready_pane(harness: &mut WidgetTestApp, directory: &Rc<FixtureDirectory>) -> Pane {
    let mut pane = pane(harness, directory);
    select_event(&mut pane);
    load(harness, &mut pane);
    let snapshot = reading(
        PredictionRange::Day1,
        1,
        &[(1_000, "0.40"), (2_000, "0.60")],
    );
    report_snapshot(harness, &mut pane, snapshot);
    sync(harness, &mut pane);
    harness.drain();
    build_body(harness, &mut pane);
    build_header(harness, &mut pane);
    pane
}

#[test]
fn the_widget_and_its_config_have_their_stable_identity_and_defaults() {
    let config = PredictionMarketConfig::default();

    assert_eq!(
        PredictionMarketWidget::ID.as_str(),
        "kytrix.prediction_market"
    );
    assert_eq!(
        PredictionMarketWidget::DISPLAY_NAME,
        "widget.names.prediction_market"
    );
    assert_eq!(
        PredictionMarketWidget::CATEGORY,
        WidgetCategory::Market,
        "a prediction market is a market pane"
    );
    assert_eq!(config.range, PredictionRange::default());
    assert!(config.order_book_visible);
    assert_eq!(config.order_book_ratio, DEFAULT_BOOK_RATIO);
    assert_eq!(config.order_book_grouping, GROUPING_MULTIPLIERS[0]);
    assert_eq!(
        PredictionMarketConfig::SCHEMA_VERSION,
        app_config::widget::WidgetConfigSchemaVersion::V1
    );
}

#[test]
fn the_config_is_the_range_the_pane_was_built_with() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        range: PredictionRange::Hour6,
        ..PredictionMarketConfig::default()
    };
    let mut widget_pane = pane_over(&mut harness, Rc::clone(&directory), config);

    assert_eq!(
        widget_pane
            .widget
            .config()
            .expect("the pane persists")
            .range,
        PredictionRange::Hour6
    );
    select_event(&mut widget_pane);
    load(&mut harness, &mut widget_pane);
    assert_eq!(
        widget_pane
            .widget
            .current_key()
            .expect("a stream key")
            .range,
        PredictionRange::Hour6,
        "the load reads the range the pane was built with"
    );
}

#[test]
fn the_config_should_round_trip() {
    let config = PredictionMarketConfig {
        range: PredictionRange::Week1,
        order_book_visible: false,
        order_book_ratio: 0.35,
        order_book_grouping: 10,
    };

    let encoded = serde_json::to_string(&config).expect("the config encodes");
    let decoded: PredictionMarketConfig =
        serde_json::from_str(&encoded).expect("the config decodes");

    assert_eq!(decoded, config);
}

#[test]
fn a_pane_without_an_event_should_wait_with_the_select_prompt() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);

    load(&mut harness, &mut pane);

    assert_eq!(
        state(&harness, &pane),
        WidgetState::waiting(Notice::message(
            "widget.prediction_market.select_event_prompt"
        ))
    );
    assert!(!pane.widget.has_content());
}

#[test]
fn a_pane_without_a_loaded_market_should_wait_for_the_directory() {
    let mut harness = harness();
    let directory = FixtureDirectory::empty();
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);

    load(&mut harness, &mut pane);

    assert!(pane.widget.awaiting_market);
    assert_eq!(
        state(&harness, &pane),
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![VenueId::Polymarket],
            supported: event_venues(),
        })
    );
    assert!(!pane.widget.has_content());
}

#[test]
fn a_load_should_read_the_configured_range() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    pane.widget.config.range = PredictionRange::All;

    load(&mut harness, &mut pane);

    assert_eq!(state(&harness, &pane), WidgetState::Connecting);
    assert_eq!(
        pane.widget
            .current_key()
            .expect("the load asked for a stream")
            .range,
        PredictionRange::All
    );
    assert!(!pane.widget.pending.is_empty(), "the load is in flight");
}

#[test]
fn switching_the_range_should_drop_the_chart_and_connect_until_the_reading_lands() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    assert!(pane.widget.has_content());
    let publishes = pane.publishes.get();

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Range(PredictionRange::Week1),
    );

    assert_eq!(pane.widget.config.range, PredictionRange::Week1);
    assert_eq!(
        pane.publishes.get(),
        publishes + 1,
        "the choice is written back"
    );
    assert!(pane.widget.snapshot.is_none(), "the old reading is dropped");
    assert!(
        !pane.widget.has_content(),
        "the framework shows its spinner"
    );
    assert_eq!(state(&harness, &pane), WidgetState::Connecting);
    assert!(pane.widget.book_key.is_none(), "the book lease is released");
}

#[test]
fn a_reading_should_report_ready_and_the_reliable_connecting_should_report_connecting() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);

    report_market_status(&mut harness, &mut pane, PredictionMarketStatus::Connecting);
    assert_eq!(state(&harness, &pane), WidgetState::Connecting);

    report_snapshot(
        &mut harness,
        &mut pane,
        empty_reading(PredictionRange::Day1, 1),
    );
    assert_eq!(state(&harness, &pane), WidgetState::Ready);
    assert!(
        pane.widget.has_content(),
        "an empty reading is still content"
    );
}

#[test]
fn a_reliable_failure_should_report_the_pane_failed() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );

    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::Timeout(Duration::from_secs(10)),
        },
    );

    failed_state(&harness, &pane);
    assert!(pane.widget.book_key.is_none(), "the book lease is released");
    assert!(
        pane.widget.snapshot.is_some(),
        "the reading the user was reading stays behind the banner"
    );
}

#[test]
fn a_terminal_chart_failure_should_release_the_book_lease() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    assert!(pane.widget.book_key.is_some());

    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::NoMarkets,
        },
    );

    assert!(pane.widget.book_key.is_none());
    assert!(pane.widget.book_pending.is_empty() || true);
}

#[test]
fn a_chart_whose_market_is_not_loaded_should_clear_the_book() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    sync(&mut harness, &mut pane);
    assert!(
        pane.widget.book_snapshot.is_some(),
        "the ladder is on screen before the venue refuses"
    );
    assert!(pane.widget.book_panel.grouped.is_some());

    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::Market(MarketError::Catalog(Box::new(
                CatalogError::CatalogNotLoaded {
                    route: event_ref().route,
                },
            ))),
        },
    );

    assert!(
        pane.widget.awaiting_market,
        "the chart waits for its market"
    );
    assert!(pane.widget.snapshot.is_none(), "the reading is dropped");
    assert!(
        pane.widget.book_key.is_none(),
        "the book's stream is retired"
    );
    assert!(
        pane.widget.book_snapshot.is_none(),
        "the book of a market not in memory is dropped"
    );
    assert!(
        pane.widget.book_panel.grouped.is_none(),
        "the panel forgets the ladder it can no longer vouch for"
    );
    assert_eq!(
        state(&harness, &pane),
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![VenueId::Polymarket],
            supported: event_venues(),
        })
    );
}

#[test]
fn a_message_from_another_range_or_an_older_revision_should_be_refused() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let held = pane.widget.snapshot.clone().expect("a reading");

    let stale_key = PredictionMarketStreamKey {
        event: event_ref(),
        market: None,
        range: PredictionRange::Week1,
        epoch: 0,
    };
    let other = reading(PredictionRange::Week1, 9, &[(3_000, "0.9")]);
    let message = Arc::new(PredictionMarketStreamMessage::Snapshot(other));
    harness.with_cx(|cx| pane.widget.apply_market_latest(stale_key, &message, cx));

    assert!(Arc::ptr_eq(
        &held,
        pane.widget.snapshot.as_ref().expect("kept")
    ));

    let key = pane.widget.current_key().expect("a key");
    let older = reading(PredictionRange::Day1, 0, &[(1_000, "0.1")]);
    let message = Arc::new(PredictionMarketStreamMessage::Snapshot(older));
    harness.with_cx(|cx| pane.widget.apply_market_latest(key, &message, cx));

    assert!(Arc::ptr_eq(
        &held,
        pane.widget.snapshot.as_ref().expect("kept")
    ));
}

#[test]
fn a_reliable_message_should_be_refused_by_key_waiting_and_failure() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        empty_reading(PredictionRange::Day1, 1),
    );

    let other_key = PredictionMarketStreamKey {
        event: event_ref(),
        market: None,
        range: PredictionRange::Week1,
        epoch: 0,
    };
    let message = Arc::new(PredictionMarketStreamMessage::Status(
        PredictionMarketStatus::Connecting,
    ));
    harness.with_cx(|cx| pane.widget.apply_market_reliable(other_key, &message, cx));
    assert_eq!(state(&harness, &pane), WidgetState::Ready);

    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::NoMarkets,
        },
    );
    let key = pane.widget.current_key().expect("a key");
    let message = Arc::new(PredictionMarketStreamMessage::Status(
        PredictionMarketStatus::Ready { revision: 2 },
    ));
    harness.with_cx(|cx| pane.widget.apply_market_reliable(key, &message, cx));
    failed_state(&harness, &pane);
}

#[test]
fn a_directory_generation_should_rerun_load_and_keep_the_reading() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let held = pane.widget.snapshot.clone().expect("a reading");
    assert!(pane.widget.seen_generation == 0);

    let generation = harness.services().catalog_generation;
    assert!(harness.app_mut().write(&generation, 1));
    sync(&mut harness, &mut pane);

    assert_eq!(pane.widget.seen_generation, 1);
    assert!(
        pane.widget
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &held)),
        "a generation refresh keeps the reading the user is looking at"
    );
    assert_eq!(
        state(&harness, &pane),
        WidgetState::Connecting,
        "the new generation is read under the spinner"
    );
    assert!(
        !pane.widget.pending.is_empty(),
        "the reload asks for a new stream generation"
    );
}

#[test]
fn a_directory_generation_should_drop_a_reading_whose_market_left() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    directory.unload(VenueId::Polymarket);

    let generation = harness.services().catalog_generation;
    assert!(harness.app_mut().write(&generation, 1));
    sync(&mut harness, &mut pane);

    assert!(pane.widget.awaiting_market);
    assert!(pane.widget.snapshot.is_none());
    assert!(pane.widget.book_key.is_none());
    assert_eq!(
        state(&harness, &pane),
        WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![VenueId::Polymarket],
            supported: event_venues(),
        })
    );
}

#[test]
fn a_failed_pane_should_ignore_a_venue_commit() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::NoMarkets,
        },
    );
    let failed = state(&harness, &pane);
    let publishes = pane.publishes.get();

    let generation = harness.services().catalog_generation;
    assert!(harness.app_mut().write(&generation, 2));
    sync(&mut harness, &mut pane);

    assert_eq!(state(&harness, &pane), failed, "a failure is terminal");
    assert_eq!(pane.publishes.get(), publishes);
}

#[test]
fn an_explicit_load_should_start_over_while_a_recovery_keeps_the_reading() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let held = pane.widget.snapshot.clone().expect("a reading");

    pane.widget.snapshot = None;
    load(&mut harness, &mut pane);
    assert!(pane.widget.snapshot.is_none());

    report_snapshot(&mut harness, &mut pane, held.clone());
    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::NoMarkets,
        },
    );
    load(&mut harness, &mut pane);
    assert!(
        pane.widget
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &held)),
        "a retry keeps the reading the user was reading"
    );
    assert_eq!(
        state(&harness, &pane),
        WidgetState::Connecting,
        "the retry is reported as work in progress"
    );
}

#[test]
fn an_empty_reading_should_keep_the_legend_and_the_ranges_around_an_empty_chart() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        empty_reading(PredictionRange::Day1, 1),
    );
    sync(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);

    let legend = harness
        .find_by_debug_prefix(pane.page, "TEST_PREDICTION_MARKET_LEGEND")
        .expect("the legend is built");
    let text = harness.text_of(legend);
    assert!(
        text.contains("Yes"),
        "the legend names the first side: {text:?}"
    );
    assert!(
        text.contains("No"),
        "the legend names the second side: {text:?}"
    );

    let empty = harness
        .find_by_debug_prefix(pane.page, "TEST_PREDICTION_MARKET_EMPTY")
        .expect("the empty overlay is built");
    assert!(
        harness.visible(empty),
        "an empty reading says so over the chart"
    );
    assert!(
        pane.widget
            .chart
            .node()
            .is_some_and(|node| harness.has_canvas(node)),
        "the chart is a canvas even when it has no point to draw"
    );
}

#[test]
fn a_reading_with_points_should_draw_the_legend_above_the_chart() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let pane = ready_pane(&mut harness, &directory);

    let legend = harness
        .find_by_debug_prefix(pane.page, "TEST_PREDICTION_MARKET_LEGEND")
        .expect("the legend is built");
    let text = harness.text_of(legend);
    assert!(
        text.contains("Yes 60%"),
        "the legend states the latest value: {text:?}"
    );
    assert!(text.contains("No 60%"), "both sides are read: {text:?}");

    let chart = pane.widget.chart.node().expect("the chart node");
    assert!(harness.has_canvas(chart), "the chart draws a canvas");
    let empty = harness
        .find_by_debug_prefix(pane.page, "TEST_PREDICTION_MARKET_EMPTY")
        .expect("the empty overlay is built");
    assert!(
        !harness.visible(empty),
        "a reading with points does not say it has no data"
    );
}

#[test]
fn a_live_reading_should_pulse_only_when_the_newest_point_advances() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);

    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    assert!(
        pane.widget.pulse.is_none(),
        "a first reading is not a new observation"
    );

    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 2, &[(1_000, "0.4")]),
    );
    assert!(pane.widget.pulse.is_none());

    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 3, &[(1_000, "0.4"), (2_000, "0.5")]),
    );
    assert!(pane.widget.pulse.is_some(), "the newest point advanced");
}

#[test]
fn a_tick_during_a_running_pulse_should_not_restart_it() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(
            PredictionRange::Day1,
            2,
            &[(1_000, "0.4"), (2_000, "0.5"), (3_000, "0.6")],
        ),
    );
    assert!(pane.widget.pulse.is_some());
    sync(&mut harness, &mut pane);
    advance(&mut harness, &mut pane, Duration::from_millis(16));
    assert!(pane.widget.pulse.is_some());
    assert_eq!(
        pane.widget.pulse_seen_ticks, 1,
        "one tick advanced the breath"
    );

    report_snapshot(
        &mut harness,
        &mut pane,
        reading(
            PredictionRange::Day1,
            3,
            &[(1_000, "0.4"), (2_000, "0.5"), (3_000, "0.6")],
        ),
    );
    sync(&mut harness, &mut pane);
    assert_eq!(pane.widget.pulse_seen_ticks, 1);
}

#[test]
fn the_endpoint_pulse_should_run_one_cycle_and_then_end() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(
            PredictionRange::Day1,
            2,
            &[(1_000, "0.4"), (2_000, "0.5"), (3_000, "0.6")],
        ),
    );
    assert!(pane.widget.pulse.is_some());

    for _ in 0..(PULSE_DURATION_MS / FRAME_TICK_MS + 2) {
        advance(&mut harness, &mut pane, Duration::from_millis(16));
    }

    assert!(pane.widget.pulse.is_none(), "the effect ended");
    assert!(
        pane.widget.pulse_timer.is_none(),
        "and stopped asking for frames"
    );
}

#[test]
fn reduce_motion_should_skip_the_pulse() {
    let mut harness = harness();
    let reduce = harness.services().reduce_motion;
    assert!(harness.app_mut().write(&reduce, true));
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);

    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 2, &[(1_000, "0.4"), (2_000, "0.5")]),
    );

    assert!(
        pane.widget.pulse.is_none(),
        "a reader who asked for less motion is shown the final state"
    );
}

#[test]
fn a_chart_snapshot_should_start_the_book_request_with_the_markets_legs() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    let snapshot = reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]);
    let target = snapshot
        .book_target()
        .expect("the market has a book target");
    report_snapshot(&mut harness, &mut pane, snapshot);

    let key = pane.widget.book_key.clone().expect("the book is requested");
    assert_eq!(key.event, target.event);
    assert_eq!(key.leg, target.leg);
    assert!(!pane.widget.book_pending.is_empty());
}

#[test]
fn a_book_reading_from_a_book_the_pane_no_longer_follows_should_be_refused() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let stale_key = PredictionOrderBookStreamKey {
        event: event_ref(),
        leg: pane.widget.book_key.as_ref().expect("a book").leg.clone(),
        epoch: 0,
    };
    let book = book_of(&pane, vec![("0.4", "10")], vec![("0.6", "10")]);
    let message = Arc::new(PredictionOrderBookStreamMessage::Snapshot(book));

    harness.with_cx(|cx| pane.widget.apply_book_latest(stale_key, &message, cx));

    assert!(pane.widget.book_snapshot.is_none());
}

#[test]
fn a_book_failure_should_be_shown_in_the_panel_and_recoverable() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let book = book_of(&pane, vec![("0.4", "10")], vec![("0.6", "10")]);
    report_book_snapshot(&mut harness, &mut pane, book);
    sync(&mut harness, &mut pane);

    report_book_status(
        &mut harness,
        &mut pane,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
        },
    );
    sync(&mut harness, &mut pane);
    assert!(matches!(
        pane.widget.book_region(),
        BookRegion::Reading { failed: Some(_) }
    ));

    let old_key = pane.widget.book_key.clone().expect("a book");
    request(&mut harness, &mut pane, HeaderRequest::RetryBook);

    assert!(
        pane.widget.book_error.is_none(),
        "the error stops being the answer"
    );
    assert_ne!(
        pane.widget.book_key.as_ref().expect("a book"),
        &old_key,
        "a retry is a new generation of the same book"
    );
    assert!(
        pane.widget.book_snapshot.is_some(),
        "the ladder the reader was reading stays on screen"
    );
}

#[test]
fn a_book_failure_without_a_ladder_should_offer_a_retry() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    assert!(pane.widget.book_snapshot.is_none());
    report_book_status(
        &mut harness,
        &mut pane,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(Duration::from_secs(5)),
        },
    );
    sync(&mut harness, &mut pane);
    harness.drain();

    let window = harness.window();
    let chrome = pane.widget.book_chrome.as_ref().expect("chrome");
    let node = chrome.status_retry_node;
    let retry = chrome
        .status_retry
        .action_identity()
        .expect("the retry acts");
    let label = chrome
        .status_retry
        .label_text()
        .expect("the way back is labelled")
        .to_owned();
    assert!(
        harness.visible(chrome.status_retry_parent),
        "a failed region shows the way back"
    );
    assert!(!label.is_empty());
    let old_key = pane.widget.book_key.clone().expect("a book");

    assert!(
        harness
            .app_mut()
            .dispatch_action(window, node, retry, ActionSource::Programmatic)
    );
    sync(&mut harness, &mut pane);

    assert!(
        pane.widget.book_error.is_none(),
        "a retry stops being the answer"
    );
    assert_ne!(
        pane.widget.book_key.as_ref().expect("a book"),
        &old_key,
        "a retry is a new generation of the same book"
    );

    harness.switch_language(LanguageCode::ZhCn);
    sync(&mut harness, &mut pane);
    let rebuilt = pane.widget.book_chrome.as_ref().expect("chrome");
    assert_eq!(rebuilt.retry_language, LanguageCode::ZhCn);
    assert_eq!(
        rebuilt
            .status_retry
            .label_text()
            .expect("the way back is labelled"),
        "\u{91cd}\u{8bd5}",
        "the retry is rebuilt in the language showing now"
    );
    assert!(harness.tree().get(node).is_err(), "the old button is gone");
}

#[test]
fn the_toggle_should_hide_the_book_panel_and_report_the_change() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    assert!(pane.widget.config.order_book_visible);
    assert!(pane.widget.book_key.is_some());
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    let publishes = pane.publishes.get();

    request(&mut harness, &mut pane, HeaderRequest::ToggleBook);

    assert!(!pane.widget.config.order_book_visible);
    assert_eq!(
        pane.publishes.get(),
        publishes + 1,
        "the choice is written back"
    );
    assert!(
        pane.widget.book_key.is_none(),
        "a hidden book holds no stream"
    );
    assert!(!pane.widget.shows_book(), "the region is not drawn");
    assert!(
        pane.widget.book_chrome.is_none(),
        "its nodes are taken down"
    );
    assert!(
        pane.widget.book_snapshot.is_some(),
        "the last ladder stays in memory so showing it again is instant"
    );
}

#[test]
fn a_hidden_book_should_not_read_until_it_is_shown() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        order_book_visible: false,
        ..PredictionMarketConfig::default()
    };
    let mut pane = pane_over(&mut harness, Rc::clone(&directory), config);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    sync(&mut harness, &mut pane);

    assert!(
        pane.widget.book_key.is_none(),
        "nothing draws it, nothing reads it"
    );
    assert!(pane.widget.book_chrome.is_none());

    request(&mut harness, &mut pane, HeaderRequest::ToggleBook);

    assert!(
        pane.widget.book_key.is_some(),
        "showing it starts the reading"
    );
}

#[test]
fn the_splitter_should_be_drawn_only_while_the_book_is_visible() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    assert!(
        pane.widget
            .book_chrome
            .as_ref()
            .is_some_and(|chrome| harness.tree().get(chrome.splitter).is_ok())
    );

    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    request(&mut harness, &mut pane, HeaderRequest::ToggleBook);
    harness.drain();
    assert!(
        harness.tree().get(splitter).is_err(),
        "hiding the book takes the splitter down"
    );
    assert!(pane.widget.book_chrome.is_none());
}

#[test]
fn the_configured_ratio_should_move_the_panel_against_the_chart() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let pane = ready_pane(&mut harness, &directory);
    let area = pane.widget.book_chrome.as_ref().expect("chrome").area;

    assert_eq!(
        harness
            .tree()
            .get(area)
            .expect("the area")
            .style()
            .inner()
            .size
            .height,
        taffy::style::Dimension::percent(0.5),
        "the stored notch is the height the region was built with"
    );
}

#[test]
fn a_splitter_drag_should_follow_the_pointer_and_report_once() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    let bounds = harness
        .tree()
        .get(splitter)
        .expect("the splitter")
        .layout()
        .bounds;
    let start = Point::new(bounds.origin.x + 10.0, bounds.origin.y + 2.0);
    let publishes = pane.publishes.get();

    let down = Event::Input(InputEvent::PointerDown {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    assert!(harness.with_cx(|cx| pane.widget.dispatch(&down, cx)));
    assert!(pane.widget.split_drag.is_some());

    let moved = Event::Input(InputEvent::PointerMove {
        position: Point::new(start.x, start.y - 40.0),
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    assert!(harness.with_cx(|cx| pane.widget.dispatch(&moved, cx)));
    assert!(
        pane.widget.config.order_book_ratio > 0.5,
        "dragging up grows the book"
    );
    assert!(pane.widget.split_dragging);
    assert_eq!(
        pane.publishes.get(),
        publishes,
        "a drag reports nothing until it ends"
    );

    let up = Event::Input(InputEvent::PointerUp {
        position: Point::new(start.x, start.y - 40.0),
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    assert!(harness.with_cx(|cx| pane.widget.dispatch(&up, cx)));

    assert_eq!(
        pane.publishes.get(),
        publishes + 1,
        "the notch is reported once"
    );
    assert!(pane.widget.split_drag.is_none());
}

#[test]
fn a_splitter_drag_past_its_bounds_should_stop_at_the_clamp() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    let bounds = harness
        .tree()
        .get(splitter)
        .expect("the splitter")
        .layout()
        .bounds;
    let start = Point::new(bounds.origin.x + 10.0, bounds.origin.y + 2.0);

    let down = Event::Input(InputEvent::PointerDown {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&down, cx));
    let moved = Event::Input(InputEvent::PointerMove {
        position: Point::new(start.x, start.y - 10_000.0),
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&moved, cx));

    assert_eq!(pane.widget.config.order_book_ratio, MAX_BOOK_RATIO);
}

#[test]
fn a_click_outside_the_splitter_should_not_report_the_ratio() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);

    let down = Event::Input(InputEvent::PointerDown {
        position: Point::new(1.0, 1.0),
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });

    assert!(!harness.with_cx(|cx| pane.widget.dispatch(&down, cx)));
    assert!(pane.widget.split_drag.is_none());
}

#[test]
fn a_splitter_gesture_interrupted_by_hiding_should_not_report_its_notch() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    let bounds = harness
        .tree()
        .get(splitter)
        .expect("the splitter")
        .layout()
        .bounds;
    let start = Point::new(bounds.origin.x + 10.0, bounds.origin.y + 2.0);
    let down = Event::Input(InputEvent::PointerDown {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&down, cx));
    let moved = Event::Input(InputEvent::PointerMove {
        position: Point::new(start.x, start.y - 30.0),
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&moved, cx));

    request(&mut harness, &mut pane, HeaderRequest::ToggleBook);
    let publishes = pane.publishes.get();
    let up = Event::Input(InputEvent::PointerUp {
        position: Point::new(start.x, start.y - 30.0),
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&up, cx));

    assert_eq!(
        pane.publishes.get(),
        publishes,
        "the interrupted gesture chose nothing"
    );
    assert!(pane.widget.split_drag.is_none());
}

#[test]
fn a_splitter_drag_back_to_its_start_should_not_report() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    let bounds = harness
        .tree()
        .get(splitter)
        .expect("the splitter")
        .layout()
        .bounds;
    let start = Point::new(bounds.origin.x + 10.0, bounds.origin.y + 2.0);
    let publishes = pane.publishes.get();
    let down = Event::Input(InputEvent::PointerDown {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&down, cx));

    let away = Event::Input(InputEvent::PointerMove {
        position: Point::new(start.x, start.y - 40.0),
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&away, cx));
    assert!(
        pane.widget.config.order_book_ratio > 0.5,
        "the gesture moved the notch"
    );
    let back = Event::Input(InputEvent::PointerMove {
        position: start,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&back, cx));
    assert_eq!(
        pane.widget.config.order_book_ratio, 0.5,
        "and moved it back to where it started"
    );

    let up = Event::Input(InputEvent::PointerUp {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&up, cx));

    assert_eq!(
        pane.publishes.get(),
        publishes,
        "a gesture that ended where it started reports nothing"
    );
    assert!(pane.widget.split_drag.is_none());
}

#[test]
fn a_short_pane_should_not_store_a_notch_below_the_drawn_minimum() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let splitter = pane.widget.book_chrome.as_ref().expect("chrome").splitter;
    let bounds = harness
        .tree()
        .get(splitter)
        .expect("the splitter")
        .layout()
        .bounds;
    let area_height = pane
        .widget
        .split_area
        .and_then(|area| harness.tree().get(area).ok())
        .map_or(0.0, |area| area.layout().bounds.size.height);
    assert!(area_height > 0.0);
    let start = Point::new(bounds.origin.x + 10.0, bounds.origin.y + 2.0);
    let down = Event::Input(InputEvent::PointerDown {
        position: start,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&down, cx));
    let moved = Event::Input(InputEvent::PointerMove {
        position: Point::new(start.x, start.y + 10_000.0),
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    harness.with_cx(|cx| pane.widget.dispatch(&moved, cx));

    let floor = (MIN_BOOK_HEIGHT / area_height).max(MIN_BOOK_RATIO);
    assert!(
        pane.widget.config.order_book_ratio >= floor - f32::EPSILON,
        "the stored notch never asks for less than the drawn minimum: {} < {floor}",
        pane.widget.config.order_book_ratio
    );
}

#[test]
fn a_venue_reload_should_start_the_book_over() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let first = pane.widget.book_key.clone().expect("a book");
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );

    let generation = harness.services().catalog_generation;
    assert!(harness.app_mut().write(&generation, 3));
    sync(&mut harness, &mut pane);

    assert!(pane.widget.book_key.is_none(), "the old book lease is gone");
    assert!(
        pane.widget.book_snapshot.is_some(),
        "the ladder the reader was reading stays on screen"
    );
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 2, &[(1_000, "0.4"), (2_000, "0.5")]),
    );
    let second = pane.widget.book_key.clone().expect("requested again");
    assert!(
        second.epoch > first.epoch,
        "the reload is a new book generation"
    );
}

#[test]
fn a_book_error_with_a_reading_should_draw_a_strip_above_the_panel() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    report_book_status(
        &mut harness,
        &mut pane,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(Duration::from_secs(5)),
        },
    );
    sync(&mut harness, &mut pane);
    harness.drain();

    let chrome = pane.widget.book_chrome.as_ref().expect("chrome");
    let strip = harness
        .tree()
        .get(chrome.strip)
        .expect("the strip is still built");
    assert!(
        strip.layout().bounds.size.height > 0.0,
        "a failure after a reading keeps the ladder and puts a strip above it"
    );
    let ladder = harness.tree().get(chrome.ladder).expect("the ladder");
    assert!(ladder.layout().bounds.size.height > 0.0);
}

#[test]
fn the_book_region_should_say_loading_and_then_show_the_empty_ladder() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    build_body(&mut harness, &mut pane);
    sync(&mut harness, &mut pane);
    assert_eq!(pane.widget.book_region(), BookRegion::Loading);

    report_book(&mut harness, &mut pane, vec![], vec![]);
    sync(&mut harness, &mut pane);

    assert!(matches!(
        pane.widget.book_region(),
        BookRegion::Reading { failed: None }
    ));
    assert!(
        pane.widget.book_panel.is_empty(),
        "an empty page has no ladder"
    );
}

#[test]
fn a_hyperliquid_event_should_not_ask_for_an_order_book() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Hyperliquid]);
    let mut pane = pane(&mut harness, &directory);
    let hyperliquid = PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new("outcomes").expect("canonical"),
        ),
        native_event: IdentifierValue::new("event").expect("valid"),
    };
    pane.widget.selection = Some(hyperliquid);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    sync(&mut harness, &mut pane);

    assert!(pane.widget.supported_book_target().is_none());
    assert!(
        pane.widget.book_key.is_none(),
        "this build cannot read that book"
    );
    assert!(pane.widget.book_chrome.is_none());
}

#[test]
fn a_late_snapshot_after_a_book_failure_should_be_refused() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book_status(
        &mut harness,
        &mut pane,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(Duration::from_secs(5)),
        },
    );

    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );

    assert!(
        pane.widget.book_snapshot.is_none(),
        "a snapshot of the failed stream describes the book the panel gave up on"
    );
}

#[test]
fn a_retry_should_keep_the_last_ladder() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    report_book_status(
        &mut harness,
        &mut pane,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(Duration::from_secs(5)),
        },
    );

    request(&mut harness, &mut pane, HeaderRequest::RetryBook);

    assert!(
        pane.widget.book_snapshot.is_some(),
        "the reader's ladder stays"
    );
    assert!(pane.widget.book_error.is_none());
}

#[test]
fn a_book_message_should_be_refused_once_the_pane_failed_or_waits() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let key = pane.widget.book_key.clone().expect("a book");
    report_market_status(
        &mut harness,
        &mut pane,
        PredictionMarketStatus::Failed {
            error: PredictionMarketFetchError::NoMarkets,
        },
    );
    let message = Arc::new(PredictionOrderBookStreamMessage::Snapshot(book_of(
        &pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    )));

    harness.with_cx(|cx| pane.widget.apply_book_latest(key, &message, cx));

    assert!(
        pane.widget.book_snapshot.is_none(),
        "a failed pane takes no book"
    );
}

#[test]
fn a_load_without_a_loaded_market_should_clear_the_book() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    directory.unload(VenueId::Polymarket);

    load(&mut harness, &mut pane);

    assert!(pane.widget.awaiting_market);
    assert!(pane.widget.book_key.is_none());
    assert!(pane.widget.book_snapshot.is_none());
    assert!(pane.widget.snapshot.is_none());
}

#[test]
fn a_venue_unload_should_drop_the_book() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    directory.unload(VenueId::Polymarket);
    let generation = harness.services().catalog_generation;
    assert!(harness.app_mut().write(&generation, 4));
    sync(&mut harness, &mut pane);

    assert!(pane.widget.book_snapshot.is_none());
    assert!(pane.widget.book_key.is_none());
}

#[test]
fn the_grouping_select_should_appear_only_with_a_book() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let state = harness.with_cx(|cx| pane.widget.header_state(cx));
    assert!(
        state.grouping.is_none(),
        "without a reading there are no steps to offer"
    );

    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    let state = harness.with_cx(|cx| pane.widget.header_state(cx));
    assert!(
        state.grouping.is_some(),
        "a book with levels offers its steps"
    );
}

#[test]
fn a_tick_with_no_step_should_not_offer_a_grouping() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let target = pane.widget.supported_book_target().expect("a target");
    let book = Arc::new(order_book(
        target.leg,
        "1",
        vec![crate::test_support::level("0.4", "10")],
        vec![crate::test_support::level("0.6", "10")],
    ));
    report_book_snapshot(&mut harness, &mut pane, book);

    assert!(!pane.widget.book_panel.has_grouping_choice());
    let state = harness.with_cx(|cx| pane.widget.header_state(cx));
    assert!(state.grouping.is_none());
}

#[test]
fn an_empty_book_should_not_offer_a_grouping() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(&mut harness, &mut pane, vec![], vec![]);

    assert!(!pane.widget.book_panel.has_grouping_choice());
}

#[test]
fn choosing_a_grouping_should_persist_and_regroup() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_book(
        &mut harness,
        &mut pane,
        vec![("0.4", "10")],
        vec![("0.6", "10")],
    );
    let publishes = pane.publishes.get();
    let previous = pane.widget.book_panel.multiplier();

    request(&mut harness, &mut pane, HeaderRequest::Grouping(10));

    assert_eq!(pane.widget.config.order_book_grouping, 10);
    assert_eq!(pane.publishes.get(), publishes + 1);
    assert_ne!(pane.widget.book_panel.multiplier(), previous);
}

#[test]
fn a_tick_change_should_reset_an_impossible_grouping() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        order_book_grouping: 100,
        ..PredictionMarketConfig::default()
    };
    let mut pane = pane_over(&mut harness, Rc::clone(&directory), config);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    let target = pane.widget.supported_book_target().expect("a target");
    let book = Arc::new(order_book(
        target.leg,
        "0.5",
        vec![crate::test_support::level("0.4", "10")],
        vec![crate::test_support::level("0.6", "10")],
    ));
    let expected = settle_grouping(book.tick_size, 100);

    report_book_snapshot(&mut harness, &mut pane, book);

    assert_eq!(pane.widget.config.order_book_grouping, expected);
    assert_ne!(
        expected, 100,
        "the fixture tick cannot express that grouping"
    );
}

#[test]
fn an_empty_page_should_not_rewrite_the_stored_grouping() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        order_book_grouping: 10,
        ..PredictionMarketConfig::default()
    };
    let mut pane = pane_over(&mut harness, Rc::clone(&directory), config);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );

    report_book(&mut harness, &mut pane, vec![], vec![]);

    assert_eq!(
        pane.widget.config.order_book_grouping, 10,
        "an empty page's tick is a fallback, not the venue's"
    );
}

#[test]
fn a_stored_book_choice_this_build_does_not_offer_should_be_reset_and_reported() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        order_book_ratio: 0.99,
        order_book_grouping: 7,
        ..PredictionMarketConfig::default()
    };
    let mut pane = pane_over(&mut harness, Rc::clone(&directory), config);
    select_event(&mut pane);

    load(&mut harness, &mut pane);

    assert_eq!(pane.widget.config.order_book_ratio, DEFAULT_BOOK_RATIO);
    assert_eq!(
        pane.widget.config.order_book_grouping,
        GROUPING_MULTIPLIERS[0]
    );
    assert_eq!(pane.publishes.get(), 1, "the replacement is written back");
}

#[test]
fn a_supported_book_choice_should_be_kept_and_not_reported() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let config = PredictionMarketConfig {
        order_book_ratio: 0.35,
        order_book_grouping: 10,
        ..PredictionMarketConfig::default()
    };
    let mut pane = pane_over(&mut harness, Rc::clone(&directory), config);
    select_event(&mut pane);

    load(&mut harness, &mut pane);

    assert_eq!(pane.widget.config.order_book_ratio, 0.35);
    assert_eq!(pane.widget.config.order_book_grouping, 10);
    assert_eq!(pane.publishes.get(), 0, "nothing was replaced");
}

#[test]
fn the_header_should_offer_the_event_control_and_the_range_picker() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let pane = ready_pane(&mut harness, &directory);

    assert!(
        harness
            .find_by_debug_prefix(pane.header_left, "prediction_market.header.event@")
            .is_some(),
        "the left group names the event"
    );
    assert!(
        harness
            .find_by_debug_prefix(pane.header_right, "prediction_market.header.range@")
            .is_some(),
        "the right group offers the range"
    );
    assert!(
        harness
            .find_by_debug_prefix(pane.header_right, "prediction_market.header.book_toggle@")
            .is_some(),
        "the book's toggle is offered while this build can read one"
    );
}

#[test]
fn the_language_switch_should_rebuild_the_header() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let built = pane.widget.header.as_ref().expect("a header").built.clone();
    assert_eq!(built.language, LanguageCode::En);

    harness.switch_language(LanguageCode::ZhCn);
    sync(&mut harness, &mut pane);

    let rebuilt = pane.widget.header.as_ref().expect("a header").built.clone();
    assert_eq!(rebuilt.language, LanguageCode::ZhCn);
    let text = harness.text_of(pane.header_right);
    assert!(
        text.contains('时') || text.contains('天') || !text.is_empty(),
        "the pickers are rebuilt in the language showing now: {text:?}"
    );
}

#[test]
fn the_confirmed_event_should_be_applied_on_the_next_sync() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Event(event_ref(), "A chosen event".into()),
    );

    assert_eq!(pane.widget.selection.as_ref(), Some(&event_ref()));
    assert_eq!(
        pane.widget.selection_title.as_deref(),
        Some("A chosen event")
    );
    assert!(!pane.widget.pending.is_empty(), "the choice started a load");
}

#[test]
fn the_confirmed_market_should_switch_the_chart_market() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    let snapshot = pane.widget.snapshot.clone().expect("a reading");
    let other = snapshot
        .markets
        .first()
        .expect("a market choice")
        .market
        .clone();

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Market(other.clone()),
    );

    assert!(pane.widget.snapshot.is_none(), "the old reading is dropped");
    assert_eq!(pane.widget.market.as_ref(), Some(&other));
}

#[test]
fn a_language_switch_should_rewrite_the_empty_state() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = pane(&mut harness, &directory);
    select_event(&mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(
        &mut harness,
        &mut pane,
        empty_reading(PredictionRange::Day1, 1),
    );
    build_body(&mut harness, &mut pane);
    sync(&mut harness, &mut pane);
    let empty_text = harness.app().read(&pane.widget.empty_text).clone();
    assert!(!empty_text.is_empty());

    harness.switch_language(LanguageCode::ZhCn);
    sync(&mut harness, &mut pane);

    let zh = harness.app().read(&pane.widget.empty_text).clone();
    assert_ne!(zh, empty_text, "the sentence follows the language");
}

#[test]
fn two_panes_should_keep_their_own_header_handlers() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut first = ready_pane(&mut harness, &directory);
    let mut second = peer_pane(&mut harness, &directory);
    select_event(&mut second);
    load(&mut harness, &mut second);
    report_snapshot(
        &mut harness,
        &mut second,
        reading(PredictionRange::Day1, 1, &[(1_000, "0.4")]),
    );
    build_body(&mut harness, &mut second);
    build_header(&mut harness, &mut second);

    let first_action = first
        .widget
        .header
        .as_ref()
        .and_then(|header| header.left.as_ref())
        .and_then(|group| group.components.first())
        .and_then(|component| match component {
            HeaderComponent::Button(button) => button.action_identity(),
            HeaderComponent::Select(_) => None,
        })
        .expect("the first pane registered its event action");
    let second_action = second
        .widget
        .header
        .as_ref()
        .and_then(|header| header.left.as_ref())
        .and_then(|group| group.components.first())
        .and_then(|component| match component {
            HeaderComponent::Button(button) => button.action_identity(),
            HeaderComponent::Select(_) => None,
        })
        .expect("the second pane registered its event action");
    assert_ne!(first_action, second_action, "each pane minted its own key");
    assert!(harness.app().has_action_handler(&first_action));
    assert!(harness.app().has_action_handler(&second_action));

    harness.with_cx(|cx| first.widget.release(cx));
    assert!(
        !harness.app().has_action_handler(&first_action),
        "the closed pane's handler is unregistered"
    );
    assert!(
        harness.app().has_action_handler(&second_action),
        "releasing one pane removed another pane's handler"
    );
}

#[test]
fn releasing_the_pane_should_reclaim_every_value_slot() {
    let mut harness = harness();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let baseline = harness.signal_slot_stats();

    let mut pane = ready_pane(&mut harness, &directory);
    assert!(
        harness.signal_slot_stats().live > baseline.live,
        "the pane minted values"
    );
    let chart_geometry = harness.app().geometry().len();
    release_pane(&mut harness, &mut pane);

    assert_eq!(
        harness.signal_slot_stats().live,
        baseline.live,
        "release hands every value slot back"
    );
    assert_eq!(
        harness.app().geometry().len(),
        chart_geometry,
        "the chart's geometry is handed back"
    );

    let settled = harness.signal_slot_stats().slots;
    for round in 0..4 {
        let mut pane = ready_pane(&mut harness, &directory);
        release_pane(&mut harness, &mut pane);
        assert_eq!(
            harness.signal_slot_stats().slots,
            settled,
            "round {round}: slots are reused rather than grown"
        );
    }
}

#[test]
fn releasing_a_reading_pane_should_cancel_its_timer_and_hand_back_geometry() {
    let mut harness = harness();
    let baseline = harness.app().geometry().len();
    let directory = FixtureDirectory::loaded(&[VenueId::Polymarket]);
    let mut pane = ready_pane(&mut harness, &directory);
    report_snapshot(
        &mut harness,
        &mut pane,
        reading(
            PredictionRange::Day1,
            2,
            &[(1_000, "0.4"), (2_000, "0.5"), (3_000, "0.6")],
        ),
    );
    sync(&mut harness, &mut pane);
    harness.drain();
    let timer = pane.widget.pulse_timer.expect("the pulse is running");
    assert!(harness.app().geometry().len() >= baseline);

    release_pane(&mut harness, &mut pane);

    assert!(pane.widget.pulse_timer.is_none());
    assert!(
        !harness.app_mut().cancel_timer(&timer),
        "release cancelled the timer it owned"
    );
    assert_eq!(
        harness.app().geometry().len(),
        baseline,
        "every registered mesh is released"
    );
}
