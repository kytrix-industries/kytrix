//! The pane's own behaviour: what it reports, what the header offers, what the row of
//! readings draws, and what it leaves behind when it goes.
//!
//! The contract layer is driven directly — `load`, `sync`, `body`, `header`, `release`
//! are the hooks the framework calls — over the engine's headless host, because these
//! tests are about this pane's decisions rather than about the framework's dispatch.

use std::{cell::Cell, rc::Rc};

use app_config::settings::{AccountConfig, AccountWalletKind, Config};
use base_ui::style_builder::StyleBuilder;
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use ui_core::{
    event::{Event, InputEvent},
    math::Point,
    node::{builder::NodeBuilder, id::NodeId},
};
use widget::{
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    test_support::WidgetTestApp,
    view::{HeaderGroup, WidgetView},
};

use super::*;
use crate::{
    model::CryptocurrencyEtfSnapshot,
    service::test_support::{DEFAULT_FUNDS, FakeSource, FetchPlan, asset, snapshot_of},
};

/// The account every fixture pane is scoped to.
const ACCOUNT: &str = "test-account";

/// The size the pane's body is laid out in, which is what the chart is measured from.
const PAGE: (f32, f32) = (640.0, 360.0);

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
    write_config(harness, config);
}

/// Signs the pane out the way the composition root does: a settings write.
fn sign_out(harness: &mut WidgetTestApp) {
    let mut config = current_config(harness);
    config.account = None;
    write_config(harness, config);
}

/// A wall clock the test owns, and how many times the pane read it.
///
/// A countdown's label is formatted against the wall clock, which is not the engine
/// clock [`WidgetTestApp::advance`] moves; the read count is what tells one timer chain
/// from two (two chains tick twice per round).
struct TestClock {
    now: Rc<Cell<i64>>,
    reads: Rc<Cell<usize>>,
}

impl TestClock {
    fn new(now: i64) -> Self {
        Self {
            now: Rc::new(Cell::new(now)),
            reads: Rc::new(Cell::new(0)),
        }
    }

    fn set(&self, now: i64) {
        self.now.set(now);
    }

    fn reads(&self) -> usize {
        self.reads.get()
    }

    /// The wall clock's current reading.
    fn at(&self) -> i64 {
        self.now.get()
    }

    fn now(&self) -> Rc<dyn Fn() -> i64> {
        let now = Rc::clone(&self.now);
        let reads = Rc::clone(&self.reads);
        Rc::new(move || {
            reads.set(reads.get() + 1);
            now.get()
        })
    }
}

/// A pane the framework would have built, driven by a fake source.
struct Pane {
    widget: CryptocurrencyEtfWidget,
    status: WidgetStatus,
    /// The box the body and the header are built into.
    page: NodeId,
    /// The group node a header row is built into.
    header_group: NodeId,
}

fn recovery() -> WidgetAction {
    WidgetAction::new("common.retry", |_| {})
}

fn pane_with_source(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_group = harness.with_cx(|cx| {
        let tree_page = page;
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_row().build())
            .build(cx.tree_mut(), tree_page)
            .expect("the header group builds")
    });
    let widget = harness
        .with_cx(|cx| CryptocurrencyEtfWidget::new_with_source(source, status, recovery(), cx));
    Pane {
        widget,
        status,
        page,
        header_group,
    }
}

/// A pane whose countdown reads a clock the test moves.
fn pane_at_clock(harness: &mut WidgetTestApp, clock: &TestClock) -> Pane {
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_group = harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_row().build())
            .build(cx.tree_mut(), page)
            .expect("the header group builds")
    });
    let widget = harness.with_cx(|cx| {
        CryptocurrencyEtfWidget::new_with_source_at_clock(
            FakeSource::new([FetchPlan::Pending]),
            clock.now(),
            status,
            recovery(),
            cx,
        )
    });
    Pane {
        widget,
        status,
        page,
        header_group,
    }
}

/// A pane with no service connection behind it.
fn pane_without_source(harness: &mut WidgetTestApp) -> Pane {
    let scope = harness.scope();
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let page = page(harness);
    let header_group = harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_row().build())
            .build(cx.tree_mut(), page)
            .expect("the header group builds")
    });
    let widget =
        harness.with_cx(|cx| CryptocurrencyEtfWidget::new_without_source(status, recovery(), cx));
    Pane {
        widget,
        status,
        page,
        header_group,
    }
}

/// A box of the pane's size, which is what the body is laid out in.
fn page(harness: &mut WidgetTestApp) -> NodeId {
    harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_ETF_PAGE")
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

fn pane(harness: &mut WidgetTestApp) -> Pane {
    sign_in(harness, ACCOUNT);
    pane_with_source(harness, FakeSource::new([FetchPlan::Pending]))
}

fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness.with_cx(|cx| pane.widget.load(cx));
}

fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
    harness
        .with_cx(|cx| pane.widget.sync(cx))
        .expect("the widget syncs");
}

/// Builds the body the framework would build once the pane has content, and lays it out.
fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.page;
    harness
        .with_cx(|cx| pane.widget.body(parent, cx))
        .expect("the body builds");
    harness.drain();
}

/// Builds the header right group, as a pane would.
fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.header_group;
    harness
        .with_cx(|cx| pane.widget.header(HeaderGroup::Right, parent, cx))
        .expect("the header builds");
    harness.drain();
}

/// One production-shaped frame: synchronization, then the pipeline.
fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    sync(harness, pane);
    harness.drain();
}

fn reported(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
    pane.status.state(harness.app())
}

fn key() -> CryptocurrencyEtfStreamKey {
    CryptocurrencyEtfStreamKey {
        account_id: SmolStr::new_static(ACCOUNT),
    }
}

/// Reports a status as the stream would.
fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: CryptocurrencyEtfStatus) {
    let message = Arc::new(CryptocurrencyEtfStreamMessage::Status(status));
    harness.with_cx(|cx| pane.widget.apply_reliable(key(), message, cx));
}

/// Reports an answer as the stream would.
fn report_snapshot(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    revision: u64,
    snapshot: Arc<CryptocurrencyEtfSnapshot>,
) {
    let message = Arc::new(CryptocurrencyEtfStreamMessage::Snapshot { revision, snapshot });
    harness.with_cx(|cx| pane.widget.apply_latest(key(), message, cx));
}

fn ready(revision: u64, next_refresh_at: i64) -> CryptocurrencyEtfStatus {
    CryptocurrencyEtfStatus::Ready {
        revision,
        next_refresh_at,
    }
}

fn failed() -> CryptocurrencyEtfStatus {
    CryptocurrencyEtfStatus::Failed {
        error: CryptocurrencyEtfFetchError::Service(
            kytrix_service_provider::service::engine::KytrixServiceError::Disconnected,
        ),
    }
}

fn refreshing() -> CryptocurrencyEtfStatus {
    CryptocurrencyEtfStatus::Refreshing {
        next_refresh_at: i64::MAX,
    }
}

/// An answer whose first asset has the funds a test names.
fn snapshot_with(funds: &[&str]) -> Arc<CryptocurrencyEtfSnapshot> {
    snapshot_of([
        asset(CryptocurrencyEtfAsset::Btc, &[1.0, 2.0], funds),
        asset(CryptocurrencyEtfAsset::Eth, &[1.0, 2.0], DEFAULT_FUNDS),
        asset(CryptocurrencyEtfAsset::Sol, &[1.0, 2.0], DEFAULT_FUNDS),
        asset(CryptocurrencyEtfAsset::Hyp, &[1.0, 2.0], DEFAULT_FUNDS),
    ])
}

/// Offers a header request the way a control's handler would, and applies it.
fn request(harness: &mut WidgetTestApp, pane: &mut Pane, request: HeaderRequest) {
    let signal = pane.widget.requests;
    harness.with_cx(|cx| {
        cx.write(&signal, Some(request));
    });
    frame(harness, pane);
}

/// The debug node a test selector names, inside the page.
fn node(harness: &WidgetTestApp, parent: NodeId, prefix: &str) -> NodeId {
    harness
        .find_by_debug_prefix(parent, prefix)
        .unwrap_or_else(|| panic!("`{prefix}` should be shown"))
}

// ── what the pane reports ──────────────────────────────────────────────────────────

#[test]
fn a_pane_without_an_account_should_ask_for_one() {
    let mut harness = harness();
    let mut pane = {
        sign_out(&mut harness);
        pane_with_source(&mut harness, FakeSource::new([FetchPlan::Pending]))
    };
    load(&mut harness, &mut pane);

    assert_eq!(
        reported(&harness, &pane),
        WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
        "a pane nobody is signed in for says what it is waiting for"
    );
    assert!(!pane.widget.has_content());
    assert!(
        !pane.widget.has_scope(),
        "there is nothing to read without an account"
    );
}

/// A pane nobody is signed in for waits in the language the interface shows: the notice
/// is a reference into the catalogue, resolved on the frame that draws it, so the pane
/// reports nothing again — no reload, and nothing asked of the service.
#[test]
fn a_language_change_should_resolve_the_signed_out_notice_where_it_is_drawn() {
    let mut harness = harness();
    sign_out(&mut harness);
    let mut pane = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Pending]));
    load(&mut harness, &mut pane);
    let english =
        product_translate(LanguageCode::En).translate("widget.cryptocurrency_etf.logged_out");

    let notice = reported(&harness, &pane)
        .notice()
        .cloned()
        .expect("a pane nobody signed in for waits");
    assert_eq!(
        notice,
        Notice::message("widget.cryptocurrency_etf.logged_out")
    );
    assert_eq!(
        notice.resolve(&product_translate(LanguageCode::En)),
        english,
        "the pane waits in the language it is being read in"
    );

    harness.switch_language(LanguageCode::ZhCn);
    let chinese =
        product_translate(LanguageCode::ZhCn).translate("widget.cryptocurrency_etf.logged_out");
    assert_ne!(chinese, english, "the two locales have to differ");
    assert_eq!(
        notice.resolve(&product_translate(LanguageCode::ZhCn)),
        chinese,
        "and the same reference resolves in the language showing now, without a reload"
    );
}

#[test]
fn an_account_should_be_what_enables_the_controls() {
    let mut harness = harness();
    sign_out(&mut harness);
    let mut signed_out = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Pending]));
    // The account is the pane's scope: a pane loaded while nobody is signed in reads
    // nothing, and it is only a later settings write that re-scopes it (see
    // `signing_out_should_take_the_answer_with_it`).
    load(&mut harness, &mut signed_out);
    sign_in(&mut harness, ACCOUNT);
    let mut signed_in = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Pending]));
    load(&mut harness, &mut signed_in);

    assert!(!signed_out.widget.has_scope());
    assert!(
        signed_out.widget.refresh_is_disabled(harness.app()),
        "there is nothing to read without an account"
    );
    assert!(signed_in.widget.has_scope());
    assert!(
        signed_in.widget.refresh_is_disabled(harness.app()),
        "a pane already waiting for its first answer has nothing to bring forward"
    );

    report_snapshot(
        &mut harness,
        &mut signed_in,
        1,
        snapshot_with(DEFAULT_FUNDS),
    );
    report_status(&mut harness, &mut signed_in, ready(1, i64::MAX));
    assert!(
        !signed_in.widget.refresh_is_disabled(harness.app()),
        "an idle pane with an answer can read again on demand"
    );
}

/// The double-click the refresh control exists to absorb.
///
/// A refresh is refused by the service while one is in flight, and the framework shows a
/// refusal as a failure — so a control that forwarded a second click inside that window
/// would answer a fast double click with a banner.
#[test]
fn a_refresh_should_not_be_forwarded_while_an_answer_is_being_read() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    assert!(!pane.widget.refresh_is_disabled(harness.app()));

    let signal = pane.widget.requests;
    harness.with_cx(|cx| {
        cx.write(&signal, Some(HeaderRequest::Refresh));
    });
    frame(&mut harness, &mut pane);
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "the click closes the window itself, before anything has answered"
    );

    report_status(&mut harness, &mut pane, refreshing());
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "a read in flight has nothing to bring forward"
    );

    report_status(&mut harness, &mut pane, ready(2, i64::MAX));
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "readiness without the answer it describes is not a reason to read again"
    );
    report_snapshot(&mut harness, &mut pane, 2, snapshot_with(DEFAULT_FUNDS));
    assert!(
        !pane.widget.refresh_is_disabled(harness.app()),
        "and with the answer in hand there is nothing left to wait for"
    );
}

/// A failure is the framework's to recover from, not the header's.
#[test]
fn a_failed_pane_should_offer_its_recovery_rather_than_a_refresh() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, failed());

    assert!(pane.status.is_failed(harness.app()));
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "the header has nothing to bring forward, so it offers nothing"
    );
    let failure = reported(&harness, &pane)
        .failure()
        .cloned()
        .expect("a failure should be published");
    assert_eq!(
        failure.action().label_key(),
        "common.retry",
        "and the framework's own recovery is the way out of it"
    );

    // Pressing the disabled control changes nothing: the request is refused by the
    // pane's own gate rather than by the widget's paint state.
    request(&mut harness, &mut pane, HeaderRequest::Refresh);
    assert!(
        pane.status.is_failed(harness.app()),
        "a disabled control changes nothing when it is pressed"
    );
}

#[test]
fn a_pane_with_a_snapshot_should_be_content_and_report_ready() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    assert!(
        !pane.widget.has_content(),
        "a pane is not content until it has an answer"
    );

    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    assert!(
        !pane.widget.has_content(),
        "readiness before the answer it describes is not content"
    );
    assert_eq!(reported(&harness, &pane), WidgetState::Loading);

    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    assert!(pane.widget.has_content());
    assert_eq!(reported(&harness, &pane), WidgetState::Ready);
}

#[test]
fn an_answer_arriving_before_its_readiness_should_still_be_drawn() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));

    assert!(pane.widget.has_content());
    assert_eq!(reported(&harness, &pane), WidgetState::Ready);
}

#[test]
fn readiness_for_a_newer_answer_should_not_claim_the_older_one() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, ready(2, i64::MAX));
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));

    assert_eq!(
        reported(&harness, &pane),
        WidgetState::Loading,
        "an answer older than the readiness that arrived is not what was reported ready"
    );
    assert!(!pane.status.is_failed(harness.app()));
}

#[test]
fn an_answer_the_pane_has_overtaken_should_be_ignored() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 2, snapshot_with(DEFAULT_FUNDS));
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));

    assert_eq!(
        pane.widget.accepted,
        Some(2),
        "an answer older than the one on screen would only make the chart jump back"
    );
}

#[test]
fn an_answer_from_another_account_should_be_ignored() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    let message = Arc::new(CryptocurrencyEtfStreamMessage::Snapshot {
        revision: 1,
        snapshot: snapshot_with(DEFAULT_FUNDS),
    });
    harness.with_cx(|cx| {
        pane.widget.apply_latest(
            CryptocurrencyEtfStreamKey {
                account_id: SmolStr::new_static("another-account"),
            },
            message,
            cx,
        );
    });

    assert!(
        !pane.widget.has_content(),
        "another account's answer is not this pane's"
    );
}

#[test]
fn a_failure_should_keep_the_answer_on_screen_and_offer_a_recovery() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, failed());

    assert!(pane.status.is_failed(harness.app()));
    let failure = reported(&harness, &pane)
        .failure()
        .cloned()
        .expect("a failure should be published");
    assert!(
        !failure
            .text(&product_translate(LanguageCode::En))
            .is_empty(),
        "a failure has to say what went wrong"
    );
    assert_eq!(failure.action().label_key(), "common.retry");
    assert!(
        pane.widget.has_content(),
        "the chart the user was reading stays on screen"
    );
    assert!(
        pane.widget.countdown_deadline(harness.app()).is_none(),
        "a failure is off the clock, so there is nothing to count down to"
    );
}

#[test]
fn a_failure_should_be_terminal_until_the_user_recovers() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_status(&mut harness, &mut pane, failed());
    assert!(pane.status.is_failed(harness.app()));

    report_status(&mut harness, &mut pane, CryptocurrencyEtfStatus::Connecting);
    report_snapshot(&mut harness, &mut pane, 7, snapshot_with(DEFAULT_FUNDS));
    assert!(
        pane.status.is_failed(harness.app()),
        "a failed pane takes no more statuses or answers"
    );

    // The recovery the failure offered is what starts it again.
    load(&mut harness, &mut pane);
    assert!(!pane.status.is_failed(harness.app()));
    assert_eq!(reported(&harness, &pane), WidgetState::Connecting);
}

#[test]
fn a_pane_without_a_service_connection_should_report_a_failure() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let mut pane = pane_without_source(&mut harness);
    load(&mut harness, &mut pane);

    assert!(
        pane.status.is_failed(harness.app()),
        "a host with no Kytrix service cannot read, and that is a failure"
    );
}

// ── the countdown ──────────────────────────────────────────────────────────────────

#[test]
fn the_countdown_should_only_run_while_the_pane_is_ready() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let clock = TestClock::new(1_000_000);
    let mut pane = pane_at_clock(&mut harness, &clock);
    build_header(&mut harness, &mut pane);
    load(&mut harness, &mut pane);

    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(
        &mut harness,
        &mut pane,
        ready(1, clock.at() + 8 * 60 * 60 * 1_000),
    );
    frame(&mut harness, &mut pane);

    let countdown = node(&harness, pane.header_group, "TEST_ETF_COUNTDOWN");
    assert_eq!(
        harness.text_of(countdown),
        "08:00:00",
        "a ready pane counts down to the deadline the service published"
    );
    assert!(!pane.widget.refresh_is_disabled(harness.app()));

    report_status(&mut harness, &mut pane, refreshing());
    frame(&mut harness, &mut pane);
    assert_eq!(
        harness.text_of(countdown),
        "",
        "a read in flight has no countdown to show"
    );
    assert!(
        pane.widget.refresh_is_disabled(harness.app()),
        "and nothing for the control to bring forward"
    );

    report_status(&mut harness, &mut pane, ready(2, clock.at() + 60_000));
    frame(&mut harness, &mut pane);
    assert_eq!(
        harness.text_of(countdown),
        "",
        "readiness without the answer it describes is not something to count down from"
    );

    report_snapshot(&mut harness, &mut pane, 2, snapshot_with(DEFAULT_FUNDS));
    frame(&mut harness, &mut pane);
    assert_eq!(
        harness.text_of(countdown),
        "01:00",
        "and with the answer in hand the countdown starts on the new deadline"
    );
    assert!(!pane.widget.refresh_is_disabled(harness.app()));
}

/// A failure takes the loop off the clock, so there is no countdown to show.
#[test]
fn a_failed_pane_should_stop_counting_down() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let clock = TestClock::new(1_000_000);
    let mut pane = pane_at_clock(&mut harness, &clock);
    build_header(&mut harness, &mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, clock.at() + 60_000));
    frame(&mut harness, &mut pane);
    let countdown = node(&harness, pane.header_group, "TEST_ETF_COUNTDOWN");
    assert_eq!(harness.text_of(countdown), "01:00");

    report_status(&mut harness, &mut pane, failed());
    frame(&mut harness, &mut pane);

    assert_eq!(
        harness.text_of(countdown),
        "",
        "a pane whose schedule stopped has nothing to count down to"
    );
}

/// The tick chain is one chain: each engine tick writes the label once and re-arms.
#[test]
fn the_countdown_should_tick_once_per_engine_round() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let clock = TestClock::new(1_000_000);
    let mut pane = pane_at_clock(&mut harness, &clock);
    build_header(&mut harness, &mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, clock.at() + 90_000));
    frame(&mut harness, &mut pane);
    let countdown = node(&harness, pane.header_group, "TEST_ETF_COUNTDOWN");
    assert_eq!(harness.text_of(countdown), "01:30");

    // Half a tick is not a tick: nothing is re-read and nothing is redrawn.
    assert!(!harness.advance(super::COUNTDOWN_TICK / 2));
    assert_eq!(harness.text_of(countdown), "01:30");

    // The clock moves a second and the tick that is due rewrites the label.
    clock.set(1_001_000);
    assert!(harness.advance(super::COUNTDOWN_TICK));
    assert_eq!(harness.text_of(countdown), "01:29");

    // A tick that does not move the label draws nothing (I3), but it is still exactly
    // one tick: one chain reads the clock once per round.
    let reads_after_first = clock.reads();
    assert!(!harness.advance(super::COUNTDOWN_TICK));
    assert_eq!(
        clock.reads(),
        reads_after_first + 1,
        "one chain ticks once per round, not twice"
    );
}

#[test]
fn releasing_a_pane_should_stop_its_countdown() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let clock = TestClock::new(1_000_000);
    let mut pane = pane_at_clock(&mut harness, &clock);
    build_header(&mut harness, &mut pane);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, clock.at() + 60_000));
    frame(&mut harness, &mut pane);
    harness.advance(super::COUNTDOWN_TICK);
    let ticking = clock.reads();
    assert!(ticking > 0, "a ready pane reads its clock on every tick");

    harness.with_cx(|cx| pane.widget.release(cx));
    let reads = clock.reads();
    for _ in 0..4 {
        harness.advance(super::COUNTDOWN_TICK);
    }
    assert_eq!(
        clock.reads(),
        reads,
        "a released pane has no tick left to read the clock from"
    );
}

// ── the row of readings ───────────────────────────────────────────────────────────

#[test]
fn the_body_should_draw_the_reading_under_the_pointer() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let date = node(&harness, pane.page, "TEST_ETF_DATE");
    let flow = node(&harness, pane.page, "TEST_ETF_FLOW");
    let net_assets = node(&harness, pane.page, "TEST_ETF_NET_ASSETS");
    assert!(harness.text_of(date).contains("2024-01-02"));
    assert!(harness.text_of(flow).contains("+$2.00M"));
    assert!(harness.text_of(net_assets).contains("$3.00M"));

    // The chart's canvas is drawn into the host node.
    let chart = node(&harness, pane.page, "TEST_ETF_CHART");
    assert!(
        harness.has_canvas(chart),
        "the pane's body draws the chart as a canvas node"
    );
}

#[test]
fn a_single_fund_should_take_the_net_assets_reading_away() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(&["AAA", "BBB"]));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert!(
        harness
            .find_by_debug_prefix(pane.page, "TEST_ETF_NET_ASSETS")
            .is_some(),
        "the whole complex has a level to report"
    );

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Fund(CryptocurrencyEtfFundSelection::Named(SmolStr::new_static(
            "AAA",
        ))),
    );
    assert!(pane.widget.has_content(), "another fund is the same answer");
    assert!(
        harness
            .find_by_debug_prefix(pane.page, "TEST_ETF_NET_ASSETS")
            .is_none(),
        "one fund has a flow but no assets of its own"
    );
    assert!(
        harness
            .text_of(node(&harness, pane.page, "TEST_ETF_FLOW"))
            .contains("AAA net inflow"),
        "and the row names the series it is reading"
    );
}

#[test]
fn a_name_should_follow_the_language_it_is_drawn_in() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(&["AAA", "BBB"]));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let flow = node(&harness, pane.page, "TEST_ETF_FLOW");
    assert!(harness.text_of(flow).contains("ETF net inflow"));

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    assert!(
        harness.text_of(flow).contains("ETF 净流入"),
        "the row is drawn in the language showing now: {}",
        harness.text_of(flow)
    );
}

/// The pane shows trading days, not instants: a display time zone moves a clock, and a
/// trading day is a date the sheet published.
#[test]
fn the_display_time_zone_should_not_move_a_trading_day() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let date = node(&harness, pane.page, "TEST_ETF_DATE");
    let utc = harness.text_of(date);

    let mut config = current_config(&harness);
    config.data_time_zone =
        app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
    assert!(write_config(&mut harness, config), "a settings change");
    frame(&mut harness, &mut pane);

    assert_eq!(
        harness.text_of(date),
        utc,
        "a trading day is the date the funds reported, not an instant to convert"
    );
}

// ── the chart's own views ─────────────────────────────────────────────────────────

#[test]
fn the_window_and_the_fund_should_redraw_without_reading_again() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Window(CryptocurrencyEtfWindow::Days90),
    );
    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Fund(CryptocurrencyEtfFundSelection::All),
    );

    assert_eq!(pane.widget.window, CryptocurrencyEtfWindow::Days90);
    assert!(
        pane.widget.has_content(),
        "another window is the same answer drawn differently"
    );
}

#[test]
fn a_fund_the_new_asset_does_not_have_should_fall_back_to_the_whole_complex() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(&["IBIT", "FBTC"]));

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Fund(CryptocurrencyEtfFundSelection::Named(SmolStr::new_static(
            "IBIT",
        ))),
    );
    assert_eq!(
        pane.widget.fund,
        CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT"))
    );

    request(
        &mut harness,
        &mut pane,
        HeaderRequest::Asset(CryptocurrencyEtfAsset::Eth),
    );

    assert_eq!(pane.widget.asset, CryptocurrencyEtfAsset::Eth);
    assert_eq!(
        pane.widget.fund,
        CryptocurrencyEtfFundSelection::All,
        "a fund belongs to one asset's complex, so a choice under BTC is not one under ETH"
    );
    assert!(pane.widget.has_content());
}

#[test]
fn another_account_should_take_the_answer_with_it() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    assert!(pane.widget.has_content());

    sign_in(&mut harness, "second-account");
    frame(&mut harness, &mut pane);

    assert!(
        !pane.widget.has_content(),
        "the answer was served to the account that just left"
    );
    assert_eq!(reported(&harness, &pane), WidgetState::Connecting);
    assert_eq!(
        pane.widget.asset,
        CryptocurrencyEtfAsset::Btc,
        "the view is kept"
    );
}

#[test]
fn signing_out_should_take_the_answer_with_it() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));

    sign_out(&mut harness);
    frame(&mut harness, &mut pane);

    assert!(!pane.widget.has_content());
    assert_eq!(
        reported(&harness, &pane),
        WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
        "and the pane asks for an account rather than reporting a failure"
    );
    assert!(!pane.widget.has_scope());
}

// ── the header ────────────────────────────────────────────────────────────────────

/// The whole header is one group, and the order of it is the order a reader narrows the
/// chart down: how fresh the answer is, which complex, which fund inside it, and how
/// much of its history.
#[test]
fn the_header_should_offer_its_controls_in_the_order_they_narrow_the_chart_down() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    build_header(&mut harness, &mut pane);

    let bounds = ["cryptocurrency_etf.refresh", "cryptocurrency_etf.asset"].map(|prefix| {
        let node = node(&harness, pane.header_group, prefix);
        harness.tree().get(node).expect("laid out").layout().bounds
    });
    assert!(
        bounds[0].origin.x + bounds[0].size.width <= bounds[1].origin.x,
        "the refresh runs before the asset picker, left to right: {bounds:?}"
    );
}

/// A pane nobody is signed in for draws the same controls, disabled rather than gone: a
/// header that reflowed when an account signed in would move the controls under the
/// user's hand.
#[test]
fn the_header_should_keep_its_controls_without_an_account() {
    let mut harness = harness();
    sign_out(&mut harness);
    let mut pane = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Pending]));
    build_header(&mut harness, &mut pane);

    for prefix in [
        "cryptocurrency_etf.refresh",
        "cryptocurrency_etf.asset",
        "cryptocurrency_etf.fund",
        "cryptocurrency_etf.window",
    ] {
        let node = node(&harness, pane.header_group, prefix);
        let bounds = harness.tree().get(node).expect("laid out").layout().bounds;
        assert!(
            bounds.size.width > 0.0,
            "every control is still drawn: {prefix}"
        );
    }
}

#[test]
fn a_language_change_should_rebuild_the_headers_copy() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    build_header(&mut harness, &mut pane);
    let window_label = node(&harness, pane.header_group, "cryptocurrency_etf.window");
    let english = harness.text_of(window_label);

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let rebuilt = node(&harness, pane.header_group, "cryptocurrency_etf.window");
    assert_ne!(
        harness.text_of(rebuilt),
        english,
        "a picker's copy is chosen when it is built, so a language switch rebuilds it"
    );
    assert!(
        pane.widget
            .header
            .as_ref()
            .is_some_and(|header| { header.language == LanguageCode::ZhCn }),
        "and the rebuilt row records the language it was built in"
    );
}

/// A rebuild releases the components it replaces: a header rebuilt on every language
/// switch must not leave one signal scope behind per rebuild.
#[test]
fn rebuilding_the_header_should_hand_its_components_back() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    build_header(&mut harness, &mut pane);
    harness.drain();
    let baseline = ui_core::test_support::diagnostics::signal_slot_stats(harness.app()).live;

    for _ in 0..3 {
        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);
        harness.switch_language(LanguageCode::En);
        frame(&mut harness, &mut pane);
    }

    let after = ui_core::test_support::diagnostics::signal_slot_stats(harness.app()).live;
    assert_eq!(
        after, baseline,
        "six rebuilds must hand every replaced component's scope back ({baseline} → {after})"
    );
}

// ── two panes ────────────────────────────────────────────────────────────────────

/// Two panes of this widget in one application: the header's controls are application
/// level actions, so the second pane must not take the first one's handler away.
#[test]
fn two_panes_should_keep_their_own_header_handlers() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    // Both panes are mounted through the framework, which is what mints one signal
    // scope per instance — and therefore one component key per instance.
    let first = harness.mount::<CryptocurrencyEtfWidget>(CryptocurrencyEtfConfig);
    let second = harness.mount::<CryptocurrencyEtfWidget>(CryptocurrencyEtfConfig);
    let first_group = page(&mut harness);
    let second_group = page(&mut harness);
    first
        .header(harness.app_mut(), HeaderGroup::Right, first_group)
        .expect("the first pane builds its header");
    second
        .header(harness.app_mut(), HeaderGroup::Right, second_group)
        .expect("the second pane builds its header");

    let key_of = |scope: ui_core::node::signal::SignalScope| {
        format_smolstr!("{}{}{}", super::REFRESH_KEY_SUFFIX, "@", scope.index())
    };
    let first_key = key_of(first.scope());
    let second_key = key_of(second.scope());
    assert_ne!(
        first_key, second_key,
        "two panes never mint the same component key"
    );

    let first_action = ui_core::event::Action::Custom(format_smolstr!("{first_key}.click"));
    let second_action = ui_core::event::Action::Custom(format_smolstr!("{second_key}.click"));
    assert!(harness.app().has_action_handler(&first_action));
    assert!(harness.app().has_action_handler(&second_action));

    first.release(harness.app_mut());
    assert!(
        !harness.app().has_action_handler(&first_action),
        "the released pane takes its own handler down"
    );
    assert!(
        harness.app().has_action_handler(&second_action),
        "and leaves the other pane's handler alone"
    );
}

// ── the canvas and the pointer ────────────────────────────────────────────────────

/// The pointer follows the reading under it, and only a *reading* that moved is a frame.
#[test]
fn a_pointer_move_should_redraw_the_row_and_the_chart() {
    let mut harness = harness();
    let mut pane = pane(&mut harness);
    load(&mut harness, &mut pane);
    report_snapshot(&mut harness, &mut pane, 1, snapshot_with(DEFAULT_FUNDS));
    report_status(&mut harness, &mut pane, ready(1, i64::MAX));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let chart = node(&harness, pane.page, "TEST_ETF_CHART");
    let plot = harness
        .tree()
        .get(chart)
        .expect("the chart is laid out")
        .layout()
        .bounds;
    let position = Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    );
    let day = node(&harness, pane.page, "TEST_ETF_DATE");
    let before = harness.text_of(day);

    let consumed = harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerMove {
                position,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    });
    assert!(consumed, "the pane took the pointer move");
    frame(&mut harness, &mut pane);

    assert_ne!(
        harness.text_of(day),
        before,
        "the row of readings follows the pointer"
    );

    let changed = harness.with_cx(|cx| {
        pane.widget.dispatch(
            &Event::Input(InputEvent::PointerLeave {
                position,
                modifiers: ui_core::event::Modifiers::default(),
                timestamp: std::time::Instant::now(),
            }),
            cx,
        )
    });
    assert!(changed, "a pointer that left is the pane's to notice too");
}

// ── pure helpers ─────────────────────────────────────────────────────────────────

#[test]
fn a_window_should_round_trip_through_the_id_its_picker_uses() {
    for window in CryptocurrencyEtfWindow::ALL {
        assert_eq!(window_from_id(window_id(window)), Some(window));
    }
    assert_eq!(window_from_id("not a window"), None);
}

#[test]
fn a_fund_choice_should_round_trip_through_the_id_its_picker_uses() {
    let all = CryptocurrencyEtfFundSelection::All;
    let named = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT"));

    assert_eq!(fund_from_id(&fund_id(&all)), all);
    assert_eq!(fund_from_id(&fund_id(&named)), named);
    assert_ne!(fund_id(&all), fund_id(&named));
    assert_eq!(
        fund_from_id(ALL_FUNDS_ID),
        CryptocurrencyEtfFundSelection::All,
        "the picker's own id for the whole complex is what it opens with"
    );
}

/// The pane persists nothing, so a fresh one opens on the deepest complex, its most
/// recent month and every fund in it.
#[test]
fn a_fresh_pane_should_open_on_the_deepest_complex_its_recent_month_and_every_fund() {
    let mut harness = harness();
    let pane = pane(&mut harness);

    assert_eq!(pane.widget.asset, CryptocurrencyEtfAsset::Btc);
    assert_eq!(pane.widget.window, CryptocurrencyEtfWindow::Days30);
    assert_eq!(pane.widget.fund, CryptocurrencyEtfFundSelection::All);
    assert_eq!(
        <CryptocurrencyEtfConfig as app_config::widget::WidgetConfig>::SCHEMA_VERSION,
        app_config::widget::WidgetConfigSchemaVersion::V1
    );
    assert_eq!(
        CryptocurrencyEtfWidget::ID.as_str(),
        "kytrix.cryptocurrency_etf"
    );
    assert_eq!(
        pane.widget.config(),
        None,
        "this pane persists no options at all"
    );
}
