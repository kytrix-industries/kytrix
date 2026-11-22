//! The pane's interface tests: the state machine, the body it draws, and the
//! controls it offers, driven through the framework's headless host.
//!
//! # How a pane is built here
//!
//! The pane's source is the Kytrix service connection, which no headless host can
//! construct (the connector still needs the toolkit's context — design §13.9 R-1), so
//! the tests build the widget directly over the crate's [`FakeSource`] and drive its
//! hooks themselves: `load`, `sync`, `body`, `header`, `release`. The host still
//! supplies everything else the application would — the injected services, the
//! composition-root singletons, the engine, the frame pipeline — so nothing here
//! reaches a global.
//!
//! A test that needs a *live* service drives the host's current-thread runtime
//! ([`settle`]): one runtime turn, one `sync`, one frame, until the thing it is
//! waiting for happened. Nothing sleeps and no wall clock moves.

use std::{rc::Rc, sync::Arc};

use app_config::{
    settings::{AccountConfig, AccountWalletKind, Config},
    time_zone::DataTimeZone,
};
use base_ui::{
    style_builder::StyleBuilder,
    theme::{appearance::Appearance, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use ui_core::{
    event::ActionSource,
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope, tree::NodeKind},
    paint::{
        canvas::Canvas,
        primitive::{Primitive, QuadPaint},
    },
};
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

/// How many runtime/frame turns a [`settle`] may take before it calls the test a
/// failure.
const SETTLE_TURNS: usize = 64;

/// The translator the product ships: what these tests assert is the catalogue's copy.
fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// The framework's headless host, over the product catalogue.
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

/// Signs `user_id` in the way the composition root does: a settings write.
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

/// Signs the pane out the way the composition root does: a settings write.
///
/// A host that has nobody signed in already is not a change, and that is fine: the
/// write is idempotent, and every fixture starts from the host's own settings.
fn sign_out(harness: &mut WidgetTestApp) {
    let mut config = current_config(harness);
    config.account = None;
    let _ = write_config(harness, config);
}

/// Replaces the display time zone the way the settings screen does.
fn set_data_time_zone(harness: &mut WidgetTestApp, hours: i8) {
    let mut config = current_config(harness);
    config.data_time_zone = DataTimeZone::from_hours(hours).expect("the offset is offered");
    assert!(
        write_config(harness, config),
        "another time zone is a settings change"
    );
}

/// A host node with a real box: the body is `size_full`, so it needs a parent that
/// has a size before the gauge can be laid out.
fn body_parent(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_FGI_HOST")
            .style(StyleBuilder::new().size(400.0, 480.0).build())
            .build(cx.tree_mut(), root)
            .expect("the host's root is alive")
    })
}

/// A pane the framework would have built, driven by a fake source.
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

/// A pane over a source, scoped to `account` (or to nobody).
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

/// Builds the body the framework would build once the pane has content.
fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.parent;
    harness
        .with_cx(|cx| pane.widget.body(parent, cx))
        .expect("the body builds");
}

/// One production-shaped round: lay the tree out, let the pane synchronize against
/// it, and present what it wrote.
///
/// The layout pass comes first because the gauge reads its canvas' measured box: a
/// `sync` before the first layout has no box to draw into (the same one-frame
/// ordering the framework's own pump has).
fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness.drain();
    sync(harness, pane);
    harness.drain();
}

/// Lets the host's runtime deliver what the fake source published, and the pane the
/// frames it asked for, until `ready` holds.
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

/// Reports a snapshot as the stream would.
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

/// Reports the failure the stream would.
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

/// The failure the pane published.
fn published_failure(harness: &WidgetTestApp, pane: &Pane) -> Failure {
    pane.status
        .state(harness.app())
        .failure()
        .cloned()
        .expect("a failure should be published")
}

/// The reading of `snapshot(current)` as a later poll collected it.
fn recollected(current: u8, collected_at: i64) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    Arc::new(CoinMarketCapFearAndGreedIndexSnapshot {
        collected_at,
        ..(*snapshot(current)).clone()
    })
}

/// A snapshot of `current` with two rows missing, so a test can see the dash.
fn partial(current: u8) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    let mut partial = (*snapshot(current)).clone();
    partial.yesterday = None;
    partial.yearly_high = None;

    Arc::new(partial)
}

fn found(harness: &WidgetTestApp, pane: &Pane, selector: &str) -> Option<NodeId> {
    harness.find_by_debug_prefix(pane.parent, selector)
}

/// The canvas the gauge drew on.
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

fn quads(canvas: &Canvas) -> Vec<&ui_core::paint::primitive::Quad> {
    primitives(canvas)
        .into_iter()
        .filter_map(|primitive| match primitive {
            Primitive::Quad(quad) => Some(quad),
            _ => None,
        })
        .collect()
}

fn glyph_run_count(canvas: &Canvas) -> usize {
    primitives(canvas)
        .into_iter()
        .filter(|primitive| matches!(primitive, Primitive::GlyphRun(_)))
        .count()
}

/// The score's font size the gauge would use for `bounds`, so a test can assert the
/// text really is sized from the band rather than from the canvas height.
fn node_bounds(harness: &WidgetTestApp, node: NodeId) -> ui_core::math::Rect {
    harness
        .tree()
        .get(node)
        .expect("the node exists")
        .layout()
        .bounds
}

// ── identity, copy, and the pure helpers ────────────────────────────────────────

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

/// A missing key is an empty string on screen rather than an error, so a gap in the
/// interface is what a typo looks like: this is the test that turns it into a failure.
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

// ── the state machine ──────────────────────────────────────────────────────────

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

/// The reading the pane draws is what it compares: a later poll of an index that has
/// not moved carries a new collection time but the same pixels, so it is not a frame
/// and the allocation it replaced is kept.
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

/// An uncollected answer draws 「暂无数据」, so the first collected answer is a change
/// even when its rows are still the empty defaults: the empty state goes away, and
/// the pane has to keep the collected answer rather than the old one.
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

/// A refresh is one command and one answer: even when the answer draws exactly what
/// is already on screen, it still ends the wait and re-enables the header's control.
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

/// A retry keeps the reading and reports `Connecting`, so the answer that draws the
/// same is still the answer that finishes it.
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

/// Another account is another scope: the answer the pane is showing was served to
/// the previous one, so the settings write that moves the account drops it.
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

/// Another account is another read: what the new account is owed is fetched for it,
/// and signing out takes the reading away.
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

/// A refresh is one command and one answer, and the control is closed between them:
/// the service refuses a second command while one is in flight.
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
            Err(RpcCallError::Service {
                request_id: RequestId::new(1),
                source: CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
            }),
            cx,
        );
    });

    assert!(pane.status.is_failed(harness.app()));
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

/// A host that installed no Kytrix service connection cannot answer a reading, and
/// says so through the same typed failure every other failure uses.
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

// ── the body: rows, empty state, and the gauge canvas ─────────────────────────

/// A missing row is drawn as a dash rather than as a missing snapshot: the pane is
/// content, the gauge is drawn, and the six rows are in the tree with a dash where a
/// reading is absent.
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

/// Nothing collected yet is a healthy empty answer: the body says so, and no gauge is
/// drawn from a reading that does not exist.
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

/// The gauge is one canvas frame: the five bands as triangle meshes, the ten end caps
/// and the indicator as circles, and the two text lines as shaped glyph runs.
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
    assert_eq!(canvas.bounds(), bounds, "the canvas covers the node's box");
    assert!(
        bounds.size.width > 96.0 && bounds.size.height > 72.0,
        "the fixture box is large enough for a gauge: {bounds:?}"
    );

    assert_eq!(path_count(&canvas), 5, "one band per level of the scale");
    assert_eq!(quads(&canvas).len(), 12, "ten caps, the ring and the dot");
    assert!(
        glyph_run_count(&canvas) >= 2,
        "the score and the level's name are shaped into the canvas"
    );
    assert_eq!(
        harness.app().geometry().len(),
        5,
        "the five band meshes are registered in the geometry store"
    );

    for primitive in primitives(&canvas) {
        let drawn = match primitive {
            Primitive::Path(path) => path.bounds,
            Primitive::Quad(quad) => quad.bounds,
            _ => continue,
        };
        assert!(
            drawn.origin.x >= bounds.origin.x - 1.0
                && drawn.origin.y >= bounds.origin.y - 1.0
                && drawn.max().x <= bounds.max().x + 1.0
                && drawn.max().y <= bounds.max().y + 1.0,
            "every painted piece stays inside the canvas: {drawn:?} of {bounds:?}"
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

/// The reading walks across the half circle: a higher score puts the dot further
/// right, which is the one thing a test can read out of a painted frame.
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

/// The theme's fear-and-greed ramp is what colours the bands and the dot, and the
/// level's name is the default foreground.
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
    let theme = base_ui::theme::resolved::Theme::of(Appearance::Light);
    let bands: Vec<ui_core::paint::color::Color> = primitives(&canvas)
        .into_iter()
        .filter_map(|primitive| match primitive {
            Primitive::Path(path) => Some(path.fill),
            _ => None,
        })
        .collect();
    assert_eq!(
        bands,
        vec![
            theme.role(Role::FearGreedExtremeFear),
            theme.role(Role::FearGreedFear),
            theme.role(Role::FearGreedNeutral),
            theme.role(Role::FearGreedGreed),
            theme.role(Role::FearGreedExtremeGreed),
        ],
        "the bands are the scale's own roles"
    );
    let circles = quads(&canvas);
    let dot = circles.last().expect("the dot is drawn");
    let QuadPaint::Solid { fill, .. } = &dot.paint else {
        panic!("the dot is a solid quad");
    };
    assert_eq!(*fill, theme.role(Role::FgDefault));
}

/// A new reading redraws the values, not the tree: the body is built once and every
/// changing value is a binding on nodes that stay where they are.
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

/// A row that loses or gains its reading changes shape (no dot, no level name), so
/// the row nodes are replaced rather than re-bound.
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

/// A steady frame does no work: nothing is written, so no frame is asked for (I3).
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

// ── language, time zone, and the info notes ────────────────────────────────────

/// A level's name and the notes beside the headings are resolved on the frame that
/// draws them, and the gauge keeps its prepared text until that text, the reading or
/// the canvas changes — so a language change has to redraw the pane and rebuild that
/// prepared frame.
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

/// The stand-in's copy is a build-time string, so the language switch rebuilds it
/// (the same rule the surface follows for its notices).
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

/// A tooltip's content is a resolved sentence, so the language switch replaces it.
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

/// A row's date is the UTC calendar day the service generated, not a calendar value:
/// the display time zone must not move it to a day the service did not name.
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

// ── the header control ────────────────────────────────────────────────────────

/// The click sends a refresh command, not a reload: the pane stays `Ready`, keeps the
/// reading on screen and waits for the answer it asked for.
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

/// The control is drawn disabled while the first answer is outstanding, and the
/// action's entry guard is what keeps its click from reaching the service.
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

    // The kit control guards its own dispatch while it is drawn disabled, so the
    // click above cannot reach the pane's handler at all. The action's entry guard is
    // the pane's own defence for a dispatch that bypasses the control (an AT
    // activation, a programmatic call): it is asserted at the seam it protects.
    harness
        .with_cx(|cx| pane.widget.refresh(cx))
        .expect("a refused refresh is not an engine failure");
    assert!(
        !pane.widget.refresh_pending,
        "the action entry refuses a refresh the control is drawn disabled for"
    );
    assert_eq!(source.fetch_count(), 1);
}

// ── release, and two panes ────────────────────────────────────────────────────

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

/// Release takes everything the widget owns: the service bridge, the header
/// component's handler and scope, the tooltips, the canvas' registered meshes, and
/// its own nodes.
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

/// Taking the card down (an uncollected answer replacing a collected one) gives the
/// meshes back: they are not scoped to a window, so nothing else would.
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

/// Two panes in one application mint two header identities: the keys carry the
/// instance's signal scope, so each pane's click reaches its own widget.
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
