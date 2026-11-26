use std::{rc::Rc, sync::Arc};

use app_config::{
    settings::{AccountConfig, AccountWalletKind, Config},
    time_zone::DataTimeZone,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use ui_core::{
    event::ActionSource,
    math::{LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope, tree::NodeKind},
    paint::{
        canvas::Canvas,
        primitive::{Primitive, QuadPaint},
        style_builder::StyleBuilder,
    },
};
use ui_kit::theme::{appearance::Appearance, role::Role};
use widget::{
    cx::WidgetCx,
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Failure, WidgetAction, WidgetState, WidgetStatus},
    test_support::WidgetTestApp,
    view::HeaderGroup,
};

use super::*;
use crate::{
    gauge::GAUGE_SELECTOR,
    test_support::{ACCOUNT, COLLECTED_AT, FakeSource, FetchPlan, empty_snapshot, row, snapshot},
};

const SETTLE_TURNS: usize = 64;

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
    let _ = write_config(harness, config);
}

fn set_data_time_zone(harness: &mut WidgetTestApp, hours: i8) {
    let mut config = current_config(harness);
    config.data_time_zone = DataTimeZone::from_hours(hours).expect("the offset is offered");
    assert!(
        write_config(harness, config),
        "another time zone is a settings change"
    );
}

fn body_parent(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_FGI_HOST")
            .style(StyleBuilder::new().size(px(400.0), px(480.0)).build())
            .build(cx.tree_mut(), root)
            .expect("the host's root is alive")
    })
}

struct Pane {
    widget: CoinMarketCapFearAndGreedIndexWidget,
    status: WidgetStatus,
    parent: NodeId,
}

fn pane_in_scope(harness: &mut WidgetTestApp, scope: SignalScope, source: FakeSource) -> Pane {
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let parent = body_parent(harness);
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
    let widget = CoinMarketCapFearAndGreedIndexWidget::new_with_source(
        source,
        status,
        WidgetAction::new("common.retry", |_| {}),
        &mut cx,
    );

    Pane {
        widget,
        status,
        parent,
    }
}

fn pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    let scope = harness.scope();
    pane_in_scope(harness, scope, source)
}

fn pane_for(harness: &mut WidgetTestApp, account: Option<&str>, source: FakeSource) -> Pane {
    match account {
        Some(account) => sign_in(harness, account),
        None => sign_out(harness),
    }
    pane(harness, source)
}

fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness.with_cx(|cx| pane.widget.load(cx));
}

fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness
        .with_cx(|cx| pane.widget.sync(cx))
        .expect("the pane syncs");
}

fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.parent;
    harness
        .with_cx(|cx| pane.widget.body(parent, cx))
        .expect("the body builds");
}

fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness.drain();
    sync(harness, pane);
    harness.drain();
}

fn settle(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    what: &str,
    ready: impl Fn(&WidgetTestApp, &Pane) -> bool,
) {
    for _ in 0..SETTLE_TURNS {
        harness.drain_runtime();
        sync(harness, pane);
        harness.drain();
        if ready(harness, pane) {
            return;
        }
    }
    panic!("{what} should happen");
}

fn key() -> CoinMarketCapFearAndGreedIndexStreamKey {
    CoinMarketCapFearAndGreedIndexStreamKey {
        account_id: SmolStr::new_static(ACCOUNT),
    }
}

fn report_snapshot(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    snapshot: Arc<CoinMarketCapFearAndGreedIndexSnapshot>,
) -> bool {
    harness.with_cx(|cx| {
        pane.widget.apply_latest(
            key(),
            Arc::new(CoinMarketCapFearAndGreedIndexStreamMessage::Snapshot(
                snapshot,
            )),
            cx,
        )
    })
}

fn report_failure(harness: &mut WidgetTestApp, pane: &mut Pane) -> bool {
    harness.with_cx(|cx| {
        pane.widget.apply_reliable(
            key(),
            Arc::new(CoinMarketCapFearAndGreedIndexStreamMessage::Failed {
                error: CoinMarketCapFearAndGreedIndexFetchError::Service(
                    KytrixServiceError::Disconnected,
                ),
            }),
            cx,
        )
    })
}

fn published_failure(harness: &WidgetTestApp, pane: &Pane) -> Failure {
    pane.status
        .state(harness.app())
        .failure()
        .cloned()
        .expect("a failure should be published")
}

fn recollected(current: u8, collected_at: i64) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    Arc::new(CoinMarketCapFearAndGreedIndexSnapshot {
        collected_at,
        ..(*snapshot(current)).clone()
    })
}

fn partial(current: u8) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    let mut partial = (*snapshot(current)).clone();
    partial.yesterday = None;
    partial.yearly_high = None;

    Arc::new(partial)
}

fn found(harness: &WidgetTestApp, pane: &Pane, selector: &str) -> Option<NodeId> {
    harness.find_by_debug_prefix(pane.parent, selector)
}

fn gauge_canvas(harness: &WidgetTestApp, pane: &Pane) -> Arc<Canvas> {
    let node = found(harness, pane, GAUGE_SELECTOR).expect("the gauge is drawn");
    match harness.tree().get(node).expect("the node exists").kind() {
        NodeKind::Canvas(canvas) => Arc::clone(canvas),
        other => panic!("the gauge node is a canvas, not {other:?}"),
    }
}

fn primitives(canvas: &Canvas) -> Vec<&Primitive> {
    canvas.primitives().iter().collect()
}

fn path_count(canvas: &Canvas) -> usize {
    primitives(canvas)
        .into_iter()
        .filter(|primitive| matches!(primitive, Primitive::Path(_)))
        .count()
}

fn band_fills(canvas: &Canvas) -> Vec<ui_core::paint::color::Color> {
    primitives(canvas)
        .into_iter()
        .filter_map(|primitive| match primitive {
            Primitive::Path(path) => Some(path.fill),
            _ => None,
        })
        .collect()
}

fn band_colours(theme: &ui_kit::theme::resolved::Theme) -> Vec<ui_core::paint::color::Color> {
    [
        Role::FearGreedExtremeFear,
        Role::FearGreedFear,
        Role::FearGreedNeutral,
        Role::FearGreedGreed,
        Role::FearGreedExtremeGreed,
    ]
    .map(|role| theme.role(role))
    .to_vec()
}

fn quads(canvas: &Canvas) -> Vec<&ui_core::paint::primitive::Quad> {
    primitives(canvas)
        .into_iter()
        .filter_map(|primitive| match primitive {
            Primitive::Quad(quad) => Some(quad),
            _ => None,
        })
        .collect()
}

fn text_recipe_count(canvas: &Canvas) -> usize {
    primitives(canvas)
        .into_iter()
        .filter(|primitive| matches!(primitive, Primitive::Text(_)))
        .count()
}

fn node_bounds(harness: &WidgetTestApp, node: NodeId) -> LogicalRect {
    harness
        .tree()
        .get(node)
        .expect("the node exists")
        .layout()
        .bounds
}

#[test]
fn the_widget_should_have_a_stable_identity() {
    assert_eq!(
        CoinMarketCapFearAndGreedIndexWidget::ID.as_str(),
        "kytrix.coin_market_cap_fear_and_greed_index"
    );
    assert_eq!(
        CoinMarketCapFearAndGreedIndexWidget::DISPLAY_NAME,
        "widget.names.coin_market_cap_fear_and_greed_index"
    );
    assert_eq!(
        <CoinMarketCapFearAndGreedIndexConfig as WidgetConfig>::SCHEMA_VERSION,
        app_config::widget::WidgetConfigSchemaVersion::V1
    );
}

#[test]
fn the_pane_should_persist_nothing() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let pane = pane(&mut harness, FakeSource::new([FetchPlan::Pending]));

    assert!(
        pane.widget.config().is_none(),
        "an index pane has no options of its own"
    );
}

#[test]
fn every_message_this_pane_shows_should_resolve_to_text() {
    let translate = product_translate(LanguageCode::En);

    for key in [
        "common.no_data",
        "widget.names.coin_market_cap_fear_and_greed_index",
        "widget.descriptions.coin_market_cap_fear_and_greed_index",
        "widget.coin_market_cap_fear_and_greed_index.info_history",
        "widget.coin_market_cap_fear_and_greed_index.info_yearly",
        "widget.coin_market_cap_fear_and_greed_index.last_month",
        "widget.coin_market_cap_fear_and_greed_index.last_week",
        "widget.coin_market_cap_fear_and_greed_index.last_year",
        "widget.coin_market_cap_fear_and_greed_index.level.extreme_fear",
        "widget.coin_market_cap_fear_and_greed_index.level.extreme_greed",
        "widget.coin_market_cap_fear_and_greed_index.level.fear",
        "widget.coin_market_cap_fear_and_greed_index.level.greed",
        "widget.coin_market_cap_fear_and_greed_index.level.neutral",
        "widget.coin_market_cap_fear_and_greed_index.logged_out",
        "widget.coin_market_cap_fear_and_greed_index.refresh",
        "widget.coin_market_cap_fear_and_greed_index.title",
        "widget.coin_market_cap_fear_and_greed_index.yearly_extremes",
        "widget.coin_market_cap_fear_and_greed_index.yesterday",
    ] {
        assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
    }
    assert!(
        !t_smol!(
            translate,
            "widget.coin_market_cap_fear_and_greed_index.yearly_high",
            date = "2026-10-23"
        )
        .is_empty(),
        "widget.coin_market_cap_fear_and_greed_index.yearly_high has no copy"
    );

    for failure in [
        CoinMarketCapFearAndGreedIndexFailure::Fetch(
            CoinMarketCapFearAndGreedIndexFetchError::Service(KytrixServiceError::Closed),
        ),
        CoinMarketCapFearAndGreedIndexFailure::Fetch(
            CoinMarketCapFearAndGreedIndexFetchError::Parse(
                CoinMarketCapFearAndGreedIndexParseError::ScoreOutOfRange { value: 101 },
            ),
        ),
        CoinMarketCapFearAndGreedIndexFailure::Rpc(RpcCallError::Service {
            request_id: RequestId::new(2),
            source: CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
        }),
    ] {
        assert!(
            !failure.translated_message(&translate).is_empty(),
            "{failure:?} has no copy"
        );
    }
}

#[test]
fn a_row_without_a_reading_should_draw_a_dash_rather_than_nothing() {
    let translate = product_translate(LanguageCode::En);
    let reading = row("2026-10-23", 45);

    assert_eq!(value_text(None).as_str(), "—");
    assert_eq!(date_text(None).as_str(), "—");
    assert_eq!(value_text(Some(&reading)).as_str(), "45");
    assert_eq!(date_text(Some(&reading)).as_str(), "2026-10-23");
    assert_eq!(level_name(None, &translate).as_str(), "");
    assert_eq!(level_name(Some(&reading), &translate).as_str(), "Neutral");

    let yearly = ROWS[4];
    assert_eq!(
        row_label(yearly, None, 4, &translate).as_str(),
        "Yearly High (—)",
        "a missing day is named by the same dash as a missing value"
    );
    assert_eq!(
        row_label(yearly, Some(snapshot(46).as_ref()), 4, &translate).as_str(),
        "Yearly High (2025-11-20)"
    );
    assert_eq!(
        row_label(ROWS[0], Some(snapshot(46).as_ref()), 0, &translate).as_str(),
        "Yesterday",
        "a history row is a plain label"
    );
}

#[test]
fn a_pane_without_an_account_should_ask_for_one() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Pending]);
    let mut pane = pane_for(&mut harness, None, source.clone());
    load(&mut harness, &mut pane);

    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::waiting(Notice::message(
            "widget.coin_market_cap_fear_and_greed_index.logged_out"
        )),
        "a pane nobody is signed in for says what it is waiting for"
    );
    assert!(!pane.widget.has_content());
    assert!(
        pane.widget.current_key().is_none(),
        "there is nothing to read without an account"
    );
    assert_eq!(
        source.fetch_count(),
        0,
        "a pane nobody is signed in for fetches nothing"
    );
}

#[test]
fn an_account_should_start_the_stream_with_its_first_fetch() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Pending]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source.clone());
    load(&mut harness, &mut pane);
    settle(&mut harness, &mut pane, "the first fetch", |_, _| {
        source.fetch_count() == 1
    });

    assert_eq!(pane.status.state(harness.app()), WidgetState::Connecting);
    assert!(!pane.widget.has_content());
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "a pane waiting for its first answer offers no refresh"
    );
}

#[test]
fn a_snapshot_should_be_drawn_as_content() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);

    assert!(
        report_snapshot(&mut harness, &mut pane, snapshot(46)),
        "accepting an answer is a change worth a frame"
    );
    assert!(pane.widget.has_content());
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    assert!(
        !pane.widget.refresh_is_disabled(harness.app()),
        "an idle pane with an answer can read again on demand"
    );
}

#[test]
fn a_snapshot_that_changed_nothing_should_not_ask_for_a_frame() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let held = snapshot(46);
    let polled_again = recollected(46, COLLECTED_AT + 60_000);
    assert_ne!(
        held.as_ref(),
        polled_again.as_ref(),
        "the second answer differs: only its collection time moved"
    );

    assert!(
        report_snapshot(&mut harness, &mut pane, held.clone()),
        "the first answer is a change worth a frame"
    );
    harness.drain();
    let requests = harness.frame_requests();

    assert!(
        !report_snapshot(&mut harness, &mut pane, polled_again),
        "an answer that folds into the same pixels is not worth a frame"
    );
    assert_eq!(
        harness.frame_requests(),
        requests,
        "the same reading polled again asks for nothing"
    );
    assert!(Arc::ptr_eq(
        &held,
        pane.widget.snapshot.as_ref().expect("the answer is kept")
    ));
}

#[test]
fn a_collected_answer_should_replace_an_empty_one_with_the_same_rows() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);

    let uncollected = empty_snapshot();
    assert!(
        report_snapshot(&mut harness, &mut pane, uncollected.clone()),
        "the first answer is a change worth a frame"
    );
    assert!(
        pane.widget
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.is_empty()),
        "the pane holds the uncollected answer"
    );

    let collected = Arc::new(CoinMarketCapFearAndGreedIndexSnapshot {
        collected_at: COLLECTED_AT,
        ..(*uncollected).clone()
    });
    assert!(
        report_snapshot(&mut harness, &mut pane, collected.clone()),
        "the empty state disappearing is a change worth a frame"
    );
    assert!(Arc::ptr_eq(
        &collected,
        pane.widget.snapshot.as_ref().expect("the answer is kept")
    ));
}

#[test]
fn an_unchanged_answer_should_end_a_pending_refresh() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    assert!(report_snapshot(&mut harness, &mut pane, snapshot(46)));
    harness.with_cx(|cx| {
        pane.widget.refresh(cx).expect("the refresh is sent");
    });
    assert!(
        pane.widget.refresh_pending,
        "the click leaves a refresh waiting for its answer"
    );

    assert!(
        report_snapshot(&mut harness, &mut pane, snapshot(46)),
        "the answer a refresh waited for is a change even when it draws the same"
    );
    assert!(
        !pane.widget.refresh_pending,
        "the answer ends the wait the click started"
    );
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    assert!(!pane.widget.refresh_is_disabled(harness.app()));
}

#[test]
fn an_unchanged_answer_should_still_finish_a_retry() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    assert!(report_snapshot(&mut harness, &mut pane, snapshot(46)));
    report_failure(&mut harness, &mut pane);
    load(&mut harness, &mut pane);

    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Connecting,
        "a recovery reports Connecting over the reading it kept"
    );
    assert!(
        pane.widget.has_content(),
        "the reading stays on screen through the retry"
    );

    assert!(
        report_snapshot(&mut harness, &mut pane, snapshot(46)),
        "the retry's answer is a change even though it draws the same"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Ready,
        "the answer ends the Connecting the retry reported"
    );
}

#[test]
fn a_failure_should_keep_the_reading_and_offer_the_framework_recovery() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    harness.with_cx(|cx| {
        pane.widget.refresh(cx).expect("the refresh is sent");
    });
    assert!(pane.widget.refresh_pending);
    report_failure(&mut harness, &mut pane);

    assert!(pane.status.is_failed(harness.app()));
    assert!(
        !pane.widget.refresh_pending,
        "the failure the refresh was waiting for ends the wait"
    );
    let failure = published_failure(&harness, &pane);
    assert!(
        !failure.text(&harness.services().translate).is_empty(),
        "a failure has to say what went wrong"
    );
    assert_eq!(
        failure.action().label_key(),
        "common.retry",
        "a failure always offers the framework's way out"
    );
    assert!(pane.widget.has_content(), "the reading stays on screen");
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "a failed pane offers no refresh of its own"
    );

    load(&mut harness, &mut pane);
    assert_eq!(pane.status.state(harness.app()), WidgetState::Connecting);
    assert!(
        pane.widget.has_content(),
        "a retry keeps the reading the user was looking at"
    );
}

#[test]
fn a_failure_should_be_terminal_for_late_messages() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    report_failure(&mut harness, &mut pane);

    assert!(
        !report_snapshot(&mut harness, &mut pane, snapshot(40)),
        "a failed pane takes no more snapshots"
    );
    assert!(pane.status.is_failed(harness.app()));
    assert_eq!(
        pane.widget
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.current),
        Some(46),
        "the reading the user was looking at is what stays"
    );
}

#[test]
fn a_snapshot_from_another_account_should_be_ignored() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);

    let changed = harness.with_cx(|cx| {
        pane.widget.apply_latest(
            CoinMarketCapFearAndGreedIndexStreamKey {
                account_id: SmolStr::new_static("another-account"),
            },
            Arc::new(CoinMarketCapFearAndGreedIndexStreamMessage::Snapshot(
                snapshot(46),
            )),
            cx,
        )
    });

    assert!(!changed, "another account's answer is not this pane's");
    assert!(!pane.widget.has_content());
}

#[test]
fn another_account_should_take_the_reading_with_it() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));

    sign_in(&mut harness, "second-account");
    sync(&mut harness, &mut pane);

    assert!(
        !pane.widget.has_content(),
        "the answer was served to the account that just left"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Connecting,
        "another account is another read"
    );
    assert_eq!(
        pane.widget
            .current_key()
            .map(|key| key.account_id.as_str().to_owned()),
        Some("second-account".to_owned())
    );

    sign_out(&mut harness);
    sync(&mut harness, &mut pane);

    assert!(!pane.widget.has_content());
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::waiting(Notice::message(
            "widget.coin_market_cap_fear_and_greed_index.logged_out"
        )),
        "signing out asks for an account rather than reporting a failure"
    );
    assert!(pane.widget.current_key().is_none());
}

#[test]
fn an_account_change_should_read_the_new_account() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Snapshot(40)]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source.clone());
    load(&mut harness, &mut pane);
    settle(&mut harness, &mut pane, "the pane to read", |_, pane| {
        pane.widget.has_content()
    });
    assert_eq!(source.fetch_count(), 1);

    sign_in(&mut harness, "someone-else");
    sync(&mut harness, &mut pane);

    settle(
        &mut harness,
        &mut pane,
        "the replacement read",
        |_, pane| {
            pane.widget.account_id.as_deref() == Some("someone-else") && pane.widget.has_content()
        },
    );
    assert_eq!(source.fetch_count(), 2);
    assert_eq!(
        pane.widget
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.current),
        Some(40),
        "the new account is read for itself"
    );

    sign_out(&mut harness);
    sync(&mut harness, &mut pane);

    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::waiting(Notice::message(
            "widget.coin_market_cap_fear_and_greed_index.logged_out"
        ))
    );
    assert!(
        !pane.widget.has_content(),
        "signing out takes the reading with it"
    );
}

#[test]
fn an_answer_to_a_replaced_load_should_not_fail_the_pane() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending, FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    sign_in(&mut harness, "someone-else");
    sync(&mut harness, &mut pane);
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Connecting,
        "the replacement read is waiting for its own answer"
    );

    harness.with_cx(|cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::Service {
                request_id: RequestId::new(1),
                source: CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
            }),
            cx,
        );
    });

    assert!(
        !pane.status.is_failed(harness.app()),
        "the replaced load's failure is not the failure of the command now waiting"
    );
    assert_eq!(
        pane.widget.pending.dropped(),
        1,
        "the late answer is dropped and counted"
    );
    assert!(
        !pane.widget.pending.is_empty(),
        "the replacement's own answer is still waited for"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Connecting,
        "the pane is still waiting rather than failed"
    );
}

#[test]
fn a_refresh_should_close_the_control_until_the_answer_arrives() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Held(52)]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source.clone());
    load(&mut harness, &mut pane);
    settle(
        &mut harness,
        &mut pane,
        "the first answer",
        |harness, pane| pane.status.state(harness.app()) == WidgetState::Ready,
    );

    assert!(!pane.widget.refresh_is_disabled(harness.app()));
    harness.with_cx(|cx| {
        pane.widget.refresh(cx).expect("the refresh is sent");
    });
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "the click closes the window itself, before anything has answered"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Ready,
        "a refresh is not a state of its own: the reading on screen is still ready"
    );

    let block = 0;
    let _ = block;
    settle(
        &mut harness,
        &mut pane,
        "the refresh to reach the service",
        |_, _| source.fetch_count() == 2,
    );
    source.release();
    settle(
        &mut harness,
        &mut pane,
        "the refreshed answer",
        |_, pane| {
            pane.widget
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.current == 52)
        },
    );
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    assert!(
        !pane.widget.refresh_is_disabled(harness.app()),
        "the answer reopens the control"
    );

    harness.with_cx(|cx| {
        pane.widget.refresh(cx).expect("the refresh is sent");
    });
    assert!(pane.widget.refresh_pending);
    sign_in(&mut harness, "someone-else");
    sync(&mut harness, &mut pane);
    assert!(
        !pane.widget.refresh_pending,
        "the command belonged to the stream the account change replaced"
    );
}

#[test]
fn a_refused_refresh_should_be_reported_as_a_failure() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    harness.with_cx(|cx| {
        pane.widget.refresh(cx).expect("the refresh is sent");
        assert!(pane.widget.refresh_is_disabled(cx.app()));
        pane.widget.apply_rpc(
            RequestId::new(2),
            Err(RpcCallError::Service {
                request_id: RequestId::new(2),
                source: CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
            }),
            cx,
        );
    });

    assert!(pane.status.is_failed(harness.app()));
    assert_eq!(
        pane.widget.pending.dropped(),
        0,
        "the answer named the command that was waiting for it"
    );
    assert!(
        pane.widget.pending.is_empty(),
        "the answered command is no longer in flight"
    );
    assert!(
        !pane.widget.refresh_pending,
        "a refused command is not one to keep waiting for"
    );
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "a failed pane offers no refresh of its own"
    );
    let failure = published_failure(&harness, &pane);
    assert!(!failure.text(&harness.services().translate).is_empty());
    assert_eq!(failure.action().label_key(), "common.retry");
}

#[test]
fn a_host_without_the_service_connection_should_report_a_failure() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let widget =
        harness.mount::<CoinMarketCapFearAndGreedIndexWidget>(CoinMarketCapFearAndGreedIndexConfig);

    let state = widget.status().state(harness.app());
    assert!(
        state.is_failed(),
        "a pane whose connection is missing reports it: {state:?}"
    );
    let failure = state.failure().expect("the state is a failure");
    assert!(!failure.text(&harness.services().translate).is_empty());
}

#[test]
fn a_snapshot_with_missing_rows_should_draw_six_rows_and_dashes() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    assert!(
        report_snapshot(&mut harness, &mut pane, partial(46)),
        "a partly empty answer is still content"
    );
    assert!(pane.widget.has_content());
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Ready,
        "a missing row is not a missing snapshot"
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    for selector in [
        ROW_YESTERDAY,
        ROW_LAST_WEEK,
        ROW_LAST_MONTH,
        ROW_LAST_YEAR,
        ROW_YEARLY_HIGH,
        ROW_YEARLY_LOW,
    ] {
        assert!(
            found(&harness, &pane, selector).is_some(),
            "{selector} is one of the six rows the pane draws"
        );
    }
    for selector in [MISSING_YESTERDAY, MISSING_YEARLY_HIGH] {
        assert!(
            found(&harness, &pane, selector).is_some(),
            "{selector}: a row without a reading draws its dash"
        );
    }
    for selector in [
        MISSING_LAST_WEEK,
        MISSING_LAST_MONTH,
        MISSING_LAST_YEAR,
        MISSING_YEARLY_LOW,
    ] {
        assert!(
            found(&harness, &pane, selector).is_none(),
            "{selector}: a row with a reading draws no dash"
        );
    }
    assert!(
        found(&harness, &pane, GAUGE_SELECTOR).is_some(),
        "the gauge is drawn beside the rows"
    );
    for selector in [LEVEL_MARKER_YESTERDAY, LEVEL_MARKER_YEARLY_HIGH] {
        assert!(
            found(&harness, &pane, selector).is_none(),
            "{selector}: a row without a reading draws no level dot"
        );
    }
    for selector in [
        LEVEL_MARKER_LAST_WEEK,
        LEVEL_MARKER_LAST_MONTH,
        LEVEL_MARKER_LAST_YEAR,
        LEVEL_MARKER_YEARLY_LOW,
    ] {
        assert!(
            found(&harness, &pane, selector).is_some(),
            "{selector}: a row with a reading draws its level dot"
        );
    }
}

#[test]
fn an_empty_snapshot_should_say_there_is_no_data_instead_of_drawing_a_gauge() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert!(
        found(&harness, &pane, EMPTY_SELECTOR).is_none(),
        "a pane with no answer yet has nothing to say about data"
    );

    report_snapshot(&mut harness, &mut pane, snapshot(46));
    frame(&mut harness, &mut pane);
    assert!(
        found(&harness, &pane, GAUGE_SELECTOR).is_some(),
        "a collected reading is drawn as a gauge"
    );
    assert!(found(&harness, &pane, EMPTY_SELECTOR).is_none());

    report_snapshot(&mut harness, &mut pane, snapshot(0));
    frame(&mut harness, &mut pane);
    assert!(
        found(&harness, &pane, GAUGE_SELECTOR).is_some(),
        "a collected zero is a reading at the bottom of the scale, not an empty snapshot"
    );
    assert!(found(&harness, &pane, EMPTY_SELECTOR).is_none());

    assert!(
        report_snapshot(&mut harness, &mut pane, empty_snapshot()),
        "an uncollected snapshot is content of its own"
    );
    assert!(
        pane.widget.has_content(),
        "an empty snapshot must not turn the body into a placeholder"
    );
    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Ready,
        "an uncollected snapshot is a healthy answer, not a failure or a wait"
    );
    frame(&mut harness, &mut pane);
    assert!(
        found(&harness, &pane, EMPTY_SELECTOR).is_some(),
        "an uncollected snapshot says there is no data"
    );
    assert!(
        found(&harness, &pane, GAUGE_SELECTOR).is_none(),
        "nothing is drawn from a reading nobody collected"
    );
}

#[test]
fn a_collected_reading_should_draw_the_gauge_frame() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let canvas = gauge_canvas(&harness, &pane);
    let node = found(&harness, &pane, GAUGE_SELECTOR).expect("the gauge node");
    let bounds = node_bounds(&harness, node);
    assert_eq!(
        canvas.bounds().size,
        bounds.size,
        "the canvas covers the node's box"
    );
    assert!(
        bounds.size.width > px(96.0) && bounds.size.height > px(72.0),
        "the fixture box is large enough for a gauge: {bounds:?}"
    );

    assert_eq!(path_count(&canvas), 5, "one band per level of the scale");
    assert_eq!(quads(&canvas).len(), 12, "ten caps, the ring and the dot");
    assert_eq!(
        text_recipe_count(&canvas),
        2,
        "the score and the level's name are recorded as canvas text"
    );
    assert_eq!(
        harness.app().geometry().len(),
        5,
        "the five band meshes are registered in the geometry store"
    );

    let local = canvas.bounds();
    for primitive in primitives(&canvas) {
        let drawn = match primitive {
            Primitive::Path(path) => path.bounds,
            Primitive::Quad(quad) => quad.bounds,
            _ => continue,
        };
        assert!(
            drawn.origin.x >= local.origin.x - 1.0
                && drawn.origin.y >= local.origin.y - 1.0
                && drawn.max().x <= local.max().x + 1.0
                && drawn.max().y <= local.max().y + 1.0,
            "every painted piece stays inside the canvas: {drawn:?} of {local:?}"
        );
    }

    for quad in quads(&canvas) {
        let radius = quad.corner_radius[0];
        if let QuadPaint::Solid { fill, .. } = &quad.paint {
            assert_ne!(*fill, ui_core::paint::color::Color::TRANSPARENT);
        }
        assert_eq!(
            quad.bounds.size.width,
            radius * 2.0,
            "the caps and the indicator are circles rounded by their own radius"
        );
    }
}

#[test]
fn the_two_decorative_icons_should_keep_the_reference_size() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, pane.parent, cx))
        .expect("the header builds");
    frame(&mut harness, &mut pane);

    let body = found(&harness, &pane, BODY_SELECTOR).expect("the body is built");
    let card = harness.tree().get(body).expect("the body node").children()[0];
    let heading = harness.tree().get(card).expect("the card node").children()[1];
    let info_icon = harness
        .tree()
        .get(heading)
        .expect("the heading row")
        .children()[1];
    assert_eq!(
        node_bounds(&harness, info_icon).size.width,
        px(12.0),
        "the heading's info icon is drawn at the reference's 12 px"
    );

    let control = found(&harness, &pane, REFRESH_SELECTOR).expect("the refresh control");
    let button = harness
        .tree()
        .get(control)
        .expect("the control node")
        .children()[0];
    let refresh_icon = harness
        .tree()
        .get(button)
        .expect("the button node")
        .children()[0];
    assert_eq!(
        node_bounds(&harness, refresh_icon).size.width,
        px(12.0),
        "the header's refresh icon is drawn at the reference's 12 px"
    );
}

#[test]
fn the_indicator_should_move_with_the_reading() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(10));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let low_canvas = gauge_canvas(&harness, &pane);
    let low = quads(&low_canvas)
        .last()
        .map(|quad| quad.bounds)
        .expect("the dot is the last circle");
    report_snapshot(&mut harness, &mut pane, snapshot(90));
    frame(&mut harness, &mut pane);
    let high_canvas = gauge_canvas(&harness, &pane);
    let high = quads(&high_canvas)
        .last()
        .map(|quad| quad.bounds)
        .expect("the dot is the last circle");

    assert!(
        high.origin.x > low.origin.x,
        "a higher reading draws its dot further right: {low:?} vs {high:?}"
    );
}

#[test]
fn the_gauge_should_read_its_colours_from_the_theme() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let canvas = gauge_canvas(&harness, &pane);
    let theme = ui_kit::theme::resolved::Theme::of(Appearance::Light);
    assert_eq!(
        band_fills(&canvas),
        band_colours(&theme),
        "the bands are the scale's own roles"
    );
    let circles = quads(&canvas);
    let dot = circles.last().expect("the dot is drawn");
    let QuadPaint::Solid { fill, .. } = &dot.paint else {
        panic!("the dot is a solid quad");
    };
    assert_eq!(*fill, theme.role(Role::FgDefault));
}

#[test]
fn a_dark_appearance_should_repaint_the_gauge() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let light_canvas = gauge_canvas(&harness, &pane);
    assert_eq!(
        band_fills(&light_canvas),
        band_colours(&ui_kit::theme::resolved::Theme::of(Appearance::Light)),
        "the frame a light pane prepared is drawn in the light roles"
    );

    let theme = pane.widget.theme.clone();
    assert!(
        theme.set_appearance(harness.app_mut(), Appearance::Dark),
        "another appearance is a change the pane has to draw"
    );
    frame(&mut harness, &mut pane);

    let dark_canvas = gauge_canvas(&harness, &pane);
    assert_eq!(
        band_fills(&dark_canvas),
        band_colours(&ui_kit::theme::resolved::Theme::of(Appearance::Dark)),
        "the dark appearance repaints the bands in the dark roles"
    );
    assert!(
        !Arc::ptr_eq(&light_canvas, &dark_canvas),
        "the redraw prepared a canvas of its own"
    );
}

#[test]
fn a_new_reading_should_not_rebuild_the_body() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let nodes = harness.node_count();

    report_snapshot(&mut harness, &mut pane, snapshot(72));
    frame(&mut harness, &mut pane);

    assert_eq!(
        harness.node_count(),
        nodes,
        "the same set of rows is the same tree"
    );
    assert!(
        harness.text_of(pane.parent).contains("72"),
        "the new current value is drawn: {}",
        harness.text_of(pane.parent)
    );
}

#[test]
fn a_missing_row_should_come_and_go_with_the_reading() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert!(found(&harness, &pane, LEVEL_MARKER_YESTERDAY).is_some());
    assert!(found(&harness, &pane, MISSING_YESTERDAY).is_none());

    report_snapshot(&mut harness, &mut pane, partial(46));
    frame(&mut harness, &mut pane);
    assert!(
        found(&harness, &pane, LEVEL_MARKER_YESTERDAY).is_none(),
        "the row without a reading draws no dot"
    );
    assert!(
        found(&harness, &pane, MISSING_YESTERDAY).is_some(),
        "the row without a reading draws its dash"
    );
    assert!(
        found(&harness, &pane, ROW_LAST_WEEK).is_some(),
        "the rows beside it stay"
    );

    report_snapshot(&mut harness, &mut pane, snapshot(46));
    frame(&mut harness, &mut pane);
    assert!(found(&harness, &pane, LEVEL_MARKER_YESTERDAY).is_some());
    assert!(found(&harness, &pane, MISSING_YESTERDAY).is_none());
}

#[test]
fn a_steady_frame_should_do_no_work() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, pane.parent, cx))
        .expect("the header builds");
    frame(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let requests = harness.frame_requests();
    sync(&mut harness, &mut pane);
    sync(&mut harness, &mut pane);

    assert_eq!(
        harness.frame_requests(),
        requests,
        "a steady pane asks for no frame"
    );
}

#[test]
fn a_steady_canvas_should_be_kept() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let drawn = gauge_canvas(&harness, &pane);
    sync(&mut harness, &mut pane);
    sync(&mut harness, &mut pane);

    assert!(
        Arc::ptr_eq(&drawn, &gauge_canvas(&harness, &pane)),
        "a steady frame keeps the canvas it is drawn from"
    );
    assert_eq!(
        harness.app().geometry().len(),
        5,
        "a steady frame registers no mesh of its own either"
    );
}

#[test]
fn a_language_change_should_redraw_the_level_name_in_the_new_language() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(66));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget
            .gauge
            .prepared_level()
            .expect("a drawn gauge prepares its text")
            .as_str(),
        "Greed",
        "the gauge draws the level in the language of the frame that built it"
    );
    assert!(harness.text_of(pane.parent).contains("Historical Values"));

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget
            .gauge
            .prepared_level()
            .expect("the redrawn pane prepares its text again")
            .as_str(),
        "贪婪",
        "the gauge of the redrawn frame names the level in the new language"
    );
    let body = harness.text_of(pane.parent);
    assert!(body.contains("历史数据"), "the heading follows: {body}");
    assert!(
        body.contains("年度最高值（2025-11-20）"),
        "the yearly row's sentence is resolved again: {body}"
    );
    assert!(body.contains("贪婪"), "the row's level follows: {body}");
}

#[test]
fn the_empty_stand_in_should_follow_the_language() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, empty_snapshot());
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let empty = found(&harness, &pane, EMPTY_SELECTOR).expect("the stand-in is drawn");
    assert!(
        harness.text_of(empty).contains("No data"),
        "the stand-in says there is no data: {}",
        harness.text_of(empty)
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let empty = found(&harness, &pane, EMPTY_SELECTOR).expect("the stand-in is drawn");
    assert!(
        harness.text_of(empty).contains("暂无数据"),
        "the stand-in follows the language: {}",
        harness.text_of(empty)
    );
}

#[test]
fn the_info_notes_should_follow_the_language() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let english = product_translate(LanguageCode::En);
    let history_note = pane.widget.tooltips[0]
        .as_ref()
        .expect("the first section has a note")
        .content()
        .expect("the note has content")
        .text;
    assert_eq!(
        history_note,
        t_smol!(
            english,
            "widget.coin_market_cap_fear_and_greed_index.info_history"
        )
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let chinese = product_translate(LanguageCode::ZhCn);
    assert_eq!(
        pane.widget.tooltips[0]
            .as_ref()
            .expect("the first section has a note")
            .content()
            .expect("the note has content")
            .text,
        t_smol!(
            chinese,
            "widget.coin_market_cap_fear_and_greed_index.info_history"
        ),
        "the note is resolved in the language showing now"
    );
    assert_ne!(
        pane.widget.tooltips[0]
            .as_ref()
            .expect("the first section has a note")
            .content()
            .expect("the note has content")
            .text,
        history_note
    );
}

#[test]
fn the_display_time_zone_should_not_move_a_service_date() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let before = harness.text_of(pane.parent);
    assert!(before.contains("Yearly High (2025-11-20)"));

    set_data_time_zone(&mut harness, 8);
    frame(&mut harness, &mut pane);

    let after = harness.text_of(pane.parent);
    assert!(
        after.contains("Yearly High (2025-11-20)"),
        "the service's calendar day is drawn as the service wrote it: {after}"
    );
    assert_eq!(
        after, before,
        "the display zone does not move a service date"
    );
}

#[test]
fn the_header_should_refresh_without_starting_over() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Held(52)]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source.clone());
    load(&mut harness, &mut pane);
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, pane.parent, cx))
        .expect("the header builds");
    settle(
        &mut harness,
        &mut pane,
        "the first answer",
        |harness, pane| pane.status.state(harness.app()) == WidgetState::Ready,
    );
    harness.drain();

    let (node, action) = {
        let header = pane.widget.header.as_ref().expect("the control is built");
        (header.node, header.action.clone())
    };
    let window = harness.window();
    assert!(
        harness
            .app_mut()
            .dispatch_action(window, node, action, ActionSource::Programmatic)
    );
    sync(&mut harness, &mut pane);

    assert_eq!(
        pane.status.state(harness.app()),
        WidgetState::Ready,
        "a refresh is not a state of its own: the reading on screen is still ready"
    );
    assert!(
        pane.widget.refresh_pending,
        "the click leaves a refresh waiting for its reading"
    );
    assert!(
        pane.widget.has_content(),
        "a refresh does not clear the reading on screen"
    );
    assert_eq!(
        pane.widget
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.current),
        Some(46)
    );

    settle(
        &mut harness,
        &mut pane,
        "the refresh to reach the service",
        |_, _| source.fetch_count() == 2,
    );
    source.release();
    settle(
        &mut harness,
        &mut pane,
        "the refreshed answer",
        |_, pane| {
            pane.widget
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.current == 52)
        },
    );
    assert!(
        !pane.widget.refresh_pending,
        "the answer the refresh waited for ends the wait"
    );
    assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    assert_eq!(
        source.fetch_count(),
        2,
        "a refresh fetches once; it does not restart the stream"
    );
}

#[test]
fn a_disabled_refresh_should_not_forward_a_click() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Pending]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source.clone());
    load(&mut harness, &mut pane);
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, pane.parent, cx))
        .expect("the header builds");
    settle(&mut harness, &mut pane, "the first fetch", |_, _| {
        source.fetch_count() == 1
    });
    harness.drain();

    let (node, action) = {
        let header = pane.widget.header.as_ref().expect("the control is built");
        assert!(header.disabled, "the control is drawn disabled");
        (header.node, header.action.clone())
    };
    let window = harness.window();
    assert!(
        harness
            .app_mut()
            .dispatch_action(window, node, action, ActionSource::Programmatic)
    );
    sync(&mut harness, &mut pane);

    assert_eq!(
        source.fetch_count(),
        1,
        "a disabled control sends nothing to the service"
    );
    assert!(
        !pane.widget.refresh_pending,
        "a disabled control does not even open a refresh window"
    );
    assert_eq!(pane.status.state(harness.app()), WidgetState::Connecting);

    harness
        .with_cx(|cx| pane.widget.refresh(cx))
        .expect("a refused refresh is not an engine failure");
    assert!(
        !pane.widget.refresh_pending,
        "the action entry refuses a refresh the control is drawn disabled for"
    );
    assert_eq!(source.fetch_count(), 1);
}

fn with_scope<R>(
    harness: &mut WidgetTestApp,
    scope: SignalScope,
    body: impl FnOnce(&mut WidgetCx<'_>) -> R,
) -> R {
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
    body(&mut cx)
}

#[test]
fn release_should_take_everything_down() {
    let mut harness = harness();
    let source = FakeSource::new([FetchPlan::Pending]);
    let mut pane = pane_for(&mut harness, Some(ACCOUNT), source);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    let scope = harness.scope();
    with_scope(&mut harness, scope, |cx| {
        pane.widget.header(HeaderGroup::Right, pane.parent, cx)
    })
    .expect("the header builds");
    frame(&mut harness, &mut pane);
    assert_eq!(harness.app().geometry().len(), 5);
    assert_eq!(harness.layer().len(harness.window()), 0);
    let action = pane
        .widget
        .header
        .as_ref()
        .expect("the control is built")
        .action
        .clone();
    assert!(harness.app().has_action_handler(&action));

    harness.with_cx(|cx| pane.widget.release(cx));

    assert!(
        !harness.app().has_action_handler(&action),
        "the header control's action handler is unregistered"
    );
    assert_eq!(
        harness.app().geometry().len(),
        0,
        "the canvas' meshes are given back to the process-wide store"
    );
    assert_eq!(
        harness.layer().len(harness.window()),
        0,
        "the tooltips' overlays are released"
    );
    assert!(
        found(&harness, &pane, BODY_SELECTOR).is_none(),
        "the body this widget built is gone"
    );
    assert!(
        pane.widget.binding.is_none(),
        "the service bridge is dropped"
    );
    assert!(pane.widget.header.is_none());
    assert!(pane.widget.tooltips.iter().all(Option::is_none));
}

#[test]
fn a_card_that_goes_away_should_release_its_geometries() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(harness.app().geometry().len(), 5);

    report_snapshot(&mut harness, &mut pane, empty_snapshot());
    frame(&mut harness, &mut pane);

    assert_eq!(
        harness.app().geometry().len(),
        0,
        "the empty state replaced the card, and the card's meshes went with it"
    );
}

#[test]
fn changing_the_reading_should_keep_five_meshes_alive() {
    let mut harness = harness();
    let mut pane = pane_for(
        &mut harness,
        Some(ACCOUNT),
        FakeSource::new([FetchPlan::Pending]),
    );
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, snapshot(46));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(harness.app().geometry().len(), 5);

    for score in [10, 20, 30, 40, 50, 60, 70, 80] {
        report_snapshot(&mut harness, &mut pane, snapshot(score));
        frame(&mut harness, &mut pane);
        assert_eq!(
            harness.app().geometry().len(),
            5,
            "a redraw keeps exactly the five meshes it draws with (reading {score})"
        );
    }

    report_snapshot(&mut harness, &mut pane, empty_snapshot());
    frame(&mut harness, &mut pane);
    assert_eq!(
        harness.app().geometry().len(),
        0,
        "the card's meshes go when the card does"
    );
}

#[test]
fn two_panes_should_mint_different_header_keys_and_keep_both_handlers() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let first_scope = harness.scope();
    let mut first = pane_in_scope(
        &mut harness,
        first_scope,
        FakeSource::new([FetchPlan::Pending]),
    );
    let second_scope = harness.app_mut().signal_scope();
    let mut second = pane_in_scope(
        &mut harness,
        second_scope,
        FakeSource::new([FetchPlan::Pending]),
    );

    for (pane, scope) in [(&mut first, first_scope), (&mut second, second_scope)] {
        with_scope(&mut harness, scope, |cx| {
            pane.widget.header(HeaderGroup::Right, pane.parent, cx)
        })
        .expect("the header builds");
    }

    let first_action = first
        .widget
        .header
        .as_ref()
        .expect("the first control is built")
        .action
        .clone();
    let second_action = second
        .widget
        .header
        .as_ref()
        .expect("the second control is built")
        .action
        .clone();
    assert_ne!(
        first_action, second_action,
        "each pane's control registered its own action identity"
    );
    assert!(harness.app().has_action_handler(&first_action));
    assert!(harness.app().has_action_handler(&second_action));

    with_scope(&mut harness, first_scope, |cx| first.widget.release(cx));
    assert!(
        !harness.app().has_action_handler(&first_action),
        "the closed pane's handler is unregistered"
    );
    assert!(
        harness.app().has_action_handler(&second_action),
        "releasing one pane removed another pane's handler"
    );
    assert!(harness.tree().get(second.parent).is_ok());
}
