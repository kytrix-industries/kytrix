//! The pane: what it reports, what it draws, and what it refuses.
//!
//! The service is replaced by the shared fake, and the binding is cancelled so the
//! only input a test has to reason about is the one it hands the pane itself. That
//! is also what makes a "late message" testable: a message the pane should ignore
//! is one it is given after it has stopped accepting them.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    TestAppContext, Window, px, size,
};
use i18n::{language::codes::LanguageCode, t};
use market_participants_connector::{
    catalog::{batch::CatalogEntry, source::CatalogRouteUpdate},
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    market::{error::MarketError, timeframe::StatisticsTimeFrame},
    runtime::MarketParticipantNetworkSnapshot,
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Quantity, Tick},
    identifier::VenueId,
    instrument::InstrumentKind,
    listing::{
        route::VenueRoute,
        rules::{ListingPrecision, TradingRules},
    },
    market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use widget::{
    config::WidgetConfigChange,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    venue::VenueDisplay,
    view::WidgetView,
};
use widget_tokio_runtime::global;

use super::pane::*;
use crate::{
    model::{MetricKind, OpenInterestPoint, StatisticsPoints, StatisticsSnapshot},
    service::{
        StatisticsStatus, StatisticsStreamKey, StatisticsStreamMessage,
        test_support::{
            FakeSource, FetchPlan, perpetual_entry, perpetual_listing, perpetual_route,
        },
    },
    widget::{open_interest::OpenInterestWidget, taker_volume::TakerVolumeWidget},
};

/// A window of one interval is enough for every assertion here, so the tests all
/// drive the one chart whose series the fake answers with.
type Widget = OpenInterestWidget;

/// The listing every pane test follows, which is the one the fixture route
/// publishes.
fn listing() -> market_participants_model::listing::route::VenueListingKey {
    perpetual_listing("BTCUSDT")
}

/// A scope the fixture venue really serves: a perpetual listing of a route the
/// registry declares statistics for.
fn selection() -> InstrumentSelection {
    InstrumentSelection { listing: listing() }
}

/// A second perpetual listing of the same route, which a delisting test publishes
/// instead.
fn other_selection() -> InstrumentSelection {
    InstrumentSelection {
        listing: perpetual_listing("ETHUSDT"),
    }
}

fn config() -> StatisticsConfig {
    StatisticsConfig {
        selection: Some(selection()),
        timeframe: StatisticsTimeFrame::Minute5,
    }
}

fn key() -> StatisticsStreamKey {
    StatisticsStreamKey::new(
        MetricKind::OpenInterest,
        selection().listing,
        StatisticsTimeFrame::Minute5,
    )
}

fn metadata() -> ListingPrecision {
    ListingPrecision {
        price_tick: Tick::from_inner(100_000_000_000_000_000)
            .expect("the fixture tick should be valid"),
        quantity_tick: Tick::from_inner(1_000_000_000_000_000)
            .expect("the fixture tick should be valid"),
    }
}

/// A window of one interval, at `revision`.
fn snapshot(revision: u64) -> StatisticsSnapshot {
    StatisticsSnapshot {
        revision,
        metadata: Some(metadata()),
        points: StatisticsPoints::OpenInterest(Arc::from(vec![OpenInterestPoint {
            timestamp: 1_700_000_000_000,
            base_asset_volume: Quantity::from(12_u64),
            quote_asset_volume: Some(Quantity::from(34_u64)),
        }])),
    }
}

/// A window that is ready, with its next fetch far enough ahead that a test which
/// reads the countdown sees time left rather than a schedule that is already over.
fn ready(revision: u64) -> StatisticsStatus {
    StatisticsStatus::Ready {
        revision,
        next_refresh_at: i64::MAX,
    }
}

fn failed() -> StatisticsStatus {
    StatisticsStatus::Failed {
        error: crate::service::StatisticsFetchError::MarketParticipant(MarketError::ServiceClosed),
    }
}

fn refreshing() -> StatisticsStatus {
    StatisticsStatus::Refreshing {
        next_refresh_at: i64::MAX,
    }
}

/// What the fixture venue publishes for the route a pane is pointed at.
type Venue = Vec<CatalogEntry>;

/// The entries a route carries for the listing a config follows.
fn listed() -> Venue {
    vec![perpetual_entry("BTC", "USDT", "BTCUSDT")]
}

/// What a route carries once it has dropped the listing the fixture config follows.
fn delisted() -> Venue {
    vec![perpetual_entry("ETH", "USDT", "ETHUSDT")]
}

/// Installs a connector whose fixture perpetual route carries `entries`, and returns the
/// fixture so the caller can publish it once the route is registered.
///
/// The publication cannot happen here: a snapshot records the route revision it was
/// fetched under, and registering the route in the market participants root moves that revision on,
/// so a catalogue published before the registration would have nothing to serve.
fn install_connector(
    cx: &mut App,
    entries: &Venue,
) -> market_participants_connector::catalog::test_support::Fixture {
    let route = perpetual_route();
    let source_id = "binance-perpetual";
    let fixture = market_participants_connector::catalog::test_support::Fixture::with_entries(
        route,
        source_id,
        market_participants_connector::catalog::test_support::listing(entries.clone()),
        entries,
        market_participants_connector::catalog::test_support::FIXTURE_CODES,
    );
    market_participants_connector::test_support::install_root(
        cx,
        Arc::clone(fixture.catalog_arc()),
    );
    fixture
}

/// Loads the fixture route's catalog while the fixture source is still what its
/// catalogue entry binds.
///
/// This is what installs the fixture's asset terms and display codes, so it has to run
/// before the market participants root registers the route: registering it replaces the catalog
/// source binding with the real venue's, and a load after that would go to the venue.
fn load_fixture(cx: &App, fixture: &market_participants_connector::catalog::test_support::Fixture) {
    global::handle(cx)
        .handle()
        .block_on(fixture.ensure())
        .expect("the fixture perpetual route should publish its catalog");
}

/// Publishes the fixture route's listings under the route the market participants root holds.
///
/// The publication follows the registration because a snapshot records the route
/// revision it was fetched under and registering the route moves that revision on: a
/// catalogue published before it would have nothing for the pane to read, which is the
/// mismatch this fixture exists to be able to catch. Entries that belong to another route
/// — a test about a config pointed somewhere else — are left to the test's own
/// registration.
fn publish_fixture(
    entries: &Venue,
    fixture: &market_participants_connector::catalog::test_support::Fixture,
) {
    let route = perpetual_route();
    let own: Vec<_> = entries
        .iter()
        .filter(|entry| entry.listing.key.route == route)
        .cloned()
        .collect();
    if own.is_empty() {
        return;
    }
    market_participants_connector::catalog::test_support::publish_fixture_route(
        fixture.catalog(),
        &route,
        own,
    )
    .expect("the fixture route should publish its listings under the registered route");
}

/// Installs the market participants root, wired to `catalog`, which is the directory's own
/// catalogue: the pane's gate asks the directory and the service resolves listings
/// against whatever the root was wired to, so two different catalogues would let a
/// gate/read mismatch pass unnoticed.
fn install_connector_with(
    cx: &mut App,
    catalog: &Arc<market_participants_connector::catalog::store::Catalog>,
    loaded: bool,
) {
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
    let (_, receiver) = tokio::sync::watch::channel(snapshot);
    let connector = MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            receiver,
        )
        .with_runtime(global::handle(cx)),
        Arc::clone(catalog),
    )
    .expect("the market participants root should build from typed runtime inputs");
    // The pane follows the venue commits this root announces, so the fixture attaches the
    // boundary the application's own install path attaches.
    connector.attach_events(cx);
    if loaded {
        connector
            .load_fixture(
                &market_participants_connector::test_support::definitions(
                    &connector.supported_venues(),
                ),
                &[],
                &[],
            )
            .expect("the venue's routes load");
    }
    cx.set_global(connector);
}

/// Creates a pane the way the framework does: status first, then one load.
///
/// The fake never answers a fetch, so the load the pane starts stays in flight and the
/// tests below are the only thing that ever reports to it. A fake that answered
/// would race them: the service publishes on its own task, and a test that
/// reported a window at the same time would be asserting against whichever of the
/// two arrived last. The route it asks about carries what the config follows, which is
/// what a venue that still publishes the listing answers.
fn pane_with(cx: &mut TestAppContext, config: StatisticsConfig) -> Entity<Widget> {
    pane_over(cx, FakeSource::new([FetchPlan::Pending]), listed(), config)
}

/// The same, over a fake venue a test arranges itself.
fn pane_over(
    cx: &mut TestAppContext,
    source: FakeSource,
    venue: Venue,
    config: StatisticsConfig,
) -> Entity<Widget> {
    let published = Rc::new(Cell::new(0));
    pane_reporting_over(cx, source, venue, config, published)
}

/// A pane over a fake that never answers, over the state [`install_app`] already put in
/// place: what a test comparing two panes in one application builds both of them with.
fn pane_on(cx: &mut TestAppContext, config: StatisticsConfig) -> Entity<Widget> {
    let published = Rc::new(Cell::new(0));
    pane_reporting_on(
        cx,
        FakeSource::new([FetchPlan::Pending]),
        listed(),
        config,
        published,
    )
}

/// The same, over a directory whose market is published but whose venue is not
/// loaded.
fn pane_over_unloaded(cx: &mut TestAppContext, source: FakeSource) -> Entity<Widget> {
    unloaded_pane_of(cx, move |status, recovery, cx| {
        Widget::new_with_source(
            config(),
            source,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    })
}

/// A pane of any widget of this crate over a directory whose market is published but whose
/// venue is not loaded.
///
/// Which series a pane plots is the only thing that differs between the five widgets, so a
/// test about the shared pane's own behaviour — the language it follows, for one — is about
/// every one of them and is written once, over whichever type it builds.
fn unloaded_pane_of<W, F>(cx: &mut TestAppContext, build: F) -> Entity<W>
where
    W: WidgetView,
    F: FnOnce(Entity<WidgetStatus>, WidgetAction, &mut Context<W>) -> W + 'static,
{
    cx.executor().allow_parking();
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let runtime = global::handle(cx);
        let _runtime = runtime.enter();
        let fixture = install_connector(cx, &listed());
        load_fixture(cx, &fixture);
        let catalog = Arc::clone(fixture.catalog_arc());
        install_connector_with(cx, &catalog, false);
        publish_fixture(&listed(), &fixture);
    });
    // The market is installed and its commit flushed before the pane is built, which is the order
    // production has: the user loads the market, and a pane opened afterwards reads the generation
    // that load published rather than a commit about a market it was not reading.
    cx.run_until_parked();
    cx.update(|cx| {
        let pane = cx.new(|cx| {
            let status = cx.new(|_| WidgetStatus::new());
            let weak = cx.weak_entity();
            let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                let _ = weak.update(cx, |pane: &mut W, cx| pane.load(cx));
            });
            build(status, recovery, cx)
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    })
}

/// Registers another scope in the installed directory and publishes `entries` under
/// it.
///
/// A pane reads a market that is in memory, so a test whose config follows a scope
/// the fixture route is not publishes one for it: what that scope serves is a
/// different question, and it is the one the pane settles.
fn register_scope(cx: &App, route: &VenueRoute, entries: &[CatalogEntry]) {
    let catalog = cx.global::<MarketParticipantsConnector>().catalog_arc();
    let source_id = format!("fixture-{}-{}", route.venue, route.scope);
    market_participants_connector::catalog::test_support::commit_fixture_routes(
        &catalog,
        CatalogRouteUpdate::new(
            vec![
                market_participants_connector::catalog::test_support::binding(
                    &source_id,
                    market_participants_connector::catalog::test_support::listing(entries.to_vec()),
                ),
            ],
            vec![
                market_participants_connector::catalog::test_support::registration(
                    route, &source_id,
                ),
            ],
        ),
        &market_participants_connector::catalog::test_support::requirements(std::slice::from_ref(
            route,
        )),
    )
    .expect("the fixture scope should register");
    market_participants_connector::catalog::test_support::publish_fixture_catalogs(
        cx,
        &[(route.clone(), entries.to_vec())],
    );
}

/// The same, reporting the pane's config changes into a counter the test keeps.
fn pane_reporting_over(
    cx: &mut TestAppContext,
    source: FakeSource,
    venue: Venue,
    config: StatisticsConfig,
    published: Rc<Cell<usize>>,
) -> Entity<Widget> {
    cx.update(install_app);
    pane_reporting_on(cx, source, venue, config, published)
}

/// The same over the state [`install_app`] already put in place.
fn pane_reporting_on(
    cx: &mut TestAppContext,
    source: FakeSource,
    venue: Venue,
    config: StatisticsConfig,
    published: Rc<Cell<usize>>,
) -> Entity<Widget> {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let runtime = global::handle(cx);
        let _runtime = runtime.enter();
        let fixture = install_connector(cx, &venue);
        load_fixture(cx, &fixture);
        let catalog = Arc::clone(fixture.catalog_arc());
        install_connector_with(cx, &catalog, true);
        publish_fixture(&venue, &fixture);
    });
    // The market is installed and its commit flushed before the pane is built, which is the order
    // production has: the user loads the market, and a pane opened afterwards reads the generation
    // that load published rather than a commit about a market it was not reading.
    cx.run_until_parked();
    cx.update(|cx| {
        let pane = cx.new(|cx| {
            let status = cx.new(|_| WidgetStatus::new());
            let weak: gpui_kit::WeakEntity<Widget> = cx.weak_entity();
            let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                let _ = weak.update(cx, |pane, cx| pane.load(cx));
            });
            Widget::new_with_source(
                config,
                source,
                status,
                WidgetConfigChange::new(move |_| published.set(published.get() + 1)),
                recovery,
                cx,
            )
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    })
}

/// Installs the application-wide state a pane is built on: the theme, the translator
/// and the runtime.
///
/// An application installs each of them exactly once, so a test that builds two panes in
/// one application installs them once and builds both with the `…_on` helpers rather
/// than installing them again.
fn install_app(cx: &mut App) {
    base_ui::style::theme::install(cx);
    i18n::global::embedded::install(cx).expect("the embedded locales should load");
    global::install(cx).expect("the test Tokio runtime should build");
}

/// Lets a pane and its service take a turn each, until what a test waits for has happened.
///
/// Two executors are involved: the pane sends its commands from GPUI's own, and the service
/// answers on the application runtime. So both are given a turn — the pane first, because a
/// command that has not been sent is a service with nothing to answer.
fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let handle = cx.update(|cx| global::handle(cx));
        handle
            .handle()
            .block_on(async { tokio::task::yield_now().await });
        if cx.update(|cx| ready(cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what} should happen");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Counts the venue commits it is told about.
///
/// A test that asserts a pane did *not* act on a commit still has to know the commit reached it:
/// this probe subscribes the same way and after the pane did, so the delivery it counts is one the
/// pane's own handler has already seen.
struct CommitProbe {
    deliveries: Arc<AtomicUsize>,
}

impl CommitProbe {
    fn new(
        cx: &mut Context<Self>,
        events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
        deliveries: Arc<AtomicUsize>,
    ) -> Self {
        cx.subscribe(
            events,
            |probe, _, _: &market_participants_connector::venue::loaded::VenuesUpdated, _| {
                probe.deliveries.fetch_add(1, Ordering::AcqRel);
            },
        )
        .detach();
        Self { deliveries }
    }
}

fn pane(cx: &mut TestAppContext) -> Entity<Widget> {
    pane_with(cx, config())
}

/// A pane that counts how many times it reports its config, so a test can tell a
/// replacement it made itself from one the user asked for.
fn reporting_pane(
    cx: &mut TestAppContext,
    config: StatisticsConfig,
    published: Rc<Cell<usize>>,
) -> (Entity<Widget>, FakeSource) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_reporting_over(cx, source.clone(), listed(), config, published);
    (pane, source)
}

/// Every interval the config type can hold is one the picker offers and the pane can
/// poll.
///
/// This is what makes the settle step in `start` unreachable in practice: the type cannot
/// name a window outside the list, so a stored interval is always one this build serves.
/// Adding a variant without adding it to `TIMEFRAMES` would break the picker silently —
/// this is the test that turns that into a failure.
#[test]
fn every_stored_interval_should_be_one_the_picker_offers() {
    for timeframe in [
        StatisticsTimeFrame::Minute5,
        StatisticsTimeFrame::Minute15,
        StatisticsTimeFrame::Minute30,
        StatisticsTimeFrame::Hour1,
        StatisticsTimeFrame::Hour2,
        StatisticsTimeFrame::Hour4,
        StatisticsTimeFrame::Hour6,
        StatisticsTimeFrame::Hour12,
        StatisticsTimeFrame::Day1,
    ] {
        assert!(
            TIMEFRAMES.contains(&timeframe),
            "{timeframe:?} cannot be drawn by the picker or polled by the service"
        );
    }
    assert_eq!(
        TIMEFRAMES.len(),
        9,
        "a new interval on the type needs a row in the picker and in this list"
    );
}

/// A stored interval is left exactly as it was, because it is one this build serves.
#[gpui_kit::test]
fn a_stored_interval_should_survive_starting(cx: &mut TestAppContext) {
    let published = Rc::new(Cell::new(0));
    let chosen = StatisticsConfig {
        timeframe: StatisticsTimeFrame::Hour4,
        ..config()
    };
    let (pane, _) = reporting_pane(cx, chosen, published.clone());

    cx.update(|cx| {
        assert_eq!(pane.read(cx).config.timeframe, StatisticsTimeFrame::Hour4);
    });
    assert_eq!(published.get(), 0, "a supported choice is not a change");
}

/// A listing the route no longer publishes goes back to "nothing chosen", and the pane is
/// told so it can be written back with the scope it is actually polling.
#[gpui_kit::test]
fn a_listing_the_route_delisted_should_go_back_to_nothing_chosen(cx: &mut TestAppContext) {
    let published = Rc::new(Cell::new(0));
    let pane = pane_reporting(cx, delisted(), published.clone());
    settle_until(cx, "the pane to settle its selection", |cx| {
        pane.read(cx).config.selection.is_none()
    });

    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).config.selection,
            None,
            "the catalog says it no longer publishes what the pane was following"
        );
    });
    assert_eq!(
        published.get(),
        1,
        "the replacement is reported, or the next start resets it again"
    );
}

/// A listing of another product is one these endpoints cannot serve, and it goes
/// back to "nothing chosen" as well: the provider refuses it at the capability
/// boundary, and the pane knows it before it asks for a window.
#[gpui_kit::test]
fn a_listing_that_is_not_perpetual_should_go_back_to_nothing_chosen(cx: &mut TestAppContext) {
    let published = Rc::new(Cell::new(0));
    // A spot listing published by the very route that serves the statistics: the
    // route's capabilities say nothing about one listing's product.
    let spot = market_participants_connector::catalog::test_support::entry(
        &perpetual_route(),
        market_participants_connector::catalog::test_support::spot_token("binance", "BTC", "USDT"),
        "BTCUSDT",
    );
    let pane = pane_reporting_over(
        cx,
        FakeSource::new([FetchPlan::Pending]),
        vec![spot],
        config(),
        published.clone(),
    );
    settle_until(cx, "the pane to settle its selection", |cx| {
        pane.read(cx).config.selection.is_none()
    });

    cx.update(|cx| {
        assert_eq!(pane.read(cx).config.selection, None);
        assert_eq!(published.get(), 1, "the reset is reported");
    });
}

/// A listing a scope the registry does not carry is not one this pane can read,
/// which is the same reset as a delisting.
#[gpui_kit::test]
fn a_listing_on_a_route_without_statistics_should_go_back_to_nothing_chosen(
    cx: &mut TestAppContext,
) {
    let published = Rc::new(Cell::new(0));
    let spot_route = market_participants_connector::catalog::test_support::spot_route();
    let unserved = StatisticsConfig {
        selection: Some(InstrumentSelection {
            listing: market_participants_connector::catalog::test_support::listing_key(
                &spot_route,
                "BTCUSDT",
            ),
        }),
        ..config()
    };
    let pane = pane_reporting_over(
        cx,
        FakeSource::new([FetchPlan::Pending]),
        vec![market_participants_connector::catalog::test_support::entry(
            &spot_route,
            market_participants_connector::catalog::test_support::spot_token(
                "binance", "BTC", "USDT",
            ),
            "BTCUSDT",
        )],
        unserved,
        published.clone(),
    );
    // The scope the config follows is a market like any other: it is in memory, and
    // what it does not serve is what the pane settles.
    cx.update(|cx| register_scope(cx, &spot_route, &[]));
    settle_until(cx, "the pane to settle its selection", |cx| {
        pane.read(cx).config.selection.is_none()
    });

    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).config.selection,
            None,
            "the fixture spot route declares no statistics"
        );
        assert_eq!(published.get(), 1);
    });
}

/// A route that still publishes the listing leaves the selection exactly as it was.
#[gpui_kit::test]
fn a_listing_the_route_still_publishes_should_survive_starting(cx: &mut TestAppContext) {
    let published = Rc::new(Cell::new(0));
    let (pane, source) = reporting_pane(cx, config(), published.clone());
    // The poll the settle leads to is what says the settle happened at all: a lookup
    // still in flight would leave this at zero.
    settle_until(cx, "the pane to poll", |_| source.fetch_count() > 0);

    cx.update(|cx| {
        assert_eq!(pane.read(cx).config.selection, Some(selection()));
    });
    assert_eq!(published.get(), 0, "a supported scope is not a change");
}

/// The listing is settled against the program before anything is asked for: a pane whose
/// listing was delisted never polls for it.
#[gpui_kit::test]
fn a_listing_the_route_delisted_should_never_be_polled(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_over(cx, source.clone(), delisted(), config());
    settle_until(cx, "the pane to settle its selection", |cx| {
        pane.read(cx).config.selection.is_none()
    });
    assert_eq!(
        source.fetch_count(),
        0,
        "the pane polled for a listing the route no longer publishes"
    );
}

/// A route that still publishes the listing is followed as soon as it answers, which is what
/// makes the settle a step before the poll rather than instead of it.
#[gpui_kit::test]
fn a_listing_the_route_still_publishes_should_be_polled(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let _pane = pane_over(cx, source.clone(), listed(), config());

    settle_until(cx, "the pane to poll", |_| source.fetch_count() > 0);
}

/// A market the user has not loaded is a wait rather than a failure (⑮): the pane
/// names what to load, polls nothing, and keeps the scope it was given — a market that
/// is not in memory proves nothing about what its routes publish.
///
/// The fixture states the market as not loaded rather than as a directory that fails
/// to answer, because the two are one state under V3: a venue appears in the loaded
/// venues exactly when its whole catalog committed, so a load whose directory failed
/// leaves no venue behind at all.
#[gpui_kit::test]
fn a_market_that_is_not_in_memory_should_not_be_polled(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_over_unloaded(cx, source.clone());

    cx.update(|cx| {
        assert_eq!(
            source.fetch_count(),
            0,
            "a market the user has not loaded is not polled"
        );
        assert_eq!(
            pane.read(cx).config.selection,
            Some(selection()),
            "waiting for a market does not unmake the user's choice"
        );
        let status = pane.read(cx).status.state(cx);
        assert!(
            !pane.read(cx).status.is_failed(cx),
            "an unloaded market is not a failure of the pane"
        );
        assert_eq!(
            status,
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: vec![perpetual_route().venue],
                supported: Widget::listed_venues(),
            }),
            "waiting is not working"
        );
        let notice = status.notice().expect("the pane waits for its market");
        let sentence = notice.resolve(cx);
        assert!(
            sentence.contains("Binance"),
            "the notice names the market: {sentence}"
        );
    });
}

/// Switches the language and asserts the pane's waiting copy is the one the catalogue
/// answers with now.
///
/// `sentence` is what the catalogue holds in the language showing now. The notice is a
/// reference into that catalogue rather than a resolved sentence, so the language it reads
/// in is the one showing on the frame that draws it; nothing here reloads the pane or
/// re-reports for it.
fn the_waiting_copy_should_follow_the_language<M: StatisticsMetric>(
    cx: &mut TestAppContext,
    pane: &Entity<StatisticsWidget<M>>,
    sentence: impl Fn(&App) -> SharedString,
) {
    let (notice, english) = cx.update(|cx| {
        let notice = pane
            .read(cx)
            .status
            .state(cx)
            .notice()
            .cloned()
            .expect("the pane is waiting");
        let english = notice.resolve(cx);
        assert_eq!(
            english,
            sentence(cx),
            "the pane waits with its own sentence"
        );

        (notice, english)
    });

    cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

    cx.update(|cx| {
        let chinese = notice.resolve(cx);
        assert_eq!(
            chinese,
            sentence(cx),
            "the waiting copy follows the language showing now"
        );
        assert_ne!(
            chinese, english,
            "and it is not the copy of the language the user left"
        );
        assert_eq!(
            pane.read(cx).status.state(cx).notice(),
            Some(&notice),
            "the reference is what the pane reported; nothing re-reported it"
        );
    });
}

/// The market notice of a pane pointed at the fixture route, in the language showing now.
///
/// The sentence ends with every venue that answers with these statistics, one per line,
/// which is the list the pane hands over — so the fixture is built the way the pane builds
/// it: the venues come from the same definition-table answer, and each line is written the
/// way the sentence writes one — the venue's logo at the size the sentence names, then its
/// name in bold.
fn market_sentence(cx: &App) -> SharedString {
    let listed = Widget::listed_venues()
        .iter()
        .map(|venue| {
            format!(
                "![](file://{}?size=16) **{}**",
                venue.asset_path(),
                t!(cx, venue.name_key())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    t!(
        cx,
        "common.market_not_loaded",
        markets = listed,
        control = t!(cx, "window.market_participants.title")
    )
}

/// The notice is a reference into the catalogue rather than a sentence, so a pane waiting
/// for its market reads in the language now showing — without a reload and with nothing
/// re-reporting for it.
#[gpui_kit::test]
fn a_waiting_pane_should_name_its_market_in_the_language_showing_now(cx: &mut TestAppContext) {
    let pane = pane_over_unloaded(cx, FakeSource::new([FetchPlan::Pending]));

    the_waiting_copy_should_follow_the_language(cx, &pane, market_sentence);
}

/// The same for the other sentence this pane can be waiting with: nothing chosen.
#[gpui_kit::test]
fn a_pane_with_nothing_chosen_should_ask_for_one_in_the_language_showing_now(
    cx: &mut TestAppContext,
) {
    let pane = pane_with(cx, StatisticsConfig::default());

    the_waiting_copy_should_follow_the_language(cx, &pane, |cx| {
        t!(cx, "widget.perpetual_statistics.select_instrument_prompt")
    });
}

/// The notice is produced by the shared pane, so every widget this crate builds follows the
/// language: which series a pane plots is the only thing that differs between the five.
#[gpui_kit::test]
fn every_widget_of_this_crate_should_follow_the_language(cx: &mut TestAppContext) {
    let pane = unloaded_pane_of(cx, |status, recovery, cx| {
        TakerVolumeWidget::new_with_source(
            config(),
            FakeSource::new([FetchPlan::Pending]),
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    });

    the_waiting_copy_should_follow_the_language(cx, &pane, market_sentence);
}

/// Loading the venue is what a waiting pane recovers from, by itself and without
/// being reopened (⑯).
#[gpui_kit::test]
fn loading_the_venue_should_let_the_waiting_pane_poll(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_over_unloaded(cx, source.clone());
    cx.update(|cx| {
        assert!(
            pane.read(cx).status.state(cx).notice().is_some(),
            "the pane waits for a market nobody loaded"
        );
        assert_eq!(source.fetch_count(), 0);

        let venue = VenueId::from_name(market_participants_connector::catalog::test_support::VENUE)
            .expect("the fixture venue is canonical");
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(&[venue]),
                &[],
                &[],
            )
            .expect("the fixture venue's routes load");
        // A publication is what emits the directory's change signal; a load alone
        // does not.
        market_participants_connector::catalog::test_support::publish_fixture_catalogs(
            cx,
            &[(
                perpetual_route(),
                vec![perpetual_entry("BTC", "USDT", "BTCUSDT")],
            )],
        );
    });
    // The directory signals its publications through its own task, so the event that
    // re-runs the load arrives on the executor.
    cx.run_until_parked();
    settle_until(cx, "the pane to poll", |_| source.fetch_count() > 0);

    cx.update(|cx| {
        let status = pane.read(cx).status.state(cx);
        assert!(
            status.notice().is_none(),
            "the pane left the notice without being reopened"
        );
        assert!(!pane.read(cx).status.is_failed(cx));
    });
}

/// A refresh of the venue the pane reads replaces the catalogue generation its live poll resolved
/// the listing's units and precision against, while that poll keeps what it resolved when it was
/// opened. The pane remembers nothing for it — this protocol's `Load` already replaces the stream
/// its scope is on — so starting over is the whole of what it has to do: the window that was read
/// at the retired generation goes, and the units the chart is labelled in afterwards are the new
/// generation's.
///
/// What proves the poll was really replaced is the fake's own record: a stream fetches the
/// listing's precision on its first cycle, so a second fetch is a second stream.
#[gpui_kit::test]
fn a_refresh_of_the_panes_own_venue_should_start_it_over(cx: &mut TestAppContext) {
    // One window, and then a stream that never answers: the pane is reading before the refresh and
    // waiting after it, which is what makes the window it dropped observable.
    let source = FakeSource::new([FetchPlan::Success(1), FetchPlan::Pending]);
    let pane = pane_over(cx, source.clone(), listed(), config());
    settle_until(cx, "the pane to accept its first window", |cx| {
        pane.read(cx).has_content()
    });
    let generation = cx.read(|cx| pane.read(cx).generation_for_test());

    let venue = VenueId::from_name(market_participants_connector::catalog::test_support::VENUE)
        .expect("the fixture venue is canonical");
    cx.update(|cx| {
        // The same venue loads again and the catalogue it republishes restates the listing's quote
        // asset, which is the shape a production load has: it re-fetches the catalogue.
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(std::slice::from_ref(
                    &venue,
                )),
                &[],
                &[],
            )
            .expect("the venue loads again");
        market_participants_connector::catalog::test_support::publish_fixture_catalogs(
            cx,
            &[(
                perpetual_route(),
                vec![perpetual_entry("BTC", "USDC", "BTCUSDT")],
            )],
        );
    });
    settle_until(cx, "the pane to poll again", |_| {
        source.metadata_fetches() == 2
    });

    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).generation_for_test(),
            generation + 1,
            "a refresh of the venue the pane reads starts it over"
        );
        assert!(
            !pane.read(cx).has_content(),
            "and the window is gone: it was read at the generation that was replaced"
        );
        assert_eq!(
            pane.read(cx).units_for_test().quote.as_str(),
            "USDC",
            "the pane resolved the chart's units at the generation this load published"
        );
    });
}

/// A pane opened with nothing chosen reads nothing, and the instrument the user then picks is what
/// moves what it reads — not a venue commit. A commit of another venue therefore cannot move the
/// pane's scope, so the pane leaves the stream it started and the window it accepted exactly as
/// they are rather than starting over what the user is watching.
///
/// The commit is proved to have reached the pane by a probe subscribed after it did, and what
/// proves the pane did not start over is its own directory generation beside the window it was
/// drawing and the status it reported for it.
#[gpui_kit::test]
fn choosing_an_instrument_should_leave_the_pane_alone_on_another_venues_commit(
    cx: &mut TestAppContext,
) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_reporting_over(
        cx,
        source.clone(),
        listed(),
        StatisticsConfig {
            selection: None,
            ..config()
        },
        Rc::new(Cell::new(0)),
    );
    // The commits the fixture itself made — the venue loading, and the catalog publication — are
    // drained before anything is chosen: what this test is about is the commit below, and one still
    // in flight would be handled as a commit of a market the pane had only just started following.
    cx.run_until_parked();
    let deliveries = Arc::new(AtomicUsize::new(0));
    let _probe = cx.update(|cx| {
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
    });
    let delivered = deliveries.load(Ordering::Acquire);

    cx.update(|cx| {
        assert!(
            pane.read(cx).config.selection.is_none(),
            "the pane is opened with nothing chosen and nothing to read"
        );
        assert!(
            pane.update(cx, |pane, cx| pane.select_selection(selection(), cx)),
            "the picker offers the listing the loaded route publishes"
        );
    });
    settle_until(cx, "the pane to poll the instrument the user chose", |_| {
        source.fetch_count() > 0
    });
    let generation = cx.read(|cx| pane.read(cx).generation_for_test());

    // The pane is reading: the window it accepted is the whole of what it draws.
    cx.update(|cx| {
        pane.update(cx, |pane, context| {
            pane.apply_latest(
                key(),
                Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot(1)))),
                context,
            );
        });
        pane.update(cx, |pane, context| {
            pane.apply_reliable(
                key(),
                Arc::new(StatisticsStreamMessage::Status(ready(1))),
                context,
            );
        });
        assert!(pane.read(cx).has_content(), "the pane is reading a window");
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::Ready,
            "the window it accepted is what it reported ready"
        );
    });

    // Another venue commits: nothing about this pane's scope moved, so the read it is in the
    // middle of is left exactly as it is.
    cx.update(|cx| {
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(&[VenueId::Okx]),
                &[],
                &[],
            )
            .expect("another venue loads");
    });
    settle_until(
        cx,
        "the other venue's commit to reach its subscribers",
        |_| deliveries.load(Ordering::Acquire) > delivered,
    );

    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).generation_for_test(),
            generation,
            "a commit of a venue this pane does not follow must not start it over"
        );
        assert!(
            pane.read(cx).has_content(),
            "the window the user is reading must not be dropped"
        );
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::Ready,
            "and the pane must not go back to connecting"
        );
    });
}

/// The other direction of the same rule: a commit that takes the pane's own market away is a market
/// it can no longer read, so the pane goes back to waiting for it — without being reopened, and
/// without losing the choice the user made.
///
/// A pane that is waiting with nothing chosen is not the case here: the pane was reading, and the
/// commit is what moves its scope out of memory.
#[gpui_kit::test]
fn a_commit_that_takes_the_panes_own_market_away_should_send_it_back_to_waiting(
    cx: &mut TestAppContext,
) {
    let pane = pane(cx);
    cx.update(|cx| {
        assert!(
            !pane.read(cx).awaiting_market,
            "the pane is reading the market it was built over"
        );
        pane.update(cx, |pane, context| {
            pane.apply_latest(
                key(),
                Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot(1)))),
                context,
            );
        });
        pane.update(cx, |pane, context| {
            pane.apply_reliable(
                key(),
                Arc::new(StatisticsStreamMessage::Status(ready(1))),
                context,
            );
        });
        assert!(pane.read(cx).has_content(), "the pane is reading a window");
    });

    // The venue loads again and publishes no scope at all, which is what a shrinking commit does:
    // the market behind the window is not in memory any more.
    cx.update(|cx| {
        let venue = VenueId::from_name(market_participants_connector::catalog::test_support::VENUE)
            .expect("the fixture venue is canonical");
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(&[], &[(venue, Vec::new())], &[])
            .expect("the shrinking commit lands");
    });
    cx.run_until_parked();

    cx.update(|cx| {
        assert!(
            pane.read(cx).awaiting_market,
            "the pane waits for the market it lost"
        );
        assert!(
            !pane.read(cx).has_content(),
            "and the window it was drawing is not backed by a market any more"
        );
        assert_eq!(
            pane.read(cx).config.selection,
            Some(selection()),
            "a market that went away does not unmake the user's choice"
        );
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: vec![perpetual_route().venue],
                supported: Widget::listed_venues(),
            }),
            "waiting is not working"
        );
    });
}

/// A failure is terminal: a venue commit does not revive a failed pane, even when the market
/// it follows leaves the loaded venues and comes back — a set that changed twice.
///
/// The first commit is the one a pane without the terminal guard would act on: it takes the
/// pane's own scope away, so an unguarded handler would replace the failure with the neutral
/// notice. The second puts the market back, which a handler that judged "did what I read
/// change" without the guard would read as a scope that appeared and would poll for.
#[gpui_kit::test]
fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
    let pane = pane(cx);
    let failure = cx.update(|cx| {
        pane.update(cx, |pane, context| {
            pane.apply_latest(
                key(),
                Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot(1)))),
                context,
            );
        });
        // The stream reports the failure itself rather than a test cancelling the binding to
        // report one: a cancelled call answers with its own error, which would be a second
        // failure over this one and nothing to do with the commit below.
        pane.update(cx, |pane, context| {
            pane.apply_reliable(
                key(),
                Arc::new(StatisticsStreamMessage::Status(failed())),
                context,
            );
        });

        let state = pane.read(cx).status.state(cx);
        assert!(state.is_failed(), "the pane is in its terminal failure");
        state
    });
    let venue = VenueId::from_name(market_participants_connector::catalog::test_support::VENUE)
        .expect("the fixture venue is canonical");

    let deliveries = Arc::new(AtomicUsize::new(0));
    let _probe = cx.update(|cx| {
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
    });
    // The market the pane follows leaves the loaded venues...
    cx.update(|cx| {
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(&[], &[(venue, Vec::new())], &[])
            .expect("the shrinking commit lands");
    });
    settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
        deliveries.load(Ordering::Acquire) >= 1
    });
    // ...and comes back, under the route it was registered at.
    cx.update(|cx| {
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(&[venue]),
                &[],
                &[],
            )
            .expect("the venue loads again");
    });
    settle_until(
        cx,
        "the commit that puts the market back to reach its subscribers",
        |_| deliveries.load(Ordering::Acquire) >= 2,
    );

    cx.update(|cx| {
        assert!(
            pane.read(cx).status.is_failed(cx),
            "a venue commit must not revive a failed pane"
        );
        assert_eq!(
            pane.read(cx).status.state(cx),
            failure,
            "and it must not replace the failure with anything else"
        );
        assert!(
            !pane.read(cx).awaiting_market,
            "a pane in its terminal failure is not a pane waiting for a market"
        );
    });
}

/// A pane built the way the framework builds one, over a route that no longer carries the
/// listing its config follows.
fn pane_reporting(
    cx: &mut TestAppContext,
    venue: Venue,
    published: Rc<Cell<usize>>,
) -> Entity<Widget> {
    let source = FakeSource::new([FetchPlan::Pending]);
    source.delist();
    pane_reporting_over(cx, source, venue, config(), published)
}

/// The rules of the fixture perpetual listing, which a test needs to check which
/// assets the chart names its axes after.
fn listing_rules() -> TradingRules {
    perpetual_entry("BTC", "USDT", "BTCUSDT").snapshot.rules
}

/// One asset identity of the fixture catalog.
fn fixture_asset(code: &str) -> AssetId {
    market_participants_connector::catalog::test_support::token("binance", code)
}

/// Reports a status as the stream would.
///
/// Cancelling the binding first is what makes the reported message the only input
/// the pane has: the load it was still holding is dropped with the runtime, so
/// nothing else can answer.
fn report_status(pane: &Entity<Widget>, status: StatisticsStatus, cx: &mut App) -> bool {
    pane.update(cx, |pane, context| {
        pane.cancel_binding();
        pane.apply_reliable(
            key(),
            Arc::new(StatisticsStreamMessage::Status(status)),
            context,
        )
    })
}

/// Reports a window as the stream would.
fn report_snapshot(pane: &Entity<Widget>, snapshot: StatisticsSnapshot, cx: &mut App) -> bool {
    pane.update(cx, |pane, context| {
        pane.cancel_binding();
        pane.apply_latest(
            key(),
            Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot))),
            context,
        )
    })
}

#[gpui_kit::test]
fn a_pane_without_a_scope_should_ask_for_one(cx: &mut TestAppContext) {
    let pane = pane_with(cx, StatisticsConfig::default());

    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::waiting(Notice::message(
                "widget.perpetual_statistics.select_instrument_prompt"
            )),
            "a pane with no instrument says what it is waiting for"
        );
        assert!(!pane.read(cx).has_content());
        assert!(
            !pane.read(cx).has_chart(),
            "there is nothing to draw without a scope"
        );
    });
}

/// The gate comes before the ask: a pane that follows nothing needs a market just as much as
/// one that follows something, because the picker it would choose from is the loaded venues.
/// It waits for the market participant's metadata and says so, and only a loaded market turns that into
/// the "select an instrument" notice above.
#[gpui_kit::test]
fn a_pane_without_a_scope_over_no_market_should_wait_for_one(cx: &mut TestAppContext) {
    let pane = unloaded_pane_of(cx, |status, recovery, cx| {
        Widget::new_with_source(
            StatisticsConfig::default(),
            FakeSource::new([FetchPlan::Pending]),
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    });

    cx.update(|cx| {
        let status = pane.read(cx).status.state(cx);
        assert!(
            status.failure().is_none(),
            "an unloaded market is a wait rather than a failure"
        );
        assert_eq!(
            status,
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: Widget::listed_venues(),
                supported: Widget::listed_venues(),
            }),
            "nothing is chosen, so no single market can be named and the list stands for all of them"
        );

        let notice = status.notice().expect("the pane waits for its market");
        let sentence = notice.resolve(cx);
        assert!(
            sentence.contains("Binance") && sentence.contains("OKX"),
            "and the notice names the venues that could answer: {sentence}"
        );
    });
}

/// The two drawing controls are enabled by one thing: a scope.
///
/// Without an instrument there is nothing to fetch and no interval to fetch it
/// over, so the interval picker is disabled for the same reason the refresh button
/// is. This is the predicate both controls read, and it flips when a pane is
/// pointed at an instrument.
#[gpui_kit::test]
fn a_scope_should_be_what_enables_the_drawing_controls(cx: &mut TestAppContext) {
    cx.update(install_app);
    let unscoped = pane_on(cx, StatisticsConfig::default());
    let pane = pane_on(cx, config());

    cx.update(|cx| {
        assert!(
            !unscoped.read(cx).has_scope_for_test(),
            "a pane with no instrument has nothing to fetch, so its controls are disabled"
        );
        assert!(
            pane.read(cx).has_scope_for_test(),
            "the fixture that names an instrument has something to fetch"
        );
    });
}

/// A pane that is waiting for its market has nothing to fetch, so the two drawing controls in its
/// header refuse input: the intervals it would offer are the ones the route behind the scope
/// publishes, and the refresh has no schedule to bring forward. Both read the predicate this
/// asserts, which is why one assertion covers the pair.
#[gpui_kit::test]
fn waiting_for_a_market_should_disable_the_drawing_controls(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_over_unloaded(cx, source);

    cx.update(|cx| {
        assert!(
            pane.read(cx).awaiting_market,
            "the fixture pane really is waiting for its market"
        );
        assert!(
            !pane.read(cx).has_scope_for_test(),
            "a waiting pane has nothing to fetch, so the interval picker refuses input"
        );
        assert!(
            pane.read(cx).refresh_is_disabled(cx),
            "and there is no schedule to bring forward either"
        );
    });
}

#[gpui_kit::test]
fn a_pane_with_a_scope_should_draw_what_arrives(cx: &mut TestAppContext) {
    let pane = pane(cx);
    cx.update(|cx| {
        assert!(
            !pane.read(cx).has_content(),
            "a pane is not content until it has a window"
        );
    });

    cx.update(|cx| {
        report_status(&pane, ready(1), cx);
        assert!(
            !pane.read(cx).has_content(),
            "readiness before the window it describes is not content"
        );
        assert_eq!(pane.read(cx).status.state(cx), WidgetState::Loading);
    });

    cx.update(|cx| {
        let changed = report_snapshot(&pane, snapshot(1), cx);

        assert!(changed, "accepting a window is a change worth a frame");
        assert!(pane.read(cx).has_content());
        assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        assert!(!pane.read(cx).status.is_failed(cx));
    });
}

#[gpui_kit::test]
fn a_window_arriving_before_its_readiness_should_still_be_drawn(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_status(&pane, ready(1), cx);
        report_snapshot(&pane, snapshot(1), cx);
        assert!(pane.read(cx).has_content());
        assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
    });
}

#[gpui_kit::test]
fn readiness_for_a_newer_window_should_not_claim_the_older_one(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_status(&pane, ready(2), cx);
        report_snapshot(&pane, snapshot(1), cx);
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::Loading,
            "a window older than the readiness that arrived is not what was reported ready"
        );
        assert!(!pane.read(cx).status.is_failed(cx));
    });
}

#[gpui_kit::test]
fn the_countdown_should_be_the_refresh_buttons_own_label(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, ready(1), cx);
        let now = widget::util::unix_millis();
        pane.update(cx, |pane, cx| pane.tick_countdown_for_test(cx));

        let countdown = pane
            .read(cx)
            .countdown
            .clone()
            .expect("a ready pane counts down to its next fetch");
        assert!(
            countdown
                .chars()
                .all(|character| character.is_ascii_digit() || character == ':'),
            "the label is the time left on its own: {countdown}"
        );
        assert!(
            pane.read(cx).next_refresh_at.is_some_and(|due| due > now),
            "the pane counts down to a fetch that is still ahead of it"
        );
    });
}

/// The countdown belongs to a ready pane, and only to a ready pane.
///
/// A fetch in flight has a deadline too — the one it will arm when it lands — but
/// counting down to it before the data is on screen would be counting down to a
/// window the pane does not have yet. So the label appears with readiness and is
/// taken away by anything that is not it, and the control is greyed with a bare icon
/// for the same stretch.
#[gpui_kit::test]
fn a_countdown_should_only_run_while_the_pane_is_ready(cx: &mut TestAppContext) {
    let pane = pane(cx);
    let tick = |pane: &Entity<Widget>, cx: &mut App| {
        pane.update(cx, |pane, cx| pane.tick_countdown_for_test(cx));
    };

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, ready(1), cx);
        tick(&pane, cx);
        assert!(
            pane.read(cx).countdown.is_some(),
            "a ready pane counts down to its next fetch"
        );
        assert!(!pane.read(cx).refresh_is_disabled(cx));
    });

    cx.update(|cx| {
        report_status(&pane, refreshing(), cx);
        tick(&pane, cx);

        assert!(
            pane.read(cx).countdown.is_none(),
            "a fetch in flight has no countdown to show"
        );
        assert!(
            pane.read(cx).refresh_is_disabled(cx),
            "and nothing for the control to bring forward"
        );
    });

    cx.update(|cx| {
        report_status(&pane, ready(2), cx);
        tick(&pane, cx);

        assert!(
            pane.read(cx).countdown.is_none(),
            "readiness without the window it describes is not something to count down from"
        );
        assert!(pane.read(cx).refresh_is_disabled(cx));

        report_snapshot(&pane, snapshot(2), cx);
        tick(&pane, cx);

        assert!(
            pane.read(cx).countdown.is_some(),
            "and with the window in hand the countdown starts"
        );
        assert!(!pane.read(cx).refresh_is_disabled(cx));
    });
}

#[gpui_kit::test]
fn a_failed_fetch_should_offer_a_recovery_over_the_chart(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, failed(), cx);

        assert!(pane.read(cx).status.is_failed(cx));
        let failure = pane
            .read(cx)
            .status
            .state(cx)
            .failure()
            .cloned()
            .expect("a failure should be published");
        assert!(
            !failure.text(cx).is_empty(),
            "a failure has to say what went wrong"
        );
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "a failure always offers the framework's own way out"
        );
        assert!(
            pane.read(cx).has_content(),
            "the chart the user was reading stays on screen"
        );
    });
}

#[gpui_kit::test]
fn a_message_from_another_scope_should_be_ignored(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        let other = StatisticsStreamKey::new(
            MetricKind::TakerVolume,
            other_selection().listing,
            StatisticsTimeFrame::Hour1,
        );
        let changed = pane.update(cx, |pane, context| {
            pane.cancel_binding();
            pane.apply_latest(
                other,
                Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot(1)))),
                context,
            )
        });

        assert!(!changed, "another scope's window is not this pane's");
        assert!(!pane.read(cx).has_content());
    });
}

#[gpui_kit::test]
fn a_message_after_a_failure_should_be_ignored(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_status(&pane, failed(), cx);
        assert!(pane.read(cx).status.is_failed(cx));

        let status_changed = report_status(&pane, StatisticsStatus::Connecting, cx);
        let snapshot_changed = report_snapshot(&pane, snapshot(7), cx);

        assert!(!status_changed, "a failed pane takes no more statuses");
        assert!(!snapshot_changed, "a failed pane takes no more windows");
        assert!(!pane.read(cx).has_content());
        assert!(pane.read(cx).status.is_failed(cx));
    });
}

#[gpui_kit::test]
fn a_window_the_pane_has_overtaken_should_be_ignored(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(2), cx);
        let changed = report_snapshot(&pane, snapshot(1), cx);
        assert!(
            !changed,
            "a window older than the one on screen would only make the chart jump back"
        );
    });
}

#[gpui_kit::test]
fn following_another_listing_should_restart_the_chart(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        let other = other_selection();
        let accepted = pane.update(cx, |pane, context| {
            pane.select_selection(other.clone(), context)
        });

        assert!(
            accepted,
            "another perpetual listing is a scope we can follow"
        );
        assert_eq!(pane.read(cx).config.selection.as_ref(), Some(&other));
        assert!(
            !pane.read(cx).has_content(),
            "another listing is another chart, so nothing of the old one is left"
        );
        assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
    });
}

/// Two listings of the very same canonical contract are two scopes: what a pane
/// follows is the venue's own listing, so picking the other one is a different chart.
#[gpui_kit::test]
fn two_listings_of_one_contract_should_stay_apart(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        let first = pane.read(cx).config.selection.clone().expect("a scope");
        let other = InstrumentSelection {
            listing: market_participants_connector::catalog::test_support::listing_key(
                &market_participants_connector::catalog::test_support::route(
                    "binance",
                    "core-perp",
                ),
                "BTCUSDT",
            ),
        };
        assert_ne!(
            first.listing, other.listing,
            "the two keys name different listings"
        );

        assert!(pane.update(cx, |pane, context| {
            pane.select_selection(other.clone(), context)
        }));

        let pane = pane.read(cx);
        assert_eq!(pane.config.selection, Some(other.clone()));
        assert_eq!(
            pane.current_key(),
            Some(StatisticsStreamKey::new(
                MetricKind::OpenInterest,
                other.listing,
                StatisticsTimeFrame::Minute5
            )),
            "the stream key follows the other listing"
        );
    });
}

/// A route the registry does not carry a series for is not one a pane of that
/// series can be pointed at.
#[gpui_kit::test]
fn only_a_route_that_declares_the_series_should_be_readable(cx: &mut TestAppContext) {
    let _pane = pane(cx);

    cx.update(|cx| {
        assert!(
            route_serves_statistics(cx, &perpetual_route(), CapabilityId::OpenInterest),
            "the fixture perpetual route declares open interest"
        );
        assert!(
            !route_serves_statistics(
                cx,
                &market_participants_connector::catalog::test_support::spot_route(),
                CapabilityId::OpenInterest
            ),
            "a spot route serves no perpetual statistics"
        );
        assert!(
            !route_serves_statistics(
                cx,
                &market_participants_connector::catalog::test_support::perp_route(),
                CapabilityId::OpenInterest
            ),
            "a scope the registry does not declare is not readable"
        );
        assert!(
            !route_serves_statistics(cx, &perpetual_route(), CapabilityId::WalletMonitor),
            "the route declares one series per capability"
        );
    });
}

/// The interval is the pane's own choice, and another one is another window over
/// the same listing.
#[gpui_kit::test]
fn another_interval_should_keep_the_listing(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        pane.update(cx, |pane, context| {
            pane.select_timeframe(StatisticsTimeFrame::Hour4, context)
        });

        let pane = pane.read(cx);
        assert_eq!(pane.config.timeframe, StatisticsTimeFrame::Hour4);
        assert_eq!(pane.config.selection, Some(selection()));
    });
}

#[gpui_kit::test]
fn following_the_same_listing_again_should_change_nothing(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        let accepted = pane.update(cx, |pane, context| {
            pane.select_selection(selection(), context)
        });

        assert!(!accepted, "the same scope is not a reason to reload");
        assert!(pane.read(cx).has_content());
    });
}

#[gpui_kit::test]
fn another_interval_should_restart_the_chart(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        pane.update(cx, |pane, context| {
            pane.select_timeframe(StatisticsTimeFrame::Hour4, context)
        });

        assert_eq!(
            pane.read(cx).config.timeframe,
            StatisticsTimeFrame::Hour4,
            "the interval is part of the scope"
        );
        assert!(!pane.read(cx).has_content());
    });
}

/// Another interval is another scope, and it is reached the same way the first one was: the
/// program is asked again and the poll follows it.
#[gpui_kit::test]
fn another_interval_should_be_polled(cx: &mut TestAppContext) {
    let source = FakeSource::new([FetchPlan::Pending]);
    let pane = pane_over(cx, source.clone(), listed(), config());
    settle_until(cx, "the pane to poll", |_| source.fetch_count() > 0);

    cx.update(|cx| {
        pane.update(cx, |pane, context| {
            pane.select_timeframe(StatisticsTimeFrame::Hour4, context)
        });
    });
    settle_until(cx, "the pane to poll its new interval", |_| {
        source
            .queries()
            .iter()
            .any(|query| query.timeframe == StatisticsTimeFrame::Hour4)
    });
    cx.update(|cx| {
        assert_eq!(
            pane.read(cx).config.timeframe,
            StatisticsTimeFrame::Hour4,
            "the interval the user chose survives the switch"
        );
        assert_eq!(
            pane.read(cx).config.selection,
            Some(selection()),
            "so does the scope it is polled over"
        );
        assert!(
            !pane.read(cx).status.is_failed(cx),
            "switching an interval is not a failure of the pane"
        );
    });
}

/// A window's view, holding the pane under test.
struct HeaderHarness {
    pane: Entity<Widget>,
    header: std::rc::Rc<std::cell::RefCell<Option<widget::view::WidgetHeader>>>,
}

impl Render for HeaderHarness {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let row = match self.header.borrow_mut().take() {
            Some(header) => gpui_kit::div()
                .flex()
                .items_center()
                .gap_1()
                .children(header.left)
                .children(header.right),
            None => gpui_kit::div(),
        };
        gpui_kit::div()
            .size_full()
            .child(row)
            .child(self.pane.clone())
    }
}

/// A window holding one pane's body and nothing else.
///
/// The framework embeds a pane's body in a cached slot of its own; a test that has
/// to resize the window around it only needs the body, because the box the pane is
/// given is the box the framework would have given it.
struct BodyHarness {
    pane: Entity<Widget>,
}

impl Render for BodyHarness {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        self.pane.clone()
    }
}

/// A pane narrower than its chart's own minimum is still readable.
///
/// The chart keeps a minimum width, because its axes and its legend need the room;
/// a pane below that minimum has to scroll the chart inside itself rather than clip
/// an axis the user can never reach. That only happens while the pane hands its
/// chart the box it actually has: a pane that imposed the minimum on itself would
/// widen its own body past its edges, and the chart inside it would then have
/// nothing left to scroll.
#[gpui_kit::test]
fn a_pane_narrower_than_its_chart_should_scroll_it_rather_than_clip_it(cx: &mut TestAppContext) {
    let pane = pane(cx);
    cx.update(|cx| report_snapshot(&pane, snapshot(1), cx));

    let (harness, cx) = cx.add_window_view(|_, _| BodyHarness { pane });
    cx.simulate_resize(size(px(360.0), px(240.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let scroll = harness
        .read_with(cx, |harness, cx| {
            harness.pane.read(cx).chart_horizontal_scroll()
        })
        .expect("a pane with a scope has a chart to scroll");

    assert_eq!(
        scroll.bounds().size.width,
        px(360.0),
        "the chart's viewport is the pane, not the minimum the chart asked for"
    );
    assert!(
        scroll.max_offset().x > px(0.0),
        "and the rest of the chart is reachable by scrolling inside it: {:?}",
        scroll.max_offset()
    );
}

#[gpui_kit::test]
fn the_header_should_offer_the_scope_the_interval_and_a_refresh(cx: &mut TestAppContext) {
    let pane = pane(cx);

    // A view is what a window can hold, so the test mounts the pane inside one:
    // the header's content is the pane's decision, and nothing about it depends on
    // the window it is asked in.
    let (harness, window) = cx.add_window_view(|_, _| HeaderHarness {
        pane,
        header: std::rc::Rc::new(std::cell::RefCell::new(None)),
    });
    let header = window.update(|window, cx| {
        harness.update(cx, |harness, cx| {
            harness.pane.update(cx, |pane, cx| pane.header(window, cx))
        })
    });

    assert!(
        header.left.is_some(),
        "the instrument picker is in the header's left group"
    );
    assert!(
        header.right.is_some(),
        "the interval and the refresh are in its right group"
    );
}

/// The double-click the refresh control exists to absorb.
///
/// A refresh is refused by the service while one is in flight, and the framework
/// shows a refusal as a failure — so a control that forwarded a second click inside
/// that window would answer a fast double click with a banner.
#[gpui_kit::test]
fn a_refresh_should_not_be_forwarded_while_a_window_is_being_fetched(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, ready(1), cx);
        assert!(
            !pane.read(cx).refresh_is_disabled(cx),
            "an idle pane with a window can fetch again on demand"
        );
    });

    cx.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.refresh(cx);
            assert!(
                pane.refresh_is_disabled(cx),
                "the click closes the window itself, before anything has answered"
            );
        });
    });

    cx.update(|cx| {
        report_status(&pane, refreshing(), cx);
        assert!(
            pane.read(cx).refresh_is_disabled(cx),
            "a fetch in flight has nothing to bring forward"
        );
    });

    cx.update(|cx| {
        report_status(&pane, ready(2), cx);
        assert!(
            pane.read(cx).refresh_is_disabled(cx),
            "readiness without the window it describes is not a reason to fetch again"
        );
        report_snapshot(&pane, snapshot(2), cx);
        assert!(
            !pane.read(cx).refresh_is_disabled(cx),
            "and with the window in hand there is nothing left to wait for"
        );
    });
}

/// A failure is the framework's to recover from, not the header's.
///
/// A failed pane takes no more of its stream's messages, so a refresh command sent
/// from here would be accepted by the loop and its answer dropped by the pane. The
/// control is therefore greyed with the rest of the states that are not ready, and
/// the way out is the recovery the banner above the chart offers.
#[gpui_kit::test]
fn a_failed_pane_should_offer_its_recovery_rather_than_a_refresh(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, failed(), cx);

        assert!(pane.read(cx).status.is_failed(cx));
        assert!(
            pane.read(cx).refresh_is_disabled(cx),
            "the header has nothing to bring forward, so it offers nothing"
        );
        let failure = pane
            .read(cx)
            .status
            .state(cx)
            .failure()
            .cloned()
            .expect("a failure should be published");
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "and the framework's own recovery is the way out of it"
        );
    });

    cx.update(|cx| {
        pane.update(cx, |pane, cx| pane.refresh(cx));

        assert!(
            pane.read(cx).status.is_failed(cx),
            "a disabled control changes nothing when it is pressed"
        );
    });
}

/// A failure takes the loop off the clock, so there is no countdown to show.
#[gpui_kit::test]
fn a_failed_pane_should_stop_counting_down(cx: &mut TestAppContext) {
    let pane = pane(cx);

    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, ready(1), cx);
        pane.update(cx, |pane, cx| pane.tick_countdown_for_test(cx));
        assert!(
            pane.read(cx).countdown.is_some(),
            "a ready pane counts down to its next fetch"
        );

        report_status(&pane, failed(), cx);
        pane.update(cx, |pane, cx| pane.tick_countdown_for_test(cx));

        assert!(
            pane.read(cx).countdown.is_none(),
            "a pane whose schedule stopped has nothing to count down to"
        );
    });
}

#[gpui_kit::test]
fn a_recovery_should_start_the_pane_over(cx: &mut TestAppContext) {
    let pane = pane(cx);

    let recovery = cx.update(|cx| {
        report_status(&pane, failed(), cx);
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .map(|failure| failure.action().clone())
            .expect("a failure should offer a recovery")
    });

    cx.update(|cx| recovery.run(cx));

    cx.update(|cx| {
        assert!(
            !pane.read(cx).status.is_failed(cx),
            "the recovery clears the failure it answered"
        );
    });
}

/// The two names a chart writes on its axes come from the listing's own rules:
/// the asset its quantities count and the asset its amounts are quoted in.
#[gpui_kit::test]
fn the_chart_units_should_come_from_the_listings_own_rules(cx: &mut TestAppContext) {
    let _pane = pane(cx);

    cx.update(|cx| {
        let units = ChartUnits::of(&listing_rules(), cx);

        assert_eq!(units.base.as_str(), "BTC");
        assert_eq!(units.quote.as_str(), "USDT");
    });
}

/// A quantity a listing counts in contracts is named as contracts, not as one of
/// its assets: the size of a series is not a number of coins.
#[gpui_kit::test]
fn a_contract_quantity_should_be_named_as_contracts(cx: &mut TestAppContext) {
    let _pane = pane(cx);

    cx.update(|cx| {
        let mut rules = listing_rules();
        rules.common.quantity.unit =
            market_participants_model::market::units::QuantityUnit::Contracts;

        assert_eq!(ChartUnits::of(&rules, cx).base.as_str(), "Contracts");
    });
}

/// An asset the published generation does not name is the localized unknown: the
/// canonical identity is a digest a reader cannot use, and a name invented here
/// would disagree with every other pane about the same asset.
#[gpui_kit::test]
fn an_asset_the_catalog_does_not_carry_should_be_named_as_unknown(cx: &mut TestAppContext) {
    let _pane = pane(cx);

    cx.update(|cx| {
        let mut rules = listing_rules();
        rules.common.price.quote_asset = fixture_asset("XRP");

        let units = ChartUnits::of(&rules, cx);

        assert_eq!(
            units.quote.as_str(),
            i18n::t!(cx, "error.display.unknown_asset").as_ref()
        );
        assert_ne!(
            units.quote.as_str(),
            fixture_asset("XRP").to_string(),
            "an identity digest is not a name"
        );
    });
}

/// A listing whose product is not the one these endpoints serve is refused by the
/// pane before it asks for a window, and the product it was published as is what
/// decides: the route's capabilities say nothing about one listing.
#[test]
fn only_a_perpetual_listing_should_be_served_by_these_endpoints() {
    let perpetual = perpetual_entry("BTC", "USDT", "BTCUSDT");
    let spot = market_participants_connector::catalog::test_support::entry(
        &perpetual_route(),
        market_participants_connector::catalog::test_support::spot_token("binance", "BTC", "USDT"),
        "BTCUSDT",
    );

    assert_eq!(perpetual.instrument.kind(), InstrumentKind::Perpetual);
    assert_ne!(spot.instrument.kind(), InstrumentKind::Perpetual);
}

/// An empty window is content — the venue answered, with nothing in it — but a
/// chart with no interval in it looks exactly like one that has not arrived, so the
/// pane says which of the two it is.
#[gpui_kit::test]
fn an_empty_window_should_say_it_is_empty(cx: &mut TestAppContext) {
    let pane = pane(cx);
    let empty = StatisticsSnapshot {
        revision: 1,
        metadata: Some(metadata()),
        points: StatisticsPoints::OpenInterest(Arc::from(Vec::new())),
    };

    cx.update(|cx| {
        report_status(&pane, ready(1), cx);
        assert!(
            report_snapshot(&pane, empty, cx),
            "the first window is always a change"
        );
        assert!(
            pane.read(cx).has_content(),
            "an empty window is content: the venue published it"
        );
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::Ready,
            "the pane is done working: there is nothing more coming for this window"
        );
        assert_eq!(
            pane.read(cx).chart_point_count(),
            Some(0),
            "and there is nothing in the window to draw"
        );
    });
}

/// The payload a build before the listing migration wrote named a route and an
/// instrument separately, which is not a listing this build can read. Reading never
/// fails: the whole config comes back as its defaults, and the framework writes that
/// back.
#[test]
fn an_old_payload_should_decode_to_the_defaults() {
    let stored = r#"{"schema_version":1,"payload":"{\"selection\":{\"route\":{\"venue\":\"binance\",\"market\":\"perpetual\"},\"instrument\":{\"kind\":\"perpetual\"}},\"timeframe\":\"1h\"}"}"#;
    let envelope: app_config::widget::VersionedWidgetConfig =
        serde_json::from_str(stored).expect("the stored envelope should parse");

    let decoded = envelope.decode::<StatisticsConfig>();

    assert_eq!(decoded, StatisticsConfig::default());
    assert_eq!(decoded.selection, None);
}

/// A recovery is a retry of the reading the pane already has: the window the user is looking at
/// stays on screen under the banner the failure is drawn with, rather than being blanked to
/// reconnect.
#[gpui_kit::test]
fn a_recovery_should_keep_the_window_the_user_is_reading(cx: &mut TestAppContext) {
    let pane = pane(cx);
    cx.update(|cx| {
        report_snapshot(&pane, snapshot(1), cx);
        report_status(&pane, ready(1), cx);
        assert!(pane.read(cx).has_content());

        report_status(&pane, failed(), cx);
        assert!(pane.read(cx).status.is_failed(cx));
        assert!(
            pane.read(cx).has_content(),
            "the window the user was reading stays over the failure"
        );
    });

    let recovery = cx.update(|cx| {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .map(|failure| failure.action().clone())
            .expect("a failure offers a recovery")
    });
    cx.update(|cx| recovery.run(cx));

    cx.update(|cx| {
        assert!(
            !pane.read(cx).status.is_failed(cx),
            "the recovery clears the failure it answered"
        );
        assert!(
            pane.read(cx).has_content(),
            "a retry keeps the window the user is still reading"
        );
    });
}

/// A pane that failed before its scope published anything has no reading to carry over, so the
/// recovery starts that scope over: an empty chart is not content, and the framework draws the
/// connecting placeholder over it rather than a window that never arrived said to be "no data".
#[gpui_kit::test]
fn a_recovery_before_any_window_should_start_the_pane_over(cx: &mut TestAppContext) {
    let pane = pane_with(cx, StatisticsConfig::default());
    cx.run_until_parked();

    cx.update(|cx| {
        assert!(pane.read(cx).config.selection.is_none());
        assert!(
            pane.update(cx, |pane, cx| pane.select_selection(selection(), cx)),
            "the picker offers the listing the loaded route publishes"
        );
        assert!(
            pane.read(cx).has_chart(),
            "a chart is built as soon as an instrument is chosen"
        );
        assert!(
            !pane.read(cx).has_content(),
            "an empty chart is not content: no window has been accepted"
        );
    });

    // The stream for the scope the user chose fails before publishing anything.
    cx.update(|cx| {
        report_status(&pane, failed(), cx);
        assert!(pane.read(cx).status.is_failed(cx));
        assert!(!pane.read(cx).has_content());
    });

    let recovery = cx.update(|cx| {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .map(|failure| failure.action().clone())
            .expect("a failure offers a recovery")
    });
    cx.update(|cx| recovery.run(cx));

    cx.update(|cx| {
        assert!(
            !pane.read(cx).has_content(),
            "the recovery kept nothing, so the pane must not report content"
        );
        assert_eq!(
            pane.read(cx).status.state(cx),
            WidgetState::Connecting,
            "what it started is a load of that scope rather than a retry of a window it never had"
        );
    });
}
