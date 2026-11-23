use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
};

use app_config::{
    settings::{AccountConfig, AccountWalletKind, Config},
    time_zone::DataTimeZone,
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::service::{
    engine::{ErrorCode, KytrixServiceError},
    global_shipping_chokepoint::{
        GlobalShippingChokepointArea, GlobalShippingChokepointPosition,
        GlobalShippingChokepointShip, GlobalShippingChokepointUpdate,
    },
};
use network::transport::error::TransportError;
use smol_str::SmolStr;
use ui_core::{
    event::{Action, Event, InputEvent, PointerButton, ScrollPhase, ScrollUnit},
    math::Point,
    node::builder::NodeBuilder,
    test_support::record::PrimitiveKind,
};
use ui_kit::{style_builder::StyleBuilder, theme::appearance::Appearance};
use widget::{
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    test_support::{CanvasPrimitive, WidgetTestApp},
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::{
    map::{ATTRIBUTION_URL, MAP_SELECTOR},
    model::{Projection, STALE_AFTER, Vessel},
    service::{
        AisStreamError, AisStreamMessage, GlobalShippingChokepointEventSubscription,
        GlobalShippingChokepointResponse, GlobalShippingChokepointSession,
        GlobalShippingChokepointSource,
    },
    ship::SHIP_TYPES,
};

const ACCOUNT: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_ACCOUNT: &str = "22222222-2222-4222-8222-222222222222";
const PAGE: (f32, f32) = (640.0, 400.0);

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn harness() -> WidgetTestApp {
    WidgetTestApp::with_translate(product_translate(LanguageCode::En))
}

fn current_config(harness: &WidgetTestApp) -> Config {
    let signal = harness.services().config;
    (*harness.app().read(&signal)).clone()
}

fn write_config(harness: &mut WidgetTestApp, config: Config) -> bool {
    let signal = harness.services().config;
    harness.app_mut().write(&signal, config)
}

fn sign_in(harness: &mut WidgetTestApp, user_id: &str) {
    let mut config = current_config(harness);
    config.account = Some(AccountConfig {
        user_id: user_id.to_owned(),
        wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
        wallet_kind: AccountWalletKind::Ethereum,
    });
    assert!(
        write_config(harness, config),
        "signing in is a settings change"
    );
}

fn sign_out(harness: &mut WidgetTestApp) {
    let mut config = current_config(harness);
    config.account = None;
    assert!(
        write_config(harness, config),
        "signing out is a settings change"
    );
}

fn set_data_time_zone(harness: &mut WidgetTestApp, hours: i8) {
    let mut config = current_config(harness);
    config.data_time_zone = DataTimeZone::from_hours(hours).expect("an offered offset");
    assert!(
        write_config(harness, config),
        "moving the zone is a settings change"
    );
}

#[derive(Clone, Default)]
struct RecordingSource {
    accounts: Arc<Mutex<Vec<SmolStr>>>,
}

impl RecordingSource {
    fn requested(&self) -> Vec<SmolStr> {
        self.accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl GlobalShippingChokepointSource for RecordingSource {
    type Session = PendingSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SmolStr::new(account_id));
        std::future::pending().await
    }
}

#[derive(Clone)]
struct PendingSession;

impl GlobalShippingChokepointSession for PendingSession {
    type Subscription = PendingSubscription;

    async fn subscribe_global_shipping_chokepoint(
        &self,
        _area: GlobalShippingChokepointArea,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        std::future::pending().await
    }
}

struct PendingSubscription;

impl GlobalShippingChokepointEventSubscription for PendingSubscription {
    async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
        std::future::pending().await
    }
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

struct Pane {
    widget: GlobalShippingChokepointWidget,
    status: WidgetStatus,
    scope: ui_core::node::signal::SignalScope,
    page: NodeId,
    header_group: NodeId,
}

fn page(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_CHOKEPOINT_PAGE")
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

fn pane_with_source(harness: &mut WidgetTestApp, source: RecordingSource) -> Pane {
    let scope = harness.app_mut().signal_scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_group = harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_row().build())
            .build(cx.tree_mut(), page)
            .expect("the header group builds")
    });
    let window = harness.window();
    let services = Rc::clone(harness.services_rc());
    let widget = {
        let app = harness.app_mut();
        let mut cx = WidgetCx::new(app, window, &services, scope);
        GlobalShippingChokepointWidget::with_source(
            GlobalShippingChokepointConfig,
            source,
            status,
            recovery(),
            &mut cx,
        )
    };
    Pane {
        widget,
        status,
        scope,
        page,
        header_group,
    }
}

fn pane(harness: &mut WidgetTestApp) -> Pane {
    sign_in(harness, ACCOUNT);
    let mut pane = pane_with_source(harness, RecordingSource::default());
    load(harness, &mut pane);
    pane
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

fn with_scope<R>(
    harness: &mut WidgetTestApp,
    scope: ui_core::node::signal::SignalScope,
    body: impl FnOnce(&mut WidgetCx<'_>) -> R,
) -> R {
    let window = harness.window();
    let services = Rc::clone(harness.services_rc());
    let app = harness.app_mut();
    let mut cx = WidgetCx::new(app, window, &services, scope);
    body(&mut cx)
}

fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.header_group;
    let scope = pane.scope;
    let widget = &mut pane.widget;
    with_scope(harness, scope, |cx| {
        widget.header(HeaderGroup::Left, parent, cx)
    })
    .expect("the header builds");
}

fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    sync(harness, pane);
    harness.drain();
}

fn report_update(pane: &mut Pane, update: GlobalShippingChokepointUpdate, cx: &mut WidgetCx<'_>) {
    pane.widget.apply_latest(
        GlobalShippingChokepointArea::Hormuz,
        Arc::new(AisStreamMessage::Update(update)),
        cx,
    );
}

fn push(harness: &mut WidgetTestApp, pane: &mut Pane, update: GlobalShippingChokepointUpdate) {
    harness.with_cx(|cx| report_update(pane, update, cx));
}

fn fail_with(harness: &mut WidgetTestApp, pane: &mut Pane, error: AisStreamError) {
    harness.with_cx(|cx| {
        pane.widget.apply_reliable(
            GlobalShippingChokepointArea::Hormuz,
            Arc::new(AisStreamMessage::Failed(Arc::new(error))),
            cx,
        );
    });
}

fn position(mmsi: u32) -> GlobalShippingChokepointPosition {
    GlobalShippingChokepointPosition {
        mmsi,
        latitude: 26_500_000,
        longitude: 56_300_000 + (mmsi.saturating_sub(1) as i32) * 2_000_000,
        sog: 123,
        cog: 456,
        heading: 87,
        nav_status: 1,
        last_update: 1_700_000_000_000,
    }
}

fn ships(mmsi: u32) -> GlobalShippingChokepointShip {
    GlobalShippingChokepointShip {
        mmsi,
        imo: 9_074_729,
        call_sign: SmolStr::new_static("A8AB2"),
        name: SmolStr::new_static("SEA JEWEL"),
        destination: SmolStr::new_static("FUJAIRAH"),
        ship_type: 80,
        flag_mid: 636,
        draught: 92,
        dim_to_bow: 120,
        dim_to_stern: 30,
        dim_to_port: 15,
        dim_to_starboard: 17,
        eta_month: 3,
        eta_day: 14,
        eta_hour: 9,
        eta_minute: 5,
        static_updated_at: 1_700_000_000_000,
    }
}

fn ship_of_type(mmsi: u32, ship_type: u32) -> GlobalShippingChokepointShip {
    let mut ship = ships(mmsi);
    ship.ship_type = ship_type;
    ship
}

fn code_of(ship_type: ShipType) -> u32 {
    match ship_type {
        ShipType::Tanker => 80,
        ShipType::Cargo => 70,
        ShipType::Passenger => 60,
        ShipType::Fishing => 30,
        ShipType::Towing => 31,
        ShipType::Dredging => 33,
        ShipType::Military => 35,
        ShipType::Sailing => 36,
        ShipType::Pleasure => 37,
        ShipType::HighSpeed => 40,
        ShipType::Pilot => 50,
        ShipType::SearchAndRescue => 51,
        ShipType::PortTender => 53,
        ShipType::AntiPollution => 54,
        ShipType::LawEnforcement => 55,
        ShipType::Medical => 58,
        ShipType::Noncombatant => 59,
        ShipType::WingInGround => 20,
        ShipType::Other => 90,
        ShipType::Unknown => 0,
    }
}

fn keyframe(
    epoch: u64,
    sequence: u64,
    positions: Vec<GlobalShippingChokepointPosition>,
) -> GlobalShippingChokepointUpdate {
    let ships = positions
        .iter()
        .map(|position| ships(position.mmsi))
        .collect();
    GlobalShippingChokepointUpdate {
        epoch,
        keyframe: true,
        sequence,
        base: 0,
        ships,
        positions,
        removed: Vec::new(),
        timestamp: 1_700_000_000_000,
    }
}

fn keyframe_of_type(
    epoch: u64,
    sequence: u64,
    positions: Vec<GlobalShippingChokepointPosition>,
    ship_type: u32,
) -> GlobalShippingChokepointUpdate {
    let mut update = keyframe(epoch, sequence, positions);
    update.ships = update
        .ships
        .iter()
        .map(|ship| ship_of_type(ship.mmsi, ship_type))
        .collect();
    update
}

fn timeless_keyframe(
    epoch: u64,
    sequence: u64,
    positions: Vec<GlobalShippingChokepointPosition>,
) -> GlobalShippingChokepointUpdate {
    let mut update = keyframe(epoch, sequence, positions);
    update.timestamp = 0;
    update
}

fn removal(epoch: u64, base: u64, sequence: u64, mmsi: u32) -> GlobalShippingChokepointUpdate {
    GlobalShippingChokepointUpdate {
        epoch,
        keyframe: false,
        sequence,
        base,
        ships: Vec::new(),
        positions: Vec::new(),
        removed: vec![mmsi],
        timestamp: 1_700_000_000_000,
    }
}

fn map_node(harness: &WidgetTestApp, pane: &Pane) -> NodeId {
    harness
        .find_by_debug_prefix(pane.page, MAP_SELECTOR)
        .expect("the body built the map")
}

fn primitives(harness: &WidgetTestApp, pane: &Pane) -> Vec<CanvasPrimitive> {
    harness.canvas_primitives(map_node(harness, pane))
}

fn glyph_count(harness: &WidgetTestApp, pane: &Pane) -> usize {
    harness
        .canvas_primitives(map_node(harness, pane))
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
        .map(|primitive| primitive.glyphs)
        .sum()
}

fn geometry_draws(harness: &WidgetTestApp, pane: &Pane) -> usize {
    harness
        .canvas_primitives(map_node(harness, pane))
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::Geometry)
        .count()
}

fn canvas_handle(harness: &WidgetTestApp, pane: &Pane) -> Arc<ui_core::paint::canvas::Canvas> {
    harness
        .canvas_handle(map_node(harness, pane))
        .expect("the map is a canvas node")
}

fn paint_one_vessel(harness: &mut WidgetTestApp, pane: &mut Pane) {
    push(harness, pane, keyframe(7, 1, vec![position(1)]));
    build_body(harness, pane);
    frame(harness, pane);
}

fn hull_of(pane: &Pane, mmsi: u32) -> Point {
    let bounds = pane.widget.map.bounds().expect("the map has a box");
    let projection = Projection::new(
        pane.widget.map.camera(),
        [bounds.size.width, bounds.size.height],
    );
    let vessel: &Vessel = pane
        .widget
        .painted
        .iter()
        .find(|vessel| vessel.mmsi == mmsi)
        .expect("the fixture painted this vessel");
    let at = projection.screen(vessel.position);
    Point::new(bounds.origin.x + at[0], bounds.origin.y + at[1])
}

fn pointer_move(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point) -> bool {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerMove {
                position,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    })
}

fn pointer_leave(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point) {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerLeave {
                position,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        );
    });
}

fn pointer_down(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point) -> bool {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerDown {
                position,
                button: PointerButton::Primary,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    })
}

fn pointer_up(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point) -> bool {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerUp {
                position,
                button: PointerButton::Primary,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    })
}

fn pointer_cancel(harness: &mut WidgetTestApp, pane: &mut Pane) -> bool {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerCancel {
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    })
}

fn wheel(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point, step: f32) -> bool {
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::Scroll {
                position,
                delta: Point::new(0.0, step),
                unit: ScrollUnit::Line,
                phase: ScrollPhase::Discrete,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    })
}

#[test]
fn the_config_should_be_at_the_first_schema_version() {
    assert_eq!(
        GlobalShippingChokepointConfig::SCHEMA_VERSION,
        app_config::widget::WidgetConfigSchemaVersion::V1
    );
}

#[test]
fn a_pane_nobody_signed_in_for_should_wait_for_a_sign_in() {
    let mut harness = harness();
    let mut pane = pane_with_source(&mut harness, RecordingSource::default());

    load(&mut harness, &mut pane);

    assert!(!pane.widget.has_content());
    match pane.status.state(harness.app()) {
        WidgetState::Waiting(Notice::Message(key)) => {
            assert_eq!(key, "widget.global_shipping_chokepoint.sign_in");
        }
        other => panic!("a signed-out pane waits for a sign-in, got {other:?}"),
    }
}

#[test]
fn a_host_without_a_kytrix_service_should_fail_the_pane_with_the_retry() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let mounted = harness
        .try_mount::<GlobalShippingChokepointWidget>(GlobalShippingChokepointConfig)
        .expect("the pane mounts without a service");

    match mounted.status().state(harness.app()) {
        WidgetState::Failed(failure) => {
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "what is missing is the connection, so the reader is offered the retry"
            );
            assert_eq!(
                failure.text(&product_translate(LanguageCode::En)),
                "The Kytrix service connector is closed",
                "the failure names the connection it could not use"
            );
        }
        other => panic!("a pane with no Kytrix connection fails, got {other:?}"),
    }
}

#[test]
fn a_signed_in_pane_should_connect_until_a_keyframe_arrives() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);

    assert!(!pane.widget.has_content(), "no picture has arrived yet");
    assert_eq!(pane.status.state(harness.app()), WidgetState::Connecting);

    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));

    assert!(pane.widget.has_content(), "one keyframe is one picture");
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
}

#[test]
fn a_keyframe_without_a_time_should_not_become_content() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);

    push(
        &mut harness,
        &mut pane,
        timeless_keyframe(7, 1, vec![position(1)]),
    );

    assert!(
        !pane.widget.has_content(),
        "a keyframe with no time holds no state"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Connecting,
        "the pane keeps waiting rather than drawing an empty sea"
    );
}

#[test]
fn an_empty_keyframe_should_be_ready_with_the_map_alone() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);

    push(&mut harness, &mut pane, keyframe(7, 1, Vec::new()));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    assert!(pane.widget.has_content(), "an empty sea is a picture");
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    assert!(
        geometry_draws(&harness, &pane) > 0,
        "the map is drawn even when it holds no vessel"
    );
}

#[test]
fn a_gap_should_fail_the_pane_while_the_picture_stays() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));

    push(
        &mut harness,
        &mut pane,
        GlobalShippingChokepointUpdate {
            epoch: 7,
            keyframe: false,
            sequence: 5,
            base: 4,
            ships: Vec::new(),
            positions: Vec::new(),
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        },
    );

    assert!(pane.status.is_failed(harness.app()), "a gap is a failure");
    assert!(
        pane.widget.has_content(),
        "the vessels stay on the map under the banner"
    );
}

#[test]
fn a_stale_pane_should_ignore_pushes_until_the_user_retries() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    fail_with(
        &mut harness,
        &mut pane,
        AisStreamError::Transport(TransportError::Closed),
    );
    assert!(pane.status.is_failed(harness.app()));

    push(&mut harness, &mut pane, keyframe(8, 1, vec![position(2)]));

    assert!(
        pane.status.is_failed(harness.app()),
        "a failure is terminal until the user retries"
    );
    assert_eq!(pane.widget.painted.len(), 1, "the late push did not apply");
}

#[test]
fn a_silent_link_should_fail_the_pane_after_the_staleness_window() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert!(!pane.status.is_failed(harness.app()));

    assert!(!harness.advance(STALE_AFTER / 2));
    assert!(!pane.status.is_failed(harness.app()));

    harness.advance(STALE_AFTER);
    assert!(
        pane.status.is_failed(harness.app()),
        "a link that delivers nothing is a failure"
    );
}

#[test]
fn a_new_push_should_cancel_the_staleness_clock() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    harness.advance(STALE_AFTER / 2);
    push(&mut harness, &mut pane, keyframe(7, 2, vec![position(1)]));
    frame(&mut harness, &mut pane);
    harness.advance(STALE_AFTER / 2);

    assert!(
        !pane.status.is_failed(harness.app()),
        "the push that arrived is the picture being continued"
    );
}

#[test]
fn a_failed_stream_should_offer_the_framework_retry() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);

    fail_with(
        &mut harness,
        &mut pane,
        AisStreamError::Transport(TransportError::Closed),
    );

    match pane.status.state(harness.app()) {
        WidgetState::Failed(failure) => assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "a connection failure offers the retry"
        ),
        other => panic!("the stream failed, got {other:?}"),
    }
}

#[test]
fn a_missing_entitlement_should_offer_the_plans() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);

    fail_with(
        &mut harness,
        &mut pane,
        AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired)),
    );

    match pane.status.state(harness.app()) {
        WidgetState::Failed(failure) => assert_eq!(
            failure.action().label_key(),
            "widget.global_shipping_chokepoint.view_plans",
            "a missing subscription is not fixed by retrying"
        ),
        other => panic!("the stream failed, got {other:?}"),
    }
}

#[test]
fn a_message_after_signing_out_should_be_refused() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    assert!(pane.widget.has_content());

    sign_out(&mut harness);
    sync(&mut harness, &mut pane);
    push(&mut harness, &mut pane, keyframe(8, 1, vec![position(2)]));

    assert!(!pane.widget.has_content(), "the picture stays cleared");
    assert!(pane.widget.painted.is_empty());
}

#[test]
fn an_unsigned_pane_should_ignore_a_push() {
    let mut harness = harness();
    let mut pane = pane_with_source(&mut harness, RecordingSource::default());
    load(&mut harness, &mut pane);

    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));

    assert!(!pane.widget.has_content(), "nobody is signed in");
    assert!(pane.widget.painted.is_empty());
}

#[test]
fn signing_out_should_clear_the_picture_and_wait_for_a_sign_in() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    assert!(pane.widget.has_content());

    sign_out(&mut harness);
    sync(&mut harness, &mut pane);

    assert!(
        !pane.widget.has_content(),
        "the picture went with the account"
    );
    match pane.status.state(harness.app()) {
        WidgetState::Waiting(Notice::Message(key)) => {
            assert_eq!(key, "widget.global_shipping_chokepoint.sign_in");
        }
        other => panic!("a signed-out pane waits for a sign-in, got {other:?}"),
    }
}

#[test]
fn an_account_change_should_load_the_new_account() {
    let mut harness = harness();
    let source = RecordingSource::default();
    sign_in(&mut harness, ACCOUNT);
    let mut pane = pane_with_source(&mut harness, source.clone());
    load(&mut harness, &mut pane);
    harness.run_until_idle();
    assert_eq!(
        pane.widget.account_id.as_deref(),
        Some(ACCOUNT),
        "the pane follows the account the settings hold"
    );

    sign_in(&mut harness, OTHER_ACCOUNT);
    sync(&mut harness, &mut pane);

    assert_eq!(pane.widget.account_id.as_deref(), Some(OTHER_ACCOUNT));
    assert!(pane.widget.binding.is_some(), "the pane keeps its bridge");
    assert!(
        !pane.widget.status.is_failed(harness.app()),
        "another account is a reload, not a failure"
    );
    assert_eq!(
        pane.widget.status.state(harness.app()),
        WidgetState::Connecting,
        "the pane starts the new account's stream"
    );
    harness.run_until_idle();
    assert_eq!(
        source.requested(),
        [
            SmolStr::new_static(ACCOUNT),
            SmolStr::new_static(OTHER_ACCOUNT)
        ],
        "each account's stream was started for its own account"
    );
}

#[test]
fn a_replaced_command_should_drop_its_late_answer() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);

    harness.with_cx(|cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(GlobalShippingChokepointResponse::StreamStarted),
            cx,
        );
    });

    assert_eq!(
        pane.widget.pending.dropped(),
        1,
        "the answer to a replaced command is dropped and counted"
    );
    assert!(!pane.status.is_failed(harness.app()));
}

#[test]
fn the_map_should_be_a_canvas_over_the_sea_and_the_land() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);

    let theme = harness.services().theme.theme_for(Appearance::Light);
    let primitives = primitives(&harness, &pane);
    let first_quad = primitives
        .iter()
        .find(|primitive| primitive.kind == PrimitiveKind::Quad)
        .expect("the sea is the canvas' first quad");
    assert_eq!(
        first_quad.color,
        Some(theme.role(ui_kit::theme::role::Role::ChartMapOcean)),
        "the sea is painted from the map ocean role"
    );
    assert!(
        geometry_draws(&harness, &pane) >= 2,
        "the land, the base layers and the vessels are meshes"
    );
    assert!(
        glyph_count(&harness, &pane) > 0,
        "the place names, the legend and the count are shaped text"
    );
}

#[test]
fn a_steady_frame_should_not_repaint_the_map() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);

    let before = canvas_handle(&harness, &pane);
    let requests = harness.frame_requests();
    sync(&mut harness, &mut pane);
    harness.drain();

    assert!(!harness.rendered(), "a steady frame draws nothing");
    assert_eq!(harness.frame_requests(), requests, "and asks for nothing");
    assert!(
        Arc::ptr_eq(&before, &canvas_handle(&harness, &pane)),
        "a steady sync keeps the canvas it drew"
    );
}

#[test]
fn switching_the_appearance_should_repaint_the_map() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let light = canvas_handle(&harness, &pane);

    let theme = harness.services().theme.clone();
    theme.set_appearance(harness.app_mut(), Appearance::Dark);
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&light, &canvas_handle(&harness, &pane)),
        "a canvas bakes its colours in, so another appearance is another canvas"
    );
    let dark = theme.theme_for(Appearance::Dark);
    let primitives = primitives(&harness, &pane);
    let ocean = primitives
        .iter()
        .find(|primitive| primitive.kind == PrimitiveKind::Quad)
        .and_then(|primitive| primitive.color);
    assert_eq!(
        ocean,
        Some(dark.role(ui_kit::theme::role::Role::ChartMapOcean))
    );
}

#[test]
fn switching_the_language_should_repaint_the_labels() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let english = canvas_handle(&harness, &pane);
    let english_glyphs = glyph_count(&harness, &pane);

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&english, &canvas_handle(&harness, &pane)),
        "the words on the map are chosen in the language showing now"
    );
    assert!(
        glyph_count(&harness, &pane) > 0,
        "the labels still resolve in the other language ({} glyphs before)",
        english_glyphs
    );
}

#[test]
fn a_lease_that_moved_should_repaint_the_labels() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let before = canvas_handle(&harness, &pane);

    let stack = harness.app().font_stack().clone();
    harness.app_mut().set_font_stack(stack);
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&before, &canvas_handle(&harness, &pane)),
        "a dead atlas lease is a rebuilt canvas, not a steady one"
    );
}

#[test]
fn a_moved_lease_should_settle_back_into_a_steady_frame() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);
    let at = hull_of(&pane, 1);
    pointer_move(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);
    pointer_leave(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);

    let stack = harness.app().font_stack().clone();
    harness.app_mut().set_font_stack(stack);
    frame(&mut harness, &mut pane);

    let settled = canvas_handle(&harness, &pane);
    let requests = harness.frame_requests();
    sync(&mut harness, &mut pane);
    harness.drain();

    assert!(
        !harness.rendered(),
        "the frame after the rebuild draws nothing"
    );
    assert_eq!(harness.frame_requests(), requests, "and asks for nothing");
    assert!(
        Arc::ptr_eq(&settled, &canvas_handle(&harness, &pane)),
        "the canvas of the rebuilt frame is the one kept"
    );
}

#[test]
fn the_legend_should_hold_only_the_types_the_picture_holds() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    assert_eq!(pane.widget.present_types, vec![ShipType::Tanker]);
    let one_type = glyph_count(&harness, &pane);

    let mut both = keyframe(7, 2, vec![position(1), position(2)]);
    both.ships = vec![ships(1), ship_of_type(2, code_of(ShipType::Cargo))];
    push(&mut harness, &mut pane, both);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.present_types,
        vec![ShipType::Tanker, ShipType::Cargo]
    );
    assert!(
        glyph_count(&harness, &pane) > one_type,
        "the legend gained an entry"
    );
}

#[test]
fn a_filtered_out_vessel_should_not_be_drawn() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    let mut both = keyframe(7, 1, vec![position(1), position(2)]);
    both.ships = vec![ships(1), ship_of_type(2, code_of(ShipType::Cargo))];
    push(&mut harness, &mut pane, both);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(pane.widget.painted.len(), 2);

    harness.with_cx(|cx| {
        pane.widget
            .select_filter(ShipTypeFilter::Only(ShipType::Tanker), cx)
    });
    frame(&mut harness, &mut pane);

    assert_eq!(pane.widget.painted.len(), 1, "only the tanker is drawn");
    assert_eq!(
        pane.widget.present_types,
        vec![ShipType::Tanker, ShipType::Cargo],
        "the legend still says the picture holds both"
    );
}

#[test]
fn the_selected_type_should_survive_its_disappearance() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    harness.with_cx(|cx| {
        pane.widget
            .select_filter(ShipTypeFilter::Only(ShipType::Tanker), cx)
    });

    push(
        &mut harness,
        &mut pane,
        keyframe_of_type(7, 2, vec![position(2)], code_of(ShipType::Cargo)),
    );

    assert_eq!(
        pane.widget.filter,
        ShipTypeFilter::Only(ShipType::Tanker),
        "the filter is the reader's decision, not the picture's"
    );
    assert!(
        pane.widget.painted.is_empty(),
        "nothing of that type is drawn"
    );
    build_header(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let items = pane
        .widget
        .filter_control
        .as_ref()
        .expect("the header control was built")
        .select
        .items();
    assert!(
        items.iter().any(|item| item.id == "tanker"),
        "the choice is still stated, even though the type has left"
    );
}

#[test]
fn the_pointer_over_a_vessel_should_show_its_card() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let no_card = glyph_count(&harness, &pane);
    let at = hull_of(&pane, 1);

    assert!(pointer_move(&mut harness, &mut pane, at));
    frame(&mut harness, &mut pane);

    assert_eq!(pane.widget.map.hovered(), Some(1), "the pointer found it");
    assert!(
        glyph_count(&harness, &pane) > no_card,
        "the card's rows were drawn"
    );

    pointer_leave(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);
    assert_eq!(pane.widget.map.hovered(), None);
    assert_eq!(
        glyph_count(&harness, &pane),
        no_card,
        "the card's rows went with it"
    );
}

#[test]
fn a_removed_vessel_should_not_keep_its_card() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(
        &mut harness,
        &mut pane,
        keyframe(7, 1, vec![position(1), position(2)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let at = hull_of(&pane, 1);
    pointer_move(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);
    assert_eq!(pane.widget.map.hovered(), Some(1));
    pointer_leave(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);
    let no_card = glyph_count(&harness, &pane);
    pointer_move(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);

    push(&mut harness, &mut pane, removal(7, 1, 2, 1));
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.map.hovered(),
        None,
        "a vessel the picture no longer holds has no card"
    );
    assert_eq!(
        glyph_count(&harness, &pane),
        no_card,
        "the card went with the vessel"
    );
}

#[test]
fn the_update_stamps_should_follow_the_data_time_zone() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let at = hull_of(&pane, 1);
    pointer_move(&mut harness, &mut pane, at);
    frame(&mut harness, &mut pane);
    let utc = canvas_handle(&harness, &pane);

    set_data_time_zone(&mut harness, 8);
    frame(&mut harness, &mut pane);

    assert!(
        !Arc::ptr_eq(&utc, &canvas_handle(&harness, &pane)),
        "the card's stamps are read in the zone showing now"
    );
}

#[test]
fn the_credit_should_open_the_copyright_page() {
    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let recorded = Rc::clone(&opened);
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En))
        .with_open_url(move |url: &str| {
            recorded.borrow_mut().push(url.to_owned());
        });
    let mut pane = pane_with_source(&mut harness, RecordingSource::default());
    sign_in(&mut harness, ACCOUNT);
    load(&mut harness, &mut pane);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let bounds = pane.widget.map.bounds().expect("the map was laid out");
    let credit = Point::new(
        bounds.origin.x + 12.0,
        bounds.origin.y + bounds.size.height - 10.0,
    );
    harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerDown {
                position: credit,
                button: PointerButton::Primary,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        );
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerUp {
                position: credit,
                button: PointerButton::Primary,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        );
    });

    assert_eq!(opened.borrow().as_slice(), [ATTRIBUTION_URL]);
}

#[test]
fn a_cancelled_pointer_should_not_pan_or_open_the_credit() {
    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let recorded = Rc::clone(&opened);
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En))
        .with_open_url(move |url: &str| {
            recorded.borrow_mut().push(url.to_owned());
        });
    sign_in(&mut harness, ACCOUNT);
    let mut pane = pane_with_source(&mut harness, RecordingSource::default());
    load(&mut harness, &mut pane);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let bounds = pane.widget.map.bounds().expect("the map was laid out");
    let credit = Point::new(
        bounds.origin.x + 12.0,
        bounds.origin.y + bounds.size.height - 10.0,
    );
    assert!(pointer_down(&mut harness, &mut pane, credit));
    assert!(
        pointer_cancel(&mut harness, &mut pane),
        "the cancel belongs to the session the press began"
    );
    pointer_up(&mut harness, &mut pane, credit);
    assert!(
        opened.borrow().is_empty(),
        "a cancelled press is not a click on the credit"
    );

    let before = pane.widget.map.camera();
    pointer_move(
        &mut harness,
        &mut pane,
        Point::new(credit.x + 40.0, credit.y - 40.0),
    );
    assert_eq!(
        pane.widget.map.camera(),
        before,
        "a cancelled drag does not keep panning"
    );
}

#[test]
fn a_pointer_outside_the_canvas_should_not_be_the_map_s() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    let bounds = pane.widget.map.bounds().expect("the map was laid out");
    let outside = Point::new(
        bounds.origin.x - 40.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    let before = pane.widget.map.camera();

    assert!(
        !pointer_down(&mut harness, &mut pane, outside),
        "a press outside the map is not the map's to consume"
    );
    pointer_move(
        &mut harness,
        &mut pane,
        Point::new(outside.x + 30.0, outside.y),
    );
    assert_eq!(
        pane.widget.map.camera(),
        before,
        "a press outside the map does not begin a pan"
    );

    assert!(
        !wheel(&mut harness, &mut pane, outside, 1.0),
        "a wheel outside the map is not the map's to consume"
    );
    assert_eq!(
        pane.widget.map.camera(),
        before,
        "a wheel outside the map does not zoom it"
    );
}

#[test]
fn the_header_should_offer_the_types_the_picture_holds() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(
        &mut harness,
        &mut pane,
        keyframe_of_type(7, 1, vec![position(1)], code_of(ShipType::Tanker)),
    );
    build_header(&mut harness, &mut pane);

    let select = &pane
        .widget
        .filter_control
        .as_ref()
        .expect("the header control was built")
        .select;
    let items = select.items();
    let ids = items
        .iter()
        .map(|item| item.id.to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["all", "tanker"], "the picture holds one type");
    assert_eq!(
        select.chosen(harness.app()).as_slice(),
        [SmolStr::new_static("all")],
        "the filter starts on every type"
    );
}

#[test]
fn the_header_key_should_be_the_pane_s_own() {
    let mut harness = harness();
    let mut first = pane(&mut harness);
    let mut second = pane_with_source(&mut harness, RecordingSource::default());
    load(&mut harness, &mut second);
    build_header(&mut harness, &mut first);
    build_header(&mut harness, &mut second);

    let first_key = first
        .widget
        .filter_control
        .as_ref()
        .expect("the first control")
        .select
        .key()
        .to_owned();
    let second_key = second
        .widget
        .filter_control
        .as_ref()
        .expect("the second control")
        .select
        .key()
        .to_owned();
    assert_ne!(
        first_key, second_key,
        "two panes of this widget never share one action identity"
    );

    harness.with_cx(|cx| first.widget.release(cx));
    assert!(
        harness
            .app()
            .has_action_handler(&Action::Custom(smol_str::format_smolstr!(
                "{second_key}.trigger"
            ))),
        "the surviving pane still owns its filter"
    );
}

#[test]
fn two_panes_should_live_side_by_side() {
    let mut harness = harness();
    let mut first = pane(&mut harness);
    let mut second = pane_with_source(&mut harness, RecordingSource::default());
    load(&mut harness, &mut second);
    build_header(&mut harness, &mut first);
    build_header(&mut harness, &mut second);
    push(&mut harness, &mut first, keyframe(7, 1, vec![position(1)]));
    push(&mut harness, &mut second, keyframe(7, 1, vec![position(2)]));
    build_body(&mut harness, &mut first);
    build_body(&mut harness, &mut second);
    frame(&mut harness, &mut first);
    frame(&mut harness, &mut second);

    assert_ne!(
        map_node(&harness, &first),
        map_node(&harness, &second),
        "each pane draws into its own canvas"
    );
    assert_eq!(first.widget.painted.len(), 1);
    assert_eq!(second.widget.painted.len(), 1);
    assert_eq!(first.widget.painted[0].mmsi, 1);
    assert_eq!(second.widget.painted[0].mmsi, 2);
}

#[test]
fn releasing_the_pane_should_return_its_geometry_and_its_value_slots() {
    let mut harness = harness();
    let baseline = harness.app().geometry().len();
    let slots = harness.signal_slot_stats();
    let mut pane = pane(&mut harness);
    paint_one_vessel(&mut harness, &mut pane);
    assert!(
        harness.app().geometry().len() > baseline,
        "the map registered the coastline and the bases"
    );
    assert!(
        harness.signal_slot_stats().live > slots.live,
        "the widget minted signals"
    );

    harness.with_cx(|cx| pane.widget.release(cx));
    harness.app_mut().release_scope(pane.scope);

    assert_eq!(
        harness.app().geometry().len(),
        baseline,
        "every mesh the map registered is returned"
    );
    assert_eq!(
        harness.signal_slot_stats().live,
        slots.live,
        "releasing the pane reclaims its value slots"
    );
}

#[test]
fn the_map_should_paint_only_once_the_body_is_laid_out() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    push(&mut harness, &mut pane, keyframe(7, 1, vec![position(1)]));
    assert!(pane.widget.has_content());
    assert!(
        harness
            .find_by_debug_prefix(pane.page, MAP_SELECTOR)
            .is_none(),
        "no body means no canvas"
    );

    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    assert!(
        harness
            .find_by_debug_prefix(pane.page, MAP_SELECTOR)
            .is_some(),
        "the ready pane owns a canvas"
    );
}

#[test]
fn every_ship_type_should_round_trip_through_the_filter() {
    for ship_type in SHIP_TYPES {
        assert_eq!(
            ShipTypeFilter::from_key(ship_type.key()),
            Some(ShipTypeFilter::Only(ship_type)),
            "{ship_type:?}"
        );
        assert_eq!(
            ShipType::from_code(code_of(ship_type)),
            ship_type,
            "{ship_type:?}"
        );
    }
}
