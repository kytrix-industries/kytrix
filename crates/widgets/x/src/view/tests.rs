//! Tests for the X pane: the state it reports, the rows it draws and the pictures it
//! registers.
//!
//! (The migrated suite is written in place of the old element-tree tests; the
//! per-test ledger lives in `.docs/gui-migration/test-ledger/w9-x.md`.)

use std::{path::PathBuf, rc::Rc, time::Instant};

use app_config::{
    settings::{AccountConfig, AccountWalletKind, Config},
    time_zone::DataTimeZone,
    widget::{VersionedWidgetConfig, decode_or_default},
};
use base_ui::kit::button::Button;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::{
        KytrixServiceError, TweetReference, XProfileEvent, XTweetEvent, XTweetMedia,
        XTweetTextSegment,
    },
    x::{XApiError, XMutationOutcome, XMutationUnknown, XProfile, XSelection, XSelectionsResponse},
};
use network::transport::error::TransportError;
use ui_core::{
    event::{ActionSource, Modifiers},
    math::Point,
    node::{signal::SignalScope, tree::NodeKind},
};
use url::Url;
use widget::test_support::WidgetTestApp;

use super::*;
use crate::{
    media::{MediaCache, MediaDownloader},
    model::TweetState,
    test_support::{
        ACCOUNT, FakeFeed, FakeSession, FakeSource, FakeSubscriptionControl, FakeTransport,
        media_cache, search_profile, seed_picture, selection, selections,
    },
};

/// The account every fixture of this test module is *not* read as.
const OTHER_ACCOUNT: &str = "someone-else";

/// How many runtime / frame turns a settle loop may take before it calls the test a
/// failure.
const SETTLE_TURNS: usize = 64;

/// The translator the product ships, in `language`.
fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// The framework's headless host, over the product catalogue.
fn harness() -> WidgetTestApp {
    WidgetTestApp::with_translate(translate(LanguageCode::En))
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

fn with_cx_of<R>(
    harness: &mut WidgetTestApp,
    scope: SignalScope,
    body: impl FnOnce(&mut WidgetCx<'_>) -> R,
) -> R {
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
    body(&mut cx)
}

/// A pane the framework would have built, over a source the test answers.
struct Pane {
    widget: XWidget,
    status: WidgetStatus,
    parent: NodeId,
    scope: SignalScope,
    cache: MediaCache,
    _cache_root: tempfile::TempDir,
}

/// A pane over `source` in the scope of the caller's choosing.
fn pane_in_scope(harness: &mut WidgetTestApp, source: FakeSource, scope: SignalScope) -> Pane {
    let (cache, cache_root) = media_cache();
    let downloader = MediaDownloader::with_cache(FakeTransport::failing(), cache.clone());
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    // The body has to be built under a parent with a size: the framework's slot is the
    // whole pane, and a row that is laid out against nothing draws nothing.
    let root = harness.root();
    let parent = harness.with_cx(|cx| {
        NodeBuilder::boxed()
            .id("TEST_TWEET_HOST")
            .style(StyleBuilder::new().flex_col().size_full().build())
            .build(cx.tree_mut(), root)
            .expect("the host exists")
    });
    let status = WidgetStatus::new(harness.app_mut(), scope);
    let widget = {
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        XWidget::new_with_source(
            source,
            downloader,
            status,
            WidgetAction::new("common.retry", |_| {}),
            &mut cx,
        )
    };

    Pane {
        widget,
        status,
        parent,
        scope,
        cache,
        _cache_root: cache_root,
    }
}

fn pane_with(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    let scope = harness.scope();
    pane_in_scope(harness, source, scope)
}

/// A pane reading an account with the service it was given, as `load` would start it.
fn pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    sign_in(harness, ACCOUNT);
    pane_with(harness, source)
}

/// A pane nobody is signed in for.
fn signed_out_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    pane_with(harness, source)
}

/// A pane no message of which arrives from the binding: the tests below report
/// statuses and feeds by hand, and a live service would report its own.
fn quiet_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
    let mut pane = pane(harness, source);
    cancel_binding(&mut pane);
    pane
}

fn cancel_binding(pane: &mut Pane) {
    if let Some(binding) = pane.widget.binding.as_mut() {
        binding.cancel();
    }
}

/// A pane reading an account whose stream is live, with what history published for it.
///
/// The subscription's control is returned with the pane: what keeps a fake subscription
/// fed is the sender the control holds, so it has to outlive the call.
fn live_pane(
    harness: &mut WidgetTestApp,
    source: &FakeSource,
    session: &FakeSession,
    tweets: Vec<XTweetEvent>,
    profiles: Vec<XProfileEvent>,
) -> (Pane, FakeSubscriptionControl) {
    session.answer_history(tweets, profiles);
    let mut pane = pane(harness, source.clone());
    let subscription = session.subscribe();
    load(harness, &mut pane);
    settle(harness, &mut pane, "the timeline to arrive", |_, pane| {
        pane.widget.feed.is_some()
    });
    (pane, subscription)
}

/// A quiet pane whose stream is live and whose monitored accounts are known.
fn ready_pane(harness: &mut WidgetTestApp) -> Pane {
    let mut pane = quiet_pane(harness, FakeSource::default());
    pane.widget.ready = true;
    pane.widget.selections = Some(selection_data(vec![selection("7", "seven")], 3));
    pane
}

fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
    with_cx_of(harness, pane.scope, |cx| pane.widget.load(cx));
}

fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
    with_cx_of(harness, pane.scope, |cx| pane.widget.sync(cx)).expect("the widget syncs");
}

/// Builds the body the framework would build once the pane has content.
fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let parent = pane.parent;
    with_cx_of(harness, pane.scope, |cx| pane.widget.body(parent, cx)).expect("the body builds");
}

fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
    sync(harness, pane);
    harness.drain();
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

/// Reports a feed as the stream would, with the binding taken out of the way.
fn report_update(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    revision: u64,
    feed: TweetSnapshot,
    media: MediaStates,
) -> bool {
    let scope = pane
        .widget
        .current_scope()
        .expect("the pane follows an account");
    cancel_binding(pane);
    let update = TweetUpdate::published(revision, feed, media);
    with_cx_of(harness, pane.scope, |cx| {
        pane.widget
            .apply_latest(scope, Arc::new(TweetStreamMessage::Update(update)), cx)
    })
}

fn report_feed(
    harness: &mut WidgetTestApp,
    pane: &mut Pane,
    revision: u64,
    feed: TweetSnapshot,
) -> bool {
    report_update(harness, pane, revision, feed, MediaStates::default())
}

/// Reports a status as the stream would.
fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: TweetStatus) -> bool {
    let scope = pane
        .widget
        .current_scope()
        .expect("the pane follows an account");
    cancel_binding(pane);
    with_cx_of(harness, pane.scope, |cx| {
        pane.widget
            .apply_reliable(scope, Arc::new(TweetStreamMessage::Status(status)), cx)
    })
}

fn state_of(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
    pane.status.state(harness.app())
}

fn notice_of(harness: &WidgetTestApp, pane: &Pane) -> Option<Notice> {
    state_of(harness, pane).notice().cloned()
}

fn failure_text(harness: &WidgetTestApp, pane: &Pane) -> String {
    state_of(harness, pane)
        .failure()
        .expect("the pane reported a failure")
        .text(&harness.services().translate)
        .to_string()
}

fn recovery_label(harness: &WidgetTestApp, pane: &Pane) -> String {
    state_of(harness, pane)
        .failure()
        .expect("the pane reported a failure")
        .action()
        .label(&harness.services().translate)
        .to_string()
}

fn feed_ids(pane: &Pane) -> Vec<String> {
    pane.widget.feed.as_ref().map_or_else(Vec::new, |feed| {
        feed.items()
            .iter()
            .map(|item| item.tweet_id().to_owned())
            .collect()
    })
}

fn usernames_of(pane: &Pane) -> Vec<String> {
    pane.widget
        .selections
        .as_ref()
        .map_or_else(Vec::new, |data| {
            data.selections()
                .iter()
                .map(|selection| {
                    selection.profile.as_ref().map_or_else(
                        || selection.x_user_id.to_string(),
                        |profile| profile.username.to_string(),
                    )
                })
                .collect()
        })
}

fn body_root(pane: &Pane) -> NodeId {
    pane.widget.body_root.expect("the body was built")
}

fn body_text(harness: &WidgetTestApp, pane: &Pane) -> String {
    harness.text_of(body_root(pane))
}

fn row_count(harness: &WidgetTestApp, pane: &Pane) -> usize {
    pane.widget
        .list
        .as_ref()
        .map_or(0, |list| list.count(harness.app()))
}

fn mounted_rows(harness: &WidgetTestApp, pane: &Pane) -> Vec<NodeId> {
    pane.widget.list.as_ref().map_or_else(Vec::new, |list| {
        list.mounted_rows(harness.app())
            .into_iter()
            .map(|(_, node)| node)
            .collect()
    })
}

/// Whether the subtree under `root` holds an image node.
fn has_image_node(harness: &WidgetTestApp, root: NodeId) -> bool {
    let tree = harness.tree();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let target = tree.get(node).expect("the node exists");
        if matches!(target.kind(), NodeKind::Image(_)) {
            return true;
        }
        stack.extend(target.children().iter().copied());
    }
    false
}

/// The widest image node under `root`, which is the picture itself rather than one of the
/// small placeholder marks a tile draws when it has none.
fn largest_image_node(harness: &WidgetTestApp, root: NodeId) -> Option<NodeId> {
    let tree = harness.tree();
    let mut stack = vec![root];
    let mut best: Option<(NodeId, f32)> = None;
    while let Some(node) = stack.pop() {
        let target = tree.get(node).expect("the node exists");
        if matches!(target.kind(), NodeKind::Image(_)) {
            let width = target.layout().bounds.size.width;
            if best.is_none_or(|(_, best)| width > best) {
                best = Some((node, width));
            }
        }
        stack.extend(target.children().iter().copied());
    }
    best.map(|(node, _)| node)
}

/// Dispatches the click a kit control's action identity names.
fn click_button(harness: &mut WidgetTestApp, button: &Button) {
    let node = button.node().expect("the control is built");
    let action = button
        .action_identity()
        .expect("the control registered a click handler");
    let window = harness.window();
    assert!(
        harness
            .app_mut()
            .dispatch_action(window, node, action, ActionSource::Programmatic),
        "the control is registered"
    );
}

/// Builds the header control the pane would build into the window's header row.
fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
    let root = harness.root();
    with_cx_of(harness, pane.scope, |cx| {
        pane.widget.header(HeaderGroup::Left, root, cx)
    })
    .expect("the header control builds");
}

/// Sends a primary-button pointer-up at `position` through the pane's own dispatch.
fn click_at(harness: &mut WidgetTestApp, pane: &mut Pane, position: Point) -> bool {
    let event = Event::Input(InputEvent::PointerUp {
        position,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
        timestamp: Instant::now(),
    });
    with_cx_of(harness, pane.scope, |cx| pane.widget.dispatch(&event, cx))
}

/// Clicks the centre of `node`, as a reader would.
fn click_node(harness: &mut WidgetTestApp, pane: &mut Pane, node: NodeId) -> bool {
    let bounds = harness
        .tree()
        .get(node)
        .expect("the node is alive")
        .layout()
        .bounds;
    let centre = Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    click_at(harness, pane, centre)
}

/// A picture's URL on X's media host.
fn picture_url(name: &str) -> Url {
    Url::parse(&format!("https://pbs.twimg.com/media/{name}.jpg")).expect("a picture URL")
}

/// Puts one finished picture on disk, so a row can draw it without fetching it.
fn land_picture(cache: &MediaCache, url: &Url) -> PathBuf {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the seeding runtime builds");
    runtime.block_on(seed_picture(cache, url, b"picture"))
}

fn media_states(entries: &[(&Url, MediaState)]) -> MediaStates {
    let mut states = HashMap::with_capacity(entries.len());
    for (url, state) in entries {
        states.insert(SmolStr::new(url.as_str()), state.clone());
    }
    Arc::new(states)
}

fn selection_data(entries: Vec<XSelection>, limit: usize) -> SelectionData {
    SelectionData::new(XSelectionsResponse {
        selections: entries,
        x_account_limit: limit,
    })
}

/// One account X was asked about, still waiting for its profile: the state a row is
/// named by what the pane remembers about it.
fn selection_without_profile(x_user_id: &str) -> XSelection {
    XSelection {
        x_user_id: x_user_id.into(),
        status: kytrix_service_provider::service::x::XSelectionStatus::Active,
        created_at: 1,
        updated_at: 2,
        profile: None,
    }
}

fn profile_event(x_user_id: &str, username: &str) -> XProfileEvent {
    XProfileEvent {
        x_user_id: x_user_id.into(),
        username: username.into(),
        display_name: format_smolstr!("{username} display"),
        avatar_url: format_smolstr!("https://pbs.twimg.com/{username}.jpg"),
        verified: true,
        revision: 1,
    }
}

fn tweet_event(tweet_id: &str, timestamp: i64, x_user_id: &str, text: &str) -> XTweetEvent {
    XTweetEvent {
        tweet_id: tweet_id.into(),
        timestamp,
        x_user_id: x_user_id.into(),
        text_segments: vec![XTweetTextSegment {
            text: text.into(),
            url: None,
        }],
        reference: None,
        media: Vec::new(),
    }
}

fn media_tweet(tweet_id: &str, timestamp: i64, url: &Url) -> XTweetEvent {
    let mut event = tweet_event(tweet_id, timestamp, "42", "with a picture");
    event.media = vec![XTweetMedia::Image {
        image_url: SmolStr::new(url.as_str()),
        width: 100,
        height: 100,
    }];
    event
}

/// A feed a test grows the way a subscription does.
///
/// The same state is kept between snapshots, so a later snapshot shares the tweets it
/// inherited from the one before it — which is what makes a report an edit to the list
/// rather than a new set of rows.
struct Feed {
    state: TweetState,
}

impl Feed {
    fn new() -> Self {
        Self {
            state: TweetState::new(),
        }
    }

    fn profile(&mut self, profile: XProfileEvent) {
        let _ = self.state.upsert_profile(profile);
    }

    fn push(&mut self, tweet: XTweetEvent) -> TweetSnapshot {
        let _ = self.state.push(tweet);
        self.state.snapshot()
    }

    fn snapshot(&mut self) -> TweetSnapshot {
        self.state.snapshot()
    }
}

/// The timestamp a fixture five minutes old is stamped with.
fn five_minutes_ago() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp() * 1_000 - 5 * 60 * 1_000
}

#[test]
fn the_widget_should_have_a_stable_identity() {
    assert_eq!(XWidget::ID.as_str(), "kytrix.x");
    assert_eq!(XWidget::DISPLAY_NAME, "widget.names.x");
    assert_eq!(XWidget::CATEGORY, WidgetCategory::Monitoring);
    assert_eq!(XWidget::DESCRIPTION, Some("widget.descriptions.x"));
    assert_eq!(XWidget::ICON, Some("widgets/x.png"));
    assert_eq!(
        <XConfig as WidgetConfig>::SCHEMA_VERSION,
        WidgetConfigSchemaVersion::V1
    );
}

#[test]
fn the_empty_config_should_round_trip_through_its_envelope() {
    let stored = VersionedWidgetConfig::encode(&XConfig).expect("an X config should encode");

    assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
    assert_eq!(stored.decode::<XConfig>(), XConfig);
    assert_eq!(decode_or_default::<XConfig>(None), XConfig);
}

/// What a pane follows is the account's own list of monitored accounts, so the pane has
/// nothing of its own to persist: a config it returned would make every save write an
/// empty envelope into the layout (AGENTS §4.3).
#[test]
fn a_pane_should_persist_no_config() {
    let mut harness = harness();
    let pane = pane(&mut harness, FakeSource::default());

    assert!(
        pane.widget.config().is_none(),
        "a pane with no options writes no envelope"
    );
}

/// The i18n keys one source file names, as the catalogue literals it reads.
///
/// Scanning the sources rather than keeping a hand-written list is what keeps the guard
/// from going stale: the dialog's keys fell out of the list once already, and a missing
/// key is an empty string on screen rather than an error.
fn keys_in(source: &'static str) -> Vec<&'static str> {
    let bytes = source.as_bytes();
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let Some(length) = bytes[start..].iter().position(|byte| *byte == b'"') else {
            break;
        };
        let end = start + length;
        let literal = &source[start..end];
        let is_key = (literal.starts_with("widget.") || literal.starts_with("common."))
            && literal
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_');
        if is_key && !keys.contains(&literal) {
            keys.push(literal);
        }
        index = end + 1;
    }
    keys
}

/// A missing key is an empty string on screen rather than an error, so a gap in the
/// interface is what a typo looks like: this is the test that turns it into a failure.
///
/// Every key the crate's sources name must resolve to non-empty copy in **both** locales
/// the pane ships, so a key added to one catalogue and not the other (or renamed in the
/// source and not in the catalogues) fails here rather than showing a blank.
#[test]
fn every_message_this_pane_shows_should_resolve_to_text() {
    let mut keys: Vec<&'static str> = Vec::new();
    for source in [
        include_str!("../view.rs"),
        include_str!("../dialog.rs"),
        include_str!("../model.rs"),
        include_str!("../service.rs"),
        include_str!("../media.rs"),
    ] {
        for key in keys_in(source) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys.sort_unstable();

    // The floor is the scan's own failure mode: a parser that stops finding keys would
    // otherwise make this test pass without checking anything.
    assert!(
        keys.len() >= 40,
        "the guard must cover the pane's whole vocabulary: {keys:?}"
    );
    for language in [LanguageCode::En, LanguageCode::ZhCn] {
        let translate = translate(language);
        for key in &keys {
            assert!(
                !t_smol!(translate, key).is_empty(),
                "{key} has no copy in {language:?}"
            );
        }
    }

    let translate = translate(LanguageCode::En);
    for status in [
        XSelectionStatus::Active,
        XSelectionStatus::Paused,
        XSelectionStatus::Pending,
    ] {
        assert!(
            !status_label(status, &translate).is_empty(),
            "{status:?} has no copy"
        );
    }

    for failure in [
        SelectionFailure::Rejected(XApiError::Authentication),
        SelectionFailure::Unknown(XMutationUnknown::Timeout),
        SelectionFailure::Rpc(RpcCallError::DeadlineExceeded {
            request_id: RequestId::new(2),
        }),
    ] {
        assert!(
            !failure.translated_message(&translate).is_empty(),
            "{failure:?} has no copy"
        );
    }

    for failure in [
        XFailure::Stream(TweetStreamError::Service(KytrixServiceError::Closed)),
        XFailure::Stream(TweetStreamError::Transport(TransportError::Closed)),
        XFailure::Rpc(RpcCallError::ServiceClosed {
            request_id: RequestId::new(3),
        }),
    ] {
        assert!(
            !failure.translated_message(&translate).is_empty(),
            "{failure:?} has no copy"
        );
    }
}

#[test]
fn a_signed_out_pane_should_wait_and_follow_nothing() {
    let mut harness = harness();
    let source = FakeSource::default();
    let mut pane = signed_out_pane(&mut harness, source.clone());

    load(&mut harness, &mut pane);

    assert!(!pane.widget.has_content());
    assert_eq!(pane.widget.current_scope(), None);
    assert_eq!(
        notice_of(&harness, &pane),
        Some(Notice::message("widget.x.logged_out")),
        "a pane nobody signed in for is not a failure"
    );
    assert!(!state_of(&harness, &pane).is_failed());
    assert!(
        source.requested().is_empty(),
        "a sign-out takes the subscription with it: nothing is followed"
    );
}

#[test]
fn a_pane_should_report_connecting_when_it_starts() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());

    load(&mut harness, &mut pane);

    assert_eq!(pane.widget.account_id.as_deref(), Some(ACCOUNT));
    assert_eq!(
        state_of(&harness, &pane),
        WidgetState::Connecting,
        "a pane that has asked for its stream is connecting"
    );
}

#[test]
fn a_pane_whose_account_could_not_be_taken_should_fail_and_offer_a_way_out() {
    let mut harness = harness();
    let source = FakeSource::default();
    source.refuse(KytrixServiceError::Authentication(SmolStr::new_static(
        "the test credential is gone",
    )));
    let mut pane = pane(&mut harness, source);

    load(&mut harness, &mut pane);
    settle(
        &mut harness,
        &mut pane,
        "the failure to be reported",
        |harness, pane| pane.status.is_failed(harness.app()),
    );

    let state = state_of(&harness, &pane);
    assert!(state.is_failed());
    assert_eq!(recovery_label(&harness, &pane), "Retry");
    let expected = XFailure::Rpc(RpcCallError::Service {
        request_id: RequestId::new(1),
        source: XApiError::Authentication,
    })
    .translated_message(&harness.services().translate);
    assert_eq!(failure_text(&harness, &pane), expected.to_string());
}

#[test]
fn a_pane_should_be_content_only_once_a_timeline_arrives() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    let (mut pane, _subscription) = live_pane(
        &mut harness,
        &source,
        &session,
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    settle(
        &mut harness,
        &mut pane,
        "the stream to report ready",
        |harness, pane| pane.status.state(harness.app()) == WidgetState::Ready,
    );

    assert!(pane.widget.has_content());
    assert_eq!(feed_ids(&pane), ["1"]);
    assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
}

/// An empty snapshot is content (AGENTS §6): the pane has a feed, it just has nothing
/// in it yet, and the body says so rather than calling the pane waiting.
#[test]
fn an_empty_timeline_should_be_content() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());

    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    assert!(report_feed(
        &mut harness,
        &mut pane,
        1,
        Feed::new().snapshot()
    ));

    assert!(pane.widget.has_content(), "an empty feed is still a feed");
    assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
    assert!(
        notice_of(&harness, &pane).is_none(),
        "an empty timeline is not a pane waiting"
    );
}

#[test]
fn a_failure_should_keep_the_timeline_and_offer_a_way_out() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    report_status(&mut harness, &mut pane, TweetStatus::Ready);

    let error = TweetStreamError::Transport(TransportError::Closed);
    report_status(&mut harness, &mut pane, TweetStatus::Failed(error.clone()));

    assert!(state_of(&harness, &pane).is_failed());
    assert_eq!(
        failure_text(&harness, &pane),
        XFailure::Stream(error)
            .translated_message(&harness.services().translate)
            .to_string()
    );
    assert_eq!(recovery_label(&harness, &pane), "Retry");
    assert_eq!(
        feed_ids(&pane),
        ["1"],
        "timeline a reader was looking at is not dropped by a failure"
    );
    assert!(pane.widget.has_content());
}

#[test]
fn a_message_after_a_failure_should_not_be_applied() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    report_status(
        &mut harness,
        &mut pane,
        TweetStatus::Failed(TweetStreamError::Service(KytrixServiceError::Closed)),
    );

    let mut later = Feed::new();
    later.profile(profile_event("42", "kytrix"));
    assert!(!report_feed(
        &mut harness,
        &mut pane,
        9,
        later.push(tweet_event("9", 1_700_000_100_000, "42", "tweet 9")),
    ));
    assert_eq!(feed_ids(&pane), ["1"]);
}

#[test]
fn a_status_after_a_failure_should_be_refused() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(
        &mut harness,
        &mut pane,
        TweetStatus::Failed(TweetStreamError::Transport(TransportError::Closed)),
    );

    report_status(&mut harness, &mut pane, TweetStatus::Ready);

    assert!(
        state_of(&harness, &pane).is_failed(),
        "a failure is terminal until the user recovers it"
    );
}

/// The latest channel coalesces, so a report can arrive more than once: a report that
/// says nothing new writes nothing and asks for no frame (I3).
#[test]
fn a_duplicate_timeline_should_not_notify() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1"));
    report_feed(&mut harness, &mut pane, 1, snapshot.clone());
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let requests = harness.frame_requests();

    assert!(!report_feed(&mut harness, &mut pane, 1, snapshot));
    frame(&mut harness, &mut pane);

    assert_eq!(
        harness.frame_requests(),
        requests,
        "the same timeline again asks for no frame"
    );
    assert_eq!(feed_ids(&pane), ["1"]);
}

#[test]
fn a_recovery_should_keep_the_timeline_and_an_account_change_should_drop_it() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );

    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.start(true, cx))
        .expect("a recovery starts again");
    assert!(
        pane.widget.has_content(),
        "a recovery keeps what a reader was looking at"
    );
    assert_eq!(state_of(&harness, &pane), WidgetState::Connecting);

    sign_in(&mut harness, OTHER_ACCOUNT);
    sync(&mut harness, &mut pane);

    assert!(
        !pane.widget.has_content(),
        "another account is another timeline"
    );
    assert!(pane.widget.feed.is_none());
    assert_eq!(pane.widget.account_id.as_deref(), Some(OTHER_ACCOUNT));
}

#[test]
fn an_account_change_should_start_the_pane_over_on_the_new_account() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    let (mut pane, _subscription) = live_pane(
        &mut harness,
        &source,
        &session,
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    assert_eq!(feed_ids(&pane), ["1"]);
    let other = source.session(OTHER_ACCOUNT);
    let _other_control = other.subscribe();

    sign_in(&mut harness, OTHER_ACCOUNT);
    settle(
        &mut harness,
        &mut pane,
        "the pane to follow the new account",
        |_, pane| {
            pane.widget.account_id.as_deref() == Some(OTHER_ACCOUNT)
                && source.requested().len() == 2
        },
    );

    assert_eq!(
        source.requested(),
        [
            SmolStr::new_static(ACCOUNT),
            SmolStr::new_static(OTHER_ACCOUNT),
        ],
        "the session the pane opened is the new account's"
    );
    assert!(
        feed_ids(&pane).is_empty(),
        "another account is another timeline, so the one it left is dropped"
    );
    assert!(!pane.widget.has_content());
}

#[test]
fn signing_out_should_take_the_feed_with_it() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    assert!(pane.widget.has_content());

    sign_out(&mut harness);
    sync(&mut harness, &mut pane);

    assert!(!pane.widget.has_content());
    assert!(pane.widget.feed.is_none());
    assert_eq!(
        notice_of(&harness, &pane),
        Some(Notice::message("widget.x.logged_out"))
    );
}

/// A waiting pane follows the language without being reloaded: the notice is a
/// reference into the catalogue, resolved on the frame that draws it, so the language
/// moving writes no state at all.
#[test]
fn a_language_change_should_relabel_the_sentence_of_a_signed_out_pane() {
    let mut harness = harness();
    let mut pane = signed_out_pane(&mut harness, FakeSource::default());
    load(&mut harness, &mut pane);
    let notice = notice_of(&harness, &pane).expect("a pane nobody signed in for stands in");
    assert_eq!(notice, Notice::message("widget.x.logged_out"));
    let english = notice.resolve(&harness.services().translate);
    assert_eq!(
        english,
        t_smol!(harness.services().translate, "widget.x.logged_out"),
        "the pane starts out in the language showing now"
    );
    assert!(
        !pane
            .status
            .report(harness.app_mut(), WidgetState::waiting(notice.clone())),
        "the same notice again is not a write"
    );

    harness.switch_language(LanguageCode::ZhCn);

    let chinese = notice.resolve(&harness.services().translate);
    assert_ne!(chinese, english, "the two locales differ");
    assert_eq!(
        chinese,
        t_smol!(harness.services().translate, "widget.x.logged_out"),
        "the same reference resolves in the language showing now"
    );
    assert_eq!(notice_of(&harness, &pane), Some(notice));
}

#[test]
fn a_timeline_from_another_account_should_not_be_shown() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    let mut other = Feed::new();
    other.profile(profile_event("42", "kytrix"));
    let snapshot = other.push(tweet_event("9", 1_700_000_100_000, "42", "tweet 9"));
    let scope = TweetScope::new(Some(&SmolStr::new_static(OTHER_ACCOUNT)))
        .expect("another account is a scope");
    let applied = with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_latest(
            scope,
            Arc::new(TweetStreamMessage::Update(TweetUpdate::published(
                9,
                snapshot,
                MediaStates::default(),
            ))),
            cx,
        )
    });

    assert!(!applied, "another account's tweets are not this pane's");
    assert_eq!(feed_ids(&pane), ["1"]);
}

#[test]
fn an_update_older_than_the_one_on_screen_should_not_be_shown() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    let revision = pane.widget.revision;
    let mut stale = Feed::new();
    stale.profile(profile_event("42", "kytrix"));
    let applied = report_feed(
        &mut harness,
        &mut pane,
        revision,
        stale.push(tweet_event("2", 1_700_000_100_000, "42", "tweet 2")),
    );

    assert!(
        !applied,
        "the revision already applied is not a newer timeline"
    );
    assert_eq!(feed_ids(&pane), ["1"]);
}

#[test]
fn a_timeline_that_fills_up_should_keep_the_newest_fifty() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    let mut feed = FakeFeed::new();
    for index in 0..MAX_TWEETS {
        feed.push(&index.to_string(), index as i64);
    }
    let snapshot = feed.push("new", MAX_TWEETS as i64);
    assert!(report_feed(&mut harness, &mut pane, 1, snapshot));

    let ids = feed_ids(&pane);
    assert_eq!(ids.len(), MAX_TWEETS);
    assert_eq!(ids[0], "new");
}

#[test]
fn a_frame_should_draw_the_rows_its_timeline_holds() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "hello world")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let body = body_root(&pane);
    let text = body_text(&harness, &pane);
    assert!(
        text.contains("kytrix display"),
        "the author is drawn: {text}"
    );
    assert!(text.contains("@kytrix"), "the handle is drawn: {text}");
    assert!(text.contains("hello world"), "the tweet is drawn: {text}");
    assert!(
        harness
            .find_by_debug_prefix(body, "TEST_TWEET_ROW:1")
            .is_some(),
        "the feed's row is mounted"
    );
    assert_eq!(row_count(&harness, &pane), 1);
    assert_eq!(mounted_rows(&harness, &pane).len(), 1);
    assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
}

#[test]
fn a_repost_row_should_say_who_reposted_it() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    feed.profile(profile_event("7", "seven"));
    let mut outer = tweet_event("outer", 1_700_000_000_000, "42", "reposting");
    outer.reference = Some(TweetReference::Repost(Box::new(tweet_event(
        "inner",
        1_700_000_000_000,
        "7",
        "inner words",
    ))));
    report_feed(&mut harness, &mut pane, 1, feed.push(outer));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let reposted_by = pane.widget.plans.borrow()[0].reposted_by.clone();
    assert_eq!(
        reposted_by.as_deref(),
        Some("kytrix display"),
        "the plan says who reposted the timeline entry"
    );
    let line = t_smol!(
        harness.services().translate,
        "widget.x.reposted",
        name = reposted_by.clone().unwrap_or_default()
    );
    let body = body_root(&pane);
    assert!(
        harness.has_text(body, line.as_str()),
        "the repost line is the localized sentence: {}",
        harness.text_of(body)
    );
    let text = body_text(&harness, &pane);
    assert!(text.contains("inner words"), "the reposted tweet is drawn");
    assert!(
        text.contains("seven display"),
        "the reposted author is drawn"
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);
    let localized = t_smol!(
        harness.services().translate,
        "widget.x.reposted",
        name = reposted_by.unwrap_or_default()
    );
    assert_ne!(localized, line, "the two locales differ");
    assert!(
        harness.has_text(body_root(&pane), localized.as_str()),
        "the repost line follows the language without rebuilding the row: {}",
        body_text(&harness, &pane)
    );
}

#[test]
fn a_quote_row_should_draw_the_quoted_text() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    feed.profile(profile_event("7", "seven"));
    let mut outer = tweet_event("outer", 1_700_000_000_000, "42", "quoting this");
    outer.reference = Some(TweetReference::Quote(Box::new(tweet_event(
        "quoted",
        1_700_000_000_000,
        "7",
        "quoted words",
    ))));
    report_feed(&mut harness, &mut pane, 1, feed.push(outer));
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let plans = pane.widget.plans.borrow();
    let quote = plans[0].quote.as_ref().expect("the plan carries the quote");
    assert_eq!(quote.text.as_str(), "quoted words");
    drop(plans);
    let body = body_root(&pane);
    assert!(
        harness
            .find_by_debug_prefix(body, "TEST_TWEET_QUOTE:quoted")
            .is_some(),
        "the quoted tweet is drawn under the tweet quoting it"
    );
    assert!(
        harness.has_text(body, "quoted words"),
        "the quoted text is drawn"
    );
}

#[test]
fn a_feed_nothing_of_which_survives_should_be_held_by_a_list_built_again() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut first = Feed::new();
    first.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        first.push(tweet_event("1", 1_700_000_000_000, "42", "first post")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    assert_eq!(row_count(&harness, &pane), 1);
    let held = mounted_rows(&harness, &pane);
    assert_eq!(held.len(), 1);

    let mut replacement = Feed::new();
    replacement.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        2,
        replacement.push(tweet_event("9", 1_700_000_100_000, "42", "second post")),
    );
    frame(&mut harness, &mut pane);

    assert_eq!(
        feed_ids(&pane),
        ["9"],
        "the pane holds the feed it was given"
    );
    assert_eq!(row_count(&harness, &pane), 1);
    assert_eq!(mounted_rows(&harness, &pane).len(), 1);
    let built = mounted_rows(&harness, &pane);
    assert_ne!(
        held, built,
        "a feed none of what the list held survives in is built again"
    );
    let text = body_text(&harness, &pane);
    assert!(
        text.contains("second post"),
        "the surviving feed is drawn: {text}"
    );
    assert!(!text.contains("first post"), "the old row is gone: {text}");
}

#[test]
fn the_age_text_should_follow_the_language() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", five_minutes_ago(), "42", "hello world")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let english = t_smol!(
        harness.services().translate,
        "widget.x.age_minutes",
        count = 5
    );
    let text = body_text(&harness, &pane);
    assert!(
        text.contains(english.as_str()),
        "the age is drawn in the language showing now: {text}"
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let chinese = t_smol!(
        harness.services().translate,
        "widget.x.age_minutes",
        count = 5
    );
    let text = body_text(&harness, &pane);
    assert_ne!(chinese, english, "the two locales differ");
    assert!(
        text.contains(chinese.as_str()),
        "the age follows the language without reloading the feed: {text}"
    );
    assert_eq!(feed_ids(&pane), ["1"], "the feed itself is unchanged");
}

#[test]
fn an_empty_timeline_should_say_it_in_the_language_showing_now() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    report_feed(&mut harness, &mut pane, 1, Feed::new().snapshot());
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let body = body_root(&pane);
    let empty = harness
        .find_by_debug_prefix(body, "TEST_TWEET_EMPTY")
        .expect("an empty feed draws its stand-in");
    assert!(
        harness.text_of(empty).contains("No data"),
        "the stand-in says the feed holds nothing"
    );

    harness.switch_language(LanguageCode::ZhCn);
    frame(&mut harness, &mut pane);

    let empty = harness
        .find_by_debug_prefix(body, "TEST_TWEET_EMPTY")
        .expect("the stand-in is rebuilt for the language");
    let text = harness.text_of(empty);
    assert!(
        text.contains("暂无数据"),
        "the stand-in follows the language on screen: {text}"
    );
    assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
}

#[test]
fn the_header_control_should_follow_the_language() {
    let mut harness = harness();
    let mut pane = pane(&mut harness, FakeSource::default());
    build_header(&mut harness, &mut pane);
    let button = pane
        .widget
        .manage_button
        .as_ref()
        .expect("the header built its control")
        .clone();
    assert_eq!(
        button.a11y_label_text(),
        Some(t_smol!(translate(LanguageCode::En), "widget.x.manage").as_str())
    );
    let built = button
        .action_identity()
        .expect("the control registered a click");

    harness.switch_language(LanguageCode::ZhCn);
    sync(&mut harness, &mut pane);

    let rebuilt = pane
        .widget
        .manage_button
        .as_ref()
        .expect("the rebuilt control is held")
        .clone();
    assert_eq!(
        rebuilt.a11y_label_text(),
        Some(t_smol!(harness.services().translate, "widget.x.manage").as_str()),
        "the control is rebuilt in the language showing now"
    );
    let rebuilt_action = rebuilt
        .action_identity()
        .expect("the rebuilt control registered a click");
    assert_eq!(
        rebuilt_action, built,
        "the control keeps the same position key when it is rebuilt"
    );
    assert!(harness.app().has_action_handler(&rebuilt_action));
}

/// A tweet's age is read in the zone the settings display data in: the day it was
/// published is the day the reader saw on their own clock.
#[test]
fn moving_the_display_zone_should_move_the_date_boundary() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    // 2020-01-01T20:30:00Z: the evening of the first in UTC, already the second at +8.
    let published = 1_577_910_600_000;
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", published, "42", "dated")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let utc = t_smol!(
        harness.services().translate,
        "widget.x.age_full_date",
        year = 2020,
        month = 1,
        day = 1
    );
    assert!(
        body_text(&harness, &pane).contains(utc.as_str()),
        "the row is drawn in UTC to begin with"
    );

    let mut config = current_config(&harness);
    config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
    assert!(write_config(&mut harness, config), "the zone is a change");
    frame(&mut harness, &mut pane);
    let east = t_smol!(
        harness.services().translate,
        "widget.x.age_full_date",
        year = 2020,
        month = 1,
        day = 2
    );
    let text = body_text(&harness, &pane);
    assert!(
        text.contains(east.as_str()),
        "the date follows the zone the settings display data in: {text}"
    );
    assert!(
        !text.contains(utc.as_str()),
        "the old date is not left behind"
    );

    let mut config = current_config(&harness);
    config.data_time_zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
    assert!(write_config(&mut harness, config));
    frame(&mut harness, &mut pane);
    let text = body_text(&harness, &pane);
    assert!(
        text.contains(utc.as_str()),
        "the western zone puts the tweet back on the first: {text}"
    );
    assert!(!text.contains(east.as_str()));
}

#[test]
fn a_row_with_a_picture_should_draw_a_tile() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let url = picture_url("tile");
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot,
        media_states(&[(&url, MediaState::Loading)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let body = body_root(&pane);
    let tile = harness
        .find_by_debug_prefix(body, "TEST_TWEET_MEDIA:1:0")
        .expect("a tweet carrying a picture draws a tile");
    assert!(
        has_image_node(&harness, tile),
        "the tile draws the picture or the state it is in"
    );
    assert_eq!(
        pane.widget.media.get(url.as_str()),
        Some(&MediaState::Loading)
    );
}

/// A picture the stream says is on disk is read once and handed to the engine's
/// resource cache; the row then draws it as an image node.
#[test]
fn a_picture_that_landed_should_be_drawn_as_an_image() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let url = picture_url("landed");
    let path = land_picture(&pane.cache, &url);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    // The timeline arrives first with the picture still out; once the pane has drawn it,
    // the picture arriving is the only thing that changes.
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot.clone(),
        media_states(&[(&url, MediaState::Loading)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    report_update(
        &mut harness,
        &mut pane,
        2,
        snapshot,
        media_states(&[(&url, MediaState::Ready(path.clone()))]),
    );
    sync(&mut harness, &mut pane);
    harness.drain_runtime();
    harness.drain();
    assert!(
        pane.widget
            .images
            .borrow()
            .contains_key(&SmolStr::new(url.as_str())),
        "the cached picture is registered with the engine"
    );

    let body = body_root(&pane);
    let tile = harness
        .find_by_debug_prefix(body, "TEST_TWEET_MEDIA:1:0")
        .expect("the picture's tile is drawn");
    let picture = largest_image_node(&harness, tile).expect("a picture is drawn in the tile");
    let bounds = harness
        .tree()
        .get(picture)
        .expect("the picture")
        .layout()
        .bounds;
    assert!(
        bounds.size.width > 100.0,
        "the picture fills its tile rather than being the tile's placeholder mark"
    );
    assert_eq!(
        pane.widget.media.get(url.as_str()),
        Some(&MediaState::Ready(path))
    );
}

#[test]
fn a_picture_the_service_reported_as_failed_should_be_drawn_as_one() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let url = picture_url("failed");
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot,
        media_states(&[(&url, MediaState::Failed)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.media.get(url.as_str()),
        Some(&MediaState::Failed)
    );
    let body = body_root(&pane);
    let tile = harness
        .find_by_debug_prefix(body, "TEST_TWEET_MEDIA:1:0")
        .expect("a failed picture is still a tile");
    assert!(has_image_node(&harness, tile), "the tile draws its failure");
    assert!(
        pane.widget.hits.failed.borrow().contains_key(&tile),
        "the tile is what a click asks for the picture again through"
    );
}

#[test]
fn a_frame_should_offer_a_failed_picture_again() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let url = picture_url("retry");
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot,
        media_states(&[(&url, MediaState::Failed)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let tile = harness
        .find_by_debug_prefix(body_root(&pane), "TEST_TWEET_MEDIA:1:0")
        .expect("the failed picture's tile is drawn");
    assert!(
        pane.widget.pending.is_empty(),
        "nothing is in flight before the click"
    );
    let bounds = harness.tree().get(tile).expect("the tile").layout().bounds;
    let window = harness
        .tree()
        .get(harness.root())
        .expect("the window root")
        .layout()
        .bounds;
    // The newest row sits against the bottom of the list, so part of its gallery is off
    // the top of the window: the click has to land on the part a reader could see.
    let point = Point::new(
        bounds.origin.x + 2.0,
        bounds.origin.y.max(window.origin.y) + 2.0,
    );
    let handled = click_at(&mut harness, &mut pane, point);

    assert!(handled, "the failed picture's tile is a target");
    assert_eq!(
        *harness.app().read(&pane.widget.media_retry),
        Some(SmolStr::new(url.as_str())),
        "the click asks for that picture again, and the next frame sends it"
    );
    frame(&mut harness, &mut pane);
    assert!(
        !pane.widget.pending.is_empty(),
        "asking for the picture again is a call the pane tracks"
    );
    assert_eq!(
        *harness.app().read(&pane.widget.media_retry),
        None,
        "the request is taken once"
    );
}

/// The failed picture's tile is reachable without a pointer: it is in the a11y tree
/// (not hidden) and it is focusable, and activating it asks for the picture again.
#[test]
fn a_failed_picture_should_be_offered_again_from_the_keyboard() {
    let mut harness = harness();
    harness.enable_a11y_capture();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let url = picture_url("keyboard-retry");
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot,
        media_states(&[(&url, MediaState::Failed)]),
    );
    build_body(&mut harness, &mut pane);
    sync(&mut harness, &mut pane);
    // The a11y snapshot is delivered by the frame that draws the rows: the frame after it
    // is idle and carries none, so the read is of the drawing frame.
    harness.drain();

    let tile = harness
        .find_by_debug_prefix(body_root(&pane), "TEST_TWEET_MEDIA:1:0")
        .expect("the failed picture's tile is drawn");
    let action = pane
        .widget
        .media_retry_action
        .clone()
        .expect("the pane registered the retry action");
    assert!(
        !harness.a11y_hidden(tile),
        "a failed picture is a way to ask for it again, not decoration"
    );
    let window = harness.window();
    assert!(
        harness.app_mut().set_focus(window, Some(tile)),
        "the tile a keyboard reader can reach is what makes the retry possible"
    );
    assert!(
        harness
            .app_mut()
            .dispatch_action_focused(window, action, ActionSource::Key),
        "the focused tile's activation is the retry"
    );
    frame(&mut harness, &mut pane);

    assert_eq!(
        *harness.app().read(&pane.widget.media_retry),
        None,
        "the activation asked for the picture through the same request a click does"
    );
    assert!(
        !pane.widget.pending.is_empty(),
        "the picture is being fetched again"
    );
}

/// One tweet whose whole body is one link, so a pointer anywhere over the text is over
/// the link.
fn linked_tweet(tweet_id: &str, timestamp: i64, text: &str, url: &str) -> XTweetEvent {
    let mut event = tweet_event(tweet_id, timestamp, "42", text);
    event.text_segments = vec![XTweetTextSegment {
        text: text.into(),
        url: Some(url.into()),
    }];
    event
}

/// A click on a row opens the tweet it draws.
#[test]
fn a_click_on_a_row_should_open_its_tweet() {
    let opened = Rc::new(RefCell::new(Vec::new()));
    let mut harness = {
        let opened = Rc::clone(&opened);
        WidgetTestApp::with_translate(translate(LanguageCode::En))
            .with_open_url(move |url| opened.borrow_mut().push(url.to_owned()))
    };
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "open me")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let body = body_root(&pane);
    let row = harness
        .find_by_debug_prefix(body, "TEST_TWEET_ROW:1")
        .expect("the row is mounted");
    assert!(
        click_node(&mut harness, &mut pane, row),
        "the row is a target"
    );

    assert_eq!(opened.borrow().as_slice(), [tweet_url("kytrix", "1")]);
}

/// A link inside a tweet is drawn in the theme's link colour, and the colour follows the
/// appearance: a run's colour is a static load, so the rows are built for an appearance
/// and rebuilt when it moves (AGENTS §7).
#[test]
fn a_link_should_be_drawn_in_the_brand_colour() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let target = "https://example.com/article";
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(linked_tweet("1", 1_700_000_000_000, target, target)),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let text = harness
        .find_by_debug_prefix(body_root(&pane), "TEST_TWEET_TEXT:1")
        .expect("the tweet's text node is drawn");
    let theme = harness.services().theme.clone();
    let light_link = theme.theme_for(Appearance::Light).role(Role::BrandBase);
    let dark_link = theme.theme_for(Appearance::Dark).role(Role::BrandBase);
    assert_eq!(
        run_colours(&harness, text),
        vec![light_link],
        "the link's run is drawn in the theme's link colour"
    );

    assert!(theme.set_appearance(harness.app_mut(), Appearance::Dark));
    frame(&mut harness, &mut pane);

    assert_ne!(
        light_link, dark_link,
        "the two appearances have different link colours"
    );
    let text = harness
        .find_by_debug_prefix(body_root(&pane), "TEST_TWEET_TEXT:1")
        .expect("the rebuilt row draws the tweet's text");
    assert_eq!(
        run_colours(&harness, text),
        vec![dark_link],
        "the link follows the appearance it is drawn in"
    );
}

/// The runs a text node draws, which is where a link's colour lives.
fn run_colours(harness: &WidgetTestApp, node: NodeId) -> Vec<ui_core::paint::color::Color> {
    match harness.tree().get(node).expect("the text node").kind() {
        NodeKind::Text(content) => content.runs().iter().map(|run| run.style.color).collect(),
        other => panic!("expected a text node, found {other:?}"),
    }
}

/// A click over a link opens the link, not the tweet the link is in: the byte the pointer
/// is over names which link was clicked, which is the same range the run is coloured for.
#[test]
fn a_click_on_a_link_should_open_the_link_not_the_tweet() {
    let opened = Rc::new(RefCell::new(Vec::new()));
    let mut harness = {
        let opened = Rc::clone(&opened);
        WidgetTestApp::with_translate(translate(LanguageCode::En))
            .with_open_url(move |url| opened.borrow_mut().push(url.to_owned()))
    };
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let target = "https://example.com/article";
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(linked_tweet("1", 1_700_000_000_000, target, target)),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);

    let text = harness
        .find_by_debug_prefix(body_root(&pane), "TEST_TWEET_TEXT:1")
        .expect("the tweet's text node is drawn");
    // The text node fills the column, so its centre is past the glyphs: a click over the
    // link has to land on the glyphs the link is drawn as, near the start of the line.
    let bounds = harness
        .tree()
        .get(text)
        .expect("the text node is alive")
        .layout()
        .bounds;
    let point = Point::new(
        bounds.origin.x + 8.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    assert!(
        click_at(&mut harness, &mut pane, point),
        "the text is a target"
    );

    assert_eq!(
        opened.borrow().as_slice(),
        [target],
        "the link opens rather than the tweet it is in"
    );
}

#[test]
fn the_manage_control_should_ask_the_pane_to_open_the_dialog() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    build_header(&mut harness, &mut pane);
    let manage = pane
        .widget
        .manage_button
        .as_ref()
        .expect("the header built its control")
        .clone();
    assert_eq!(pane.widget.seen_open_requests, 0);

    click_button(&mut harness, &manage);
    assert_eq!(
        *harness.app().read(&pane.widget.open_dialog_requests),
        1,
        "the control asks the pane for the dialog"
    );
    sync(&mut harness, &mut pane);

    assert_eq!(
        pane.widget.seen_open_requests, 1,
        "the pane handles the request on the next frame"
    );
}

/// Opening the dialog is the pane asking for the accounts and for their avatars: the
/// read waits for the stream, because a read goes over a live one.
#[test]
fn opening_the_dialog_should_watch_and_read_the_accounts() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    assert!(pane.widget.dialog.is_none());

    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.open_dialog(cx))
        .expect("the dialog opens");

    assert!(pane.widget.dialog.is_some(), "the pane holds its dialog");
    let pending = pane
        .widget
        .selection_pending
        .as_ref()
        .expect("the accounts are being read");
    assert_eq!(pending.operation, SelectionOperation::Loading);
    assert!(
        !pending.sent,
        "a read before the stream is live has no session to answer it"
    );

    report_status(&mut harness, &mut pane, TweetStatus::Ready);
    let pending = pane
        .widget
        .selection_pending
        .as_ref()
        .expect("the read is still the dialog's");
    assert!(pending.sent, "the readiness that arrives sends the read");
}

#[test]
fn a_read_should_wait_for_a_live_stream() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    session.answer_history(
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    session.answer_selections(selections(vec![selection("7", "seven")], 3));
    let mut pane = pane(&mut harness, source);
    let _subscription = session.subscribe();
    load(&mut harness, &mut pane);
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));

    assert!(
        !pane
            .widget
            .selection_pending
            .as_ref()
            .expect("the read is held")
            .sent,
        "a read asked for before the stream is live is not sent"
    );
    assert_eq!(session.state().fetches, 0);
    settle(&mut harness, &mut pane, "the read to go out", |_, _| {
        session.state().fetches == 1
    });
    settle(
        &mut harness,
        &mut pane,
        "the accounts to arrive",
        |_, pane| pane.widget.selections.is_some(),
    );

    assert_eq!(usernames_of(&pane), ["seven"]);
}

#[test]
fn a_read_asked_for_before_the_stream_is_live_should_be_sent_when_it_comes_up() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    assert!(!pane.widget.ready);

    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));
    let pending = pane
        .widget
        .selection_pending
        .as_ref()
        .expect("the read is held");
    assert!(!pending.sent);
    assert!(pane.widget.pending.is_empty(), "nothing went out yet");

    report_status(&mut harness, &mut pane, TweetStatus::Ready);

    let pending = pane
        .widget
        .selection_pending
        .as_ref()
        .expect("the read is still waiting for its answer");
    assert!(pending.sent, "the readiness that arrives sends the read");
    assert!(!pane.widget.pending.is_empty(), "the read is in flight");
}

#[test]
fn a_change_should_be_refused_while_a_read_is_pending_or_a_search_is_out() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));
    assert!(
        pane.widget
            .selection_pending
            .as_ref()
            .is_some_and(|pending| pending.sent),
        "the read is out"
    );

    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .add_selection(SmolStr::new_static("eight"), cx)),
        "a change is not sent while a read is pending"
    );
    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .delete_selection(SmolStr::new_static("7"), cx)),
        "nor is a removal"
    );

    pane.widget.selection_pending = None;
    pane.widget.searching = Some(SmolStr::new_static("nine"));
    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .add_selection(SmolStr::new_static("eight"), cx)),
        "a change is not sent while a search is out"
    );
    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .delete_selection(SmolStr::new_static("7"), cx)),
        "nor is a removal"
    );

    pane.widget.searching = None;
    assert!(
        with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .add_selection(SmolStr::new_static("eight"), cx)),
        "the rule is not a blanket refusal"
    );
}

#[test]
fn a_search_that_is_not_one_should_not_be_sent() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);

    for invalid in ["", "   ", "with\ncontrol", &"中".repeat(40)] {
        assert!(
            !with_cx_of(&mut harness, pane.scope, |cx| pane
                .widget
                .search_users(SmolStr::new(invalid), cx)),
            "{invalid:?} is not a query"
        );
    }

    assert!(pane.widget.searching.is_none());
    assert!(pane.widget.pending.is_empty());
    assert!(
        with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .search_users(SmolStr::new_static("nine"), cx)),
        "a query X could be asked about is sent"
    );
    assert_eq!(pane.widget.searching.as_deref(), Some("nine"));
}

/// A search is not a change to the accounts, but while one is out the pane is not in a
/// position to ask about them: a change waits until the search is answered or the dialog
/// that asked for it goes.
#[test]
fn a_search_that_is_out_should_turn_a_change_away() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    session.answer_selections(selections(vec![selection("7", "seven")], 3));
    let (mut pane, _subscription) = live_pane(
        &mut harness,
        &source,
        &session,
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));
    settle(
        &mut harness,
        &mut pane,
        "the accounts to arrive",
        |_, pane| pane.widget.selections.is_some(),
    );
    // Nothing answers the search: what the search being out is asserted against is the
    // change the pane turns away for as long as it is out.
    session.block_search();
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .search_users(SmolStr::new_static("nine"), cx)));
    settle(&mut harness, &mut pane, "the search to be out", |_, _| {
        session.state().searches_made == 1
    });
    assert!(pane.widget.searching.is_some());

    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .add_selection(SmolStr::new_static("eight"), cx)),
        "a change is not sent while a search is out"
    );
    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .delete_selection(SmolStr::new_static("7"), cx)),
        "nor is a removal"
    );
    assert_eq!(session.state().add_calls, 0);
    assert_eq!(session.state().delete_calls, 0);
    assert!(
        pane.widget.selection_pending.is_none(),
        "neither change took the search's place"
    );
}

#[test]
fn a_change_should_be_refused_while_the_stream_is_not_live() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.selections = Some(selection_data(vec![selection("7", "seven")], 3));
    assert!(!pane.widget.ready);

    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .add_selection(SmolStr::new_static("eight"), cx)),
        "a change goes over the session the stream was opened with"
    );
    assert!(
        !with_cx_of(&mut harness, pane.scope, |cx| pane
            .widget
            .delete_selection(SmolStr::new_static("7"), cx)),
        "nor is a removal"
    );
    assert!(pane.widget.pending.is_empty());
}

/// A change the pane stopped waiting for — because the user recovered the stream — is
/// dropped rather than decided: a change that is in flight when the stream is restarted
/// is let go, and the read that follows settles the list.
#[test]
fn a_change_a_started_stream_displaced_should_be_forgotten() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    session.answer_selections(selections(vec![selection("7", "seven")], 3));
    let (mut pane, _subscription) = live_pane(
        &mut harness,
        &source,
        &session,
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    // The accounts are read first, because a change goes over the session the read came
    // from.
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));
    settle(
        &mut harness,
        &mut pane,
        "the accounts to arrive",
        |_, pane| pane.widget.selections.is_some(),
    );

    let block = session.block_mutation();
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .add_selection(SmolStr::new_static("eight"), cx)));
    let target = pane
        .widget
        .selection_pending
        .as_ref()
        .and_then(|pending| pending.target.clone());
    assert_eq!(
        target.as_deref(),
        Some("eight"),
        "the account that was asked for is the one the change carries"
    );
    settle(
        &mut harness,
        &mut pane,
        "the change to be in flight",
        |_, _| session.state().add_calls == 1,
    );

    let replacement = source.session(ACCOUNT);
    replacement.answer_history(
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    let _replacement_subscription = replacement.subscribe();
    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.start(true, cx))
        .expect("a recovery starts again");

    assert!(
        pane.widget.selection_pending.is_none(),
        "a change whose answer cannot arrive is dropped rather than remembered"
    );
    assert!(pane.widget.notice.is_none());
    settle(
        &mut harness,
        &mut pane,
        "the displaced change to be let go",
        |_, _| block.is_dropped(),
    );
}

/// The answer to a command the binding replaced is a late one: its identity is not in
/// the table, so it is dropped and counted rather than applied to the command that
/// replaced it (AGENTS §8.2). The answer to the command that is in flight is applied.
#[test]
fn the_answer_to_a_replaced_command_should_be_dropped() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    load(&mut harness, &mut pane);
    load(&mut harness, &mut pane);

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
            cx,
        );
    });

    assert!(
        !state_of(&harness, &pane).is_failed(),
        "the old failure is not the new command's"
    );
    assert_eq!(
        pane.widget.pending.dropped(),
        1,
        "the late answer is dropped and counted"
    );

    // The command the binding is carrying now is answered by its own identity.
    load(&mut harness, &mut pane);
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(3),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(3),
            }),
            cx,
        );
    });

    assert_eq!(
        failure_text(&harness, &pane),
        XFailure::Rpc(RpcCallError::ServiceClosed {
            request_id: RequestId::new(3),
        })
        .translated_message(&harness.services().translate)
        .to_string()
    );
    assert!(
        pane.widget.pending.is_empty(),
        "the answer is taken off the table"
    );
}

/// A failure is terminal: a reply that was already out when the pane gave up does not
/// steer it, and it is not even taken off the table as if it answered something
/// (AGENTS §4.4 — the identity attribution and the failure gate are two doors, and every
/// entry checks both).
#[test]
fn a_reply_after_a_failure_should_not_be_applied() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    load(&mut harness, &mut pane);
    report_status(
        &mut harness,
        &mut pane,
        TweetStatus::Failed(TweetStreamError::Service(KytrixServiceError::Closed)),
    );
    assert!(state_of(&harness, &pane).is_failed());

    // A read of the monitored accounts was in flight when the stream failed; its answer
    // arrives after the pane gave up.
    pane.widget
        .pending
        .track(RequestId::new(2), PendingCommand::FetchSelections);
    let answer = selection_data(vec![selection("7", "seven")], 4);
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget
            .apply_rpc(RequestId::new(2), Ok(TweetResponse::Selections(answer)), cx);
    });

    assert!(
        pane.widget.selections.is_none(),
        "a failed pane does not take the answer it was waiting for before it failed"
    );
    assert_eq!(
        pane.widget.pending.take(RequestId::new(2)),
        Some(PendingCommand::FetchSelections),
        "the answer is not attributed either: the gate is before the identity"
    );
    assert!(
        state_of(&harness, &pane).is_failed(),
        "the failure is still the pane's state"
    );
}

/// A search is a procedure of the page that ran it, so an answer that arrives after the
/// dialog is gone has nothing to be handed to.
#[test]
fn a_search_answer_for_a_dialog_that_is_gone_should_be_dropped() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .search_users(SmolStr::new_static("nine"), cx)));
    assert!(pane.widget.searching.is_some());
    assert!(pane.widget.dialog.is_none());
    let profiles: Arc<[XProfile]> = Arc::from(vec![search_profile("9", "nine")]);

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(TweetResponse::SearchResults(profiles)),
            cx,
        );
    });

    assert!(
        pane.widget.searching.is_none(),
        "the answer is taken even though there is nobody to hand it to"
    );
    assert!(pane.widget.pending.is_empty());
}

/// The calls the pane makes for the dialog are ones it is not waiting for an answer to:
/// what they displace is closed out rather than left waiting for an answer that can no
/// longer arrive.
#[test]
fn a_call_the_pane_does_not_wait_for_should_close_out_what_it_displaced() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    pane.widget.searching = Some(SmolStr::new_static("nine"));
    pane.widget.selection_pending = Some(SelectionPending {
        operation: SelectionOperation::Loading,
        target: None,
        sent: true,
    });

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.watch_selections(true, cx)
    })
    .expect("the watch is a call");

    assert!(
        pane.widget.searching.is_none(),
        "a search the call took the place of is not waited for"
    );
    assert!(
        pane.widget.selection_pending.is_none(),
        "a read the call took the place of is closed out"
    );
}

#[test]
fn a_search_that_is_out_when_the_dialog_closes_should_leave_nothing_waiting() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.open_dialog(cx))
        .expect("the dialog opens");
    pane.widget.searching = Some(SmolStr::new_static("nine"));

    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.close_dialog(cx))
        .expect("the dialog closes");

    assert!(pane.widget.dialog.is_none());
    assert!(
        pane.widget.searching.is_none(),
        "the pane stops waiting for the search of the closed dialog"
    );
    assert!(
        pane.widget.selection_pending.is_none(),
        "nothing a dismissal displaced is left waiting"
    );
}

#[test]
fn a_confirmed_add_should_join_the_list_and_be_noticed() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .add_selection(SmolStr::new_static("eight"), cx)));

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(TweetResponse::Added(XMutationOutcome::Confirmed(
                selection("8", "eight"),
            ))),
            cx,
        );
    });

    assert_eq!(usernames_of(&pane), ["seven", "eight"]);
    assert_eq!(pane.widget.notice, Some(SelectionNotice::Added));
    assert!(pane.widget.selection_pending.is_none());
}

#[test]
fn a_refused_add_should_say_why_and_leave_the_list_alone() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.add_selection(SmolStr::new_static("eight"), cx);
    });

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(TweetResponse::Added(XMutationOutcome::Rejected(
                XApiError::QuotaExceeded,
            ))),
            cx,
        );
    });

    assert_eq!(
        pane.widget.notice,
        Some(SelectionNotice::Problem(SelectionFailure::Rejected(
            XApiError::QuotaExceeded
        )))
    );
    assert_eq!(usernames_of(&pane), ["seven"]);
}

#[test]
fn an_unknown_add_should_say_so_and_leave_the_list_alone() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.add_selection(SmolStr::new_static("eight"), cx);
    });

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(TweetResponse::Added(XMutationOutcome::Unknown(
                XMutationUnknown::Timeout,
            ))),
            cx,
        );
    });

    assert_eq!(
        pane.widget.notice,
        Some(SelectionNotice::Problem(SelectionFailure::Unknown(
            XMutationUnknown::Timeout
        )))
    );
    assert_eq!(usernames_of(&pane), ["seven"]);
}

#[test]
fn a_confirmed_delete_should_leave_the_list() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    pane.widget.selections = Some(selection_data(
        vec![selection("7", "seven"), selection("8", "eight")],
        3,
    ));
    pane.widget.hints = Arc::new(HashMap::from([(
        SmolStr::new_static("7"),
        SmolStr::new_static("pending seven"),
    )]));
    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .delete_selection(SmolStr::new_static("7"), cx)));

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_rpc(
            RequestId::new(1),
            Ok(TweetResponse::Deleted(XMutationOutcome::Confirmed(()))),
            cx,
        );
    });

    assert_eq!(usernames_of(&pane), ["eight"]);
    assert!(
        pane.widget.notice.is_none(),
        "a removal is visible in the list itself"
    );
    assert!(
        pane.widget.hints.is_empty(),
        "what was remembered about the removed row goes with it"
    );
}

/// A read the account refuses is a problem the pane holds for the dialog rather than a
/// list it reads as empty, and what the pane had read stays as it was.
#[test]
fn a_change_that_could_not_be_read_should_be_a_problem() {
    let mut harness = harness();
    let source = FakeSource::default();
    let session = source.session(ACCOUNT);
    let (mut pane, _subscription) = live_pane(
        &mut harness,
        &source,
        &session,
        vec![tweet_event("1", 1_700_000_000_000, "42", "tweet 1")],
        vec![profile_event("42", "kytrix")],
    );
    session.refuse_selections(XApiError::Closed);

    assert!(with_cx_of(&mut harness, pane.scope, |cx| pane
        .widget
        .load_selections(cx)));
    settle(
        &mut harness,
        &mut pane,
        "the refusal to be noticed",
        |_, pane| pane.widget.notice.is_some(),
    );

    assert!(matches!(
        pane.widget.notice,
        Some(SelectionNotice::Problem(SelectionFailure::Rpc(_)))
    ));
    assert!(pane.widget.selection_pending.is_none());
    assert!(pane.widget.selections.is_none());
}

/// What is remembered about a row waiting to be activated is a memo beside the rows: a
/// read of the accounts keeps it for the rows the read still holds, and an account
/// change is a pane starting over with a set of rows that are not the ones it named.
#[test]
fn a_remembered_name_should_survive_a_read_and_not_an_account_change() {
    let mut harness = harness();
    let mut pane = ready_pane(&mut harness);
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.add_selection(SmolStr::new_static("eight"), cx);
    });
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_add_result(
            Ok(TweetResponse::Added(XMutationOutcome::Confirmed(
                selection_without_profile("8"),
            ))),
            cx,
        );
    });
    assert_eq!(
        pane.widget.hints.get("8").map(SmolStr::as_str),
        Some("eight"),
        "an account that came back without a profile is remembered by its username"
    );

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_selections_result(
            Ok(TweetResponse::Selections(selection_data(
                vec![selection("7", "seven"), selection_without_profile("8")],
                3,
            ))),
            cx,
        );
    });
    assert_eq!(
        pane.widget.hints.get("8").map(SmolStr::as_str),
        Some("eight"),
        "a read keeps the name of a row it still holds"
    );
    assert!(
        pane.widget
            .selections
            .as_ref()
            .is_some_and(|data| data.contains_username("eight", &pane.widget.hints)),
        "and the account is still recognised by that name"
    );

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_selections_result(
            Ok(TweetResponse::Selections(selection_data(
                vec![selection("7", "seven")],
                3,
            ))),
            cx,
        );
    });
    assert!(
        pane.widget.hints.is_empty(),
        "a name whose row the read no longer holds is not remembered"
    );

    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.add_selection(SmolStr::new_static("eight"), cx);
    });
    with_cx_of(&mut harness, pane.scope, |cx| {
        pane.widget.apply_add_result(
            Ok(TweetResponse::Added(XMutationOutcome::Confirmed(
                selection_without_profile("8"),
            ))),
            cx,
        );
    });
    assert!(!pane.widget.hints.is_empty());
    sign_in(&mut harness, OTHER_ACCOUNT);
    sync(&mut harness, &mut pane);
    assert!(
        pane.widget.hints.is_empty(),
        "an account change drops what was remembered about the account it left"
    );
}

/// Two panes of one application build the same control with different keys: releasing
/// one pane's control leaves the other's handler registered (AGENTS §9).
#[test]
fn two_panes_should_keep_their_own_header_handlers() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let mut first = quiet_pane(&mut harness, FakeSource::default());
    build_header(&mut harness, &mut first);
    let first_button = first
        .widget
        .manage_button
        .as_ref()
        .expect("the first pane built its control")
        .clone();
    let first_action = first_button
        .action_identity()
        .expect("the first control registered a click");

    let second_scope = harness.app_mut().signal_scope();
    let mut second = pane_in_scope(&mut harness, FakeSource::default(), second_scope);
    let root = harness.root();
    let services = Rc::clone(harness.services_rc());
    let window = harness.window();
    {
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, second_scope);
        second
            .widget
            .header(HeaderGroup::Left, root, &mut cx)
            .expect("the second pane builds its control");
    }
    let second_button = second
        .widget
        .manage_button
        .as_ref()
        .expect("the second pane built its control")
        .clone();
    let second_action = second_button
        .action_identity()
        .expect("the second control registered a click");
    let first_key = with_cx_of(&mut harness, first.scope, |cx| {
        cx.unique_key("x.header.manage")
    });
    let second_key = with_cx_of(&mut harness, second.scope, |cx| {
        cx.unique_key("x.header.manage")
    });
    assert_ne!(
        first_key, second_key,
        "each pane's control is keyed by its own instance"
    );
    assert_ne!(first_action, second_action);
    assert!(harness.app().has_action_handler(&first_action));
    assert!(harness.app().has_action_handler(&second_action));

    with_cx_of(&mut harness, first.scope, |cx| {
        let _ = first.widget.release_manage_button(cx, true);
    });
    assert!(
        !harness.app().has_action_handler(&first_action),
        "the closed pane's handler is unregistered"
    );
    assert!(
        harness.app().has_action_handler(&second_action),
        "releasing one pane must not unregister another pane's handler"
    );
}

#[test]
fn two_panes_should_not_share_their_feed_state() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let mut first = quiet_pane(&mut harness, FakeSource::default());
    let second_scope = harness.app_mut().signal_scope();
    let second = pane_in_scope(&mut harness, FakeSource::default(), second_scope);
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut first,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );

    assert!(first.widget.has_content());
    assert!(
        !second.widget.has_content(),
        "one pane's feed is not another pane's"
    );
    assert!(second.widget.feed.is_none());
    assert_eq!(feed_ids(&first), ["1"]);
}

/// The framework drops a pane by releasing the scope it was mounted in; everything the
/// pane built has to be gone by then, or the scope's values leak and the next mount
/// mints fresh slots (AGENTS §11).
#[test]
fn releasing_the_pane_should_reclaim_what_it_built() {
    let mut harness = harness();
    sign_in(&mut harness, ACCOUNT);
    let baseline = harness.signal_slot_stats();
    let mut mounted_slots = None;

    for _ in 0..8 {
        let scope = harness.app_mut().signal_scope();
        let mut pane = pane_in_scope(&mut harness, FakeSource::default(), scope);
        report_status(&mut harness, &mut pane, TweetStatus::Ready);
        let mut feed = Feed::new();
        feed.profile(profile_event("42", "kytrix"));
        report_feed(
            &mut harness,
            &mut pane,
            1,
            feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
        );
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the pane mints its own values"
        );
        let body = body_root(&pane);

        with_cx_of(&mut harness, pane.scope, |cx| pane.widget.release(cx));
        assert!(
            harness.tree().get(body).is_err(),
            "the pane takes its own body subtree down"
        );
        assert!(pane.widget.list.is_none(), "the list is released");
        assert!(
            pane.widget.manage_button.is_none(),
            "the header control is released"
        );
        drop(pane);
        // The framework releases the pane's scope after dropping it.
        harness.app_mut().release_scope(scope);

        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing the pane returns every live value slot"
        );
        let used = harness.signal_slot_stats().slots;
        assert_eq!(
            *mounted_slots.get_or_insert(used),
            used,
            "a mount reuses the slots it was given rather than minting afresh"
        );
    }
}

#[test]
fn releasing_the_pane_should_be_idempotent() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let body = body_root(&pane);

    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.release(cx));
    with_cx_of(&mut harness, pane.scope, |cx| pane.widget.release(cx));

    assert!(pane.widget.binding.is_none());
    assert!(pane.widget.body_root.is_none());
    assert!(pane.widget.list.is_none());
    assert!(!pane.widget.has_content());
    assert!(
        harness.tree().get(body).is_err(),
        "the body subtree was taken down once and stays down"
    );
}

/// A pane with nothing changing does not ask for a frame (I3).
#[test]
fn a_steady_pane_should_not_ask_for_a_frame() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    report_feed(
        &mut harness,
        &mut pane,
        1,
        feed.push(tweet_event("1", 1_700_000_000_000, "42", "tweet 1")),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let requests = harness.frame_requests();

    sync(&mut harness, &mut pane);
    harness.drain();
    sync(&mut harness, &mut pane);
    harness.drain();

    assert_eq!(
        harness.frame_requests(),
        requests,
        "a steady pane asks for no frame"
    );
}

/// A picture state arriving is a change the rows are built against, so the visible
/// window is rebuilt for it; a report whose picture states are equal does not rebuild
/// anything — even though the service hands every event its own map.
#[test]
fn a_media_only_update_should_rebuild_the_visible_window() {
    let mut harness = harness();
    let mut pane = quiet_pane(&mut harness, FakeSource::default());
    pane.widget.ready = true;
    let url = picture_url("window");
    let mut feed = Feed::new();
    feed.profile(profile_event("42", "kytrix"));
    let snapshot = feed.push(media_tweet("1", 1_700_000_000_000, &url));
    report_update(
        &mut harness,
        &mut pane,
        1,
        snapshot.clone(),
        media_states(&[(&url, MediaState::Loading)]),
    );
    build_body(&mut harness, &mut pane);
    frame(&mut harness, &mut pane);
    let loading_rows = mounted_rows(&harness, &pane);
    assert_eq!(loading_rows.len(), 1);

    let failed = media_states(&[(&url, MediaState::Failed)]);
    report_update(&mut harness, &mut pane, 2, snapshot.clone(), failed);
    frame(&mut harness, &mut pane);
    let failed_rows = mounted_rows(&harness, &pane);
    assert_eq!(failed_rows.len(), 1);
    assert_ne!(
        loading_rows, failed_rows,
        "a picture state the rows were built against rebuilds the visible window"
    );
    assert_eq!(row_count(&harness, &pane), 1);

    // The service publishes a fresh map with every event (a new `Arc` even when nothing
    // about the pictures changed): the rebuild is decided by what the states are, not by
    // which allocation they came in.
    report_update(
        &mut harness,
        &mut pane,
        3,
        snapshot,
        media_states(&[(&url, MediaState::Failed)]),
    );
    frame(&mut harness, &mut pane);
    assert_eq!(
        mounted_rows(&harness, &pane),
        failed_rows,
        "a report that changes no picture state rebuilds nothing"
    );
}
