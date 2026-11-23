use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use app_config::time_zone::DataTimeZone;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::{
    catalog::test_support as catalog_fixture, market::timeframe::StatisticsTimeFrame,
};
use market_participants_model::{
    decimal::Quantity,
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::{ListingPrecision, TradingRules},
    },
    market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use smol_str::SmolStr;
use ui_core::node::{builder::NodeBuilder, id::NodeId, tree::NodeKind};
use widget::{
    config::WidgetConfigChange,
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState},
    test_support::WidgetTestApp,
    view::{HeaderGroup, WidgetView},
};

use super::{
    COUNTDOWN_TICK, PaneScaffold, StatisticsConfig, StatisticsMetric, StatisticsWidget, TIMEFRAMES,
    directory::{ChartUnits, ListingSupport, StatisticsDirectory, market_venues},
};
use crate::{
    chart::ChartDispatch,
    model::{MetricKind, OpenInterestPoint, StatisticsPoints, StatisticsSnapshot},
    service::{
        StatisticsFetchError, StatisticsStatus, StatisticsStreamKey, StatisticsStreamMessage,
        test_support::{FakeSource, FetchPlan, perpetual_listing, perpetual_route},
    },
    widget::{
        long_short_account_ratio::LongShortAccountRatioWidgetSeries,
        open_interest::OpenInterestWidgetSeries, taker_volume::TakerVolumeWidgetSeries,
        top_trader_account_ratio::TopTraderAccountRatioWidgetSeries,
        top_trader_position_ratio::TopTraderPositionRatioWidgetSeries,
    },
};

type Widget = StatisticsWidget<OpenInterestWidgetSeries>;

fn listing() -> VenueListingKey {
    perpetual_listing("BTCUSDT")
}

fn other_listing() -> VenueListingKey {
    perpetual_listing("ETHUSDT")
}

fn selection() -> InstrumentSelection {
    InstrumentSelection { listing: listing() }
}

fn other_selection() -> InstrumentSelection {
    InstrumentSelection {
        listing: other_listing(),
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
        listing(),
        StatisticsTimeFrame::Minute5,
    )
}

fn metadata() -> ListingPrecision {
    ListingPrecision {
        price_tick: "0.01".parse().expect("a price tick parses"),
        quantity_tick: "0.001".parse().expect("a quantity tick parses"),
    }
}

fn open_interest_points(count: usize) -> Vec<OpenInterestPoint> {
    (0..count)
        .map(|index| {
            let step = u64::try_from(index).unwrap_or(0) + 1;
            OpenInterestPoint {
                timestamp: i64::try_from(index).unwrap_or(0) * 300_000,
                base_asset_volume: Quantity::from(step * 10),
                quote_asset_volume: Some(Quantity::from(step * 100)),
            }
        })
        .collect()
}

fn snapshot(revision: u64) -> StatisticsSnapshot {
    StatisticsSnapshot {
        revision,
        metadata: Some(metadata()),
        points: StatisticsPoints::OpenInterest(open_interest_points(3).into()),
    }
}

fn empty_snapshot(revision: u64) -> StatisticsSnapshot {
    StatisticsSnapshot {
        revision,
        metadata: Some(metadata()),
        points: StatisticsPoints::OpenInterest(Vec::new().into()),
    }
}

fn ready(revision: u64, next_refresh_at: i64) -> StatisticsStatus {
    StatisticsStatus::Ready {
        revision,
        next_refresh_at,
    }
}

fn failed() -> StatisticsStatus {
    StatisticsStatus::Failed {
        error: StatisticsFetchError::Timeout(std::time::Duration::from_secs(5)),
    }
}

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn listing_rules() -> TradingRules {
    catalog_fixture::rules(
        catalog_fixture::token(catalog_fixture::VENUE, "USDT"),
        catalog_fixture::token(catalog_fixture::VENUE, "BTC"),
    )
}

fn supported() -> ListingSupport {
    ListingSupport::Supported(Box::new(listing_rules()))
}

#[derive(Clone)]
struct FixtureDirectory {
    loaded: Rc<RefCell<Vec<VenueRoute>>>,
    support: Rc<RefCell<ListingSupport>>,
    capabilities: Rc<RefCell<Vec<CapabilityId>>>,
    capability: CapabilityId,
    units: ChartUnits,
}

impl FixtureDirectory {
    fn new(loaded: bool, support: ListingSupport) -> Self {
        Self {
            loaded: Rc::new(RefCell::new(if loaded {
                vec![perpetual_route()]
            } else {
                Vec::new()
            })),
            support: Rc::new(RefCell::new(support)),
            capabilities: Rc::new(RefCell::new(vec![CapabilityId::OpenInterest])),
            capability: CapabilityId::OpenInterest,
            units: ChartUnits {
                base: SmolStr::new_static("BTC"),
                quote: SmolStr::new_static("USDT"),
            },
        }
    }

    fn offering(self, capability: CapabilityId) -> Self {
        *self.capabilities.borrow_mut() = vec![capability];
        Self { capability, ..self }
    }

    fn load(&self) {
        let mut loaded = self.loaded.borrow_mut();
        if !loaded.contains(&perpetual_route()) {
            loaded.push(perpetual_route());
        }
    }

    fn unload(&self) {
        self.loaded.borrow_mut().clear();
    }

    fn without_capability(&self) {
        self.capabilities.borrow_mut().clear();
    }
}

impl StatisticsDirectory for FixtureDirectory {
    fn loaded_scopes(&self) -> Arc<[instrument_selector_ui::instrument_selector::LoadedScope]> {
        self.loaded
            .borrow()
            .iter()
            .map(
                |route| instrument_selector_ui::instrument_selector::LoadedScope {
                    route: route.clone(),
                    capabilities: self.capabilities.borrow().clone(),
                    provides_catalog: true,
                },
            )
            .collect::<Vec<_>>()
            .into()
    }

    fn any_market_loaded(&self) -> bool {
        self.loaded
            .borrow()
            .iter()
            .any(|route| market_venues(self.capability).contains(&route.venue))
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.loaded.borrow().iter().any(|held| held == route)
    }

    fn listing_support(&self, _listing: &VenueListingKey) -> ListingSupport {
        self.support.borrow().clone()
    }

    fn units(&self, _rules: &TradingRules, _translate: &Translate) -> ChartUnits {
        self.units.clone()
    }
}

struct Fixture {
    harness: WidgetTestApp,
    widget: Widget,
    directory: FixtureDirectory,
    publishes: Rc<Cell<usize>>,
    clock: Rc<Cell<i64>>,
    page: NodeId,
    header_row: Option<NodeId>,
}

impl Fixture {
    fn new(
        config: StatisticsConfig,
        loaded: bool,
        support: ListingSupport,
        plans: impl IntoIterator<Item = FetchPlan>,
    ) -> Self {
        Self::with_source(config, loaded, support, FakeSource::new(plans))
    }

    fn with_source<S: crate::service::StatisticsSource>(
        config: StatisticsConfig,
        loaded: bool,
        support: ListingSupport,
        source: S,
    ) -> Self {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let directory = FixtureDirectory::new(loaded, support);
        let status = harness.status();
        let recovery = WidgetAction::new("common.retry", |_| {});
        let publishes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counted_publications = Rc::clone(&publishes);
        let change = WidgetConfigChange::new(move |_| {
            counted_publications.set(counted_publications.get() + 1);
        });
        let clock: Rc<Cell<i64>> = Rc::new(Cell::new(1_000_000));
        let clock_handle = Rc::clone(&clock);
        let directory_handle: Rc<dyn StatisticsDirectory> = Rc::new(directory.clone());

        let widget = harness.with_cx(|cx| {
            Widget::with_source(
                config,
                source,
                Some(directory_handle),
                PaneScaffold {
                    status,
                    config_change: change,
                    recovery,
                    now: Rc::new(move || clock_handle.get()),
                },
                cx,
            )
        });
        let page = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_PERPETUAL_STATISTICS_BODY")
                .style(
                    base_ui::style_builder::StyleBuilder::new()
                        .width(640.0)
                        .height(360.0)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the body parent builds")
        });
        Self {
            harness,
            widget,
            directory,
            publishes,
            clock,
            page,
            header_row: None,
        }
    }

    fn load(&mut self) {
        let Self {
            harness, widget, ..
        } = self;
        harness.with_cx(|cx| widget.load(cx));
    }

    fn sync(&mut self) {
        let Self {
            harness, widget, ..
        } = self;
        harness
            .with_cx(|cx| widget.sync(cx))
            .expect("the pane syncs");
    }

    fn settle(&mut self) {
        for _ in 0..12 {
            self.harness.drain_runtime();
            self.sync();
            self.harness.drain();
        }
    }

    fn state(&self) -> WidgetState {
        self.widget.status.state(self.harness.app())
    }

    fn translate(&self) -> &Translate {
        &self.harness.services().translate
    }

    fn has_content(&self) -> bool {
        self.widget.has_content()
    }

    fn build_body(&mut self) {
        let Self {
            harness,
            widget,
            page,
            ..
        } = self;
        if widget.has_content() {
            harness
                .with_cx(|cx| widget.body(*page, cx))
                .expect("the body builds");
            harness.drain();
        }
    }

    fn build_header(&mut self) {
        let Self {
            harness, widget, ..
        } = self;
        let row = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_PERPETUAL_STATISTICS_HEADER_HOST")
                .style(
                    base_ui::style_builder::StyleBuilder::new()
                        .width(640.0)
                        .height(32.0)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the header host builds")
        });
        harness
            .with_cx(|cx| widget.header(HeaderGroup::Left, row, cx))
            .expect("the left group builds");
        harness
            .with_cx(|cx| widget.header(HeaderGroup::Right, row, cx))
            .expect("the right group builds");
        harness.drain();
        self.header_row = Some(row);
    }

    fn report_status(&mut self, status: StatisticsStatus) {
        let Self {
            harness, widget, ..
        } = self;
        let message = Arc::new(StatisticsStreamMessage::Status(status));
        harness.with_cx(|cx| widget.apply_reliable(key(), message, cx));
    }

    fn report_snapshot(&mut self, snapshot: StatisticsSnapshot) {
        let Self {
            harness, widget, ..
        } = self;
        let message = Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot)));
        harness.with_cx(|cx| widget.apply_latest(key(), message, cx));
    }

    fn select(&mut self, selection: InstrumentSelection) {
        let Self {
            harness, widget, ..
        } = self;
        harness.with_cx(|cx| {
            widget.select_selection(selection, cx);
        });
    }

    fn set_timeframe(&mut self, timeframe: StatisticsTimeFrame) {
        let Self {
            harness, widget, ..
        } = self;
        harness.with_cx(|cx| widget.select_timeframe(timeframe, cx));
    }

    fn request_refresh(&mut self) {
        let Self {
            harness, widget, ..
        } = self;
        harness.with_cx(|cx| widget.refresh(cx));
    }

    fn chart(&self) -> Option<&ChartDispatch> {
        self.widget.chart_for_test()
    }

    fn canvas(&self) -> Option<&ui_core::paint::canvas::Canvas> {
        let chart = self.chart()?;
        chart.canvas_for_test(self.harness.app(), self.harness.window())
    }

    fn plot(&self) -> ui_core::math::Rect {
        self.chart()
            .and_then(ChartDispatch::layout_for_test)
            .expect("the chart was laid out")
            .plot
    }

    fn countdown(&self) -> Option<SmolStr> {
        self.widget.countdown_label(self.harness.app())
    }

    fn set_zone(&mut self, hours: i8) {
        let signal = self.harness.services().config;
        let mut config = (*self.harness.app().read(&signal)).clone();
        config.data_time_zone = DataTimeZone::from_hours(hours).expect("the offset is offered");
        self.harness.app_mut().write(&signal, config);
    }

    fn bump_generation(&mut self, generation: u64) {
        let signal = self.harness.services().catalog_generation;
        self.harness.app_mut().write(&signal, generation);
    }

    fn paint(&mut self) {
        self.build_body();
        self.sync();
        self.harness.drain();
    }
}

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
        assert!(TIMEFRAMES.contains(&timeframe), "{timeframe:?} is offered");
    }
}

#[test]
fn a_pane_without_a_scope_should_ask_for_one() {
    let mut fixture = Fixture::new(StatisticsConfig::default(), true, supported(), []);
    fixture.load();

    let WidgetState::Waiting(notice) = fixture.state() else {
        panic!("expected a waiting notice, got {:?}", fixture.state());
    };
    assert_eq!(
        notice.resolve(fixture.translate()).as_str(),
        product_translate(LanguageCode::En)
            .translate("widget.perpetual_statistics.select_instrument_prompt")
    );
    assert!(!fixture.has_content());
    assert!(!fixture.widget.has_scope_for_test());
}

#[test]
fn a_pane_without_a_scope_over_no_market_should_wait_for_one() {
    let mut fixture = Fixture::new(StatisticsConfig::default(), false, supported(), []);
    fixture.load();

    let WidgetState::Waiting(notice) = fixture.state() else {
        panic!("a pane without its market waits");
    };
    assert!(
        notice.is_markdown(fixture.translate()) || !notice.resolve(fixture.translate()).is_empty()
    );
    assert!(!fixture.has_content());
}

#[test]
fn a_listing_the_route_delisted_should_go_back_to_nothing_chosen() {
    let mut fixture = Fixture::new(config(), true, ListingSupport::Unsupported, []);
    fixture.load();

    assert_eq!(
        fixture.widget.config.selection, None,
        "the stored listing is withdrawn"
    );
    assert_eq!(fixture.publishes.get(), 1, "the reset is written back");
    assert!(matches!(fixture.state(), WidgetState::Waiting(_)));
}

#[test]
fn a_listing_the_route_still_publishes_should_survive_starting() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();

    assert_eq!(
        fixture.widget.config.selection.as_ref(),
        Some(&selection()),
        "the stored scope survives the question"
    );
    assert_eq!(fixture.publishes.get(), 0, "nothing was written back");
}

#[test]
fn a_listing_the_route_delisted_should_never_be_polled() {
    let mut fixture = Fixture::new(config(), true, ListingSupport::Unsupported, []);
    fixture.load();
    fixture.settle();

    assert!(
        fixture.widget.pending_is_empty_for_test(),
        "a withdrawn listing is not polled"
    );
    assert!(!fixture.has_content());
}

#[test]
fn a_market_that_is_not_in_memory_should_not_be_polled() {
    let mut fixture = Fixture::new(config(), false, supported(), []);
    fixture.load();
    fixture.settle();

    assert!(fixture.widget.awaiting_market_for_test());
    assert!(fixture.widget.pending_is_empty_for_test());
    let WidgetState::Waiting(Notice::MarketNotLoaded { venues, .. }) = fixture.state() else {
        panic!("a pane without its market waits");
    };
    assert_eq!(venues, vec![perpetual_route().venue]);
}

#[test]
fn a_pane_with_a_scope_should_draw_what_arrives() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();

    assert!(fixture.has_content(), "a window is content");
    assert_eq!(fixture.state(), WidgetState::Ready);
    assert!(fixture.canvas().is_some_and(|canvas| !canvas.is_empty()));
}

#[test]
fn a_window_arriving_before_its_readiness_should_still_be_drawn() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();

    assert!(fixture.has_content());
    assert!(fixture.chart().is_some());
}

#[test]
fn readiness_for_a_newer_window_should_not_claim_the_older_one() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(2, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();

    assert_eq!(
        fixture.state(),
        WidgetState::Loading,
        "the pane is not ready for a window it does not have"
    );
}

#[test]
fn a_failed_fetch_should_offer_a_recovery_over_the_chart() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();

    fixture.report_status(failed());
    fixture.sync();

    let WidgetState::Failed(failure) = fixture.state() else {
        panic!("a failed fetch is a failure");
    };
    assert_eq!(failure.action().label_key(), "common.retry");
    assert!(fixture.has_content(), "the chart under the banner is kept");
    assert!(failure.text(fixture.translate()).contains("timed out"));
    assert!(
        fixture.widget.refresh_is_disabled(fixture.harness.app()),
        "a failed pane offers its recovery rather than a refresh"
    );
}

#[test]
fn a_message_from_another_scope_should_be_ignored() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    let held = fixture.widget.chart_revision();
    let other = StatisticsStreamKey::new(
        MetricKind::OpenInterest,
        other_listing(),
        StatisticsTimeFrame::Minute5,
    );
    let message = Arc::new(StatisticsStreamMessage::Snapshot(Arc::new(snapshot(9))));
    fixture
        .harness
        .with_cx(|cx| fixture.widget.apply_latest(other, message, cx));

    assert_eq!(
        fixture.widget.chart_revision(),
        held,
        "a window of another scope is not this pane's"
    );
}

#[test]
fn a_message_after_a_failure_should_be_ignored() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Pending]);
    fixture.load();
    fixture.settle();
    fixture.report_status(failed());
    fixture.sync();

    fixture.report_snapshot(snapshot(1));
    fixture.report_status(ready(1, 1_060_000));
    fixture.sync();

    assert!(matches!(fixture.state(), WidgetState::Failed(_)));
    assert_eq!(
        fixture.widget.chart_revision(),
        Some(0),
        "the window published after the failure was not applied"
    );
    assert!(!fixture.has_content());
}

#[test]
fn a_window_the_pane_has_overtaken_should_be_ignored() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(2, 1_060_000));
    fixture.report_snapshot(snapshot(2));
    fixture.sync();

    fixture.report_snapshot(snapshot(1));
    fixture.sync();

    assert_eq!(
        fixture.widget.chart_revision(),
        Some(2),
        "the newer window wins"
    );
}

#[test]
fn following_another_listing_should_restart_the_chart() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    assert!(fixture.has_content());

    fixture.select(other_selection());
    fixture.sync();

    assert_eq!(
        fixture.widget.config.selection.as_ref(),
        Some(&other_selection())
    );
    assert_eq!(fixture.publishes.get(), 1, "the new scope is written back");
    assert!(
        !fixture.has_content(),
        "the old window is not the new scope's"
    );
    assert_eq!(fixture.state(), WidgetState::Connecting);
}

#[test]
fn following_the_same_listing_again_should_change_nothing() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();

    assert!(
        !fixture
            .harness
            .with_cx(|cx| fixture.widget.select_selection(selection(), cx)),
        "the same scope is not a change"
    );
    assert_eq!(fixture.publishes.get(), 0);
}

#[test]
fn another_interval_should_keep_the_listing_and_restart_the_chart() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();

    fixture.set_timeframe(StatisticsTimeFrame::Hour1);
    fixture.sync();

    assert_eq!(
        fixture.widget.config.selection.as_ref(),
        Some(&selection()),
        "the listing survives another interval"
    );
    assert_eq!(
        fixture.widget.timeframe_for_test(),
        StatisticsTimeFrame::Hour1
    );
    assert_eq!(fixture.publishes.get(), 1);
}

#[test]
fn another_interval_should_be_polled() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();

    fixture.set_timeframe(StatisticsTimeFrame::Hour1);
    fixture.settle();

    assert_eq!(
        fixture.widget.current_key(),
        Some(StatisticsStreamKey::new(
            MetricKind::OpenInterest,
            listing(),
            StatisticsTimeFrame::Hour1
        )),
        "the pane polls the interval it follows"
    );
}

#[test]
fn the_countdown_should_follow_the_schedule() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    fixture.build_header();
    fixture.sync();
    fixture.harness.drain();

    assert_eq!(fixture.countdown().as_deref(), Some("01:00"));

    fixture.clock.set(1_000_000 + 1_000);
    assert!(
        fixture.harness.advance(COUNTDOWN_TICK),
        "the countdown tick draws the label it wrote"
    );
    assert_eq!(fixture.countdown().as_deref(), Some("00:59"));
}

#[test]
fn a_failed_pane_should_stop_counting_down() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.sync();
    assert!(fixture.countdown().is_some());
    assert!(
        fixture.widget.countdown_is_armed_for_test(),
        "a ready pane is waiting for its next tick"
    );

    fixture.report_status(failed());
    fixture.sync();

    assert_eq!(
        fixture.countdown(),
        None,
        "a failure has nothing to count down to"
    );
    assert!(
        !fixture.widget.countdown_is_armed_for_test(),
        "a failure cancels the tick it was waiting for"
    );
    fixture.harness.drain();
    fixture.clock.set(1_000_000 + 10_000);
    assert!(
        !fixture.harness.advance(COUNTDOWN_TICK),
        "a cancelled countdown draws nothing when its tick would have been due"
    );
    assert_eq!(
        fixture.countdown(),
        None,
        "a failed pane has no countdown to tick"
    );
}

#[test]
fn a_refresh_should_not_be_forwarded_while_a_window_is_being_fetched() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(StatisticsStatus::Refreshing {
        next_refresh_at: 1_060_000,
    });
    fixture.sync();

    assert!(fixture.widget.refresh_is_disabled(fixture.harness.app()));
    fixture.request_refresh();
    fixture.sync();

    assert!(!fixture.widget.refresh_pending_for_test());
}

#[test]
fn a_refresh_should_send_its_command_while_the_pane_is_ready() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();

    fixture.request_refresh();
    fixture.sync();

    assert!(fixture.widget.refresh_pending_for_test());
}

#[test]
fn an_answer_to_a_replaced_command_should_be_dropped() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();

    fixture.request_refresh();
    fixture.sync();
    let replaced = RequestId::new(1);
    fixture.load();
    fixture.sync();

    let result = Err(
        widget::rpc::RpcCallError::<crate::service::StatisticsServiceError>::ServiceClosed {
            request_id: replaced,
        },
    );
    fixture
        .harness
        .with_cx(|cx| fixture.widget.apply_rpc(replaced, result, cx));

    assert!(
        !matches!(fixture.state(), WidgetState::Failed(_)),
        "a late answer to a replaced command does not fail the pane"
    );
    assert_eq!(fixture.widget.dropped_answers_for_test(), 1);
}

#[test]
fn the_header_should_offer_the_scope_the_interval_and_a_refresh() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();
    fixture.build_header();

    let row = fixture.header_row.expect("the header row is built");
    assert!(
        fixture.harness.with_cx(|cx| node_under(
            cx,
            row,
            "TEST_PERPETUAL_STATISTICS_HEADER_INSTRUMENT"
        )),
        "the left group holds the instrument control"
    );
    assert!(
        fixture.harness.with_cx(|cx| node_under(
            cx,
            row,
            "TEST_PERPETUAL_STATISTICS_HEADER_DRAWING"
        )),
        "the right group holds the drawing controls"
    );
    assert!(
        fixture
            .harness
            .with_cx(|cx| node_under(cx, row, "TEST_PERPETUAL_STATISTICS_COUNTDOWN")),
        "the countdown is a bound text node"
    );
    assert!(!fixture.widget.header_actions_for_test().is_empty());
}

#[test]
fn a_pane_that_is_waiting_for_its_market_should_disable_the_drawing_controls() {
    let mut fixture = Fixture::new(config(), false, supported(), []);
    fixture.load();
    fixture.settle();
    fixture.build_header();

    assert!(
        !fixture.widget.has_scope_for_test(),
        "waiting for the market is not having a scope"
    );
    assert!(fixture.widget.refresh_is_disabled(fixture.harness.app()));
}

#[test]
fn the_waiting_notice_should_name_the_market() {
    let mut fixture = Fixture::new(config(), false, supported(), []);
    fixture.load();
    fixture.settle();

    let WidgetState::Waiting(notice) = fixture.state() else {
        panic!("a pane without its market waits");
    };
    let text = notice.resolve(fixture.translate());
    assert!(
        !text.is_empty(),
        "the market notice resolves to a sentence: {text}"
    );
}

#[test]
fn an_empty_window_should_say_there_is_no_data() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(empty_snapshot(1));
    fixture.report_status(ready(1, 1_060_000));
    fixture.sync();
    fixture.paint();

    assert!(fixture.has_content(), "an empty window is content");
    assert!(
        fixture.harness.with_cx(|cx| node_under(
            cx,
            fixture.page,
            "TEST_PERPETUAL_STATISTICS_EMPTY"
        )),
        "the pane draws its empty-window overlay"
    );
    let text = texts(&fixture, fixture.page);
    assert!(
        text.contains("No data"),
        "the overlay says there is no data: {text}"
    );
    assert_eq!(
        children_of(&fixture, "TEST_PERPETUAL_STATISTICS_CHART_HOST"),
        [
            "TEST_PERPETUAL_STATISTICS_CHART",
            "TEST_PERPETUAL_STATISTICS_EMPTY"
        ],
        "the empty-window sentence is drawn after the chart, not behind it"
    );
    assert!(fixture.canvas().is_some_and(|canvas| !canvas.is_empty()));
}

#[test]
fn a_display_zone_should_rewrite_the_time_labels() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    let before = fixture.plot();

    fixture.set_zone(8);
    fixture.sync();
    fixture.harness.drain();

    assert_eq!(fixture.plot(), before, "the plot does not move");
    assert!(fixture.canvas().is_some(), "the labels are redrawn");
}

#[test]
fn a_language_switch_should_keep_the_scope_and_redraw_the_pane() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(snapshot(1));
    fixture.report_status(ready(1, 1_060_000));
    fixture.sync();
    fixture.paint();
    fixture.build_header();

    fixture.harness.switch_language(LanguageCode::ZhCn);
    fixture.sync();
    fixture.harness.drain();

    assert_eq!(
        fixture.widget.config.selection.as_ref(),
        Some(&selection()),
        "a language switch changes no scope"
    );
    assert!(fixture.has_content());
    assert!(
        !product_translate(LanguageCode::ZhCn)
            .translate("common.no_data")
            .is_empty(),
        "the catalogue has the sentence in the language showing now"
    );
}

#[test]
fn a_directory_generation_should_rerun_load() {
    let mut fixture = Fixture::new(config(), false, supported(), []);
    fixture.load();
    fixture.settle();
    assert!(fixture.widget.awaiting_market_for_test());

    fixture.directory.load();
    fixture.bump_generation(1);
    fixture.settle();

    assert!(
        !fixture.widget.awaiting_market_for_test(),
        "the market being loaded starts the pane again"
    );
    assert!(
        matches!(
            fixture.state(),
            WidgetState::Connecting | WidgetState::Loading | WidgetState::Ready
        ),
        "the pane is reading rather than waiting: {:?}",
        fixture.state()
    );
}

#[test]
fn the_charts_name_should_be_in_the_catalogue_in_every_language() {
    for language in [LanguageCode::En, LanguageCode::ZhCn] {
        let translate = product_translate(language);
        let name = translate.translate("widget.perpetual_statistics.chart");
        assert!(
            !name.is_empty(),
            "the chart's name resolves in {language:?}"
        );
    }
}

#[test]
fn a_directory_generation_should_keep_the_window_the_pane_is_showing() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    let primitives = fixture
        .canvas()
        .expect("the pane drew a chart")
        .primitives()
        .len();
    assert!(fixture.has_content());

    fixture.directory.load();
    fixture.bump_generation(1);
    fixture.sync();
    fixture.harness.drain();

    assert!(
        fixture.has_content(),
        "a directory publication does not blank the window the user is reading (AGENTS §8.3)"
    );
    assert_eq!(
        fixture.widget.chart_revision(),
        Some(1),
        "the window on screen is not rebuilt by a publication"
    );
    assert_eq!(
        fixture.state(),
        WidgetState::Ready,
        "a ready pane stays ready across a publication"
    );
    assert_eq!(
        fixture
            .canvas()
            .expect("the chart is still drawn")
            .primitives()
            .len(),
        primitives,
        "the labels and the lines are still on the canvas"
    );

    fixture.report_snapshot(snapshot(2));
    fixture.report_status(ready(2, 1_060_000));
    fixture.sync();

    assert_eq!(
        fixture.widget.chart_revision(),
        Some(2),
        "the reloaded window keeps receiving the stream's windows"
    );
}

#[test]
fn a_market_that_was_taken_away_should_send_the_pane_back_to_waiting() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    assert!(!fixture.widget.awaiting_market_for_test());

    fixture.directory.unload();
    fixture.bump_generation(2);
    fixture.settle();

    assert!(fixture.widget.awaiting_market_for_test());
    assert!(!fixture.has_content());
}

#[test]
fn a_directory_publication_should_not_revive_a_failed_pane() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.load();
    fixture.settle();
    fixture.report_status(failed());
    fixture.sync();

    fixture.directory.load();
    fixture.bump_generation(3);
    fixture.sync();

    assert!(
        matches!(fixture.state(), WidgetState::Failed(_)),
        "only the user's own recovery leaves a failure"
    );
}

#[test]
fn the_pane_should_be_built_and_released_without_leaking() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    fixture.build_header();
    fixture.sync();
    let live = fixture.harness.signal_slot_stats().live;
    let nodes = fixture.harness.node_count();
    assert!(
        fixture.widget.countdown_is_armed_for_test(),
        "the countdown the release has to cancel is waiting for a tick"
    );

    fixture.harness.with_cx(|cx| fixture.widget.release(cx));

    assert!(
        fixture.harness.node_count() < nodes,
        "release takes the nodes this widget owned down"
    );
    assert!(
        fixture.harness.signal_slot_stats().live < live,
        "release hands its signal scopes back"
    );
    assert_eq!(
        fixture.harness.app().geometry().len(),
        0,
        "the chart's meshes go with it"
    );
    assert_eq!(fixture.countdown(), None, "the countdown is cancelled");
    assert!(
        !fixture.widget.countdown_is_armed_for_test(),
        "release takes the pending tick with it"
    );
}

#[test]
fn two_panes_should_keep_their_own_header_handlers() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let mut panes: Vec<(Widget, ui_core::node::signal::SignalScope)> = Vec::new();
    for _ in 0..2 {
        let directory = FixtureDirectory::new(true, supported());
        let status = harness.status();
        let recovery = WidgetAction::new("common.retry", |_| {});
        let change = WidgetConfigChange::new(|_| {});
        let handle: Rc<dyn StatisticsDirectory> = Rc::new(directory);
        let services = Rc::clone(harness.services_rc());
        let scope = harness.app_mut().signal_scope();
        let window = harness.window();
        let app = harness.app_mut();
        let mut cx = widget::cx::WidgetCx::new(app, window, &services, scope);
        let widget = Widget::with_source(
            config(),
            FakeSource::new([FetchPlan::Success(1)]),
            Some(handle),
            PaneScaffold {
                status,
                config_change: change,
                recovery,
                now: Rc::new(|| 1_000_000),
            },
            &mut cx,
        );
        panes.push((widget, scope));
    }
    let row = harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_TWO_PANES_HEADER")
            .style(
                base_ui::style_builder::StyleBuilder::new()
                    .width(640.0)
                    .height(64.0)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the header host builds")
    });
    for (widget, scope) in panes.iter_mut() {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let app = harness.app_mut();
        let mut cx = widget::cx::WidgetCx::new(app, window, &services, *scope);
        widget
            .header(HeaderGroup::Right, row, &mut cx)
            .expect("the right group builds");
    }
    harness.drain();

    let actions: Vec<_> = panes
        .iter()
        .flat_map(|(pane, _)| pane.header_actions_for_test())
        .collect();
    assert_eq!(actions.len(), 2, "each pane registered its own refresh");
    assert_ne!(actions[0], actions[1], "two panes never share an action");
    assert!(harness.app().has_action_handler(&actions[0]));
    assert!(harness.app().has_action_handler(&actions[1]));

    let (first, scope) = &mut panes[0];
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let app = harness.app_mut();
    let mut cx = widget::cx::WidgetCx::new(app, window, &services, *scope);
    first.release(&mut cx);
    assert!(
        harness.app().has_action_handler(&actions[1]),
        "the second pane's handler survives the first one's release"
    );
}

#[test]
fn only_a_route_that_declares_the_series_should_be_readable() {
    let mut fixture = Fixture::new(config(), true, supported(), []);
    fixture.directory.without_capability();
    fixture.load();
    fixture.sync();

    assert!(
        fixture.chart().is_none(),
        "a route that does not declare this series is not one the pane draws"
    );
}

#[test]
fn the_chart_units_should_come_from_the_directory() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();

    let (left, right) = fixture
        .chart()
        .expect("the pane drew a chart")
        .axis_titles_for_test();
    assert!(
        left.contains("BTC") && right.contains("USDT"),
        "the axes name the units the directory states: {left} / {right}"
    );
}

#[test]
fn a_recovery_should_keep_the_window_the_user_is_reading() {
    let mut fixture = Fixture::new(config(), true, supported(), [FetchPlan::Success(1)]);
    fixture.load();
    fixture.settle();
    fixture.report_status(ready(1, 1_060_000));
    fixture.report_snapshot(snapshot(1));
    fixture.sync();
    fixture.paint();
    assert!(fixture.has_content());
    fixture.report_status(failed());
    fixture.sync();

    fixture.load();
    fixture.sync();

    assert!(
        fixture.has_content(),
        "a recovery keeps the window the user was reading"
    );
    assert_eq!(fixture.widget.chart_revision(), Some(1));
}

#[test]
fn an_old_payload_should_decode_to_the_defaults() {
    let stored = r#"{"schema_version":1,"payload":"{\"selection\":{\"route\":{\"venue\":\"binance\",\"market\":\"perpetual\"},\"instrument\":{\"kind\":\"perpetual\"}},\"timeframe\":\"1h\"}"}"#;
    let envelope: app_config::widget::VersionedWidgetConfig =
        serde_json::from_str(stored).expect("the stored envelope should parse");

    let decoded = envelope.decode::<StatisticsConfig>();

    assert_eq!(decoded, StatisticsConfig::default());
    assert_eq!(decoded.selection, None);
}

#[test]
fn every_widget_of_this_crate_should_state_its_own_identity_and_series() {
    let translate = product_translate(LanguageCode::En);
    let ids = [
        assert_widget::<OpenInterestWidgetSeries>(
            &translate,
            "kytrix.open_interest",
            "widget.names.open_interest",
            "widget.descriptions.open_interest",
            "widgets/open_interest.png",
            MetricKind::OpenInterest,
        ),
        assert_widget::<TakerVolumeWidgetSeries>(
            &translate,
            "kytrix.taker_volume",
            "widget.names.taker_volume",
            "widget.descriptions.taker_volume",
            "widgets/taker_volume.png",
            MetricKind::TakerVolume,
        ),
        assert_widget::<TopTraderAccountRatioWidgetSeries>(
            &translate,
            "kytrix.top_trader_account_ratio",
            "widget.names.top_trader_account_ratio",
            "widget.descriptions.top_trader_account_ratio",
            "widgets/top_trader_account_ratio.png",
            MetricKind::TopTraderAccountRatio,
        ),
        assert_widget::<TopTraderPositionRatioWidgetSeries>(
            &translate,
            "kytrix.top_trader_position_ratio",
            "widget.names.top_trader_position_ratio",
            "widget.descriptions.top_trader_position_ratio",
            "widgets/top_trader_position_ratio.png",
            MetricKind::TopTraderPositionRatio,
        ),
        assert_widget::<LongShortAccountRatioWidgetSeries>(
            &translate,
            "kytrix.long_short_account_ratio",
            "widget.names.long_short_account_ratio",
            "widget.descriptions.long_short_account_ratio",
            "widgets/long_short_account_ratio.png",
            MetricKind::LongShortAccountRatio,
        ),
    ];

    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 5, "five widgets have five identities");
    for id in unique {
        assert!(
            id.starts_with("kytrix."),
            "`{id}` is one of this application's widget ids"
        );
    }
}

#[test]
fn a_pane_without_a_directory_should_report_that_it_cannot_read() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let widget = harness
        .try_mount::<Widget>(config())
        .expect("the widget mounts");
    harness.drain();

    let state = widget.status().state(harness.app());
    let WidgetState::Failed(failure) = state else {
        panic!("a host with no directory is a failure, got {state:?}");
    };
    assert_eq!(failure.action().label_key(), "common.retry");
    assert!(!failure.text(&harness.services().translate).is_empty());
    widget.release(harness.app_mut());
}

fn assert_widget<M: StatisticsMetric>(
    translate: &Translate,
    id: &str,
    name: &str,
    description: &str,
    icon: &str,
    metric: MetricKind,
) -> String {
    assert_eq!(M::ID.as_str(), id, "the persisted identity");
    assert_eq!(M::DISPLAY_NAME, name, "the name key");
    assert_eq!(M::DESCRIPTION, Some(description), "the description key");
    assert_eq!(M::ICON, icon, "the gallery picture");
    assert_eq!(M::METRIC, metric, "the series this widget plots");
    assert_eq!(
        M::METRIC.capability(),
        metric.capability(),
        "a series is served under its own capability"
    );
    for key in [name, description] {
        assert!(
            !translate.translate(key).is_empty(),
            "`{key}` is in the catalogue"
        );
    }
    assert_polls::<M>(metric);
    M::ID.as_str().to_owned()
}

fn assert_polls<M: StatisticsMetric>(metric: MetricKind) {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let directory = FixtureDirectory::new(true, supported()).offering(metric.capability());
    let status = harness.status();
    let recovery = WidgetAction::new("common.retry", |_| {});
    let change = WidgetConfigChange::new(|_| {});
    let handle: Rc<dyn StatisticsDirectory> = Rc::new(directory);
    let mut pane = harness.with_cx(|cx| {
        StatisticsWidget::<M>::with_source(
            config(),
            FakeSource::new([FetchPlan::Success(1)]),
            Some(handle),
            PaneScaffold {
                status,
                config_change: change,
                recovery,
                now: Rc::new(|| 1_000_000),
            },
            cx,
        )
    });
    harness.with_cx(|cx| pane.load(cx));
    for _ in 0..6 {
        harness.drain_runtime();
    }
    harness.with_cx(|cx| pane.sync(cx)).expect("the pane syncs");

    assert_eq!(
        pane.current_key().map(|key| key.metric),
        Some(metric),
        "a pane polls the series its widget names"
    );
    let chart = pane
        .chart_for_test()
        .expect("a supported route builds its chart");
    let drawn = match metric {
        MetricKind::OpenInterest => matches!(chart, ChartDispatch::OpenInterest(_)),
        MetricKind::TakerVolume => matches!(chart, ChartDispatch::TakerVolume(_)),
        MetricKind::TopTraderAccountRatio
        | MetricKind::TopTraderPositionRatio
        | MetricKind::LongShortAccountRatio => matches!(chart, ChartDispatch::Ratio(_)),
    };
    assert!(drawn, "the chart is the one this series draws");
}

fn children_of(fixture: &Fixture, parent: &str) -> Vec<String> {
    let tree = fixture.harness.tree();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        if tree.debug_id(node) == Some(parent) {
            return target
                .children()
                .iter()
                .map(|child| tree.debug_id(*child).unwrap_or_default().to_owned())
                .collect();
        }
        stack.extend(target.children().iter().copied());
    }
    panic!("no node with the debug id `{parent}`");
}

fn node_under(cx: &mut widget::cx::WidgetCx<'_>, parent: NodeId, id: &str) -> bool {
    let mut stack = vec![parent];
    while let Some(node) = stack.pop() {
        let Ok(target) = cx.tree().get(node) else {
            continue;
        };
        if cx.tree().debug_id(node) == Some(id) {
            return true;
        }
        stack.extend(target.children().iter().copied());
    }
    false
}

fn texts(fixture: &Fixture, parent: NodeId) -> String {
    let tree = fixture.harness.tree();
    let mut stack = vec![parent];
    let mut text = String::new();
    while let Some(node) = stack.pop() {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        if let NodeKind::Text(content) = target.kind() {
            text.push_str(content.text.as_str());
            text.push(' ');
            continue;
        }
        stack.extend(target.children().iter().copied());
    }
    text
}
