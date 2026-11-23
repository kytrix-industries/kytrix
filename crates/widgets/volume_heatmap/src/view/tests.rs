use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use app_config::{settings::Config, time_zone::DataTimeZone};
use base_ui::theme::appearance::Appearance;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_model::{
    decimal::Quantity, identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};
use smol_str::SmolStr;
use ui_core::{
    event::{Event, InputEvent},
    math::Point,
    node::{builder::NodeBuilder, tree::NodeKind},
    paint::canvas::Canvas,
};
use widget::{
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    test_support::WidgetTestApp,
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::{
    model::{HeatMapEntry, PriceDirection},
    service::{
        VolumeHeatMapResponse, VolumeHeatMapSnapshot, VolumeHeatMapStreamMessage,
        test_support::{FakeSource, binance, okx, perp, route, spot},
    },
};

const PAGE: (f32, f32) = (640.0, 360.0);

const COUNTDOWN: i64 = 600_000;

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn harness() -> WidgetTestApp {
    WidgetTestApp::with_translate(product_translate(LanguageCode::En))
}

#[derive(Clone)]
struct TestClock {
    now: Rc<Cell<i64>>,
}

impl TestClock {
    fn new(now: i64) -> Self {
        Self {
            now: Rc::new(Cell::new(now)),
        }
    }

    fn set(&self, now: i64) {
        self.now.set(now);
    }

    fn now(&self) -> Rc<dyn Fn() -> i64> {
        let now = Rc::clone(&self.now);
        Rc::new(move || now.get())
    }
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

struct FixtureDirectory {
    venues: RefCell<Vec<VenueId>>,
    routes: RefCell<Vec<(VenueId, VenueRoute)>>,
}

impl FixtureDirectory {
    fn new(venues: &[VenueId]) -> Rc<Self> {
        let directory = Rc::new(Self {
            venues: RefCell::new(Vec::new()),
            routes: RefCell::new(Vec::new()),
        });
        for venue in venues {
            directory.load(*venue);
        }
        directory
    }

    fn load(&self, venue: VenueId) {
        if !self.venues.borrow().contains(&venue) {
            self.venues.borrow_mut().push(venue);
        }
        for route in [spot(venue_name(venue)), perp(venue_name(venue))] {
            self.routes.borrow_mut().push((venue, route));
        }
    }

    fn unload(&self, venue: VenueId) {
        self.venues.borrow_mut().retain(|loaded| *loaded != venue);
        self.routes
            .borrow_mut()
            .retain(|(owner, _)| *owner != venue);
    }
}

fn venue_name(venue: VenueId) -> &'static str {
    match venue {
        VenueId::Binance => "binance",
        VenueId::Okx => "okx",
        VenueId::Hyperliquid => "hyperliquid",
        other => panic!("the fixture has no route builder for {other}"),
    }
}

impl MarketDirectory for FixtureDirectory {
    fn readable_venues(&self) -> Vec<VenueId> {
        self.venues.borrow().clone()
    }

    fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute> {
        self.routes
            .borrow()
            .iter()
            .filter(|(owner, _)| *owner == venue)
            .map(|(_, route)| route.clone())
            .collect()
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.routes
            .borrow()
            .iter()
            .any(|(_, loaded)| loaded == route)
    }
}

struct Pane {
    widget: VolumeHeatMapWidget,
    status: WidgetStatus,
    page: NodeId,
    header_left: NodeId,
    header_right: NodeId,
}

fn pane_over(
    harness: &mut WidgetTestApp,
    source: FakeSource,
    directory: Rc<FixtureDirectory>,
    config: VolumeHeatMapConfig,
    clock: &TestClock,
) -> Pane {
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_left = group(harness, page);
    let header_right = group(harness, page);
    let directory_handle: Rc<dyn MarketDirectory> = directory;
    let now = clock.now();
    let widget = harness.with_cx(|cx| {
        VolumeHeatMapWidget::with_source(
            config,
            source,
            directory_handle,
            status,
            WidgetConfigChange::default(),
            recovery(),
            now,
            cx,
        )
    });
    Pane {
        widget,
        status,
        page,
        header_left,
        header_right,
    }
}

fn pane(harness: &mut WidgetTestApp, directory: &Rc<FixtureDirectory>) -> Pane {
    pane_over(
        harness,
        FakeSource::pending(),
        Rc::clone(directory),
        VolumeHeatMapConfig::default(),
        &TestClock::new(0),
    )
}

fn pane_without_services(harness: &mut WidgetTestApp) -> Pane {
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_left = group(harness, page);
    let header_right = group(harness, page);
    let now = TestClock::new(0).now();
    let widget = harness.with_cx(|cx| {
        VolumeHeatMapWidget::assemble(
            VolumeHeatMapConfig::default(),
            None,
            None,
            status,
            WidgetConfigChange::default(),
            recovery(),
            now,
            cx,
        )
    });
    Pane {
        widget,
        status,
        page,
        header_left,
        header_right,
    }
}

fn page(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_PAGE")
            .style(
                StyleBuilder::new()
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
            .id("TEST_VOLUME_HEATMAP_HEADER_GROUP")
            .style(StyleBuilder::new().flex_row().build())
            .build(cx.tree_mut(), parent)
            .expect("the header group builds")
    })
}

fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness.with_cx(|cx| pane.widget.load(cx));
}

fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness
        .with_cx(|cx| pane.widget.sync(cx))
        .expect("the widget syncs");
}

fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.page;
    harness
        .with_cx(|cx| pane.widget.body(parent, cx))
        .expect("the body builds");
    harness.drain();
}

fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let left = pane.header_left;
    let right = pane.header_right;
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Left, left, cx))
        .expect("the left header group builds");
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, right, cx))
        .expect("the right header group builds");
    harness.drain();
}

fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    sync(harness, pane);
    harness.drain();
}

fn reported(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
    pane.status.state(harness.app())
}

fn entries() -> Arc<[HeatMapEntry]> {
    Arc::from([HeatMapEntry {
        instrument: market_participants_connector::catalog::test_support::spot_token(
            "fixture", "BTC", "USDT",
        ),
        base_asset_label: SmolStr::new_static("BTC"),
        notional_asset: market_participants_connector::catalog::test_support::token(
            "fixture", "USDT",
        ),
        notional: Quantity::from(120_u64),
        notional_label: SmolStr::new_static("120"),
        direction: PriceDirection::Up,
    }])
}

fn ready(revision: u64) -> VolumeHeatMapStatus {
    VolumeHeatMapStatus::Ready {
        venue: binance(),
        revision,
        next_refresh_at: COUNTDOWN,
    }
}

fn refreshing() -> VolumeHeatMapStatus {
    VolumeHeatMapStatus::Refreshing {
        venue: binance(),
        next_refresh_at: COUNTDOWN,
    }
}

fn failed() -> VolumeHeatMapStatus {
    VolumeHeatMapStatus::Failed {
        venue: binance(),
        error: VolumeHeatMapFetchError::MarketParticipant(
            market_participants_connector::market::error::MarketError::ServiceClosed,
        ),
    }
}

fn key(venue: VenueId) -> VolumeHeatMapStreamKey {
    VolumeHeatMapStreamKey::new(venue)
}

fn report_status(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    venue: VenueId,
    status: VolumeHeatMapStatus,
) {
    let message = Arc::new(VolumeHeatMapStreamMessage::Status(status));
    harness.with_cx(|cx| pane.widget.apply_reliable(key(venue), message, cx));
}

fn report_map(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    venue: VenueId,
    revision: u64,
    entries: Arc<[HeatMapEntry]>,
) {
    let message = Arc::new(VolumeHeatMapStreamMessage::Snapshot(Arc::new(
        VolumeHeatMapSnapshot { revision, entries },
    )));
    harness.with_cx(|cx| pane.widget.apply_latest(key(venue), message, cx));
}

fn a_whole_map(harness: &mut WidgetTestApp, pane: &mut Pane) {
    report_map(harness, pane, binance(), 1, entries());
}

fn publish_generation(harness: &mut WidgetTestApp) {
    let signal = harness.services().catalog_generation;
    let next = *harness.app().read(&signal) + 1;
    harness.app_mut().write(&signal, next);
}

fn canvas(harness: &WidgetTestApp, pane: &Pane) -> Arc<Canvas> {
    let node = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_CANVAS")
        .expect("the pane's body draws a canvas");
    match harness
        .tree()
        .get(node)
        .expect("the canvas node exists")
        .kind()
    {
        NodeKind::Canvas(canvas) => Arc::clone(canvas),
        other => panic!("the map's node is a canvas, not {other:?}"),
    }
}

fn countdown_label(harness: &WidgetTestApp, pane: &Pane) -> Option<SmolStr> {
    let signal = pane.widget.countdown.label;
    (*harness.app().read(&signal)).clone()
}

fn venue_item_labels(pane: &Pane) -> Vec<SmolStr> {
    pane.widget
        .venue_control
        .as_ref()
        .expect("the venue picker is built")
        .select
        .items()
        .iter()
        .map(|item| item.label.clone())
        .collect()
}

fn venue_item_ids(pane: &Pane) -> Vec<SmolStr> {
    pane.widget
        .venue_control
        .as_ref()
        .expect("the venue picker is built")
        .select
        .items()
        .iter()
        .map(|item| item.id.clone())
        .collect()
}

fn scope_entry(capabilities: &[CapabilityId], publishes: bool) -> Arc<ScopeEntry> {
    Arc::new(ScopeEntry {
        scope: market_participants_model::identifier::VenueScopeId::new("spot")
            .expect("the fixture scope is canonical"),
        catalog_source: publishes.then(|| {
            market_participants_connector::catalog::source::CatalogSourceId::new("fixture-spot")
                .expect("the fixture source id is canonical")
        }),
        normalization: Vec::new(),
        capabilities: capabilities
            .iter()
            .map(
                |capability| market_participants_connector::venue::definition::CapabilitySupport {
                    capability: *capability,
                    needs_auth: false,
                },
            )
            .collect(),
    })
}

fn directory_pair(
    venue: &str,
    scope: &str,
    entry: Arc<ScopeEntry>,
) -> (VenueRoute, Arc<ScopeEntry>) {
    (route(venue, scope), entry)
}

#[test]
fn a_pane_without_a_directory_should_wait_for_its_market() {
    let mut harness = harness();
    let mut pane = pane_without_services(&mut harness);
    load(&mut harness, &mut pane);

    let status = reported(&harness, &pane);
    assert!(
        status.notice().is_some(),
        "the pane waits for a market it cannot read"
    );
    assert!(status.failure().is_none(), "waiting is not a failure");
    assert!(!pane.widget.has_content());
}

#[test]
fn an_unloaded_market_should_leave_the_pane_waiting() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);

    let status = reported(&harness, &pane);
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
        "waiting is neither working nor ready"
    );
    let notice = status.notice().expect("the pane waits for its market");
    let sentence = notice.resolve(&harness.services().translate);
    assert!(
        sentence.contains("Binance") && sentence.contains("OKX"),
        "the notice names the markets that answer the whole-scope ticker read: {sentence}"
    );
    assert!(
        !sentence.contains("Longbridge"),
        "and not the one that answers a ticker only one listing at a time: {sentence}"
    );
    assert!(!pane.widget.has_content());

    report_map(&mut harness, &mut pane, binance(), 1, entries());
    assert!(
        !pane.widget.has_content(),
        "a venue the registry does not hold is no market to draw"
    );
}

#[test]
fn waiting_for_the_market_should_disable_the_header_controls() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);

    assert!(
        pane.widget.awaiting_market,
        "the fixture pane really is waiting"
    );
    assert!(
        pane.widget.venue_picker_is_disabled(),
        "there is no market to switch the map to, so the picker refuses input"
    );
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "and there is no schedule for the refresh to bring forward"
    );
}

#[test]
fn a_pane_waiting_for_its_markets_should_follow_the_language() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);

    let notice = reported(&harness, &pane)
        .notice()
        .cloned()
        .expect("the pane waits for its markets");
    let english = notice.resolve(&harness.services().translate);
    assert!(
        english.contains("Binance") && english.contains("OKX"),
        "the notice names the markets that answer the whole-scope ticker read: {english}"
    );

    harness.switch_language(LanguageCode::ZhCn);

    let chinese = notice.resolve(&harness.services().translate);
    assert!(
        chinese.contains("币安") && chinese.contains("欧易"),
        "the same reference resolves in the language showing now: {chinese}"
    );
    assert_ne!(
        chinese, english,
        "the copy of the language the interface left is gone"
    );
}

#[test]
fn loading_the_venues_should_let_the_waiting_pane_start() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[]);
    let source = FakeSource::pending();
    let mut pane = pane_over(
        &mut harness,
        source.clone(),
        Rc::clone(&directory),
        VolumeHeatMapConfig::default(),
        &TestClock::new(0),
    );
    load(&mut harness, &mut pane);
    assert!(
        reported(&harness, &pane).notice().is_some(),
        "the pane waits for a market nothing has published"
    );

    directory.load(binance());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);
    harness.run_until_idle();

    let status = reported(&harness, &pane);
    assert!(
        status.notice().is_none(),
        "the pane left the notice without being reopened"
    );
    assert!(
        status.failure().is_none(),
        "the pane started reading rather than failing"
    );
    assert!(
        status.is_busy(),
        "the pane started reading rather than waiting: {status:?}"
    );
    assert!(
        source.fetch_count() > 0,
        "the service behind the pane really ran a round"
    );
}

#[test]
fn a_late_message_after_the_market_went_away_should_not_clear_the_notice() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    assert!(pane.widget.has_content(), "the map is what arrived");

    directory.unload(binance());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);
    assert!(
        reported(&harness, &pane).notice().is_some(),
        "the pane reports the neutral notice"
    );

    report_status(&mut harness, &mut pane, binance(), ready(2));
    assert!(
        reported(&harness, &pane).notice().is_some(),
        "the notice is what the pane reports after a late status"
    );
    report_map(&mut harness, &mut pane, binance(), 2, entries());
    assert!(
        !pane.widget.has_content(),
        "a map from the market that went away must not become content again"
    );
    assert!(
        reported(&harness, &pane).notice().is_some(),
        "the notice is what the pane reports after a late map"
    );
}

#[test]
fn the_venue_picker_should_offer_every_loaded_market() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);
    assert_eq!(
        venue_item_ids(&pane),
        [SmolStr::new(binance().as_str())],
        "a loaded market is a choice the picker offers"
    );

    directory.load(okx());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);

    assert_eq!(
        venue_item_ids(&pane),
        [
            SmolStr::new(binance().as_str()),
            SmolStr::new(okx().as_str())
        ],
        "the venues are offered in the order the interface states them"
    );
}

#[test]
fn the_venue_choice_should_survive_a_language_change() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance(), okx()]);
    let mut pane = pane_over(
        &mut harness,
        FakeSource::pending(),
        Rc::clone(&directory),
        VolumeHeatMapConfig { venue: Some(okx()) },
        &TestClock::new(0),
    );
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);

    assert_eq!(pane.widget.config.venue, Some(okx()));
    assert_eq!(
        pane.widget.reading.as_ref().map(|reading| reading.venue),
        Some(okx()),
        "the pane reads the venue its configuration names"
    );
    assert_eq!(
        venue_item_labels(&pane),
        [SmolStr::new_static("Binance"), SmolStr::new_static("OKX")],
        "the picker states the venues in the language showing now"
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.config.venue,
        Some(okx()),
        "a translated name cannot move the selection"
    );
    assert_eq!(
        venue_item_labels(&pane),
        [SmolStr::new_static("币安"), SmolStr::new_static("欧易")],
        "the items are re-stated in the language that is showing now"
    );
}

#[test]
fn a_stored_venue_that_is_not_loaded_should_be_reset_and_reported() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let reported = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&reported);
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(&mut harness);
    let change = WidgetConfigChange::new(move |_| counter.set(counter.get() + 1));
    let now = TestClock::new(0).now();
    let directory_handle: Rc<dyn MarketDirectory> =
        Rc::clone(&directory) as Rc<dyn MarketDirectory>;
    let mut widget = harness.with_cx(|cx| {
        VolumeHeatMapWidget::with_source(
            VolumeHeatMapConfig { venue: Some(okx()) },
            FakeSource::pending(),
            directory_handle,
            status,
            change,
            recovery(),
            now,
            cx,
        )
    });
    harness.with_cx(|cx| widget.load(cx));

    assert_eq!(
        widget.config.venue,
        Some(binance()),
        "the market that is loaded is the one the pane falls back to"
    );
    assert!(
        reported.get() > 0,
        "a reset that is not reported is a reset the user sees every start"
    );
    let _ = page;
}

#[test]
fn a_venue_loaded_after_the_map_arrived_should_join_the_picker() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    let before = pane
        .widget
        .reading
        .clone()
        .expect("the pane reads the loaded venue");
    assert_eq!(before.venue, binance());

    directory.load(okx());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.venues,
        [binance(), okx()],
        "the market loaded after the pane was opened is a choice it offers"
    );
    assert_eq!(
        pane.widget.reading.as_ref(),
        Some(&before),
        "and it is not a reason to stop reading the market that is on screen"
    );
}

#[test]
fn unloading_the_venue_on_screen_should_move_the_pane_to_the_loaded_one() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance(), okx()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    assert_eq!(
        pane.widget.config.venue,
        Some(binance()),
        "the first loaded venue is what a pane with no stored choice reads"
    );

    directory.unload(binance());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.config.venue,
        Some(okx()),
        "the pane moves to the market that is loaded"
    );
    assert_eq!(
        pane.widget.reading.as_ref().map(|reading| reading.venue),
        Some(okx())
    );
}

#[test]
fn a_commit_that_takes_the_market_away_should_report_the_reset() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    let published = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&published);
    pane.widget.config_change = WidgetConfigChange::new(move |_| counter.set(counter.get() + 1));
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(pane.widget.config.venue, Some(binance()));
    let stated = published.get();
    assert!(stated > 0, "the venue the pane chose was published");

    directory.unload(binance());
    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.config.venue, None,
        "a market that is not loaded is not a venue the pane can persist"
    );
    assert!(
        published.get() > stated,
        "the reset is reported, not only held in memory"
    );
    assert!(
        reported(&harness, &pane).notice().is_some(),
        "nothing on screen is backed by a market any more"
    );
    assert!(
        pane.widget
            .venue_control
            .as_ref()
            .expect("the header is still built")
            .select
            .chosen(harness.app())
            .is_empty(),
        "the picker states no choice either"
    );
}

#[test]
fn a_commit_that_leaves_the_panes_market_alone_does_not_restart_its_round() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let source = FakeSource::pending();
    let mut pane = pane_over(
        &mut harness,
        source.clone(),
        Rc::clone(&directory),
        VolumeHeatMapConfig::default(),
        &TestClock::new(0),
    );
    load(&mut harness, &mut pane);
    harness.run_until_idle();
    let reading = pane
        .widget
        .reading
        .clone()
        .expect("the pane reads a market");
    assert!(
        !reading.routes.is_empty(),
        "the map reads the scopes of the venue"
    );
    let fetches = source.fetch_count();
    assert_eq!(
        fetches,
        reading.routes.len(),
        "a round asks every scope the map reads exactly once"
    );

    publish_generation(&mut harness);
    frame(&mut harness, &mut pane);
    harness.run_until_idle();

    assert_eq!(
        pane.widget.reading.as_ref(),
        Some(&reading),
        "the market the map reads is the one it was already reading"
    );
    assert_eq!(
        source.fetch_count(),
        fetches,
        "a commit that leaves the map's market as it was must not start its round over"
    );
}

#[test]
fn choosing_another_venue_should_drop_the_map_of_the_previous_one() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance(), okx()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    assert!(pane.widget.has_content());
    assert_eq!(pane.widget.accepted, Some(1));

    harness.with_cx(|cx| pane.widget.select_venue(okx(), cx));

    assert!(
        !pane.widget.has_content(),
        "the map of the market participant the user left is not content for the one they chose"
    );
    assert_eq!(pane.widget.accepted, None);
    assert_eq!(
        pane.widget.reading.as_ref().map(|reading| reading.venue),
        Some(okx())
    );
}

#[test]
fn a_route_that_publishes_no_catalog_should_not_be_read() {
    let entries = vec![directory_pair(
        "binance",
        "spot",
        scope_entry(&[CapabilityId::AllTickers], true),
    )];
    let without_catalog = vec![directory_pair(
        "binance",
        "spot",
        scope_entry(&[CapabilityId::AllTickers], false),
    )];

    assert_eq!(
        routes_of(&entries, binance()),
        [route("binance", "spot")],
        "a scope that publishes its catalog is a market this map reads"
    );
    assert!(
        routes_of(&without_catalog, binance()).is_empty(),
        "a scope with no catalog has no listing to resolve a ticker against"
    );
}

#[test]
fn a_route_that_does_not_answer_the_whole_scope_ticker_read_should_not_be_read() {
    let entries = vec![
        directory_pair(
            "binance",
            "usd-m-perp",
            scope_entry(&[CapabilityId::AllTickers], true),
        ),
        directory_pair(
            "binance",
            "spot",
            scope_entry(&[CapabilityId::Tickers], true),
        ),
    ];

    assert_eq!(
        routes_of(&entries, binance()),
        [route("binance", "usd-m-perp")],
        "a loaded scope that serves only the listing-scoped ticker read is not a market this \
         map is drawn from"
    );
}

#[test]
fn a_loaded_venue_without_the_whole_scope_ticker_read_should_not_be_offered() {
    let entries = vec![
        directory_pair(
            "binance",
            "spot",
            scope_entry(&[CapabilityId::AllTickers], true),
        ),
        directory_pair(
            "okx",
            "spot",
            scope_entry(&[CapabilityId::AllTickers], true),
        ),
        directory_pair(
            "longbridge",
            "spot",
            scope_entry(&[CapabilityId::Tickers], true),
        ),
    ];
    let supported = [VenueId::Binance, VenueId::Okx, VenueId::Longbridge];

    assert_eq!(
        readable_venues(&supported, &entries),
        [VenueId::Binance, VenueId::Okx],
        "the picker offers the loaded venues that answer the whole-scope read"
    );
}

#[test]
fn the_pane_should_report_each_state_it_enters() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);

    load(&mut harness, &mut pane);
    assert_eq!(
        reported(&harness, &pane),
        WidgetState::Connecting,
        "a load starts a stream"
    );

    report_status(&mut harness, &mut pane, binance(), ready(1));
    assert_eq!(
        reported(&harness, &pane),
        WidgetState::Loading,
        "readiness before the map it describes is still a load"
    );

    a_whole_map(&mut harness, &mut pane);
    assert_eq!(
        reported(&harness, &pane),
        WidgetState::Ready,
        "the map is on screen"
    );

    report_status(&mut harness, &mut pane, binance(), failed());
    let failure = reported(&harness, &pane)
        .failure()
        .cloned()
        .expect("a failed round is reported");
    assert_eq!(
        failure.action().label_key(),
        "common.retry",
        "every failure carries the framework's own way out"
    );
    assert!(!failure.text(&harness.services().translate).is_empty());
}

#[test]
fn a_status_about_another_venue_should_be_ignored() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), ready(1));
    assert_eq!(reported(&harness, &pane), WidgetState::Ready);

    report_status(
        &mut harness,
        &mut pane,
        okx(),
        VolumeHeatMapStatus::Ready {
            venue: okx(),
            revision: 9,
            next_refresh_at: COUNTDOWN,
        },
    );

    assert_eq!(
        pane.widget.accepted,
        Some(1),
        "the map on screen is the one the pane is reading"
    );
    assert_eq!(reported(&harness, &pane), WidgetState::Ready);
}

#[test]
fn an_empty_map_should_still_be_content() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    report_map(&mut harness, &mut pane, binance(), 1, Arc::from([]));

    assert!(pane.widget.has_content());
    assert_eq!(pane.widget.accepted, Some(1));
}

#[test]
fn readiness_before_the_map_it_describes_should_keep_the_pane_loading() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);

    report_status(&mut harness, &mut pane, binance(), ready(1));
    assert!(!pane.widget.has_content());
    assert_eq!(reported(&harness, &pane), WidgetState::Loading);

    a_whole_map(&mut harness, &mut pane);
    assert_eq!(reported(&harness, &pane), WidgetState::Ready);
}

#[test]
fn a_map_the_pane_has_overtaken_should_be_ignored() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);

    report_map(&mut harness, &mut pane, binance(), 2, entries());
    report_map(&mut harness, &mut pane, binance(), 1, entries());
    assert_eq!(
        pane.widget.accepted,
        Some(2),
        "an older revision is not a change"
    );
}

#[test]
fn messages_after_a_failure_should_be_ignored() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), failed());

    report_map(&mut harness, &mut pane, binance(), 2, entries());
    report_status(&mut harness, &mut pane, binance(), ready(2));
    assert!(pane.status.is_failed(harness.app()));
    assert_eq!(pane.widget.accepted, Some(1));
    assert_eq!(countdown_label(&harness, &pane), None);
}

#[test]
fn the_countdown_should_only_run_while_the_pane_is_ready() {
    let mut harness = harness();
    let clock = TestClock::new(0);
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane_over(
        &mut harness,
        FakeSource::pending(),
        Rc::clone(&directory),
        VolumeHeatMapConfig::default(),
        &clock,
    );
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);

    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), ready(1));
    frame(&mut harness, &mut pane);
    assert_eq!(
        countdown_label(&harness, &pane),
        Some(SmolStr::new_static("10:00")),
        "a ready pane counts down to the deadline the service published"
    );
    assert!(!pane.widget.refresh_is_disabled(harness.app()));

    clock.set(60_000);
    harness.advance(COUNTDOWN_TICK);
    assert_eq!(
        countdown_label(&harness, &pane),
        Some(SmolStr::new_static("09:00")),
        "the label follows the clock the pane counts against"
    );

    clock.set(0);
    report_status(&mut harness, &mut pane, binance(), refreshing());
    frame(&mut harness, &mut pane);
    assert_eq!(
        countdown_label(&harness, &pane),
        None,
        "a round in flight has no countdown to show"
    );
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "and nothing for the control to bring forward"
    );

    report_status(&mut harness, &mut pane, binance(), ready(2));
    frame(&mut harness, &mut pane);
    assert_eq!(
        countdown_label(&harness, &pane),
        None,
        "readiness without the map it describes is not something to count down from"
    );
    assert!(pane.widget.refresh_is_disabled(harness.app()));

    report_map(&mut harness, &mut pane, binance(), 2, entries());
    frame(&mut harness, &mut pane);
    assert_eq!(
        countdown_label(&harness, &pane),
        Some(SmolStr::new_static("10:00")),
        "and with the map in hand the countdown starts on the new deadline"
    );
    assert!(!pane.widget.refresh_is_disabled(harness.app()));
}

#[test]
fn releasing_the_pane_should_cancel_its_countdown() {
    let mut harness = harness();
    let clock = TestClock::new(0);
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane_over(
        &mut harness,
        FakeSource::pending(),
        Rc::clone(&directory),
        VolumeHeatMapConfig::default(),
        &clock,
    );
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), ready(1));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(
        countdown_label(&harness, &pane),
        Some(SmolStr::new_static("10:00")),
        "the countdown is running on the deadline the map carried"
    );

    harness.with_cx(|cx| pane.widget.release(cx));
    let left_by_release = countdown_label(&harness, &pane);
    assert_eq!(left_by_release, None, "release takes the countdown down");
    clock.set(60_000);
    harness.advance(COUNTDOWN_TICK);

    assert_eq!(
        countdown_label(&harness, &pane),
        left_by_release,
        "the released pane's tick never ran: the label release left behind is the last one"
    );
}

#[test]
fn a_failed_map_should_offer_its_recovery_rather_than_a_refresh() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), ready(1));
    frame(&mut harness, &mut pane);
    assert!(!pane.widget.refresh_is_disabled(harness.app()));

    report_status(&mut harness, &mut pane, binance(), failed());
    frame(&mut harness, &mut pane);

    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "the header has nothing to bring forward, so it offers nothing"
    );
    assert_eq!(countdown_label(&harness, &pane), None);
    let failure = reported(&harness, &pane)
        .failure()
        .cloned()
        .expect("a failure should be published");
    assert_eq!(failure.action().label_key(), "common.retry");

    harness.with_cx(|cx| pane.widget.refresh(cx));
    assert!(
        pane.status.is_failed(harness.app()),
        "a disabled control changes nothing when it is pressed"
    );
}

#[test]
fn a_refresh_should_not_be_forwarded_while_a_map_is_being_fetched() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, binance(), ready(1));
    assert!(
        !pane.widget.refresh_is_disabled(harness.app()),
        "an idle pane with a map can read again on demand"
    );

    harness.with_cx(|cx| {
        pane.widget.refresh(cx);
        assert!(
            pane.widget.refresh_is_disabled(cx.app()),
            "the click closes the window itself, before anything has answered"
        );
        pane.widget
            .apply_rpc_result(Ok(VolumeHeatMapResponse::RefreshQueued), cx);
        assert!(
            pane.widget.refresh_is_disabled(cx.app()),
            "a queued refresh is still a refresh in flight"
        );
    });

    report_status(&mut harness, &mut pane, binance(), refreshing());
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "a fetch in flight has nothing to bring forward"
    );

    report_status(&mut harness, &mut pane, binance(), ready(2));
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "readiness without the map it describes is not a reason to read again"
    );
    report_map(&mut harness, &mut pane, binance(), 2, entries());
    assert!(
        !pane.widget.refresh_is_disabled(harness.app()),
        "and with the map in hand there is nothing left to wait for"
    );
}

#[test]
fn a_reply_to_a_replaced_command_should_be_dropped() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    load(&mut harness, &mut pane);
    assert_eq!(pane.widget.pending.dropped(), 0);

    harness.with_cx(|cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::Service {
                request_id: RequestId::new(1),
                source: VolumeHeatMapServiceError::StreamBusy,
            }),
            cx,
        );
    });

    assert!(
        !pane.status.is_failed(harness.app()),
        "the replaced command's failure is not the new command's"
    );
    assert_eq!(
        pane.widget.pending.dropped(),
        1,
        "the late answer was dropped and counted"
    );
}

#[test]
fn the_body_should_build_the_map_as_a_canvas_node() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let node = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_CANVAS")
        .expect("the map is a node of the body");
    assert!(
        harness.has_canvas(node),
        "the pane's body draws the map as a canvas node"
    );
}

#[test]
fn an_empty_map_should_say_it_holds_nothing() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    report_map(&mut harness, &mut pane, binance(), 1, Arc::from([]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let overlay = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_EMPTY")
        .expect("an empty map draws its stand-in");
    assert_eq!(
        harness.text_of(overlay),
        "No data",
        "the stand-in is the catalogue's no-data sentence"
    );
}

#[test]
fn a_steady_canvas_should_be_kept() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let drawn = canvas(&harness, &pane);
    frame(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    assert!(
        Arc::ptr_eq(&drawn, &canvas(&harness, &pane)),
        "a steady frame keeps the canvas it is drawn from"
    );
}

#[test]
fn another_map_should_redraw_the_canvas() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let drawn = canvas(&harness, &pane);

    let mut second = (*entries()).to_vec();
    second[0].notional_label = SmolStr::new_static("240");
    report_map(&mut harness, &mut pane, binance(), 2, Arc::from(second));
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&drawn, &canvas(&harness, &pane)),
        "a new map is a new canvas"
    );
}

#[test]
fn a_dark_appearance_should_redraw_the_map() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let light = canvas(&harness, &pane);

    let theme = harness.services().theme.clone();
    theme.set_appearance(harness.app_mut(), Appearance::Dark);
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&light, &canvas(&harness, &pane)),
        "the dark appearance repaints the map in the dark roles"
    );
}

#[test]
fn a_language_change_should_rebuild_the_header_and_the_empty_state() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance(), okx()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    report_map(&mut harness, &mut pane, binance(), 1, Arc::from([]));
    build_body(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let nodes = harness.node_count();

    let overlay = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_EMPTY")
        .expect("an empty map draws its stand-in");
    assert_eq!(harness.text_of(overlay), "No data");
    assert_eq!(
        pane.widget
            .refresh_control
            .as_ref()
            .expect("the refresh control is built")
            .button
            .a11y_label_text(),
        Some("Refresh the volume heat map now"),
        "the button states the copy of the language it was built in"
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let overlay = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_EMPTY")
        .expect("the stand-in is rebuilt, not dropped");
    assert_eq!(
        harness.text_of(overlay),
        "暂无数据",
        "the stand-in's sentence is resolved in the language showing now"
    );
    assert_eq!(
        pane.widget
            .refresh_control
            .as_ref()
            .expect("the refresh control is rebuilt in its slot")
            .button
            .a11y_label_text(),
        Some("立即刷新成交量热力图"),
        "the button is rebuilt with the copy of the language that is showing"
    );
    assert_eq!(
        venue_item_labels(&pane),
        [SmolStr::new_static("币安"), SmolStr::new_static("欧易")]
    );
    assert!(
        harness.node_count() <= nodes + 2,
        "a rebuilt stand-in and a rebuilt button are not a rebuilt body: {} → {}",
        nodes,
        harness.node_count()
    );
}

#[test]
fn a_map_that_gains_tiles_should_remove_the_empty_state() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    report_map(&mut harness, &mut pane, binance(), 1, Arc::from([]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert!(
        harness
            .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_EMPTY")
            .is_some(),
        "the empty map says it holds nothing"
    );

    report_map(&mut harness, &mut pane, binance(), 2, entries());
    frame(&mut harness, &mut pane);

    assert!(
        harness
            .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_EMPTY")
            .is_none(),
        "the stand-in is gone once the map holds something"
    );
    assert!(
        pane.widget.has_content() && !pane.widget.chart.is_empty(),
        "the map on screen is the one with tiles"
    );
}

#[test]
fn a_language_change_should_rebuild_the_venue_picker() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);

    let trigger = pane
        .widget
        .venue_control
        .as_ref()
        .expect("the picker is built")
        .select
        .trigger_node()
        .expect("the picker is mounted");
    assert_eq!(
        harness.text_of(trigger),
        "Choose a market participant",
        "with no venue chosen the trigger states the placeholder"
    );
    let nodes = harness.node_count();

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let rebuilt = pane
        .widget
        .venue_control
        .as_ref()
        .expect("the picker is rebuilt in place")
        .select
        .trigger_node()
        .expect("the rebuilt picker is mounted");
    assert_ne!(rebuilt, trigger, "the copy is rebuilt rather than re-bound");
    assert!(
        harness.tree().get(trigger).is_err(),
        "the old trigger went with the row this widget removed"
    );
    assert_eq!(
        harness.text_of(rebuilt),
        "请选择市场参与者",
        "the placeholder is in the language showing now"
    );
    assert_eq!(
        harness.node_count(),
        nodes,
        "the rebuilt picker replaces the old one instead of adding to it"
    );

    let english = pane
        .widget
        .venue_select_options(&product_translate(LanguageCode::En));
    assert_eq!(
        english.label.as_deref(),
        Some("Market participant"),
        "the picker's accessible name is the catalogue's word for the market"
    );
    assert_eq!(english.placeholder.as_str(), "Choose a market participant");
    let chinese = pane
        .widget
        .venue_select_options(&product_translate(LanguageCode::ZhCn));
    assert_eq!(chinese.label.as_deref(), Some("市场参与者"));
    assert_eq!(chinese.placeholder.as_str(), "请选择市场参与者");
}

#[test]
fn a_header_row_that_is_gone_should_release_its_controls() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    build_header(&mut harness, &mut pane);
    assert!(
        pane.widget.venue_control.is_some() && pane.widget.refresh_control.is_some(),
        "the pane built both controls"
    );

    harness.with_cx(|cx| {
        let window = cx.window();
        cx.app_mut()
            .remove_node(window, pane.header_left)
            .expect("the left row is removed");
        cx.app_mut()
            .remove_node(window, pane.header_right)
            .expect("the right row is removed");
    });
    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    assert!(
        pane.widget.venue_control.is_none(),
        "the picker's component was released instead of rebuilt into a stale row"
    );
    assert!(
        pane.widget.refresh_control.is_none(),
        "the refresh button was released instead of rebuilt into a stale row"
    );
}

#[test]
fn a_display_time_zone_change_should_not_redraw_the_map() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let source = FakeSource::pending();
    let mut pane = pane_over(
        &mut harness,
        source.clone(),
        Rc::clone(&directory),
        VolumeHeatMapConfig::default(),
        &TestClock::new(0),
    );
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    harness.run_until_idle();
    let drawn = canvas(&harness, &pane);
    let fetches = source.fetch_count();

    let signal = harness.services().config;
    let mut config: Config = (*harness.app().read(&signal)).clone();
    config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
    harness.app_mut().write(&signal, config);
    frame(&mut harness, &mut pane);

    assert!(
        Arc::ptr_eq(&drawn, &canvas(&harness, &pane)),
        "the map draws no time, so another data time zone is not a redraw"
    );
    assert_eq!(
        source.fetch_count(),
        fetches,
        "and it is not a reason to read the market again"
    );
}

#[test]
fn the_pointer_should_follow_the_tile_under_it() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let node = harness
        .find_by_debug_prefix(pane.page, "TEST_VOLUME_HEATMAP_CANVAS")
        .expect("the map is drawn");
    let bounds = harness
        .tree()
        .get(node)
        .expect("the map is laid out")
        .layout()
        .bounds;
    let middle = Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    let requests = harness.frame_requests();

    let consumed = harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerMove {
                position: middle,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    });
    assert!(consumed, "the map consumes a pointer move over it");
    assert_eq!(pane.widget.chart.hovered(), Some(0));
    assert!(
        harness.frame_requests() > requests,
        "a tile that moved under the pointer is a frame"
    );

    let requests = harness.frame_requests();
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerLeave {
                position: middle,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    });
    assert_eq!(
        pane.widget.chart.hovered(),
        None,
        "a pointer that left is over no tile"
    );
    assert!(harness.frame_requests() > requests);
}

#[test]
fn preparing_and_releasing_should_leave_the_geometry_store_alone() {
    let mut harness = harness();
    let directory = FixtureDirectory::new(&[binance()]);
    let mut pane = pane(&mut harness, &directory);
    load(&mut harness, &mut pane);
    a_whole_map(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let baseline = harness.app().geometry().len();

    for _ in 0..3 {
        frame(&mut harness, &mut pane);
        report_map(&mut harness, &mut pane, binance(), 2, entries());
        frame(&mut harness, &mut pane);
    }
    assert_eq!(
        harness.app().geometry().len(),
        baseline,
        "the map draws quads, not registered meshes"
    );

    harness.with_cx(|cx| pane.widget.release(cx));
    assert_eq!(
        harness.app().geometry().len(),
        baseline,
        "and releasing it leaves nothing behind"
    );
}

#[test]
fn releasing_the_pane_should_reclaim_its_signal_scope() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let baseline = harness.signal_slot_stats();

    let mut needed = 0;
    for cycle in 0..3 {
        let pane = harness.mount::<VolumeHeatMapWidget>(VolumeHeatMapConfig::default());
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "cycle {cycle}: a mounted pane mints values of its own"
        );
        pane.release(harness.app_mut());
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "cycle {cycle}: release hands every value slot back"
        );
        let slots = harness.signal_slot_stats().slots;
        if cycle == 0 {
            needed = slots;
        } else {
            assert_eq!(
                slots, needed,
                "cycle {cycle}: the slots the first mount needed are reused, not grown"
            );
        }
    }

    let pane = harness.mount::<VolumeHeatMapWidget>(VolumeHeatMapConfig::default());
    let group = page(&mut harness);
    pane.header(harness.app_mut(), HeaderGroup::Left, group)
        .expect("the venue picker builds");
    let group = page(&mut harness);
    pane.header(harness.app_mut(), HeaderGroup::Right, group)
        .expect("the refresh control builds");
    assert!(
        harness.signal_slot_stats().live > baseline.live,
        "the mounted header mints its controls' values"
    );
    pane.release(harness.app_mut());
    assert_eq!(
        harness.signal_slot_stats().live,
        baseline.live,
        "the pane releases its components, the picker's own scope included"
    );
}

#[test]
fn two_panes_should_keep_their_own_header_handlers() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let first = harness.mount::<VolumeHeatMapWidget>(VolumeHeatMapConfig::default());
    let second = harness.mount::<VolumeHeatMapWidget>(VolumeHeatMapConfig::default());
    let first_group = page(&mut harness);
    let second_group = page(&mut harness);
    first
        .header(harness.app_mut(), HeaderGroup::Left, first_group)
        .expect("the first pane builds its picker");
    second
        .header(harness.app_mut(), HeaderGroup::Left, second_group)
        .expect("the second pane builds its picker");
    let first_refresh_group = page(&mut harness);
    let second_refresh_group = page(&mut harness);
    first
        .header(harness.app_mut(), HeaderGroup::Right, first_refresh_group)
        .expect("the first pane builds its refresh");
    second
        .header(harness.app_mut(), HeaderGroup::Right, second_refresh_group)
        .expect("the second pane builds its refresh");

    let suffixes = [super::REFRESH_KEY_SUFFIX, super::VENUE_KEY_SUFFIX];
    let keys_of = |scope: ui_core::node::signal::SignalScope| {
        suffixes
            .iter()
            .map(|suffix| smol_str::format_smolstr!("{suffix}@{}", scope.index()))
            .collect::<Vec<_>>()
    };
    let first_keys = keys_of(first.scope());
    let second_keys = keys_of(second.scope());
    for (position, (first_key, second_key)) in first_keys.iter().zip(&second_keys).enumerate() {
        assert_ne!(
            first_key, second_key,
            "two panes never mint the same component key for control {position}"
        );
    }

    let action_of = |key: &SmolStr| {
        if key.starts_with(super::REFRESH_KEY_SUFFIX) {
            ui_core::event::Action::Custom(smol_str::format_smolstr!("{key}.click"))
        } else {
            ui_core::event::Action::Custom(smol_str::format_smolstr!("{key}.trigger"))
        }
    };
    for key in first_keys.iter().chain(&second_keys) {
        assert!(
            harness.app().has_action_handler(&action_of(key)),
            "every pane's control keeps its own handler: {key}"
        );
    }

    first.release(harness.app_mut());
    for key in &first_keys {
        assert!(
            !harness.app().has_action_handler(&action_of(key)),
            "the released pane takes its own handler down: {key}"
        );
    }
    for key in &second_keys {
        assert!(
            harness.app().has_action_handler(&action_of(key)),
            "and leaves the other pane's handler alone: {key}"
        );
    }
}

#[test]
fn the_widget_should_have_a_stable_identity() {
    assert_eq!(VolumeHeatMapWidget::ID.as_str(), "kytrix.volume_heatmap");
    assert_eq!(
        VolumeHeatMapWidget::DISPLAY_NAME,
        "widget.names.volume_heatmap"
    );
    assert_eq!(
        VolumeHeatMapConfig::SCHEMA_VERSION,
        WidgetConfigSchemaVersion::V1
    );
}

#[test]
fn every_message_this_pane_shows_should_resolve_to_text() {
    let mut harness = harness();
    let translate = harness.services().translate.clone();
    for key in [
        "widget.names.volume_heatmap",
        "widget.volume_heatmap.refresh_now",
        "widget.volume_heatmap.choose_venue",
        "common.market_participants",
        "common.no_data",
        "error.volume_heatmap.notional_overflow",
        "error.volume_heatmap.asset_not_named",
        "error.volume_heatmap.too_many_tickers",
        "error.volume_heatmap.timeout",
        "error.volume_heatmap.stream_unavailable",
    ] {
        assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
    }

    let failures = [
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::MarketParticipant(
            market_participants_connector::market::error::MarketError::ServiceClosed,
        )),
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::Model(
            HeatMapModelError::NotionalOverflow {
                instrument: Box::new(
                    market_participants_connector::catalog::test_support::spot_token(
                        "fixture", "BTC", "USDT",
                    ),
                ),
            },
        )),
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::TooManyTickers {
            route: route("binance", "spot"),
            count: 10_001,
            limit: 10_000,
        }),
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::Timeout(Duration::from_secs(5))),
        VolumeHeatMapFailure::Rpc(RpcCallError::Service {
            request_id: RequestId::new(1),
            source: VolumeHeatMapServiceError::StreamBusy,
        }),
        VolumeHeatMapFailure::Rpc(RpcCallError::Service {
            request_id: RequestId::new(2),
            source: VolumeHeatMapServiceError::StreamClosed,
        }),
    ];
    for failure in failures {
        assert!(
            !failure.translated_message(&translate).is_empty(),
            "{failure:?} has no copy"
        );
    }
    let _ = &mut harness;
}
