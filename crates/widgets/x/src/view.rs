//! The X pane: the timeline it follows, and the accounts it manages.
//!
//! # What moved, and what it moved to
//!
//! The old pane was an element tree of the retired toolkit: a list of rendered rows,
//! two dialogs owned as entities of their own, and process-global managers for the
//! settings and the executor. The migrated pane is the framework's
//! five-phase widget over engine nodes: the feed is a [`List`] whose rows are built
//! once into their containers and updated through `bind_text` / `bind_style` /
//! `bind_a11y`, the header is a [`Button`], and the account dialog is a
//! [`TweetDialog`] built from `base_ui::kit` components over the same signals.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the
//! pane never has to decide whether a tweet belongs to the account on screen: a
//! message published for an account the pane has left is a message the binding does
//! not deliver, and one that arrives by hand is refused by comparing the key. The
//! account is read from [`WidgetCx::account`] on every frame and compared — the
//! settings signal is what schedules the frame, so a sign-in or sign-out is a reload
//! on the frame that presents it.
//!
//! # The pane owns the accounts, the service owns the session
//!
//! Following a timeline and managing the accounts it comes from are two things the
//! same account is asked for, and the same service answers both with the same
//! session. So the pane is what waits for both, and the accounts the dialog draws are
//! the pane's rather than the dialog's: they outlive any one dialog, and a dialog
//! that held them would lose them on the next open. The dialog's own page state (the
//! typed filter, the query, what one search answered, which page is showing) belongs
//! to the dialog and dies with it.
//!
//! # One call at a time, attributed by identity
//!
//! A request reaches the service through the pane's binding, and the binding replaces
//! the call it holds with the next one. [`WidgetBinding::call`] returns the
//! [`RequestId`] that identifies the call, and [`Pending`] remembers which command is
//! waiting for that identity: a reply whose id the table does not know is a late
//! answer to a replaced call, dropped rather than applied to whatever replaced it.
//! The stream guards (stream key, terminal failure) and the id attribution are two
//! independent doors — every entry checks both.
//!
//! # The timeline is a list of measured rows
//!
//! A tweet is a paragraph, a gallery and a quoted tweet, so its height is the sum of
//! things only the text system can measure. The body is therefore a bounded variable
//! list ([`ListOptions::variable`]) that holds the newest tweets and remembers where a
//! reader is. A newer timeline is reported to it as the tweets it has not been shown
//! rather than as a new set of rows: a list rebuilt from scratch would re-measure
//! every row on screen and, worse, move a reader who had scrolled into the timeline
//! back to the top.
//!
//! # Pictures are drawn from what the stream said
//!
//! A row draws a picture from the state the service published with the tweets it
//! belongs to, never by fetching anything itself: the service's media cache writes the
//! bytes, the pane reads the cached file once and hands it to the engine's resource
//! cache ([`App::resources_mut`]), and the row holds the resulting
//! [`ImageSource`] — a pending handle that the engine rewrites in place when the decode
//! lands, so nothing rebuilds for a picture arriving. A picture whose bytes are not on
//! disk yet is a tile that says so, and one that failed is a tile a click asks again.
//! Reading the cached file is the one synchronous disk read this pane does; it happens
//! once per URL on the frame the media state first arrives.
//!
//! # Empty is content
//!
//! A live stream whose accounts have not posted has an *empty timeline*, and an empty
//! timeline is content: the body draws `common.no_data` over the list rather than
//! leaving the framework's placeholder to say the pane is waiting
//! (`crates/widgets/AGENTS.md` §6). Only "no account" — nobody to read a timeline for —
//! is a neutral [`Notice`].

use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use ahash::AHashMap as HashMap;
use app_config::{
    network::Config as NetworkConfig,
    settings::Config as SettingsConfig,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::{IconKind, RasterTier, raster},
        list::{List, ListOptions, SelectionMode},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::KytrixServiceError,
    x::{XApiError, XMutationOutcome, XMutationUnknown, XSelectionStatus},
};
use network::transport::hub::{TransportHub, TransportPolicy};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::OffsetDateTime;
use ui_core::{
    a11y::{A11yOverride, A11yPatch},
    error::EngineError,
    event::{Event, InputEvent, PointerButton},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        image::{ImageFit, ImageSource},
        signal::Signal,
        tree::NodeTree,
    },
    paint::{color::Color, style::StylePatch},
    text::{TextAlign, TextOverflow, TextStyle},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    dialog::{DialogRequest, DialogView, SearchAnswer, TweetDialog},
    media::{MediaDownloader, SharedTransport},
    model::{
        MAX_TWEETS, MediaState, MediaStates, SelectionData, TweetAge, TweetItem, TweetMediaItem,
        TweetReferenceItem, TweetSnapshot, absolute_time, retained_hints, tweet_age, unshown,
    },
    service::{
        TweetProtocol, TweetRequest, TweetResponse, TweetScope, TweetService, TweetStatus,
        TweetStreamError, TweetStreamMessage, TweetUpdate,
    },
};

/// The height a row is estimated at until the list has measured one.
///
/// A tweet is a face, a header line, a paragraph and sometimes a gallery; a couple of
/// rows either side of the viewport keeps a row from appearing a moment after it was
/// scrolled to. The list replaces the estimate with the measured height.
const ROW_ESTIMATE: f32 = 240.0;

/// The size of the face a timeline row is drawn with.
const AVATAR_SIZE: f32 = 40.0;

/// The size of the face a quoted tweet is drawn with.
const QUOTE_AVATAR_SIZE: f32 = 28.0;

/// How wide a gallery of several pictures is laid out, and how tall.
const GALLERY_WIDE_WIDTH: f32 = 510.0;
const GALLERY_HEIGHT: f32 = 320.0;

/// The size a tweet's body text is drawn at, from the kit's size ladder (the theme has
/// no body typography token yet; the ladder is the same source kit controls read).
const BODY_SIZE: UnifiedSize = UnifiedSize::Medium;

/// The size secondary lines (handles, times, quoted text) are drawn at.
const META_SIZE: UnifiedSize = UnifiedSize::Small;

/// The length of the trail of the bounded feed: the newest 50 tweets, as before.
const FEED_CAPACITY: usize = MAX_TWEETS;

/// The longest search string the service accepts, in bytes.
const MAX_SEARCH_QUERY_BYTES: usize = 100;

/// The X widget carries no options.
///
/// What a pane follows is the account's own list of monitored accounts, which is read
/// from the service rather than configured here, so there is nothing to persist but the
/// schema version.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct XConfig;

impl WidgetConfig for XConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports.
#[derive(Clone, Debug, Eq, PartialEq)]
enum XFailure {
    Stream(TweetStreamError),
    Rpc(RpcCallError<XApiError>),
}

impl TranslatedError for XFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

/// What the dialog is doing, or waiting for.
///
/// A search is not here: it is a procedure of the dialog's own page rather than
/// something the accounts are waiting for, and it is carried by the pane apart from what
/// it waits on the accounts with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionOperation {
    /// The accounts are being read for a dialog that has none to show.
    Loading,
    /// The accounts are being read for a dialog that is already showing some.
    Refreshing,
    /// An account is being monitored.
    Adding,
    /// An account is being left.
    Deleting,
}

impl SelectionOperation {
    /// Whether this operation is a read rather than a change.
    pub(super) const fn is_read(self) -> bool {
        matches!(self, Self::Loading | Self::Refreshing)
    }
}

/// What the dialog is waiting for.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionPending {
    operation: SelectionOperation,
    /// What the operation is about: the username being monitored or the id being left.
    target: Option<SmolStr>,
    /// Whether the request is out.
    ///
    /// A read asked for before the stream is live is held here instead: there is no
    /// session behind the account yet, and the readiness that arrives later is what
    /// makes the read possible.
    sent: bool,
}

/// What the last change said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionNotice {
    /// The account is now being monitored.
    Added,
    /// Why the last change did not happen, or did not happen for certain.
    Problem(SelectionFailure),
}

/// Why a change to the monitored accounts did not happen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionFailure {
    /// The service refused the change.
    Rejected(XApiError),
    /// The change may or may not have been made, which is what the service answers when
    /// it could not tell.
    Unknown(XMutationUnknown),
    /// The call itself failed.
    Rpc(RpcCallError<XApiError>),
}

impl TranslatedError for SelectionFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Rejected(error) => error.translated_message(translate),
            Self::Unknown(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

/// One command this pane sent and has not seen the answer to.
///
/// The binding replaces its one command slot on every call and aborts the call it
/// replaced, but a reply can already be queued when that happens. The identity the
/// binding returned is what says whose answer arrived: only a reply tracked here may
/// steer the pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingCommand {
    /// A read: `Load` or `Retry`.
    Start,
    /// The stop that lets go of the stream an account left behind.
    Stop,
    /// A read of the monitored accounts.
    FetchSelections,
    /// Whether a dialog is showing the accounts' avatars.
    WatchSelections,
    /// A search of X.
    Search,
    /// Monitoring one more account.
    Add,
    /// Leaving one account.
    Delete,
    /// Fetching one picture again.
    RetryMedia,
}

/// The image resources the rows and the dialog draw.
type ImageMap = HashMap<SmolStr, ImageSource>;

/// The bytes-to-resource handles one pane has registered, shared with its row builders.
type RowImages = Rc<RefCell<ImageMap>>;

/// The icon resources one pane has registered, by kind.
///
/// A row builder has no `App` and therefore cannot call [`IconSpec::build_themed`]; the
/// pane registers the icon PNGs once and hands the row builders the resource ids, and
/// the image nodes take their colour from `bind_style`'s `foreground` channel.
#[derive(Clone)]
struct RowIcons {
    repost: ImageSource,
    verified: ImageSource,
    warning: ImageSource,
    play: ImageSource,
    retry: ImageSource,
}

/// One picture a row draws, resolved from the model at accept time.
#[derive(Clone, Debug, PartialEq)]
struct MediaPlan {
    url: SmolStr,
    kind: MediaKind,
    ratio: f32,
    /// Whether this is a still frame standing in for an animation that is not fetched.
    poster_only: bool,
}

/// Which of the kinds of picture one tile is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaKind {
    Image,
    Video,
    Gif,
}

/// What one frame's row draws, resolved at accept time.
///
/// Rows are built by a list closure that has no `App`, so everything structural —
/// whether there is text, how many pictures there are, what is quoted — is a value in
/// this plan rather than a read of the frame. The dynamic parts (ages, labels) are
/// `bind_*` closures over the same plan signal.
#[derive(Clone, Debug, PartialEq)]
struct RowPlan {
    tweet_id: SmolStr,
    /// The top-level row this plan belongs to (its own id, or the quoting tweet's id
    /// when this plan is the quoted tweet underneath it).
    row_id: SmolStr,
    /// Whether this plan is a quoted tweet rather than the row's own.
    quote_of: bool,
    /// The URL a click on the row opens.
    url: SmolStr,
    /// Who reposted this timeline entry, when it is one.
    reposted_by: Option<SmolStr>,
    author_name: SmolStr,
    author_handle: SmolStr,
    avatar_url: SmolStr,
    verified: bool,
    /// The timestamp the row's age is derived from.
    published_at: i64,
    text: SmolStr,
    /// Whether the tweet says anything at all (an empty body is a gallery-only tweet).
    has_text: bool,
    /// Byte ranges of `text` that are links, with the URLs they open.
    links: Rc<[(Range<usize>, SmolStr)]>,
    media: Arc<[MediaPlan]>,
    quote: Option<Box<RowPlan>>,
}

/// The images and links a row's descendants are, so the pane's `dispatch` can tell what
/// a pointer landed on.
#[derive(Clone, Default)]
struct RowHits {
    /// Row root → the tweet URL a click opens.
    rows: Rc<RefCell<HashMap<NodeId, SmolStr>>>,
    /// Text node → the link ranges inside it.
    links: Rc<RefCell<HashMap<NodeId, Rc<[(Range<usize>, SmolStr)]>>>>,
    /// A failed picture's tile → the URL to fetch again.
    failed: Rc<RefCell<HashMap<NodeId, SmolStr>>>,
}

impl RowHits {
    fn clear(&self) {
        self.rows.borrow_mut().clear();
        self.links.borrow_mut().clear();
        self.failed.borrow_mut().clear();
    }
}

/// The state a list row builder is handed, beyond its parent and index.
#[derive(Clone)]
struct RowContext {
    plans: Rc<RefCell<Arc<[RowPlan]>>>,
    plan_signal: Signal<Arc<[RowPlan]>>,
    language: Signal<LanguageCode>,
    config: Signal<SettingsConfig>,
    theme: ThemeHandle,
    translate: Translate,
    images: RowImages,
    /// The picture states the rows draw, mirrored from the feed's media map.
    media: Rc<RefCell<MediaStates>>,
    icons: RowIcons,
    hits: RowHits,
}

impl RowContext {
    fn media(&self) -> MediaStates {
        Arc::clone(&self.media.borrow())
    }
}

/// One pane: the timeline it holds, and the accounts it manages.
pub struct XWidget {
    /// The executor bridge, absent on a host that installed no Kytrix service.
    binding: Option<WidgetBinding<TweetProtocol>>,
    /// The commands in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    /// Opens a URL through the application's own command.
    open_url: Rc<dyn Fn(&str)>,
    /// The account the pane reads for, which is its whole scope.
    account_id: Option<SmolStr>,
    /// The tweets the pane is showing, once a stream has published them.
    feed: Option<TweetSnapshot>,
    /// What is known about every picture the feed wants.
    media: MediaStates,
    /// The same picture states as a shared cell the row builders read (they have no
    /// `App` to read a signal with).
    media_mirror: Rc<RefCell<MediaStates>>,
    /// How many updates the stream this feed came from has published.
    revision: u64,
    /// Whether the subscription has reported itself live.
    ///
    /// It is what keeps a read of the monitored accounts from being sent before there
    /// is a session to answer it, and a change from being sent while the stream carrying
    /// the account is down.
    ready: bool,
    /// What the list's rows are drawn from, shared with the row builders.
    plans: Rc<RefCell<Arc<[RowPlan]>>>,
    /// The same plans as an engine signal, so a row's text can follow the language and
    /// the display time zone without rebuilding the row.
    plan_signal: Signal<Arc<[RowPlan]>>,
    /// The picture state the rows were last built against, so a media change rebuilds
    /// the visible window.
    built_media: Option<MediaStates>,
    /// The image resources registered for the pictures on screen.
    images: RowImages,
    /// The icon resources the rows draw, registered once in `body`.
    icons: Option<RowIcons>,
    /// What a pointer lands on inside the feed.
    hits: RowHits,
    /// The list itself, built by [`WidgetView::body`], where its parent is known.
    list: Option<List>,
    /// The root of the body this pane built.
    body_root: Option<NodeId>,
    /// The stand-in a feed with nothing in it draws.
    empty: Option<NodeId>,
    /// The language the stand-in was built in.
    empty_language: Option<LanguageCode>,
    /// The accounts the account monitors, as the service last answered them.
    selections: Option<SelectionData>,
    /// The query the binding is carrying a search of X for, if it is carrying one.
    searching: Option<SmolStr>,
    /// What the dialog is waiting for, if anything.
    selection_pending: Option<SelectionPending>,
    /// What the dialog says about the last change.
    notice: Option<SelectionNotice>,
    /// The username of every monitored account that came back without a profile.
    hints: Arc<HashMap<SmolStr, SmolStr>>,
    /// The open dialog, if one is open.
    dialog: Option<TweetDialog>,
    /// The control that opens the monitored-accounts dialog.
    manage_button: Option<Button>,
    /// The header group the control lives in, so a rebuild can find its parent.
    manage_parent: Option<NodeId>,
    /// What the control was built with: the language, and whether an account scopes the
    /// pane. A difference is what makes the control rebuild.
    manage_built: Option<(LanguageCode, bool)>,
    /// Opens the dialog request: the control's handler has no widget to borrow, so it
    /// asks the next `sync` for the open instead of performing it.
    open_dialog_requests: Signal<u64>,
    seen_open_requests: u64,
}

impl WidgetView for XWidget {
    type Config = XConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.x");
    const DISPLAY_NAME: &'static str = "widget.names.x";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.x");
    const ICON: Option<&'static str> = Some("widgets/x.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        // Pictures are fetched by this pane rather than by the service they belong to,
        // and the transport they are fetched over is the application's own build policy:
        // the two X image origins are the only hosts a picture URL may name, and they
        // are always reached over HTTPS.
        let config: NetworkConfig = cx.config_ref(|config| config.network_config());
        let transport = SharedTransport::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            config,
        );
        let binding = source.map(|source| {
            WidgetBinding::spawn(
                cx,
                TweetService::new(source, MediaDownloader::new(transport)),
            )
        });

        Ok(Self::assemble(binding, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the x pane could not start: {error}");
        }
    }

    /// A pane is content once a stream has published a timeline for it.
    ///
    /// An empty timeline is content: the pane has a feed, it is just a feed with nothing
    /// in it yet, and the body says so with `common.no_data` rather than leaving the
    /// framework's placeholder to call it waiting.
    fn has_content(&self) -> bool {
        self.feed.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_TWEET_BODY")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);
        self.icons = Some(register_row_icons(cx)?);
        self.list = Some(self.make_list(cx, root)?);
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The settings are the pane's scope: signing in as someone else is another
        // timeline, and signing out is no timeline at all.
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for message in messages {
            match message {
                Inbound::Reliable { key, message } => {
                    let _ = self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    let _ = self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }

        // Pictures whose bytes the stream published are registered before anything
        // draws them: the row builders read this map, and the decode happens after.
        self.register_media(cx);

        let requests = *cx.read(&self.open_dialog_requests);
        if requests != self.seen_open_requests {
            self.seen_open_requests = requests;
            self.open_dialog(cx)?;
        }
        self.sync_dialog(cx)?;

        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_empty_state(cx)?;
        self.sync_manage_button(cx)?;

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is
        // one frame away, so ask for that frame.
        if self.feed.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        Ok(())
    }

    /// The control that opens the monitored-accounts dialog.
    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Left {
            return Ok(());
        }
        self.release_manage_button(cx, true)?;
        self.build_manage_button(parent, cx)
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(InputEvent::PointerUp {
            position, button, ..
        }) = event
        else {
            return false;
        };
        if *button != PointerButton::Primary {
            return false;
        }
        let Some(hit) = cx.tree().hit_test(*position) else {
            return false;
        };

        // A link inside a tweet's text is the most specific target: the byte the pointer
        // is over names which link (if any) was clicked.
        let links = self.hits.links.borrow().get(&hit.node).cloned();
        if let Some(links) = links {
            let byte = cx
                .window_mut()
                .text_byte_index_at(hit.node, hit.local_point)
                .ok();
            if let Some(byte) = byte
                && let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&byte))
            {
                (self.open_url)(url);
                return true;
            }
        }

        // A failed picture's tile asks for it again rather than opening the tweet. The
        // hit is one of the tile's children (its icon), so the tile is found by walking
        // up to the nearest ancestor the pane registered.
        let failed_url = {
            let failed = self.hits.failed.borrow();
            let mut node = Some(hit.node);
            let mut found = None;
            while let Some(current) = node {
                if let Some(url) = failed.get(&current) {
                    found = Some(url.clone());
                    break;
                }
                node = cx.tree().get(current).ok().and_then(|node| node.parent());
            }
            found
        };
        if let Some(url) = failed_url {
            self.retry_media(url, cx);
            return true;
        }

        // Otherwise the row the pointer is in opens its tweet. The hit is a descendant
        // of the row container, so the row is the nearest ancestor the pane registered.
        let row_url = {
            let rows = self.hits.rows.borrow();
            let mut node = Some(hit.node);
            let mut found = None;
            while let Some(current) = node {
                if let Some(url) = rows.get(&current) {
                    found = Some(url.clone());
                    break;
                }
                node = cx.tree().get(current).ok().and_then(|node| node.parent());
            }
            found
        };
        let Some(url) = row_url else {
            return false;
        };
        (self.open_url)(&url);
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        self.pending.clear();
        // The header's nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handler are this widget's to release.
        let _ = self.release_manage_button(cx, false);
        if let Some(mut dialog) = self.dialog.take() {
            dialog.release(cx);
        }
        // The list documents its own teardown order: its nodes first, then the component.
        if let Some(root) = self.body_root.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        if let Some(list) = self.list.take() {
            let _ = list.release(cx.app_mut());
        }
        self.feed = None;
        self.hits.clear();
        self.images.borrow_mut().clear();
        self.icons = None;
        self.empty = None;
        self.empty_language = None;
    }
}

impl XWidget {
    /// The seam the framework and the tests build a pane through.
    ///
    /// The pane has no opinion about where its timeline comes from, so the binding is an
    /// argument: the production constructor answers it with the injected Kytrix service
    /// connection, and a test with a fake subscription.
    fn assemble(
        binding: Option<WidgetBinding<TweetProtocol>>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let open_url = Rc::clone(&cx.services().open_url);
        let plans: Arc<[RowPlan]> = Arc::from([]);
        let plans = Rc::new(RefCell::new(plans));
        let plan_signal = cx.signal(Arc::clone(&plans.borrow()));
        let open_dialog_requests = cx.signal(0_u64);
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            open_url,
            account_id,
            feed: None,
            media: Arc::new(HashMap::new()),
            media_mirror: Rc::new(RefCell::new(Arc::new(HashMap::new()))),
            revision: 0,
            ready: false,
            plans,
            plan_signal,
            built_media: None,
            images: Rc::new(RefCell::new(HashMap::new())),
            icons: None,
            hits: RowHits::default(),
            list: None,
            body_root: None,
            empty: None,
            empty_language: None,
            selections: None,
            searching: None,
            selection_pending: None,
            notice: None,
            hints: Arc::new(HashMap::new()),
            dialog: None,
            manage_button: None,
            manage_parent: None,
            manage_built: None,
            open_dialog_requests,
            seen_open_requests: 0,
        }
    }

    /// A pane over a source the caller answers, for the crate's own tests.
    #[cfg(test)]
    fn new_with_source<S, T>(
        source: S,
        downloader: MediaDownloader<T>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self
    where
        S: super::service::TweetSource,
        T: super::media::MediaTransport,
    {
        let binding = WidgetBinding::spawn(cx, TweetService::new(source, downloader));
        Self::assemble(Some(binding), status, recovery, cx)
    }

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<TweetScope> {
        TweetScope::new(self.account_id.as_ref())
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the timeline the user is still reading — blanking a feed to
    /// reconnect would flash it away — while an account change starts over, because the
    /// pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_no_content_notice(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's
            // behalf: an account that signed out must not keep a seat it is not using.
            self.hand_over(TweetRequest::Stop, PendingCommand::Stop, cx)?;
            cx.request_frame()?;
            return Ok(());
        };
        self.ready = false;
        // Every stream numbers its own updates from the beginning, so what this pane last
        // applied belongs to the stream being replaced rather than to the one starting
        // here.
        self.revision = 0;
        if !retry {
            self.clear(cx)?;
        }
        // Starting a stream replaces whatever the binding was awaiting, so a change that
        // was in flight is dropped rather than remembered.
        self.selection_pending = None;
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let request = if retry {
            TweetRequest::Retry(scope)
        } else {
            TweetRequest::Load(scope)
        };
        let Some(binding) = self.binding.as_mut() else {
            self.fail(TweetStreamError::Service(KytrixServiceError::Closed), cx);
            return Ok(());
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, PendingCommand::Start);
        cx.request_frame()?;

        Ok(())
    }

    /// Drops what a pane is leaving behind.
    fn clear(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.feed = None;
        self.media = Arc::new(HashMap::new());
        *self.media_mirror.borrow_mut() = Arc::clone(&self.media);
        self.built_media = None;
        self.revision = 0;
        self.ready = false;
        self.replace_plans(Arc::from([]), cx);
        self.hits.clear();
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), 0)?;
        }
        self.selections = None;
        self.searching = None;
        self.selection_pending = None;
        self.notice = None;
        self.hints = Arc::new(HashMap::new());
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

        Ok(())
    }

    /// Reports the notice that stands in while the pane has nothing to show.
    fn report_no_content_notice(&self, cx: &mut WidgetCx<'_>) {
        let notice = if self.current_scope().is_none() {
            Notice::message("widget.x.logged_out")
        } else {
            Notice::message("widget.x.waiting")
        };
        self.status
            .report(cx.app_mut(), WidgetState::waiting(notice));
    }

    /// Reports the state of a pane whose stream is live.
    fn report_ready(&self, cx: &mut WidgetCx<'_>) {
        if self.feed.is_some() {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        } else if self.current_scope().is_some() {
            self.status.report(cx.app_mut(), WidgetState::Loading);
        } else {
            self.report_no_content_notice(cx);
        }
    }

    /// Reports why a subscription stopped.
    fn fail(&self, error: TweetStreamError, cx: &mut WidgetCx<'_>) {
        let failure = Arc::new(XFailure::Stream(error));
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(failure, self.recovery.clone()),
        );
    }

    /// Applies one RPC reply, once its identity says it answers the command in flight.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_load_result(result, cx),
            Some(PendingCommand::FetchSelections) => self.apply_selections_result(result, cx),
            Some(PendingCommand::Search) => self.apply_search_result(result, cx),
            Some(PendingCommand::Add) => self.apply_add_result(result, cx),
            Some(PendingCommand::Delete) => self.apply_delete_result(result, cx),
            // The stop, the watch and a picture fetched again are calls the pane does not
            // report on: what they answer is either nothing or drawn on the stream.
            Some(
                PendingCommand::Stop | PendingCommand::WatchSelections | PendingCommand::RetryMedia,
            ) => {}
            None => {
                log::debug!("the x pane ignored reply {request_id}: no command is waiting for it")
            }
        }
    }

    fn apply_load_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            self.ready = false;
            // A read the dialog is waiting for is answered by the account being taken, so
            // it does not outlive the failure of the request that would have taken it.
            self.selection_pending = None;
            let failure = Arc::new(XFailure::Rpc(error));
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(failure, self.recovery.clone()),
            );
            let _ = cx.request_frame();
        }
    }

    /// Applies the timeline a subscription published.
    fn apply_latest(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let TweetStreamMessage::Update(update) = message.as_ref() else {
            return false;
        };
        // The latest channel coalesces, so an update can arrive after a newer one already
        // has: what a stream published before the last thing this pane applied is not a
        // timeline any more.
        if update.revision() <= self.revision {
            return false;
        }
        if let Err(error) = self.accept(update, cx) {
            log::error!("the x pane could not take the timeline it was published: {error}");
            return false;
        }
        if self.ready {
            self.report_ready(cx);
        }
        true
    }

    /// Applies a status the stream reported.
    fn apply_reliable(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let TweetStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            TweetStatus::Connecting => {
                self.ready = false;
                // The timeline already read stays: a reconnecting feed is not a reason to
                // blank what a reader was looking at.
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            TweetStatus::Ready => {
                self.ready = true;
                self.report_ready(cx);
                // What a pane shows is only as true as the read it came from, and a change
                // the pane stopped waiting for was never read back. The stream coming up
                // is what makes reading the accounts possible again.
                if self.selection_pending.is_some() {
                    let _ = self.send_pending_selections(cx);
                }
                if self.dialog.is_some() {
                    // A search the dialog had out is displaced by the read this asks for:
                    // a page waiting for an answer the read took the place of would wait
                    // forever.
                    if let Err(error) = self.watch_selections(true, cx) {
                        log::error!("the x pane could not watch the accounts: {error}");
                    }
                    if self.selection_pending.is_none() {
                        self.load_selections(cx);
                    }
                }
            }
            TweetStatus::Failed(error) => {
                self.ready = false;
                // A read that never went out has nothing left to wait for: the stream it
                // was waiting for failed, and the dialog says so rather than spinning.
                if self
                    .selection_pending
                    .as_ref()
                    .is_some_and(|pending| !pending.sent)
                {
                    self.selection_pending = None;
                }
                self.fail(error.clone(), cx);
            }
        }
        let _ = cx.request_frame();

        false
    }

    /// Tells the list about the tweets this feed added.
    fn accept(&mut self, update: &TweetUpdate, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let unshown = unshown(
            self.feed.as_ref().map_or(&[], TweetSnapshot::items),
            update.feed().items(),
        );
        let media_changed = match self.built_media.as_ref() {
            Some(built) => !Arc::ptr_eq(built, update.media()),
            None => true,
        };
        self.feed = Some(update.feed().clone());
        self.media = update.media().clone();
        *self.media_mirror.borrow_mut() = Arc::clone(&self.media);
        self.revision = update.revision();
        let plans = build_plans(update.feed());
        self.replace_plans(plans, cx);
        self.register_media(cx);

        let Some(list) = self.list.as_ref() else {
            // The body is built from `has_content`, which the framework asks before this
            // hook: the frame that builds it reads the count from the list it creates.
            cx.request_frame()?;
            return Ok(());
        };
        let count = update.feed().items().len();
        match unshown {
            // The oldest of the tweets it has not been shown goes in first, so the newest
            // of them ends up where a list that draws its newest tweet first draws it.
            Some(pushed) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            // Nothing the list holds survives, so there is no edit it can be told: the
            // list is rebuilt for the feed it has now, and the reader starts over on the
            // tweet that feed draws first.
            None => list.set_items(cx.app_mut(), count)?,
        }
        if media_changed {
            list.set_count(cx.app_mut(), count)?;
            self.built_media = Some(Arc::clone(update.media()));
        }
        cx.request_frame()?;

        Ok(())
    }

    /// Replaces the plans the rows are built from, and the signal their text follows.
    fn replace_plans(&mut self, plans: Arc<[RowPlan]>, cx: &mut WidgetCx<'_>) {
        *self.plans.borrow_mut() = Arc::clone(&plans);
        cx.write(&self.plan_signal, plans);
    }

    /// Registers every picture the stream says is on disk, once.
    fn register_media(&mut self, cx: &mut WidgetCx<'_>) {
        let mut registered = false;
        for (url, state) in self.media.iter() {
            if !matches!(state, MediaState::Ready(_)) {
                continue;
            }
            if self.images.borrow().contains_key(url) {
                continue;
            }
            let MediaState::Ready(path) = state else {
                continue;
            };
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    log::warn!("the x pane could not read a cached picture: {error}");
                    continue;
                }
            };
            if bytes.is_empty() {
                continue;
            }
            let pending = cx
                .app_mut()
                .resources_mut()
                .insert_async(Bytes::from(bytes));
            self.images
                .borrow_mut()
                .insert(url.clone(), ImageSource::Pending(pending));
            registered = true;
        }
        if registered && let Some(list) = &self.list {
            let count = list.count(cx.app());
            if count > 0 {
                let _ = list.set_count(cx.app_mut(), count);
            }
        }
    }

    /// Builds the list the body draws its feed with.
    fn make_list(&mut self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<List, EngineError> {
        let context = RowContext {
            plans: Rc::clone(&self.plans),
            plan_signal: self.plan_signal,
            language: cx.language(),
            config: cx.services().config,
            theme: cx.theme().clone(),
            translate: cx.translate().clone(),
            images: Rc::clone(&self.images),
            media: Rc::clone(&self.media_mirror),
            icons: self
                .icons
                .clone()
                .expect("the row icons are registered before the list is built"),
            hits: self.hits.clone(),
        };
        let count = self.feed.as_ref().map_or(0, |feed| feed.items().len());
        let key = format_smolstr!("XList:{}", parent.to_bits());
        let theme = cx.theme().clone();
        let window = cx.window();
        let list = List::new(
            cx.app_mut(),
            theme,
            key,
            ListOptions::variable(ROW_ESTIMATE)
                .deque(FEED_CAPACITY)
                .reverse()
                .keep_on_newest()
                .separator()
                .selection(SelectionMode::None)
                .padding(0.0, 0.0),
            move |tree, row_parent, index| build_row(tree, row_parent, index, &context),
        );
        list.set_initial_count(cx.app_mut(), count);
        self.built_media = Some(Arc::clone(&self.media));
        list.mount(cx.app_mut(), window, parent)?;

        Ok(list)
    }

    /// Shows `common.no_data` over a feed that holds nothing.
    ///
    /// The stand-in is the widget's, not the framework's: an empty timeline is content,
    /// so this is the body saying there is nothing in it rather than a placeholder
    /// saying there is no body. Its copy is static, so it is built for the language
    /// showing now and rebuilt when that moves.
    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self.feed.as_ref().is_some_and(TweetSnapshot::is_empty);
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node)?;
                self.empty_language = None;
            }
            return Ok(());
        }

        let language = *cx.read(&cx.language());
        let stale = self
            .empty
            .is_none_or(|_| self.empty_language != Some(language));
        if !stale {
            return Ok(());
        }
        if let Some(node) = self.empty.take() {
            remove_own_node(cx, node)?;
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };
        let overlay = NodeBuilder::boxed()
            .id("TEST_TWEET_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let text = t_smol!(cx.translate(), "common.no_data");
        EmptyState::new(cx.theme().clone(), "XTweetEmpty")
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);

        Ok(())
    }

    /// Builds the pane's manage control into `parent`.
    fn build_manage_button(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let enabled = self.account_id.is_some();
        let label = t_smol!(cx.translate(), "widget.x.manage");
        let key = cx.unique_key("x.header.manage");
        let theme = cx.theme().clone();
        let window = cx.window();
        let requests = self.open_dialog_requests;

        let mut button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::Faders)
            .a11y_label(label.clone())
            .tooltip_text(label);
        if enabled {
            button = button.on_click(move |ctx| {
                // A button handler owns no widget; it asks the next sync for the open.
                ctx.update(&requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        } else {
            button = button.disabled(true);
        }
        button.build(cx.app_mut(), window, parent)?;

        self.manage_button = Some(button);
        self.manage_parent = Some(parent);
        self.manage_built = Some((language, enabled));

        Ok(())
    }

    /// Rebuilds the manage control when the language or the account moved.
    fn sync_manage_button(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.as_ref() else {
            return Ok(());
        };
        button.sync(cx.app_mut())?;
        let state = (*cx.read(&cx.language()), self.account_id.is_some());
        if self.manage_built == Some(state) {
            return Ok(());
        }
        let Some(parent) = self.manage_parent else {
            return Ok(());
        };
        // The pane may have rebuilt the header row since: its group is not this widget's
        // node, so a stale parent means the control already went with it.
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }
        self.release_manage_button(cx, true)?;
        self.build_manage_button(parent, cx)
    }

    /// Removes the control's node (when the widget owns the teardown) and releases the
    /// component's scope and action handler.
    fn release_manage_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.take() else {
            return Ok(());
        };
        if remove_node && let Some(node) = button.node() {
            remove_own_node(cx, node)?;
        }
        let _ = button.release(cx.app_mut());
        self.manage_parent = None;
        self.manage_built = None;

        Ok(())
    }

    /// Hands one request to the service, of which the binding carries one at a time.
    ///
    /// The requests that come here are the ones the pane is not waiting for an answer to
    /// — a stream watched for a dialog that opened, a picture fetched again, a stream
    /// stopped — so what the binding was carrying is taken over, and the dialog's waiter
    /// for it is closed out first.
    fn hand_over(
        &mut self,
        request: TweetRequest,
        command: PendingCommand,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        self.drop_displaced(cx);
        let Some(binding) = self.binding.as_mut() else {
            return Ok(());
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
        Ok(())
    }

    /// Closes out what a call the pane is not waiting on takes the place of.
    fn drop_displaced(&mut self, cx: &mut WidgetCx<'_>) {
        let search = self.searching.take().is_some();
        let sent = self
            .selection_pending
            .as_ref()
            .is_some_and(|pending| pending.sent);
        if sent {
            self.selection_pending = None;
        }
        if search || sent {
            let _ = cx.request_frame();
        }
    }

    /// Says whether a dialog is showing the monitored accounts.
    fn watch_selections(
        &mut self,
        watched: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            return Ok(());
        };
        if !self.ready {
            return Ok(());
        }
        self.hand_over(
            TweetRequest::WatchSelections { scope, watched },
            PendingCommand::WatchSelections,
            cx,
        )
    }

    /// Reads the accounts the account monitors.
    ///
    /// A read goes over a live stream, because a live stream is what says the service is
    /// holding the account: before it there is no session to answer one. A read asked
    /// for too early waits here instead, and the readiness that arrives later is what
    /// sends it.
    pub(super) fn load_selections(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if self.selection_pending.is_some() {
            return false;
        }
        self.selection_pending = Some(SelectionPending {
            operation: if self.selections.is_some() {
                SelectionOperation::Refreshing
            } else {
                SelectionOperation::Loading
            },
            target: None,
            sent: false,
        });
        self.notice = None;
        if self.ready {
            let _ = self.send_selections(scope, cx);
        }
        let _ = cx.request_frame();
        true
    }

    /// Sends the read a dialog is waiting for.
    fn send_pending_selections(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            return Ok(());
        };
        if self
            .selection_pending
            .as_ref()
            .is_some_and(|pending| !pending.sent && pending.operation.is_read())
        {
            self.send_selections(scope, cx)?;
            cx.request_frame()?;
        }
        Ok(())
    }

    fn send_selections(
        &mut self,
        scope: TweetScope,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if let Some(pending) = self.selection_pending.as_mut() {
            pending.sent = true;
        }
        let Some(binding) = self.binding.as_mut() else {
            return Ok(());
        };
        let request_id = binding.call(cx, TweetRequest::FetchSelections(scope));
        self.pending
            .track(request_id, PendingCommand::FetchSelections);
        Ok(())
    }

    /// Searches X for an account to monitor.
    pub(super) fn search_users(&mut self, query: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        let query = SmolStr::new(query.trim());
        if !self.may_change(cx) || !valid_search_query(&query) {
            return false;
        }
        let Some(binding) = self.binding.as_mut() else {
            return false;
        };
        self.searching = Some(query.clone());
        let _ = cx.request_frame();
        let request_id = binding.call(cx, TweetRequest::SearchUsers { scope, query });
        self.pending.track(request_id, PendingCommand::Search);
        true
    }

    /// Monitors one more account.
    pub(super) fn add_selection(&mut self, username: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        let Some(data) = self.selections.as_ref() else {
            return false;
        };
        if data.quota_full() || data.contains_username(&username, &self.hints) {
            return false;
        }
        if self.binding.is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Adding, Some(username.clone()), cx);
        let binding = self
            .binding
            .as_mut()
            .expect("the binding was checked before the change began");
        let request_id = binding.call(cx, TweetRequest::AddSelection { scope, username });
        self.pending.track(request_id, PendingCommand::Add);
        true
    }

    /// Stops monitoring one account.
    pub(super) fn delete_selection(&mut self, x_user_id: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        if self.binding.is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Deleting, Some(x_user_id.clone()), cx);
        let binding = self
            .binding
            .as_mut()
            .expect("the binding was checked before the change began");
        let request_id = binding.call(cx, TweetRequest::DeleteSelection { scope, x_user_id });
        self.pending.track(request_id, PendingCommand::Delete);
        true
    }

    /// Fetches one picture again.
    fn retry_media(&mut self, url: SmolStr, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if let Err(error) = self.hand_over(
            TweetRequest::RetryMedia { scope, url },
            PendingCommand::RetryMedia,
            cx,
        ) {
            log::error!("the x pane could not ask for a picture again: {error}");
        }
    }

    /// Whether the pane is in a position to ask the service about the monitored accounts.
    fn may_change(&self, cx: &WidgetCx<'_>) -> bool {
        self.ready
            && self.selections.is_some()
            && self.selection_pending.is_none()
            && self.searching.is_none()
            && !self.status.is_failed(cx.app())
    }

    fn begin_change(
        &mut self,
        operation: SelectionOperation,
        target: Option<SmolStr>,
        cx: &mut WidgetCx<'_>,
    ) {
        self.selection_pending = Some(SelectionPending {
            operation,
            target,
            sent: true,
        });
        self.notice = None;
        let _ = cx.request_frame();
    }

    fn apply_selections_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        self.selection_pending = None;
        match result {
            Ok(TweetResponse::Selections(data)) => {
                // What is remembered is kept for the rows this read still has and has no
                // profile for, and for no others.
                self.hints = Arc::new(retained_hints(&self.hints, data.selections()));
                self.selections = Some(data);
                self.notice = None;
            }
            // The protocol types the answer, so anything else is a service that answered
            // the wrong question rather than a shape a reader could describe.
            Ok(_) => {}
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
            }
        }
        let _ = cx.request_frame();
    }

    /// Hands what a search answered to the dialog that asked for it.
    fn apply_search_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // An answer arrives only for the call the binding was carrying, so a search the
        // pane is no longer waiting for — the dialog was dismissed while it was out — has
        // none to hand over.
        let Some(query) = self.searching.take() else {
            return;
        };
        let answer = match result {
            Ok(TweetResponse::SearchResults(profiles)) => SearchAnswer::Found(profiles),
            Ok(_) => return,
            Err(error) => SearchAnswer::Failed(error),
        };
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.answered_search(cx, &query, answer);
        }
        let _ = cx.request_frame();
    }

    fn apply_add_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Added(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                let _ = cx.request_frame();
                return;
            }
        };
        self.notice = match outcome {
            // What the list becomes is the confirmation applied to it, and an account that
            // came back without a profile is named by what was asked for rather than
            // numbered.
            XMutationOutcome::Confirmed(selection) => {
                let grown = self
                    .selections
                    .as_ref()
                    .map(|data| data.with_added(selection.clone()));
                match grown {
                    Some(data) => {
                        self.selections = Some(data);
                        if selection.profile.is_none()
                            && let Some(username) = requested
                        {
                            Arc::make_mut(&mut self.hints)
                                .insert(selection.x_user_id, SmolStr::new(username.as_str()));
                        }
                        Some(SelectionNotice::Added)
                    }
                    None => None,
                }
            }
            XMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            // The change may or may not have been made, so the list the pane holds is not
            // one it can change: it is left as it was, and the next read settles it.
            XMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }

    fn apply_delete_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Deleted(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                let _ = cx.request_frame();
                return;
            }
        };
        self.notice = match outcome {
            XMutationOutcome::Confirmed(()) => match requested {
                Some(x_user_id) => {
                    self.selections = self
                        .selections
                        .as_ref()
                        .map(|data| data.with_deleted(&x_user_id));
                    Arc::make_mut(&mut self.hints).remove(&x_user_id);
                    None
                }
                None => None,
            },
            XMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            XMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }

    /// The pane's state, read for the frame the dialog draws it in.
    fn dialog_view(&self) -> DialogView {
        DialogView {
            signed_in: self.account_id.is_some(),
            ready: self.ready,
            selections: self.selections.clone(),
            searching: self.searching.clone(),
            operation: self
                .selection_pending
                .as_ref()
                .map(|pending| pending.operation),
            notice: self.notice.clone(),
            hints: Arc::clone(&self.hints),
            media: Arc::clone(&self.media),
            images: Rc::clone(&self.images),
            recovery: self.recovery.clone(),
        }
    }

    /// Opens the monitored-accounts dialog.
    fn open_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.dialog.is_some() {
            return Ok(());
        }
        let dialog = match TweetDialog::show(cx) {
            Ok(dialog) => dialog,
            Err(error) => {
                log::error!("the x pane could not open its account dialog: {error}");
                return Ok(());
            }
        };
        self.dialog = Some(dialog);
        if let Err(error) = self.watch_selections(true, cx) {
            log::error!("the x pane could not watch the accounts: {error}");
        }
        self.load_selections(cx);
        cx.request_frame()?;

        Ok(())
    }

    /// Syncs the open dialog: publishes this frame's state, takes the requests the user
    /// made through it, and closes it when it finishes.
    fn sync_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let finished = self.dialog.as_ref().is_some_and(TweetDialog::finished);
        if finished {
            return self.close_dialog(cx);
        }
        let view = self.dialog_view();
        let requests = {
            let Some(dialog) = self.dialog.as_mut() else {
                return Ok(());
            };
            if dialog.sync(cx, &view).is_err() {
                None
            } else {
                Some(dialog.take_requests(cx))
            }
        };
        let Some(requests) = requests else {
            return self.close_dialog(cx);
        };
        for request in requests {
            match request {
                DialogRequest::Reload => {
                    self.load_selections(cx);
                }
                DialogRequest::Search(query) => {
                    self.search_users(query, cx);
                }
                DialogRequest::Add(username) => {
                    self.add_selection(username, cx);
                }
                DialogRequest::Delete(x_user_id) => {
                    self.delete_selection(x_user_id, cx);
                }
            }
        }
        Ok(())
    }

    /// Takes the dialog down after it closed itself, and stops what it was the reason
    /// for.
    fn close_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(mut dialog) = self.dialog.take() else {
            return Ok(());
        };
        dialog.release(cx);
        self.searching = None;
        if let Err(error) = self.watch_selections(false, cx) {
            log::error!("the x pane could not stop watching the accounts: {error}");
        }
        cx.request_frame()?;

        Ok(())
    }
}

/// Registers the icons the rows draw, once per body.
fn register_row_icons(cx: &mut WidgetCx<'_>) -> Result<RowIcons, EngineError> {
    let mut register = |kind: IconKind| -> Result<ImageSource, EngineError> {
        let image = cx
            .app_mut()
            .resources_mut()
            .insert(raster(kind, RasterTier::Small48))?;
        Ok(ImageSource::Ready(image.id))
    };
    Ok(RowIcons {
        repost: register(IconKind::Repeat)?,
        verified: register(IconKind::CheckCircle)?,
        warning: register(IconKind::WarningCircle)?,
        play: register(IconKind::Play)?,
        retry: register(IconKind::ArrowsClockwise)?,
    })
}

/// Removes a node this pane built, if it is still in the tree.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

/// Whether one search string is one X could be asked about.
pub(super) fn valid_search_query(query: &str) -> bool {
    let query = query.trim();
    !query.is_empty()
        && query.len() <= MAX_SEARCH_QUERY_BYTES
        && !query.chars().any(char::is_control)
}

/// The URL of one tweet on X.
pub(super) fn tweet_url(username: &str, tweet_id: &str) -> String {
    format!("https://x.com/{username}/status/{tweet_id}")
}

/// Resolves the feed into the row plans the list draws.
fn build_plans(feed: &TweetSnapshot) -> Arc<[RowPlan]> {
    feed.items()
        .iter()
        .map(|item| {
            let author = feed.author(&item.x_user_id);
            plan_of(item, &author, feed)
        })
        .collect()
}

/// Resolves one tweet (and the tweet it quotes) into a row plan.
fn plan_of(
    item: &Arc<TweetItem>,
    outer_author: &super::model::TweetAuthor,
    feed: &TweetSnapshot,
) -> RowPlan {
    let (tweet, reposted) = item.displayed();
    let author = feed.author(&tweet.x_user_id);
    let links: Rc<[(Range<usize>, SmolStr)]> = tweet
        .text
        .links()
        .iter()
        .map(|link| (link.range.clone(), link.url.clone()))
        .collect();
    let tweet_id = SmolStr::new(tweet.tweet_id());
    let row_id = tweet_id.clone();
    RowPlan {
        tweet_id,
        row_id: row_id.clone(),
        quote_of: false,
        url: SmolStr::new(tweet_url(author.username.as_ref(), tweet.tweet_id())),
        reposted_by: reposted.then(|| SmolStr::new(outer_author.display_name.as_str())),
        author_name: SmolStr::new(author.display_name.as_str()),
        author_handle: format_smolstr!("@{}", author.username),
        avatar_url: SmolStr::new(author.avatar_url.as_str()),
        verified: author.verified,
        published_at: tweet.timestamp(),
        text: tweet.text.shared(),
        has_text: !tweet.text.is_empty(),
        links,
        media: media_plans(&tweet.media),
        quote: match tweet.reference.as_ref() {
            Some(TweetReferenceItem::Quote(quoted)) => {
                Some(Box::new(plan_of_quote(quoted, feed, row_id)))
            }
            _ => None,
        },
    }
}

/// Resolves one quoted tweet into a row plan (it has no repost line of its own).
fn plan_of_quote(tweet: &Arc<TweetItem>, feed: &TweetSnapshot, row_id: SmolStr) -> RowPlan {
    let author = feed.author(&tweet.x_user_id);
    let links: Rc<[(Range<usize>, SmolStr)]> = tweet
        .text
        .links()
        .iter()
        .map(|link| (link.range.clone(), link.url.clone()))
        .collect();
    RowPlan {
        tweet_id: SmolStr::new(tweet.tweet_id()),
        row_id,
        quote_of: true,
        url: SmolStr::new(tweet_url(author.username.as_ref(), tweet.tweet_id())),
        reposted_by: None,
        author_name: SmolStr::new(author.display_name.as_str()),
        author_handle: format_smolstr!("@{}", author.username),
        avatar_url: SmolStr::new(author.avatar_url.as_str()),
        verified: author.verified,
        published_at: tweet.timestamp(),
        text: tweet.text.shared(),
        has_text: !tweet.text.is_empty(),
        links,
        media: media_plans(&tweet.media),
        quote: None,
    }
}

/// Finds the plan a row's bind closure draws: the row's own plan, or the quoted plan
/// nested under it.
fn plan_in<'a>(
    plans: &'a [RowPlan],
    row_id: &SmolStr,
    tweet_id: &SmolStr,
    quote_of: bool,
) -> Option<&'a RowPlan> {
    if !quote_of {
        return plans.iter().find(|plan| plan.tweet_id == *tweet_id);
    }
    plans
        .iter()
        .find(|plan| plan.tweet_id == *row_id)
        .and_then(|plan| plan.quote.as_deref())
        .filter(|quote| quote.tweet_id == *tweet_id)
}

/// The pictures one tweet carries, resolved into what a row needs.
fn media_plans(media: &[TweetMediaItem]) -> Arc<[MediaPlan]> {
    media
        .iter()
        .map(|item| MediaPlan {
            url: item.url().clone(),
            kind: match item {
                TweetMediaItem::Image { .. } => MediaKind::Image,
                TweetMediaItem::Video { .. } => MediaKind::Video,
                TweetMediaItem::Gif { .. } => MediaKind::Gif,
            },
            ratio: item.aspect_ratio(),
            poster_only: matches!(
                item,
                TweetMediaItem::Gif {
                    poster_only: true,
                    ..
                }
            ),
        })
        .collect()
}

/// Builds one row of the timeline into the list's row container.
fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    context: &RowContext,
) -> Result<(), EngineError> {
    let plan = context.plans.borrow().get(index).cloned();
    let Some(plan) = plan else {
        return Ok(());
    };
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_TWEET_ROW:{}", plan.tweet_id))
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(6.0)
                .padding_sides(12.0, 12.0, 12.0, 12.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .bind_a11y(row_a11y(plan.tweet_id.clone(), context))
        .build(tree, parent)?;
    context.hits.rows.borrow_mut().insert(row, plan.url.clone());

    if let Some(reposted_by) = plan.reposted_by.clone() {
        build_repost_line(tree, row, reposted_by, context)?;
    }
    build_tweet(tree, row, &plan, context)?;

    Ok(())
}

/// The a11y description of one row: who wrote it, and how long ago.
fn row_a11y(
    tweet_id: SmolStr,
    context: &RowContext,
) -> impl Fn(&mut ReadCtx<'_>) -> A11yPatch + 'static {
    let plan_signal = context.plan_signal;
    let language = context.language;
    let config = context.config;
    let translate = context.translate.clone();
    move |read| {
        let _language = read.read(&language);
        let plans = read.read(&plan_signal);
        let settings = read.read(&config);
        let zone = widget::time_zone::data_time_zone(&settings);
        let Some(plan) = plans.iter().find(|plan| plan.tweet_id == tweet_id) else {
            return A11yPatch::default();
        };
        let age = tweet_age(plan.published_at, OffsetDateTime::now_utc(), zone);
        A11yPatch {
            label: Some(t_smol!(
                translate,
                "widget.x.tweet",
                author = plan.author_handle.clone(),
                time = age_text(age, &translate)
            )),
            description: Some(absolute_time(plan.published_at, zone)),
            ..A11yPatch::default()
        }
    }
}

/// The line above a repost that says who reposted it.
fn build_repost_line(
    tree: &mut NodeTree,
    parent: NodeId,
    reposted_by: SmolStr,
    context: &RowContext,
) -> Result<(), EngineError> {
    let line = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(4.0)
                .padding_sides(0.0, 0.0, 0.0, AVATAR_SIZE + 12.0)
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;
    build_icon(
        tree,
        line,
        context.icons.repost,
        14.0,
        context.theme.clone(),
        Role::FgMuted,
    )?;
    NodeBuilder::text(reposted_by, TextStyle::new(META_SIZE.text_size()))
        .style(StyleBuilder::new().min_width_0().build())
        .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
        .build(tree, line)?;

    Ok(())
}

/// One tweet's own content: its face, its author, its text, its pictures and what it
/// quotes.
fn build_tweet(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let content = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(12.0)
                .min_width_0()
                .width_full()
                .build(),
        )
        .build(tree, parent)?;
    build_avatar(tree, content, plan, context)?;

    let column = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .gap(6.0)
                .min_width_0()
                .build(),
        )
        .build(tree, content)?;
    build_tweet_header(tree, column, plan, context)?;
    if plan.has_text {
        build_text(tree, column, plan, context)?;
    }
    if !plan.media.is_empty() {
        build_gallery(tree, column, plan, context)?;
    }
    if let Some(quote) = plan.quote.as_deref() {
        build_quote(tree, column, quote, context)?;
    }

    Ok(())
}

/// A face, or the stand-in that says there is one to come.
fn build_avatar(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let size = if plan.quote_of {
        QUOTE_AVATAR_SIZE
    } else {
        AVATAR_SIZE
    };
    let style = StyleBuilder::new()
        .width(size)
        .height(size)
        .flex_none()
        .build();
    let image = if plan.avatar_url.is_empty() {
        None
    } else {
        context.images.borrow().get(&plan.avatar_url).copied()
    };
    let Some(image) = image else {
        // No bytes yet (or no face to fetch): a surface-toned tile with an `@` is what
        // the old interface drew for a face that had not arrived.
        let placeholder = NodeBuilder::boxed()
            .style(style)
            .bind_style(themed_background(
                context.theme.clone(),
                Role::SurfaceRaised,
            ))
            .build(tree, parent)?;
        let mut text_style = TextStyle::new(META_SIZE.text_size());
        text_style.alignment = TextAlign::Center;
        text_style.color = Color::WHITE;
        NodeBuilder::text(SmolStr::new_static("@"), text_style)
            .style(StyleBuilder::new().width_full().height_full().build())
            .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, placeholder)?;
        return Ok(());
    };
    let node = match image {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Cover),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Cover),
    };
    node.style(style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;

    Ok(())
}

/// Who wrote one tweet, when, and whether X vouches for them.
fn build_tweet_header(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let name_row = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(4.0)
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;

    NodeBuilder::text(
        plan.author_name.clone(),
        TextStyle {
            size: BODY_SIZE.text_size(),
            weight: 600,
            ..TextStyle::new(BODY_SIZE.text_size())
        },
    )
    .style(StyleBuilder::new().min_width_0().build())
    .bind_text({
        let plan_signal = context.plan_signal;
        let row_id = plan.row_id.clone();
        let tweet_id = plan.tweet_id.clone();
        let quote_of = plan.quote_of;
        move |read| {
            let plans = read.read(&plan_signal);
            plan_in(&plans, &row_id, &tweet_id, quote_of)
                .map_or_else(SmolStr::default, |plan| plan.author_name.clone())
        }
    })
    .bind_style(themed_foreground(context.theme.clone(), Role::FgDefault))
    .build(tree, name_row)?;

    if plan.verified {
        build_icon(
            tree,
            name_row,
            context.icons.verified,
            16.0,
            context.theme.clone(),
            Role::BrandBase,
        )?;
    }

    NodeBuilder::text(
        SmolStr::new_static("·"),
        TextStyle::new(META_SIZE.text_size()),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
    .build(tree, name_row)?;

    NodeBuilder::text(SmolStr::default(), TextStyle::new(META_SIZE.text_size()))
        .style(StyleBuilder::new().flex_none().build())
        .bind_text({
            let plan_signal = context.plan_signal;
            let config = context.config;
            let language = context.language;
            let translate = context.translate.clone();
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let _language = read.read(&language);
                let plans = read.read(&plan_signal);
                let settings = read.read(&config);
                let zone = widget::time_zone::data_time_zone(&settings);
                plan_in(&plans, &row_id, &tweet_id, quote_of).map_or_else(
                    SmolStr::default,
                    |plan| {
                        age_text(
                            tweet_age(plan.published_at, OffsetDateTime::now_utc(), zone),
                            &translate,
                        )
                    },
                )
            }
        })
        .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
        .build(tree, name_row)?;

    NodeBuilder::text(
        plan.author_handle.clone(),
        TextStyle::new(META_SIZE.text_size()),
    )
    .style(StyleBuilder::new().min_width_0().build())
    .bind_text({
        let plan_signal = context.plan_signal;
        let row_id = plan.row_id.clone();
        let tweet_id = plan.tweet_id.clone();
        let quote_of = plan.quote_of;
        move |read| {
            let plans = read.read(&plan_signal);
            plan_in(&plans, &row_id, &tweet_id, quote_of)
                .map_or_else(SmolStr::default, |plan| plan.author_handle.clone())
        }
    })
    .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
    .build(tree, parent)?;

    Ok(())
}

/// One tweet's text, with the runs that are links styled and clickable.
///
/// Link colours have no dynamic channel inside a run (the engine's runs are static
/// styles), so the text is drawn in one colour and the links are hit-tested by byte
/// range: the pane's `dispatch` turns a click over a link range into that link's URL.
/// The links are underlined so they are still visible as links.
fn build_text(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let mut style = TextStyle::new(if plan.quote_of {
        META_SIZE.text_size()
    } else {
        BODY_SIZE.text_size()
    });
    style.overflow = TextOverflow::Clip;
    let decoration = StylePatch::default();
    let links = Rc::clone(&plan.links);
    let node = NodeBuilder::text(plan.text.clone(), style)
        .style(StyleBuilder::new().min_width_0().build())
        .bind_text({
            let plan_signal = context.plan_signal;
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let plans = read.read(&plan_signal);
                plan_in(&plans, &row_id, &tweet_id, quote_of)
                    .map_or_else(SmolStr::default, |plan| plan.text.clone())
            }
        })
        .bind_style({
            let theme = context.theme.clone();
            move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..decoration.clone()
            }
        })
        .bind_a11y({
            let plan_signal = context.plan_signal;
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let plans = read.read(&plan_signal);
                let text = plan_in(&plans, &row_id, &tweet_id, quote_of)
                    .map_or_else(SmolStr::default, |plan| plan.text.clone());
                A11yPatch {
                    description: Some(text),
                    ..A11yPatch::default()
                }
            }
        })
        .build(tree, parent)?;
    if !links.is_empty() {
        context.hits.links.borrow_mut().insert(node, links);
    }

    Ok(())
}

/// A gallery of the pictures one tweet carries.
fn build_gallery(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let count = plan.media.len().min(4);
    let columns = if count == 1 { 1 } else { 2 };
    let tile_width = if count == 1 {
        GALLERY_WIDE_WIDTH.min(GALLERY_HEIGHT * plan.media[0].ratio)
    } else {
        (GALLERY_WIDE_WIDTH - 4.0) * 0.5
    };
    let rows = if count > 2 { 2 } else { 1 };
    let tile_height = GALLERY_HEIGHT / rows as f32;
    let grid = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(4.0)
                .width(tile_width * columns as f32 + 4.0 * (columns - 1) as f32)
                .max_width(GALLERY_WIDE_WIDTH)
                .build(),
        )
        .build(tree, parent)?;
    let mut index = 0;
    for _ in 0..rows {
        let line = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .gap(4.0)
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(tree, grid)?;
        let mut in_line = 0;
        while in_line < columns && index < count {
            build_media_tile(tree, line, plan, index, tile_width, tile_height, context)?;
            in_line += 1;
            index += 1;
        }
    }

    Ok(())
}

/// One picture: what is known about it, or the way to ask for it again.
fn build_media_tile(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    index: usize,
    width: f32,
    height: f32,
    context: &RowContext,
) -> Result<(), EngineError> {
    let item = &plan.media[index];
    let media = context.media();
    let state = media.get(&item.url);
    let style = StyleBuilder::new()
        .width(width)
        .height(height)
        .flex_none()
        .build();
    let tile = NodeBuilder::boxed()
        .id(format_smolstr!(
            "TEST_TWEET_MEDIA:{}:{index}",
            plan.tweet_id
        ))
        .style(style)
        .bind_style(themed_background(
            context.theme.clone(),
            Role::SurfaceRaised,
        ))
        .a11y(A11yOverride {
            // A still frame standing in for an animation says what it is; every other
            // picture is decoration the row's label already names.
            label: item.poster_only.then(|| SmolStr::new_static("GIF")),
            hidden: !item.poster_only,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;

    match state {
        Some(MediaState::Ready(_)) => {
            let image = context.images.borrow().get(&item.url).copied();
            if let Some(image) = image {
                let node = match image {
                    ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Cover),
                    ImageSource::Pending(pending) => {
                        NodeBuilder::image_pending(pending, ImageFit::Cover)
                    }
                };
                node.style(StyleBuilder::new().width(width).height(height).build())
                    .a11y(A11yOverride {
                        hidden: true,
                        ..A11yOverride::default()
                    })
                    .build(tree, tile)?;
            } else {
                build_icon_centered(
                    tree,
                    tile,
                    context.icons.warning,
                    20.0,
                    context.theme.clone(),
                    Role::FgMuted,
                )?;
            }
        }
        Some(MediaState::Failed) => {
            build_icon_centered(
                tree,
                tile,
                context.icons.warning,
                20.0,
                context.theme.clone(),
                Role::StatusDangerBase,
            )?;
            context
                .hits
                .failed
                .borrow_mut()
                .insert(tile, item.url.clone());
        }
        Some(MediaState::Loading) | None => {
            build_icon_centered(
                tree,
                tile,
                context.icons.retry,
                18.0,
                context.theme.clone(),
                Role::FgMuted,
            )?;
        }
    }
    // A video's still frame is not the video; the play mark says so.
    if item.kind == MediaKind::Video {
        build_icon(
            tree,
            tile,
            context.icons.play,
            24.0,
            context.theme.clone(),
            Role::FgOnBrand,
        )?;
    }

    Ok(())
}

/// A tweet that is quoted under the tweet quoting it.
fn build_quote(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let quote = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_TWEET_QUOTE:{}", plan.tweet_id))
        .style(
            StyleBuilder::new()
                .flex_row()
                .gap(8.0)
                .padding(8.0)
                .border(1.0)
                .min_width_0()
                .build(),
        )
        .bind_style({
            let theme = context.theme.clone();
            move |read| StylePatch {
                border: Some(ui_core::paint::primitive::Border {
                    width: 1.0,
                    color: theme.role(read, Role::LineDivider),
                }),
                ..StylePatch::default()
            }
        })
        .build(tree, parent)?;
    context
        .hits
        .rows
        .borrow_mut()
        .insert(quote, plan.url.clone());
    build_avatar(tree, quote, plan, context)?;
    let column = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .gap(4.0)
                .min_width_0()
                .build(),
        )
        .build(tree, quote)?;
    build_tweet_header(tree, column, plan, context)?;
    if plan.has_text {
        build_text(tree, column, plan, context)?;
    }
    if !plan.media.is_empty() {
        build_gallery(tree, column, plan, context)?;
    }

    Ok(())
}

/// Draws one icon image at `size`, toned by a theme role.
fn build_icon(
    tree: &mut NodeTree,
    parent: NodeId,
    icon: ImageSource,
    size: f32,
    theme: ThemeHandle,
    role: Role,
) -> Result<(), EngineError> {
    let node = match icon {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Contain),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Contain),
    };
    node.style(
        StyleBuilder::new()
            .width(size)
            .height(size)
            .flex_none()
            .build(),
    )
    .bind_style(themed_foreground(theme, role))
    .a11y(A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    })
    .build(tree, parent)?;

    Ok(())
}

/// Draws one icon centred in its parent.
fn build_icon_centered(
    tree: &mut NodeTree,
    parent: NodeId,
    icon: ImageSource,
    size: f32,
    theme: ThemeHandle,
    role: Role,
) -> Result<(), EngineError> {
    let node = match icon {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Contain),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Contain),
    };
    node.style(StyleBuilder::new().width(size).height(size).build())
        .bind_style(themed_foreground(theme, role))
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;

    Ok(())
}

/// A dynamic foreground colour read through the theme handle.
fn themed_foreground(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        foreground: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

/// A dynamic background colour read through the theme handle.
fn themed_background(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        background: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

/// How long ago a tweet was published, in the reader's language.
fn age_text(age: TweetAge, translate: &Translate) -> SmolStr {
    match age {
        TweetAge::Seconds(seconds) => t_smol!(translate, "widget.x.age_seconds", count = seconds),
        TweetAge::Minutes(minutes) => {
            t_smol!(translate, "widget.x.age_minutes", count = minutes)
        }
        TweetAge::Hours(hours) => t_smol!(translate, "widget.x.age_hours", count = hours),
        TweetAge::Date { month, day } => {
            t_smol!(translate, "widget.x.age_date", month = month, day = day)
        }
        TweetAge::FullDate { year, month, day } => t_smol!(
            translate,
            "widget.x.age_full_date",
            year = year,
            month = month,
            day = day
        ),
        TweetAge::Unknown => SmolStr::new_static("--"),
    }
}

/// The status of one monitored account, in the reader's language.
pub(super) fn status_label(status: XSelectionStatus, translate: &Translate) -> SmolStr {
    match status {
        XSelectionStatus::Active => t_smol!(translate, "widget.x.active"),
        XSelectionStatus::Paused => t_smol!(translate, "widget.x.paused"),
        XSelectionStatus::Pending => t_smol!(translate, "widget.x.pending"),
    }
}

#[cfg(test)]
mod tests;
