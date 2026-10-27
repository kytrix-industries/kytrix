//! The Hyperliquid wallet pane: what the monitored wallets are worth, and what they did.
//!
//! # Two views of one list
//!
//! The pane shows either every monitored wallet at once, or one wallet's detail. What it is
//! showing is the pane's whole status: the framework draws one banner or one placeholder
//! around the body, so the pane reports the state of the view a reader is looking at rather
//! than of every stream it is running. A wallet that failed while the wallet beside it is fine
//! is a row that says so; a wallet's detail that failed is the pane's own failure, because
//! there is nothing else on screen to read.
//!
//! # The two views are pages of a stack
//!
//! The list is the root of a [`NavStack`] and a wallet a reader opens is a page pushed over it,
//! so opening one and coming back are the push and pop of a router rather than a control that
//! redraws the body in place — which is what lets one page slide over the other, and what keeps
//! the list a reader is on while the wallet is drawn above it. Only the body is a stack: the
//! control that comes back is the pane's own header, which the framework draws around the body
//! and rebuilds every frame, so it stays where a reader looks for it.
//!
//! Each page is a view of its own, because the stack is what decides which one is showing and
//! draws both while one slides over the other. What either page draws is read from the pane,
//! which owns every reading the two of them show.
//!
//! # What a wallet is
//!
//! A wallet is one address. The pane persists the addresses it follows and nothing else: which of
//! the venue's partitions an address holds data in is the venue's answer, so it is read from the
//! registry instead of being chosen by the user, and an address is one row however many partitions
//! it trades on. "Which wallets am I following" is therefore one list that a stream key, a request
//! and a row are all built from.
//!
//! # What a frame costs
//!
//! The body is one row per wallet, and a row is an account line above a timeline. The timeline
//! measures itself once through a canvas and remembers the width it was drawn at, so a
//! snapshot that changed nothing a reader can see does not rebucket anything, and the pane is
//! notified only when a reading it shows actually moved.

use std::{convert::Infallible, sync::Arc, time::Duration};

use ahash::AHashMap;
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    join::Join,
    scroll_bar::Scrollbar,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext, ClipboardItem, Context, ElementId, Empty, Entity,
    EntityId, Hsla, InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, UniformListScrollHandle, WeakEntity, Window,
    base::{
        Disableable, NavMotion, NavOperation, NavPage, NavStack, NavStackState, ScrollbarMode,
        Selectable,
        motion::{PresencePhase, Transition},
    },
    div,
    prelude::FluentBuilder,
    px, relative, uniform_list,
};
use i18n::t;
use market_participants_connector::{
    connector::MarketParticipantsConnector, venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    identifier::{VenueId, WalletAddress},
    listing::route::VenueRoute,
    routing::CapabilityId,
};
use serde::Deserialize;
use smol_str::format_smolstr;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{LocalizedError, account_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::on_data_time_zone_change,
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    detail::DetailTable,
    dialog::{AddressDialogError, WalletAddressDialog},
    model::{DetailSnapshot, DetailTab, WalletOverviewSnapshot, now_millis},
    service::{
        ClientWalletSource, WalletProtocol, WalletRequest, WalletService, WalletSource,
        WalletStatus, WalletStreamError, WalletStreamKey, WalletStreamMessage, loaded_scopes,
        wallet_routes_of,
    },
    timeline::{self, TimelineEntry},
    watch::{self, WatchListError, deserialize_addresses},
};

/// The height of one wallet's account line.
const ACCOUNT_ROW_HEIGHT: f32 = 88.0;
/// The height of one wallet's whole row: its account line, its timeline, and the divider it draws
/// over the row above it.
const WALLET_ROW_HEIGHT: f32 = ACCOUNT_ROW_HEIGHT + timeline::HEIGHT + 1.0;

/// How long one page takes to slide in over the other, or back out from under it.
const PAGE_TRANSITION: Duration = Duration::from_millis(220);

/// How far the page underneath drifts while the page over it slides, as a fraction of the width
/// of the body.
///
/// A page that is being covered moves a little the other way rather than staying where it is,
/// which is what reads as one page lying under another rather than two pages swapping places.
const PAGE_DEPTH_DRIFT: f32 = 0.3;

/// A Hyperliquid wallet monitor persists the wallet addresses it follows.
///
/// The list belongs to the pane: two monitors can follow different wallets, and a pane's list is
/// what it was left showing rather than a setting every pane shares. No route is persisted,
/// because which partitions the venue publishes is not a user's choice.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, serde::Serialize)]
pub struct HyperliquidWalletMonitorConfig {
    #[serde(deserialize_with = "deserialize_addresses")]
    addresses: Vec<WalletAddress>,
}

impl HyperliquidWalletMonitorConfig {
    /// A config that follows exactly these addresses, in the order given.
    #[cfg(test)]
    pub(super) fn monitoring(addresses: Vec<WalletAddress>) -> Self {
        Self { addresses }
    }
}

impl WidgetConfig for HyperliquidWalletMonitorConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports.
#[derive(Clone, Debug, Eq, PartialEq)]
enum HyperliquidWalletMonitorFailure {
    /// A stream stopped.
    Stream(WalletStreamError),
    /// The request that would have started the streams failed.
    Rpc(RpcCallError<Infallible>),
}

impl LocalizedError for HyperliquidWalletMonitorFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// Whether this failure is the directory answering "this route's catalog is not in
/// memory yet" rather than an account read going wrong.
///
/// The pane's own gate asks that question before it subscribes to anything, so this
/// is the answer changing underneath the request — a venue re-discovering its scopes
/// is what does it. It is a wait, not a failure: loading the market again is
/// what puts the catalog back. A reading reaches the pane as an account error, and
/// the account layer is where the directory's typed answer travels.
fn is_waiting_for_a_catalog(failure: &HyperliquidWalletMonitorFailure) -> bool {
    matches!(
        failure,
        HyperliquidWalletMonitorFailure::Stream(WalletStreamError::Account(error))
            if account_is_catalog_not_loaded(error)
    )
}

/// What one scope of the pane is doing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum ScopeStatus {
    #[default]
    Loading,
    Ready,
    Failed(Arc<HyperliquidWalletMonitorFailure>),
}

/// One wallet's account line, as the list draws it.
struct AccountRow {
    address: WalletAddress,
    snapshot: Option<Arc<WalletOverviewSnapshot>>,
    status: ScopeStatus,
}

/// One wallet's timeline, with what its own stream is doing.
struct TimelineRow {
    entry: TimelineEntry,
    status: ScopeStatus,
    ready: bool,
}

impl Default for TimelineRow {
    fn default() -> Self {
        Self {
            entry: TimelineEntry::default(),
            status: ScopeStatus::Loading,
            ready: false,
        }
    }
}

/// One wallet's totals, with what its own stream is doing.
#[derive(Default)]
struct AccountRowState {
    snapshot: Option<Arc<WalletOverviewSnapshot>>,
    status: ScopeStatus,
    ready: bool,
}

/// One tab of one wallet.
struct DetailEntry {
    snapshot: Option<Arc<DetailSnapshot>>,
    table: DetailTable,
    status: ScopeStatus,
    ready: bool,
}

/// Which arm of the open wallet's body one tab is drawn as, with what that arm needs.
///
/// See [`HyperliquidWalletMonitorWidget::detail_arm`]: the body is a match over this, so which of
/// the three a frame draws is one decision rather than two that could disagree.
enum DetailArm<'a> {
    /// The reading's table, with the entry it belongs to.
    Table(&'a DetailEntry),
    /// The loading message, which is also what a tab with no reading yet draws.
    Loading,
    /// The failure message.
    Failed(&'a HyperliquidWalletMonitorFailure),
}

/// One row of the list: the account line and the timeline under it.
struct WalletRow {
    account: AccountRow,
    timeline: TimelineEntry,
    timeline_status: ScopeStatus,
}

/// What the pane is showing.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Shown {
    Overview,
    Detail(WalletAddress, DetailTab),
}

/// One of the two pages the pane's body is made of.
enum PanePage {
    /// The wallets the pane follows, which is the page the stack opens on.
    Overview,
    /// One wallet's detail.
    Detail {
        address: WalletAddress,
        /// The tab this page draws.
        ///
        /// The pane's own tab is what a reader last chose for the wallet it has open, and a page
        /// takes it while that wallet is the one the pane is showing. A page the pane has moved on
        /// from — the wallet being closed, or the next wallet's page pushed over it — draws the tab
        /// it was left on rather than the tab the pane has since moved to.
        tab: DetailTab,
    },
}

/// One page of the pane's body, as the stack draws it.
///
/// A page is a view of its own rather than a part of the frame that draws the pane, because the
/// stack is what says which page is showing and draws both of them while one slides over the
/// other. What a page reads it reads from the pane, which owns every reading either page shows;
/// what it holds is which page it is, and what it was left showing.
struct WalletPage {
    pane: WeakEntity<HyperliquidWalletMonitorWidget>,
    page: PanePage,
}

impl Render for WalletPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A page is drawn after the pane it reads while the pane is being torn down, and a page
        // the pane is gone from has nothing to draw itself from.
        let Some(pane) = self.pane.upgrade() else {
            return div().into_any_element();
        };
        let (selector, body) = match &mut self.page {
            PanePage::Overview => (
                "TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE",
                pane.update(cx, |pane, cx| pane.overview_body(cx)),
            ),
            PanePage::Detail { address, tab } => {
                let address = address.clone();
                let left_on = *tab;
                // The pane's own tab is this page's for as long as it is the page the stack is
                // showing: a reader picks their tab on the page they are looking at, and a page the
                // stack has moved on from — one on its way back out — draws the tab it was left on
                // rather than the tab the pane has since moved to.
                let me = cx.entity().entity_id();
                let showing = pane.read_with(cx, |pane, cx| {
                    pane.stack
                        .read(cx)
                        .current()
                        .is_some_and(|current| current.entity_id() == me)
                });
                *tab = if showing {
                    pane.read_with(cx, |pane, _| pane.selected_tab)
                } else {
                    left_on
                };
                let tab = *tab;
                (
                    "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE",
                    pane.update(cx, |pane, cx| pane.detail_body(address, tab, cx)),
                )
            }
        };

        div()
            .debug_selector(move || selector.into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(body)
            .into_any_element()
    }
}

/// What an answer the pane is waiting for is about.
enum Pending {
    /// Starting the streams of the whole list.
    Scope,
    /// Restarting the overview of the wallets that failed.
    Overview(Vec<WalletAddress>),
    /// Restarting one wallet's timeline.
    Timeline(WalletAddress),
    /// Reading one tab of one wallet.
    Detail(WalletAddress, DetailTab),
}

/// One pane: the addresses it follows, and the wallet it has opened.
pub struct HyperliquidWalletMonitorWidget {
    config: HyperliquidWalletMonitorConfig,
    binding: WidgetBinding<WalletProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    overview: AHashMap<WalletAddress, AccountRowState>,
    timelines: AHashMap<WalletAddress, TimelineRow>,
    detail: AHashMap<(WalletAddress, DetailTab), DetailEntry>,
    /// The wallet whose detail is open, if one is.
    selected: Option<WalletAddress>,
    selected_tab: DetailTab,
    /// The pages the body is made of, of which the top one is showing.
    ///
    /// The list is the root the stack is opened on and the wallet a reader opens is a page over
    /// it, so what the pane is showing and what the stack holds are one answer rather than two:
    /// nothing pushes or pops a page except the pane's own navigation.
    stack: Entity<NavStackState>,
    /// The list of wallets, which is the root page of the stack.
    ///
    /// The stack holds it as well, but a pane that empties the stack to put the list back as the
    /// whole of it — which is how it comes back from a change that is not a navigation — has to
    /// have the page in hand: this is that handle.
    overview_page: Entity<WalletPage>,
    pending: Option<Pending>,
    list: UniformListScrollHandle,
    dialog: Option<WeakEntity<WalletAddressDialog>>,
    /// Whether the pane is waiting for its market rather than reading it.
    ///
    /// The pane keeps its streams while it waits — cancelling the binding is terminal,
    /// and this pane has to read the venue again once the user loads it — so a
    /// reading that was already in flight when the routes went away can still arrive.
    /// Accepting it would give the pane content again and write readiness over the
    /// notice, and the directory event that follows would then early-return on that
    /// content and leave the pane reading a route set that no longer exists. Every
    /// message entry refuses while this is set.
    awaiting_market: bool,
    /// The markets this pane is reading, as of the commit it last acted on.
    ///
    /// The set is one half of what a venue commit is compared against: one that changed it is a
    /// scope that appeared, or one a shrinking commit took away, and that is a different set of
    /// markets to read. One that leaves it as it was is a publication behind markets the pane
    /// already has, which is a reason to start over exactly when the commit landed on one of them
    /// — see `on_venues_updated`, and `market_reloaded` for what starting over sends.
    markets: Arc<[VenueRoute]>,
    /// Whether a market this pane reads was loaded or refreshed again.
    ///
    /// The markets this pane follows hold no such announcement of their own: their streams
    /// resolve the display terms they publish with when they are opened, so a commit that
    /// published the catalog again replaces the generation behind them while they keep the
    /// terms they resolved. The pane therefore starts over, and the request it sends has to be
    /// the one that reads every wallet's totals again: [`Self::load`] reads this after its own
    /// gate — a pane a shrinking commit sent back to waiting keeps it, so the load that brings
    /// the market back reloads the streams too — and it is what makes the request a `Reload`
    /// rather than the idempotent `Load`, which the service answers without touching a stream
    /// that is already live.
    market_reloaded: bool,
    /// Wakes the pane when the zone its timestamps are read in moves.
    _time_zone: Option<Subscription>,
}

impl WidgetView for HyperliquidWalletMonitorWidget {
    type Config = HyperliquidWalletMonitorConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.hyperliquid_wallet_monitor");
    const DISPLAY_NAME: &'static str = "widget.names.hyperliquid_wallet_monitor";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Monitor Hyperliquid wallet activity";
    const ICON: Option<&'static str> = Some("widgets/hyperliquid_wallet_monitor.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = ClientWalletSource::new(cx.global::<MarketParticipantsConnector>());
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The markets this pane reads have to be loaded before it reads anything:
        // a route the registry does not hold, or one whose catalog was never
        // published, is a market the user has not loaded, and the pane waits for
        // one instead of reading a venue that is not there.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // The set this pane is about to read: a commit that leaves it as it is is a
        // publication behind a market already being read.
        self.markets = self.needed_markets(cx);
        // The pane reached here only past its own gate, so the market it was waiting
        // for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        // Read past the gate: a market this pane reads loading again is what brings it back after
        // a shrinking commit, and the streams it still holds resolved their terms against the
        // generation that commit replaced.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx);
        if !retry {
            // An explicit load drops what the pane cannot vouch for; a recovery keeps what a
            // reader is looking at, because blanking a screen that may already be right is worse
            // than showing a reading that is about to be replaced.
            self.detail.clear();
            // The pane is starting over rather than navigating, so the list is what there is to
            // show without a page sliding back out from under a reader.
            self.show_overview(NavMotion::Immediate, cx);
            self.seed();
        }
        self.pending = None;
        self.status.report(WidgetState::Connecting, cx);
        cx.notify();
        self.follow(retry, market_reloaded, cx);
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// A pane is content once it has something to draw for what it is showing.
    ///
    /// An empty timeline is content: a wallet that has not traded in six hours is still the
    /// wallet this row is about, and the pane has nothing to say about it beyond being empty.
    /// An empty list of wallets is not, which is what the waiting placeholder is for.
    ///
    /// A pane that is waiting for its market has nothing to draw whatever it once held:
    /// the readings it was showing are not backed by a market any more, and the notice
    /// the framework draws while this is `false` is what the user is supposed to act on.
    fn has_content(&self) -> bool {
        if self.awaiting_market {
            return false;
        }
        match self.shown() {
            Shown::Overview => {
                !self.config.addresses.is_empty()
                    && self.overview.values().any(|row| row.snapshot.is_some())
            }
            // An open wallet is content whether or not the tab it shows has answered yet. The
            // tab group is what says which reading the table below it is, so a tab that is
            // still being read has to be waited for *under* those tabs rather than by taking
            // them off the screen — and the framework only embeds a body it is told is content.
            Shown::Detail(_, _) => true,
        }
    }

    /// The controls that belong to what the pane is showing.
    ///
    /// Which control the left group carries is what decides whether the wait disables it:
    /// the address control is the one that answers the pane's own "no addresses" sentence
    /// and stays live for it, while everything the header offers once a wallet is open —
    /// the way back, the retry of a failed reading, the refresh of an open tab — belongs to
    /// a reading of the market, and a pane that is waiting for its market has none to
    /// offer: it is showing its list, and its list is what the notice stands in for.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        let left = match self.selected.clone() {
            Some(_) => self.back_button(cx),
            None => self.manage_button(cx),
        };
        let header = WidgetHeader::empty().left(left);
        match self.shown() {
            Shown::Overview => match self.retry_failed_button(cx) {
                Some(right) => header.right(right),
                None => header,
            },
            Shown::Detail(target, tab) => match self.refresh_button(target, tab, cx) {
                Some(right) => header.right(right),
                None => header,
            },
        }
    }
}

impl HyperliquidWalletMonitorWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        config: HyperliquidWalletMonitorConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: WalletSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            WalletService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // A row exists as soon as its address is followed, so a wallet that has not answered yet
        // is a wallet that is still loading rather than one the pane has never heard of.
        let overview = config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, AccountRowState::default()))
            .collect();
        let timelines = config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, TimelineRow::default()))
            .collect();

        // The pane's body is a stack of two pages, and the page it opens on is the list of
        // wallets: the stack is built with the pane rather than in the frame that draws it,
        // because a page is an entity and a frame that built one would build another every time
        // it was drawn. The wallets it follows are read from the state built above, so the page
        // is the one the pane would have drawn anyway.
        let stack = cx.new(|_| NavStackState::new());
        let pane = cx.entity().downgrade();
        let overview_page = cx.new(|_| WalletPage {
            pane,
            page: PanePage::Overview,
        });
        stack.update(cx, |stack, cx| {
            stack.push(overview_page.clone(), NavMotion::Immediate, cx);
        });
        // What the stack changes is what the body draws, and the body is the framework's cached
        // subtree: it is drawn again when the pane is told. Every navigation below says so itself,
        // and this is what holds the two in step if one of them ever forgets to — the contract the
        // dialog's pages are held to, and the same line it is held to by.
        cx.observe(&stack, |_, _, cx| cx.notify()).detach();

        // The pane recovers from the market it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        })
        .detach();
        Self {
            config,
            binding,
            status,
            recovery,
            config_change,
            overview,
            timelines,
            detail: AHashMap::with_capacity(DetailTab::ALL.len()),
            selected: None,
            selected_tab: DetailTab::Spot,
            stack,
            overview_page,
            pending: None,
            list: UniformListScrollHandle::default(),
            dialog: None,
            awaiting_market: false,
            markets: Arc::default(),
            market_reloaded: false,
            // Every page is a cached view drawing from the pane, so moving the display
            // zone has to wake the pages as well as the pane itself.
            _time_zone: on_data_time_zone_change(cx, |this, _, cx| {
                this.overview_page.update(cx, |_, cx| cx.notify());
                let pages: Vec<EntityId> = this
                    .stack
                    .read(cx)
                    .views()
                    .map(AnyView::entity_id)
                    .collect();
                for page in pages {
                    App::notify(cx, page);
                }
                cx.notify();
            }),
        }
    }

    /// Whether the markets this pane reads are in memory: every Hyperliquid route
    /// that monitors wallets is loaded.
    ///
    /// This widget is Hyperliquid's, so it needs no other venue's routes.
    ///
    /// Reading a market is only possible once the user has loaded it, and "not
    /// loaded" is a wait rather than a failure. One question answers each route: a
    /// scope in the loaded venues is a scope whose catalog committed with it.
    fn market_ready(&self, cx: &App) -> bool {
        let connector = cx.global::<MarketParticipantsConnector>();
        let routes = wallet_routes_of(&loaded_scopes(connector.venues()));

        !routes.is_empty()
            && routes
                .iter()
                .all(|route| connector.venues().scope(route).is_some())
    }

    /// Reports what this pane is waiting with: the market it waits for, or the addresses it has
    /// nothing to read.
    ///
    /// The market comes first because it is the pane's precondition rather than its content:
    /// nothing this widget draws — the list or the wallet a reader opens — can be read before it,
    /// so a pane with no wallets to follow would still be hiding the one thing the user has to do
    /// first behind a sentence about adding wallets. Once the market is in memory an empty list is
    /// the pane's own answer, and the address control is how the user answers it.
    ///
    /// The list the sentence ends with is every venue that answers with wallet readings,
    /// which is the one this widget reads — a capability's venue set rather than the name of
    /// the pane, so the sentence would state a second venue the day one serves it.
    ///
    /// This is the one place that reference is produced — [`Self::wait_for_market`] and
    /// [`Self::report`] call this rather than holding a copy of their own — and the sentence is
    /// the framework's to resolve, in the language showing on the frame that draws it.
    fn report_notice(&self, cx: &mut Context<Self>) {
        if self.awaiting_market {
            self.status.report(
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![VenueId::Hyperliquid],
                    supported: market_participants_connector::venue::definition::venues_supporting(
                        CapabilityId::WalletMonitor,
                    ),
                }),
                cx,
            );
        } else if self.config.addresses.is_empty() {
            self.status.report(
                WidgetState::waiting(Notice::message(
                    "widget.hyperliquid_wallet_monitor.empty_addresses",
                )),
                cx,
            );
        }
    }

    /// Reports the neutral notice for the market this pane reads, and drops the
    /// readings it was showing.
    ///
    /// Which of the pane's two sentences stands in for them is
    /// [`Self::report_notice`]'s to decide.
    ///
    /// The routes behind those readings are gone, so nothing on screen is backed by
    /// a market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: its
    /// streams are still alive, and a reading that was already in flight would
    /// otherwise give the pane content again and write readiness over the notice. The
    /// followed addresses are left alone: they are the user's, and loading the market
    /// again is what brings them back. An open wallet goes with the readings, since
    /// there is nothing left of it to draw: the list is the page the pane is left on.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        // The pane is reading nothing while it waits, so a commit that puts its scopes back
        // is a set that changed.
        self.markets = Arc::default();
        self.pending = None;
        self.detail.clear();
        self.show_overview(NavMotion::Immediate, cx);
        self.seed();
        self.report_notice(cx);
        cx.notify();
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// The pane compares the markets it needs now with the set it is reading. A set that changed
    /// is a scope that came into the loaded venues or one a shrinking commit took away, and that
    /// is a different set of markets to read. A set that did not change is a publication behind
    /// markets the pane already has: if it landed on one of those markets, the user loaded that
    /// market again and the generation behind its streams was replaced, so the pane starts over —
    /// see `market_reloaded`, which is what makes the request the one that reads every wallet's
    /// totals again. That request names the whole set the pane follows, so it does not matter
    /// whether a request of the pane's own is still in flight or was replaced by it. A commit of
    /// a venue this pane reads nothing from only re-checks the precondition and never starts over
    /// what the user is watching. A failure is terminal: the event does not revive a failed pane,
    /// only the user's own recovery does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on a market this pane is reading is that market being loaded again, so the pane
        // cannot keep reading the streams it holds. This is asked before the set comparison because
        // one commit can be both — the market this pane reads is the one that shrank away — and the
        // flag then survives the gate below, so the load that brings the market back reopens the
        // streams too.
        if self.markets.iter().any(|route| route.venue == event.venue) {
            self.market_reloaded = true;
            self.load(cx);
            return;
        }
        if self.needed_markets(cx) == self.markets {
            if !self.market_ready(cx) {
                self.wait_for_market(cx);
            }
            return;
        }
        self.load(cx);
    }

    /// The markets this pane needs now, as the loaded venues answer them.
    ///
    /// Every loaded Hyperliquid scope that monitors wallets and publishes a catalog: the pane
    /// reads no other venue, and a scope whose venue is not loaded is a market to wait for
    /// rather than one to read.
    fn needed_markets(&self, cx: &App) -> Arc<[VenueRoute]> {
        let connector = cx.global::<MarketParticipantsConnector>();

        wallet_routes_of(&loaded_scopes(connector.venues())).into()
    }

    /// What the pane is showing.
    fn shown(&self) -> Shown {
        match self.selected.clone() {
            Some(address) => Shown::Detail(address, self.selected_tab),
            None => Shown::Overview,
        }
    }

    /// Starts reading every address, or restarts it for a pane the user is recovering.
    ///
    /// A recovery keeps what a reader is looking at — blanking ten wallets would flash away a
    /// screen that may already be correct — while a fresh start drops it, because the pane
    /// cannot vouch for a reading it is no longer sure of.
    ///
    /// `market_reloaded` is a commit on a market this pane reads: the user loaded it again, so the
    /// totals of every wallet this pane follows have to resolve again at the generation that commit
    /// published. A reload is a start over, so it is the list that is asking — which is why this
    /// only reaches the wallet the reader has open for a recovery.
    fn follow(&mut self, retry: bool, market_reloaded: bool, cx: &mut Context<Self>) {
        match self.selected.clone() {
            // Recovering a detail view recovers the tab it is showing, not the overview behind
            // it: the pane is about one wallet while it is open on one.
            Some(address) => self.load_detail(address, self.selected_tab, retry, cx),
            None => self.start_overview(retry, market_reloaded, cx),
        }
    }

    /// Starts reading every address.
    ///
    /// `retry` keeps the rows a reader is looking at and `market_reloaded` decides the request
    /// they are read with ([`Self::request_overview`]): the readings themselves go for a reload,
    /// because nothing written at the retired generation can be vouched for.
    fn start_overview(&mut self, retry: bool, market_reloaded: bool, cx: &mut Context<Self>) {
        if !retry {
            self.detail.clear();
            self.show_overview(NavMotion::Immediate, cx);
            self.seed();
        }
        self.request_overview(retry, market_reloaded, cx);
    }

    /// Gives every followed address the rows it is drawn and reported with.
    fn seed(&mut self) {
        self.overview = self
            .config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, AccountRowState::default()))
            .collect();
        self.timelines = self
            .config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, TimelineRow::default()))
            .collect();
    }

    /// Asks for the totals of the wallets this pane follows.
    ///
    /// `retry` is the request rather than the content: a plain `Load` asks for the followed set
    /// and leaves every stream that is already running alone, while `Reload` reads the totals of
    /// every wallet it names again and `Retry` does that for the wallets whose totals failed.
    /// A recovery names the wallets whose totals failed, because those are what a recovery is
    /// about; a reload of a market this pane reads names every wallet it follows, because a stream
    /// that is still running never failed and would otherwise be left resolving at the generation
    /// the commit replaced.
    fn request_overview(&mut self, retry: bool, market_reloaded: bool, cx: &mut Context<Self>) {
        self.pending = Some(Pending::Scope);
        let request = if market_reloaded {
            WalletRequest::Reload {
                addresses: self.config.addresses.clone(),
            }
        } else if retry {
            WalletRequest::Retry {
                addresses: self.failed_addresses(),
            }
        } else {
            WalletRequest::Load {
                addresses: self.config.addresses.clone(),
            }
        };
        self.binding.call(request, cx, Self::apply_rpc_result);
        self.report(cx);
        cx.notify();
    }

    /// The addresses whose totals failed, which is what a recovery asks for again.
    fn failed_addresses(&self) -> Vec<WalletAddress> {
        self.config
            .addresses
            .iter()
            .filter(|address| {
                self.overview
                    .get(address)
                    .is_some_and(|row| matches!(row.status, ScopeStatus::Failed(_)))
            })
            .cloned()
            .collect()
    }

    /// Applies the answer to the request the pane had in flight.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<WalletProtocol>,
        cx: &mut Context<Self>,
    ) {
        // A failure is terminal, and a pane waiting for its market reads nothing: an answer
        // that arrives after either belongs to a generation this pane no longer reports.
        if self.status.is_failed(cx) || self.awaiting_market {
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Err(error) = result else {
            return;
        };
        let failure = Arc::new(HyperliquidWalletMonitorFailure::Rpc(error));
        match pending {
            Pending::Scope => {
                for row in self.overview.values_mut() {
                    if matches!(row.status, ScopeStatus::Loading) {
                        row.status = ScopeStatus::Failed(failure.clone());
                    }
                }
                for row in self.timelines.values_mut() {
                    if matches!(row.status, ScopeStatus::Loading) {
                        row.status = ScopeStatus::Failed(failure.clone());
                    }
                }
            }
            Pending::Overview(addresses) => {
                for address in addresses {
                    if let Some(row) = self.overview.get_mut(&address) {
                        row.status = ScopeStatus::Failed(failure.clone());
                    }
                }
            }
            Pending::Timeline(address) => {
                if let Some(row) = self.timelines.get_mut(&address) {
                    row.status = ScopeStatus::Failed(failure);
                }
            }
            Pending::Detail(address, tab) => {
                if let Some(entry) = self.detail.get_mut(&(address, tab)) {
                    entry.status = ScopeStatus::Failed(failure);
                }
            }
        }
        self.report(cx);
        self.notify_dialog(cx);
    }

    /// Applies a reading a subscription published.
    ///
    /// Returns whether what a frame draws changed, which is what tells the framework to
    /// rebuild a body that is otherwise cached.
    fn apply_latest(
        &mut self,
        key: WalletStreamKey,
        message: Arc<WalletStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.status.is_failed(cx) || self.awaiting_market {
            return false;
        }
        match (key, message.as_ref()) {
            (WalletStreamKey::Overview { address }, WalletStreamMessage::Overview(snapshot)) => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(row) = self.overview.get_mut(&address) else {
                    return false;
                };
                row.snapshot = Some(snapshot.clone());
                if row.ready {
                    row.status = ScopeStatus::Ready;
                }
                self.report(cx);
                true
            }
            (WalletStreamKey::Timeline { address }, WalletStreamMessage::Timeline(snapshot)) => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(row) = self.timelines.get_mut(&address) else {
                    return false;
                };
                let changed = row.entry.replace_snapshot(snapshot);
                if row.ready {
                    row.status = ScopeStatus::Ready;
                }
                changed
            }
            (WalletStreamKey::Detail { address, tab }, WalletStreamMessage::Detail(snapshot)) => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(entry) = self.detail.get_mut(&(address, tab)) else {
                    return false;
                };
                let now = now_millis().unwrap_or(0);
                match entry.table.accept(snapshot, now, cx) {
                    Ok(()) => {
                        entry.snapshot = Some(snapshot.clone());
                        if entry.ready {
                            entry.status = ScopeStatus::Ready;
                        }
                    }
                    // A reading the table cannot display is not drawn as raw values: it is the
                    // failure of the tab that accepted it.
                    Err(error) => {
                        entry.snapshot = None;
                        entry.status = ScopeStatus::Failed(Arc::new(
                            HyperliquidWalletMonitorFailure::Stream(error.into()),
                        ));
                    }
                }
                self.report(cx);
                true
            }
            _ => false,
        }
    }

    /// Applies what a subscription reported about itself.
    ///
    /// Returns whether what a frame draws changed: a status is not content, but it is what a
    /// row shows in place of content that never arrived.
    fn apply_reliable(
        &mut self,
        key: WalletStreamKey,
        message: Arc<WalletStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.status.is_failed(cx) || self.awaiting_market {
            return false;
        }
        let WalletStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        match key {
            WalletStreamKey::Overview { address } => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(row) = self.overview.get_mut(&address) else {
                    return false;
                };
                if !apply_status(&mut row.status, &mut row.ready, status) {
                    return false;
                }
                self.report(cx);
                true
            }
            WalletStreamKey::Timeline { address } => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(row) = self.timelines.get_mut(&address) else {
                    return false;
                };
                apply_status(&mut row.status, &mut row.ready, status)
            }
            WalletStreamKey::Detail { address, tab } => {
                if !self.follows(&address) {
                    return false;
                }
                let Some(entry) = self.detail.get_mut(&(address, tab)) else {
                    return false;
                };
                let changed = apply_status(&mut entry.status, &mut entry.ready, status);
                if changed {
                    self.report(cx);
                }
                changed
            }
        }
    }

    /// Whether this pane still follows one address.
    ///
    /// It is what makes a reading for an address the user removed a reading this pane ignores: a
    /// stream that was stopped may still have one in flight.
    fn follows(&self, address: &WalletAddress) -> bool {
        self.config.addresses.contains(address)
    }

    /// Reports the state of what the pane is showing.
    ///
    /// The framework has one banner and one placeholder, so there is one status: of the view a
    /// reader is looking at. In the list a failure is only the pane's when there is nothing else
    /// to show — a wallet that failed beside nine that did not is a row, and the pane is ready
    /// because it has nine rows. A detail is different: the tab group is drawn whether or not
    /// the tab under it has a reading, so the shown tab's own failure is the pane's failure, and
    /// the banner is what offers to read that tab again.
    fn report(&mut self, cx: &mut Context<Self>) {
        // The precondition is reported wherever the pane reports from: a pane waiting for its
        // market has no reading, so no path may write `Connecting`, `Loading` or a failure over
        // the notice it is waiting with.
        if self.awaiting_market {
            self.report_notice(cx);
            return;
        }
        match self.shown() {
            Shown::Overview => {
                if self.config.addresses.is_empty() {
                    self.report_notice(cx);
                    return;
                }
                if self.has_content() {
                    self.status.report(WidgetState::Ready, cx);
                    return;
                }
                if let Some(failure) = self.every_wallet_failed() {
                    // A reading the directory answered "this route's catalog is not in
                    // memory" to is a market that is not loaded, not a wallet that
                    // went wrong: the pane reports the neutral notice and comes back
                    // when the directory publishes again. Reporting it as a failure
                    // would leave the pane terminal over a route the very next
                    // publication brings back.
                    if is_waiting_for_a_catalog(failure.as_ref()) {
                        self.wait_for_market(cx);
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status
                        .report(WidgetState::failed(failure, recovery), cx);
                    return;
                }
                self.status.report(WidgetState::Connecting, cx);
            }
            Shown::Detail(address, tab) => {
                let entry = self.detail.get(&(address, tab));
                let failure = entry.and_then(|entry| match &entry.status {
                    ScopeStatus::Failed(failure) => Some(failure.clone()),
                    ScopeStatus::Loading | ScopeStatus::Ready => None,
                });
                // A tab whose reading the directory answered "not loaded" to is the
                // market being unloaded rather than the reading failing, and the
                // notice belongs to the pane rather than to one wallet: the pane goes
                // back to the notice the whole widget is waiting behind.
                if failure
                    .as_ref()
                    .is_some_and(|failure| is_waiting_for_a_catalog(failure.as_ref()))
                {
                    self.wait_for_market(cx);
                    return;
                }
                // The tab group is drawn before any tab has a reading, so waiting for one is
                // never the framework's spinner: it is said in the table area, under the tabs.
                // A reading that has arrived is not waited for, whether or not its stream has
                // since said it is live, and the tab's own status is what says it failed.
                if let Some(failure) = failure {
                    let recovery = self.recovery.clone();
                    self.status
                        .report(WidgetState::failed(failure, recovery), cx);
                } else if entry.is_some_and(|entry| entry.snapshot.is_none()) {
                    self.status.report(WidgetState::Loading, cx);
                } else {
                    self.status.report(WidgetState::Ready, cx);
                }
            }
        }
    }

    /// The failure of the whole list, once every address in it has one.
    fn every_wallet_failed(&self) -> Option<Arc<HyperliquidWalletMonitorFailure>> {
        let mut failure = None;
        for address in &self.config.addresses {
            match self.overview.get(address) {
                Some(AccountRowState {
                    status: ScopeStatus::Failed(error),
                    ..
                }) => failure = failure.or_else(|| Some(error.clone())),
                _ => return None,
            }
        }
        failure
    }

    /// Opens one wallet's detail at its first tab.
    ///
    /// A wallet is opened from the list, and the pane's stack is the list and one wallet over it:
    /// a wallet the pane is already showing is not opened again, because what is over the list is
    /// the page the reader is looking at.
    fn select_target(&mut self, address: WalletAddress, cx: &mut Context<Self>) {
        if self.stack.read(cx).depth() > 1 {
            return;
        }
        self.selected = Some(address.clone());
        self.selected_tab = DetailTab::Spot;
        self.push_detail(address.clone(), cx);
        self.load_detail(address, DetailTab::Spot, false, cx);
        cx.notify();
    }

    /// Shows another tab of the open wallet, which is a tab of a wallet the pane has open or
    /// nothing at all: the group that offers them is drawn by the wallet's own page.
    fn select_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        let Some(address) = self.selected.clone() else {
            return;
        };
        self.selected_tab = tab;
        self.load_detail(address, tab, false, cx);
        cx.notify();
    }

    /// Goes back to the overview.
    ///
    /// The control that comes back is the pane's own, so the page leaves the way it arrived:
    /// it slides back out over the list, which is the page a reader was on before it.
    fn back(&mut self, cx: &mut Context<Self>) {
        self.show_overview(NavMotion::Animated, cx);
        self.report(cx);
        cx.notify();
    }

    /// Puts one wallet's detail on a page over the list.
    ///
    /// A page is an entity, so it is built here rather than in the frame that draws it: a frame
    /// that built one would build another on every frame it was drawn in. What it carries is the
    /// wallet it is about — and, as it draws, the tab it is left on — so it goes on drawing that
    /// wallet while it slides back out, when the pane is already showing the list again.
    fn push_detail(&mut self, address: WalletAddress, cx: &mut Context<Self>) {
        let pane = cx.entity().downgrade();
        let page = cx.new(|_| WalletPage {
            pane,
            page: PanePage::Detail {
                address,
                tab: self.selected_tab,
            },
        });
        self.stack
            .update(cx, |stack, cx| stack.push(page, NavMotion::Animated, cx));
    }

    /// Shows the list again, taking the open wallet's page off the stack.
    ///
    /// What is left of a detail — its readings, and the wallet that was open — is the caller's to
    /// drop: this is the pane going back to the list, which is a navigation when a reader asked
    /// for one and an immediate switch when the pane is starting over or the wallet has left.
    ///
    /// A navigation is slid only when the list it lands on is what the pane is drawing: a pane
    /// with nothing to draw reports a placeholder, and the framework draws that *instead of* the
    /// body — so a change started then would never be seen, and, never drawn, would still be
    /// waiting to run when the body came back.
    fn show_overview(&mut self, motion: NavMotion, cx: &mut Context<Self>) {
        self.selected = None;
        self.selected_tab = DetailTab::Spot;
        if motion == NavMotion::Animated && self.has_content() {
            self.stack
                .update(cx, |stack, cx| stack.pop_to_root(NavMotion::Animated, cx));
            return;
        }
        // The list is put back as the whole stack rather than popped back to: an immediate switch
        // that popped would leave a change of its own to run, and one already running would be
        // left to finish over whatever the pane draws next.
        let list = self.overview_page.clone();
        self.stack.update(cx, |stack, cx| {
            stack.clear(cx);
            stack.push(list, NavMotion::Immediate, cx);
        });
    }

    /// Asks for one tab of one wallet.
    ///
    /// A tab that was read before is not read again: it is what the wallet looked like when it
    /// was opened, and the control that refreshes it is what asks for a newer reading.
    fn load_detail(
        &mut self,
        address: WalletAddress,
        tab: DetailTab,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        let key = (address.clone(), tab);
        if !self.detail.contains_key(&key) {
            let table = DetailTable::new(tab, cx);
            self.detail.insert(
                key.clone(),
                DetailEntry {
                    snapshot: None,
                    table,
                    status: ScopeStatus::Loading,
                    ready: false,
                },
            );
        }
        let Some(entry) = self.detail.get_mut(&key) else {
            return;
        };
        if !retry && entry.snapshot.is_some() {
            self.report(cx);
            cx.notify();
            return;
        }
        // A failed tab still has its stream: the service keeps a failed stream parked until it
        // is replaced, so asking for it with `LoadDetail` would be accepted without opening
        // anything and the tab would wait for a reading nothing is going to send. A tab with
        // no reading is a tab to read again, whichever control asked for it.
        let replace = retry || matches!(entry.status, ScopeStatus::Failed(_));
        entry.status = ScopeStatus::Loading;
        entry.ready = false;
        self.pending = Some(Pending::Detail(address.clone(), tab));
        self.binding.call(
            if replace {
                WalletRequest::RetryDetail { address, tab }
            } else {
                WalletRequest::LoadDetail { address, tab }
            },
            cx,
            Self::apply_rpc_result,
        );
        // An answer to the request this one replaced may have arrived while it was being sent,
        // and it is about a reading this tab no longer waits for: asking again is what the tab
        // is doing, so that is what it says.
        if let Some(entry) = self.detail.get_mut(&key) {
            entry.status = ScopeStatus::Loading;
            entry.ready = false;
        }
        self.report(cx);
        cx.notify();
    }

    /// Asks for one wallet's timeline again, from the row that says it failed.
    fn retry_timeline(&mut self, address: WalletAddress, cx: &mut Context<Self>) {
        let Some(row) = self.timelines.get_mut(&address) else {
            return;
        };
        row.status = ScopeStatus::Loading;
        row.ready = false;
        self.pending = Some(Pending::Timeline(address.clone()));
        self.binding.call(
            WalletRequest::RetryTimeline { address },
            cx,
            Self::apply_rpc_result,
        );
        self.report(cx);
        cx.notify();
    }

    /// Asks for the totals of the wallets that failed again.
    fn retry_failed(&mut self, cx: &mut Context<Self>) {
        let failed = self.failed_addresses();
        if failed.is_empty() {
            return;
        }
        for address in &failed {
            if let Some(row) = self.overview.get_mut(address) {
                row.status = ScopeStatus::Loading;
                row.ready = false;
            }
        }
        self.pending = Some(Pending::Overview(failed.clone()));
        self.binding.call(
            WalletRequest::Retry { addresses: failed },
            cx,
            Self::apply_rpc_result,
        );
        self.report(cx);
        cx.notify();
    }

    /// The addresses this pane follows, in the order it draws them.
    ///
    /// The dialog reads them, so what it offers to change is the list the pane is showing.
    pub(super) fn addresses(&self) -> &[WalletAddress] {
        &self.config.addresses
    }

    /// Whether the address dialog is open.
    pub(super) fn dialog_open(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.upgrade().is_some())
    }

    /// Records the dialog a pane just opened.
    pub(super) fn attach_dialog(
        &mut self,
        dialog: WeakEntity<WalletAddressDialog>,
        cx: &mut Context<Self>,
    ) {
        self.dialog = Some(dialog);
        cx.notify();
    }

    /// Repaints the dialog, which draws the settings this pane changed.
    ///
    /// The dialog is told that it changed rather than asked to change: the pane is what answers
    /// the dialog's own requests, so this also runs inside an update of that dialog — and an
    /// entity cannot be updated while it is already being updated. Notifying it by id is what
    /// says the same thing without taking it.
    fn notify_dialog(&self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialog.as_ref().and_then(WeakEntity::upgrade) {
            App::notify(cx, dialog.entity_id());
        }
    }

    /// Monitors one more address.
    ///
    /// The list is validated before it is stored, so a change the pane cannot use is a refusal the
    /// dialog shows rather than a list that silently loses a wallet.
    pub(super) fn add_address(
        &mut self,
        address: WalletAddress,
        cx: &mut Context<Self>,
    ) -> Result<(), AddressDialogError> {
        let addresses =
            watch::with(&self.config.addresses, address).map_err(|error| match error {
                WatchListError::TooMany => AddressDialogError::Limit,
                WatchListError::Duplicate { .. } => AddressDialogError::Duplicate,
            })?;
        self.store_addresses(addresses, cx);
        Ok(())
    }

    /// Stops monitoring one wallet.
    pub(super) fn remove_address(&mut self, address: &WalletAddress, cx: &mut Context<Self>) {
        let addresses = watch::without(&self.config.addresses, address);
        self.store_addresses(addresses, cx);
    }

    /// Stores a new address list and follows it.
    fn store_addresses(&mut self, addresses: Vec<WalletAddress>, cx: &mut Context<Self>) {
        self.config.addresses = addresses;
        self.config_change.publish(cx);
        self.seed();
        self.detail
            .retain(|(address, _), _| self.config.addresses.contains(address));
        // Every row is seeded again, so what the list is drawn from is not what it was: a wallet
        // that has left takes its page with it, and a change that is still running — a page on its
        // way out while the reader is already back on the list — has nothing left to land on. It is
        // not a navigation the reader made in the pane, either way: the change was made in the
        // dialog over it, so the page goes while their attention is elsewhere rather than sliding
        // back out under it.
        if self
            .selected
            .as_ref()
            .is_none_or(|address| !self.config.addresses.contains(address))
        {
            self.show_overview(NavMotion::Immediate, cx);
        }
        self.pending = None;
        // Adding or removing a wallet is a change to what the pane follows, not a reason to
        // read a market that is not in memory: the request would open the streams of a
        // generation that does not exist, and the pane would report `Connecting` while every
        // message those streams send is refused for waiting. It waits for the market first;
        // the commit that loads it runs the load that starts the new list.
        if self.awaiting_market {
            self.report_notice(cx);
        } else if !self.market_ready(cx) {
            self.wait_for_market(cx);
        } else {
            self.request_overview(false, false, cx);
        }
        self.notify_dialog(cx);
        cx.notify();
    }

    /// The control that opens the address dialog.
    ///
    /// It refuses input while the pane waits for its market, like every other control
    /// here — the readings it offers are read from that market — with one exception
    /// that is the pane's own content rather than the market's: a pane with no addresses
    /// to follow can still be configured, and the dialog this control opens is the only
    /// way to do it. The market notice stays what the pane waits with either way (see
    /// [`Self::report_notice`]), so the wait disables the control exactly while there is
    /// something to monitor.
    fn manage_button(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.awaiting_market && !self.config.addresses.is_empty();
        let widget = cx.entity().downgrade();
        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_MONITOR_MANAGE".into())
            .flex_none()
            // A control in the header row has to take its own clicks: the row is the pane's
            // drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "HyperliquidWalletManage",
                )))
                .filled()
                .small()
                .disabled(disabled)
                .aria_label(t!(cx, "widget.hyperliquid_wallet_monitor.manage_addresses"))
                .with_tooltip(t!(cx, "widget.hyperliquid_wallet_monitor.manage_addresses"))
                .when(!disabled, |button| {
                    button.on_click(move |_, window, cx| {
                        WalletAddressDialog::show(widget.clone(), window, cx);
                    })
                })
                .child(Icon::new(IconKind::Faders).size_3()),
            )
            .into_any_element()
    }

    /// The control that goes back to the overview.
    fn back_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();
        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_MONITOR_BACK".into())
            .flex_none()
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "HyperliquidWalletBack",
                )))
                .filled()
                .small()
                .aria_label(t!(cx, "widget.hyperliquid_wallet_monitor.back"))
                .with_tooltip(t!(cx, "widget.hyperliquid_wallet_monitor.back"))
                .on_click(move |_, _, cx| {
                    if let Some(widget) = widget.upgrade() {
                        widget.update(cx, |widget, cx| widget.back(cx));
                    }
                })
                .child(Icon::new(IconKind::CaretLeft).size_3()),
            )
            .into_any_element()
    }

    /// The control that asks for the wallets that failed again.
    fn retry_failed_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let failed = self.failed_addresses().len();
        if failed == 0 {
            return None;
        }
        let widget = cx.entity().downgrade();
        // The count decides the sentence: one failed wallet is not "1 failed wallets".
        let label = if failed == 1 {
            t!(
                cx,
                "widget.hyperliquid_wallet_monitor.retry_failed_one",
                value = failed
            )
        } else {
            t!(
                cx,
                "widget.hyperliquid_wallet_monitor.retry_failed",
                value = failed
            )
        };
        Some(
            div()
                .debug_selector(|| "TEST_HYPERLIQUID_WALLET_MONITOR_RETRY_FAILED".into())
                .flex_none()
                .occlude()
                .child(
                    Button::new(ElementId::Name(SharedString::new_static(
                        "HyperliquidWalletRetryFailed",
                    )))
                    .ghost()
                    .small()
                    .aria_label(label.clone())
                    .with_tooltip(label)
                    .on_click(move |_, _, cx| {
                        if let Some(widget) = widget.upgrade() {
                            widget.update(cx, |widget, cx| widget.retry_failed(cx));
                        }
                    })
                    .child(
                        Icon::new(IconKind::ArrowsClockwise)
                            .size_3()
                            .text_color(cx.theme().roles.status.danger.base.hsla),
                    ),
                )
                .into_any_element(),
        )
    }

    /// The control that asks for the open tab again.
    fn refresh_button(
        &self,
        address: WalletAddress,
        tab: DetailTab,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let loading = self
            .detail
            .get(&(address.clone(), tab))
            .is_none_or(|entry| entry.snapshot.is_none());
        let widget = cx.entity().downgrade();
        let label = t!(cx, "widget.hyperliquid_wallet_monitor.refresh");
        Some(
            div()
                .debug_selector(|| "TEST_HYPERLIQUID_WALLET_MONITOR_REFRESH".into())
                .flex_none()
                .occlude()
                .child(
                    Button::new(ElementId::Name(SharedString::new_static(
                        "HyperliquidWalletRefresh",
                    )))
                    .ghost()
                    .small()
                    .disabled(loading)
                    .aria_label(label.clone())
                    .with_tooltip(label)
                    .on_click(move |_, _, cx| {
                        if let Some(widget) = widget.upgrade() {
                            let address = address.clone();
                            widget.update(cx, |widget, cx| {
                                widget.load_detail(address, tab, true, cx)
                            });
                        }
                    })
                    .child(Icon::new(IconKind::ArrowsClockwise).size_3()),
                )
                .into_any_element(),
        )
    }

    /// The wallets, each an account line with its timeline under it.
    fn overview_body(&self, cx: &Context<Self>) -> AnyElement {
        let rows: Arc<[WalletRow]> = self
            .config
            .addresses
            .iter()
            .map(|address| {
                let account = self.overview.get(address);
                let timeline = self.timelines.get(address);
                WalletRow {
                    account: AccountRow {
                        address: address.clone(),
                        snapshot: account.and_then(|row| row.snapshot.clone()),
                        status: account.map_or(ScopeStatus::Loading, |row| row.status.clone()),
                    },
                    timeline: timeline.map_or_else(TimelineEntry::default, |row| row.entry.clone()),
                    timeline_status: timeline
                        .map_or(ScopeStatus::Loading, |row| row.status.clone()),
                }
            })
            .collect::<Vec<_>>()
            .into();
        let body_rows = rows.clone();
        let widget = cx.entity().downgrade();

        let list = uniform_list(
            ElementId::Name(SharedString::new_static("HyperliquidWalletRows")),
            rows.len(),
            move |range, window, cx| {
                range
                    .map(|index| {
                        let Some(row) = body_rows.get(index) else {
                            return Empty.into_any_element();
                        };
                        wallet_row(index, row, &widget, window, cx)
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.list)
        .size_full();

        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_LIST".into())
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(list)
            .child(Scrollbar::vertical(self.list.clone()).mode(ScrollbarMode::Always))
            .into_any_element()
    }

    /// The body one tab of one wallet is drawn as now.
    ///
    /// The body of an open wallet is one match over the tab's own entry, and this is the whole of
    /// what decides which arm of it a frame draws: a reading on its way in is waited for in the
    /// table area — whether it is the first one the tab ever gets or a newer one asked for over an
    /// older, because the answer is what the table is for — a reading on hand is its table, a
    /// refusal is the refusal, and a tab with no reading yet is waited for. [`Self::detail_body`]
    /// matches this and nothing else, so what the pane draws and what this answers cannot drift
    /// apart — which is what lets a test ask the pane instead of restating the rule.
    fn detail_arm(&self, address: &WalletAddress, tab: DetailTab) -> DetailArm<'_> {
        match self.detail.get(&(address.clone(), tab)) {
            // A tab with no reading to draw is waited for in the table area: one that was never
            // read, one whose reading was refused, and one the service has not answered for since
            // it came back. The tab group above it is what says which reading is being waited for.
            None => DetailArm::Loading,
            Some(entry) if entry.status == ScopeStatus::Loading => DetailArm::Loading,
            Some(entry) if entry.snapshot.is_some() => DetailArm::Table(entry),
            Some(entry) => match &entry.status {
                ScopeStatus::Failed(error) => DetailArm::Failed(error),
                ScopeStatus::Loading | ScopeStatus::Ready => DetailArm::Loading,
            },
        }
    }

    /// One wallet's detail at one tab: the tab group, and the table of the tab it is showing.
    ///
    /// The tab is the page's rather than the pane's, because a page is drawn for as long as it is
    /// on the stack and a page on its way out is no longer the wallet the pane has open: what it
    /// draws is the tab it was left on.
    fn detail_body(
        &self,
        address: WalletAddress,
        tab: DetailTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        // Read out of the style before the table is handed the frame: refreshing a reading
        // borrows the application mutably, and a colour read after that would borrow it again.
        let muted = theme.roles.fg.muted.hsla;
        let danger = theme.roles.status.danger.base.hsla;
        // The tabs are drawn as one group of adjoining buttons: a button states which tab it
        // opens, so a group is built from the tabs themselves rather than from positions.
        let tabs = DetailTab::ALL
            .into_iter()
            .fold(Join::default(), |join, candidate| {
                join.button(detail_tab_button(candidate, tab, cx))
            });

        let body = match self.detail_arm(&address, tab) {
            // A reading on its way in is waited for in the table area, whether it is the first
            // one this tab ever gets or a newer one asked for over an older: the answer is what
            // the table is for, so a reading that is coming is not one to draw a replaced table
            // over.
            DetailArm::Loading => centered_message(t!(cx, "common.loading"), muted),
            // A reading with no rows is still a reading, so the table is drawn either way: its
            // own empty body is what says the wallet holds none of what this tab lists.
            DetailArm::Table(entry) => {
                let now = now_millis().unwrap_or(0);
                // The ages of a reading that is still on screen are the one thing a frame
                // can change about it, so the table is handed the moment as it is drawn.
                entry.table.refresh(now, cx);
                div()
                    .debug_selector(|| "TEST_HYPERLIQUID_WALLET_DETAIL_TABLE".into())
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .child(entry.table.render(ElementId::NamedInteger(
                        SharedString::new_static("HyperliquidWalletDetailTable"),
                        tab.index() as u64,
                    )))
                    .into_any_element()
            }
            DetailArm::Failed(error) => centered_message(error.localized_message(cx), danger),
        };

        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_DETAIL".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().flex_none().w_full().min_w_0().p_1().child(tabs))
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(body),
            )
            .into_any_element()
    }
}

impl Render for HyperliquidWalletMonitorWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The colour a page covers the one under it with is the pane's own surface, read once:
        // a page is the whole of what is drawn in the body, so it is the surface the framework
        // gives the body that a page carries with it.
        let page_surface = cx.theme().roles.surface.sunken.hsla;

        div().size_full().min_w_0().min_h_0().child(
            // One page over another: the page arriving slides in from the side a reader is going
            // to, and the page leaving drifts the other way rather than sliding out from under
            // it, so which page is on top reads from the direction of the move. A page carries
            // the body's surface with it: two of them are drawn at once, and without it what is
            // under the page on top would read through the two of them.
            NavStack::new(&self.stack)
                // The stack fills the body, and the pages of a change are laid out over each
                // other in it: what is left of the body is what a page is drawn in.
                .size_full()
                .min_w_0()
                .min_h_0()
                // A page on its way in is placed outside the stack until it arrives, so the stack
                // is what keeps it from being drawn over what is beside the pane.
                .overflow_hidden()
                .transition(Transition::new(PAGE_TRANSITION))
                .item(move |page: NavPage, _, _| {
                    let offset = match (page.phase(), page.operation()) {
                        (
                            PresencePhase::Entering,
                            Some(NavOperation::Push | NavOperation::Replace),
                        ) => 1.0 - page.progress(),
                        (PresencePhase::Exiting, Some(NavOperation::Pop)) => page.progress(),
                        (PresencePhase::Exiting, Some(NavOperation::Push)) => {
                            -PAGE_DEPTH_DRIFT * page.progress()
                        }
                        (PresencePhase::Entering, Some(NavOperation::Pop)) => {
                            -PAGE_DEPTH_DRIFT * (1.0 - page.progress())
                        }
                        _ => 0.0,
                    };
                    // A page is the size of the body wherever it is in its journey: an absolute box
                    // whose width is left to its two edges is the space between them, so a page
                    // placed off the side would be squeezed to nothing on its way in rather than
                    // carried across. Its width is therefore said, and only the edge it comes from
                    // is moved.
                    page.w_full()
                        .left(relative(offset))
                        .bg(page_surface)
                        .into_any_element()
                }),
        )
    }
}

/// Applies a status to one scope, and says whether it changed anything.
fn apply_status(status: &mut ScopeStatus, ready: &mut bool, reported: &WalletStatus) -> bool {
    match reported {
        WalletStatus::Connecting => {
            *ready = false;
            if matches!(status, ScopeStatus::Loading) {
                return false;
            }
            *status = ScopeStatus::Loading;
            true
        }
        WalletStatus::Ready => {
            *ready = true;
            if matches!(status, ScopeStatus::Ready) {
                return false;
            }
            *status = ScopeStatus::Ready;
            true
        }
        WalletStatus::Failed(error) => {
            *ready = false;
            *status = ScopeStatus::Failed(Arc::new(HyperliquidWalletMonitorFailure::Stream(
                error.clone(),
            )));
            true
        }
    }
}

/// A message in the middle of the area a tab draws into.
fn centered_message(message: SharedString, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .px_4()
        .text_center()
        .text_xs()
        .text_color(color)
        .child(message)
        .into_any_element()
}

/// One control of the detail tab group: the tab it opens, and whether that tab is the open one.
///
/// The tabs are siblings under one base name, so which tab a control is stays part of its id.
fn detail_tab_button(
    candidate: DetailTab,
    selected: DetailTab,
    cx: &Context<HyperliquidWalletMonitorWidget>,
) -> Button {
    let index = candidate.index();
    let label = tab_label(candidate, cx);
    let widget = cx.entity().downgrade();

    Button::new(ElementId::NamedInteger(
        SharedString::new_static("HyperliquidWalletDetailTab"),
        index as u64,
    ))
    .debug_selector(|| format!("TEST_HYPERLIQUID_WALLET_DETAIL_TAB_{index}"))
    .small()
    .outline()
    .aria_label(label.clone())
    .selected(candidate == selected)
    .flex_1()
    .items_center()
    .justify_center()
    .text_center()
    .on_click(move |_, _, cx| {
        if let Some(widget) = widget.upgrade() {
            widget.update(cx, |widget, cx| widget.select_tab(candidate, cx));
        }
    })
    .child(label)
}

/// The label of one detail tab.
fn tab_label(tab: DetailTab, cx: &App) -> SharedString {
    match tab {
        DetailTab::Spot => t!(cx, "widget.hyperliquid_wallet_monitor.tab_spot"),
        DetailTab::Perpetual => t!(cx, "widget.hyperliquid_wallet_monitor.tab_perpetual"),
        DetailTab::OpenOrders => t!(cx, "widget.hyperliquid_wallet_monitor.tab_open_orders"),
        DetailTab::Trades => t!(cx, "widget.hyperliquid_wallet_monitor.tab_trades"),
        DetailTab::HistoricalOrders => {
            t!(
                cx,
                "widget.hyperliquid_wallet_monitor.tab_historical_orders"
            )
        }
        DetailTab::Ledger => t!(cx, "widget.hyperliquid_wallet_monitor.tab_ledger"),
    }
}

/// One wallet's row: its account line, and its timeline under it.
///
/// The rows of the list are siblings, so which row this is stays part of the row's id; everything
/// drawn inside it is already under that id. The divider is the row's own top border rather than
/// the previous row's bottom one: the list draws a window of rows at a time, and a row that is
/// scrolled into view still has to be separated from the one above it.
fn wallet_row(
    index: usize,
    row: &WalletRow,
    widget: &WeakEntity<HyperliquidWalletMonitorWidget>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let divider = cx.theme().roles.line.divider.hsla;
    let account = account_row(&row.account, widget, cx);
    let timeline = timeline_row(row, widget, window, cx);
    div()
        .id(ElementId::NamedInteger(
            SharedString::new_static("HyperliquidWalletRow"),
            index as u64,
        ))
        .w_full()
        .h(px(WALLET_ROW_HEIGHT))
        .min_w_0()
        .flex()
        .flex_col()
        .when(index > 0, |row| row.border_t_1().border_color(divider))
        .child(div().min_w_0().w_full().child(account))
        .child(timeline)
        .into_any_element()
}

/// One wallet's account line: which wallet it is, and what it is worth.
fn account_row(
    row: &AccountRow,
    widget: &WeakEntity<HyperliquidWalletMonitorWidget>,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let address = row.address.clone();
    let value = row.snapshot.as_ref().and_then(|snapshot| {
        snapshot.summary.spot_value.map(|value| {
            SharedString::new(snapshot.precision.format_money_quantity(value).as_str())
        })
    });
    let perpetual = row.snapshot.as_ref().and_then(|snapshot| {
        snapshot.summary.perpetual_value.map(|value| {
            SharedString::new(snapshot.precision.format_money_quantity(value).as_str())
        })
    });
    let pnl = row.snapshot.as_ref().and_then(|snapshot| {
        snapshot.summary.unrealized_pnl.map(|value| {
            SharedString::new(snapshot.precision.format_profit_or_loss(value).as_str())
        })
    });
    let missing = SharedString::new_static("-");
    let select = widget.clone();
    let select_address = address.clone();
    let copy_address = address.clone();
    let retry_widget = widget.clone();
    let retry_address = address.clone();
    let label = SharedString::new(address.as_str());

    let copy = Button::new(ElementId::Name(SharedString::new_static(
        "HyperliquidWalletAccountCopy",
    )))
    .ghost()
    .small()
    .aria_label(label.clone())
    .with_tooltip(label.clone())
    .on_click(move |_, _, cx| {
        cx.stop_propagation();
        cx.write_to_clipboard(ClipboardItem::new_string(copy_address.as_str().to_owned()));
    })
    .child(Icon::new(IconKind::CopySimpleBold).size_3());

    let retry = match &row.status {
        ScopeStatus::Failed(error) => {
            let message = error.localized_message(cx);
            Some(
                Button::new(ElementId::Name(SharedString::new_static(
                    "HyperliquidWalletAccountRetry",
                )))
                .ghost()
                .small()
                .aria_label(message.clone())
                .with_tooltip(message)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    if let Some(widget) = retry_widget.upgrade() {
                        let address = retry_address.clone();
                        widget.update(cx, |widget, cx| {
                            if let Some(row) = widget.overview.get_mut(&address) {
                                row.status = ScopeStatus::Loading;
                                row.ready = false;
                            }
                            widget.pending = Some(Pending::Overview(vec![address.clone()]));
                            widget.binding.call(
                                WalletRequest::Retry {
                                    addresses: vec![address],
                                },
                                cx,
                                HyperliquidWalletMonitorWidget::apply_rpc_result,
                            );
                            widget.report(cx);
                        });
                    }
                })
                .child(
                    Icon::new(IconKind::ArrowsClockwise)
                        .size_3()
                        .text_color(theme.roles.status.danger.base.hsla),
                ),
            )
        }
        ScopeStatus::Loading | ScopeStatus::Ready => None,
    };

    let value_text = match (&row.snapshot, &row.status) {
        (None, ScopeStatus::Loading) => t!(cx, "common.loading"),
        (None, ScopeStatus::Failed(_) | ScopeStatus::Ready) => missing.clone(),
        (Some(_), _) => value.unwrap_or_else(|| missing.clone()),
    };
    let perpetual_text = if row.snapshot.is_some() {
        perpetual.unwrap_or_else(|| missing.clone())
    } else {
        missing.clone()
    };
    let pnl_text = if row.snapshot.is_some() {
        pnl.unwrap_or(missing)
    } else {
        missing
    };
    let pnl_color = row
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.summary.unrealized_pnl)
        .map_or(theme.roles.fg.muted.hsla, |value| {
            match value.inner().cmp(&0) {
                std::cmp::Ordering::Less => theme.roles.market.down.hsla,
                std::cmp::Ordering::Equal => theme.roles.market.flat.hsla,
                std::cmp::Ordering::Greater => theme.roles.market.up.hsla,
            }
        });

    div()
        .id(ElementId::Name(SharedString::new_static(
            "HyperliquidWalletAccount",
        )))
        .debug_selector(|| "TEST_HYPERLIQUID_WALLET_ACCOUNT".into())
        .role(Role::ListItem)
        .aria_label(label)
        .h(px(ACCOUNT_ROW_HEIGHT))
        .w_full()
        .min_w_0()
        .px_2()
        .py_1()
        .flex()
        .flex_col()
        .justify_center()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            if let Some(widget) = select.upgrade() {
                let address = select_address.clone();
                widget.update(cx, |widget, cx| widget.select_target(address, cx));
            }
        })
        .child(
            div()
                .min_w_0()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .child(SharedString::new(address.as_str())),
                )
                .child(copy)
                .children(retry),
        )
        .child(
            div()
                .min_w_0()
                .w_full()
                .flex()
                .flex_col()
                .text_xs()
                .text_color(theme.roles.fg.muted.hsla)
                .child(
                    div().min_w_0().w_full().truncate().child(SharedString::new(
                        format_smolstr!(
                            "{}: {value_text}",
                            t!(cx, "widget.hyperliquid_wallet_monitor.spot_value")
                        )
                        .as_str(),
                    )),
                )
                .child(
                    div()
                        .min_w_0()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .truncate()
                        .child(SharedString::new(
                            format_smolstr!(
                                "{}: {perpetual_text}",
                                t!(cx, "widget.hyperliquid_wallet_monitor.perpetual_value")
                            )
                            .as_str(),
                        ))
                        .child(
                            div().ml_1().flex_none().text_color(pnl_color).child(
                                SharedString::new(
                                    format_smolstr!(
                                        "{}: {pnl_text}",
                                        t!(cx, "widget.hyperliquid_wallet_monitor.unrealized_pnl")
                                    )
                                    .as_str(),
                                ),
                            ),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "TEST_HYPERLIQUID_WALLET_TIMELINE_TITLE".into())
                        .min_w_0()
                        .w_full()
                        .truncate()
                        .child(t!(cx, "widget.hyperliquid_wallet_monitor.timeline_title")),
                ),
        )
        .into_any_element()
}

/// One wallet's timeline, with what its own stream is doing.
fn timeline_row(
    row: &WalletRow,
    widget: &WeakEntity<HyperliquidWalletMonitorWidget>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let failure = match &row.timeline_status {
        ScopeStatus::Failed(error) => Some(error.clone()),
        ScopeStatus::Loading | ScopeStatus::Ready => None,
    };
    let retry = failure.as_ref().map(|error| {
        let message = error.localized_message(cx);
        let retry_widget = widget.clone();
        let address = row.account.address.clone();
        timeline::retry_button(message, theme.roles.status.danger.base.hsla, move |cx| {
            if let Some(widget) = retry_widget.upgrade() {
                let address = address.clone();
                widget.update(cx, |widget, cx| widget.retry_timeline(address, cx));
            }
        })
    });
    let measure_widget = widget.clone();
    let measure_address = row.account.address.clone();

    timeline::render_row(
        &row.timeline,
        timeline::TimelineRowArgs {
            precision: row
                .account
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.precision.clone()),
            loading: t!(cx, "widget.hyperliquid_wallet_monitor.timeline_loading"),
            failure: failure.map(|error| error.localized_message(cx)),
            retry,
            on_measure: std::rc::Rc::new(move |width, plot_width, cx| {
                if let Some(widget) = measure_widget.upgrade() {
                    let address = measure_address.clone();
                    widget.update(cx, |widget, cx| {
                        let changed = widget
                            .timelines
                            .get_mut(&address)
                            .is_some_and(|row| row.entry.set_dimensions(width, plot_width));
                        if changed {
                            cx.notify();
                        }
                    });
                }
            }),
        },
        window,
        cx,
    )
}

#[cfg(test)]
mod tests {
    // The migrated pane tests.
    //
    // The pane's identity is a list of watches rather than a list of addresses, so every test here
    // builds the scope it drives the pane with, and the helper names say which wallet of which
    // route a key or a reading is about.

    use std::{
        cell::Cell,
        rc::Rc,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use app_config::widget::{VersionedWidgetConfig, decode_or_default};
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        AppContext, Entity, Modifiers, MouseButton, StyleRefinement, TestAppContext,
        VisualTestContext,
    };
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        account::{
            domain::HyperliquidWalletUpdate,
            error::{AccountCatalogError, AccountError},
        },
        catalog::batch::CatalogEntry,
    };
    use market_participants_model::listing::route::{VenueListingKey, VenueRoute};
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        test_support::{
            ACCOUNT, PendingSource, SubscriptionKind, TestSource, address, config, context,
            detail_key, empty_timeline, entry, message, open_order, open_orders_snapshot,
            overview_key, overview_snapshot, perp_instrument, perpetual_route, perpetual_state,
            position, precision_for, precision_of, publish, settle_until, spot_entry, spot_route,
            spot_snapshot, spot_state, status, timeline_key, wallet_balance,
        },
        watch::MAX_WALLET_ADDRESSES,
    };

    /// Installs everything a pane reads.
    ///
    /// The pane asks the market participants connector which routes are registered, so a test that drives one
    /// installs a connector over the same catalog the fake venue answers from: what the registry
    /// publishes and what the streams report are then one venue rather than two.
    fn install(cx: &mut App) {
        install_with(cx, crate::test_support::test_routes());
    }

    /// Installs everything a pane reads, over the routes a test names.
    fn install_with(cx: &mut App, routes: Vec<(VenueRoute, Vec<CatalogEntry>)>) {
        install_market(cx, routes, true);
    }

    /// Installs everything a pane reads, with the venue loaded or not.
    fn install_market(cx: &mut App, routes: Vec<(VenueRoute, Vec<CatalogEntry>)>, loaded: bool) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        crate::test_support::install_connector_with(cx, &routes, loaded);
    }

    /// The address every test that needs one reads.
    fn wallet() -> WalletAddress {
        ACCOUNT.parse().expect("the test address should be valid")
    }

    /// The display precision the test listings are drawn with.
    fn precision() -> crate::model::DisplayPrecision {
        precision_for(vec![
            (
                perpetual_route(),
                vec![entry(
                    &perpetual_route(),
                    perp_instrument("ETH"),
                    "ETH",
                    "0.1",
                    "0.001",
                )],
            ),
            (
                spot_route(),
                vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
            ),
        ])
    }
    /// The perpetual listing every test reading is about.
    fn perpetual_listing() -> VenueListingKey {
        crate::test_support::published_perpetual("ETH")
    }

    /// A reading of one wallet's totals that has something in it.
    fn reading() -> Arc<WalletOverviewSnapshot> {
        let listing = perpetual_listing();
        overview_snapshot(
            spot_state(vec![wallet_balance("USDC", "10")]),
            perpetual_state(vec![position(&listing, "2", Some("100"))]),
            vec![context(&listing, Some("110"), None)],
            precision(),
        )
    }

    /// A pane the framework would have built, following `addresses` and driven by a fake venue.
    ///
    /// The change report is a parameter so a test can watch the write-back contract: the pane
    /// reports a configuration change through it, and the framework is what persists it.
    fn pane_with_source<S>(
        addresses: Vec<WalletAddress>,
        source: S,
        config_change: WidgetConfigChange,
        cx: &mut Context<HyperliquidWalletMonitorWidget>,
    ) -> HyperliquidWalletMonitorWidget
    where
        S: WalletSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        let mut pane = HyperliquidWalletMonitorWidget::new_with_source(
            config(addresses),
            source,
            status,
            config_change,
            recovery,
            cx,
        );
        // The framework loads a pane once it has built it, which is what records the markets the
        // pane reads. A test that keeps its pane quiet never starts it, so the set it *would* read
        // is recorded here instead: a commit that landed before the pane was built is then a
        // commit that leaves its set as it is, rather than a set that changed and would start the
        // pane over. A venue loaded afterwards still changes the set, so a pane waiting for
        // one is still woken by it.
        pane.markets = pane.needed_markets(cx);
        pane
    }

    /// A pane holding an address list with nothing started, so what a test reports to it is the
    /// only input it has.
    fn quiet_pane(
        cx: &mut TestAppContext,
        addresses: Vec<WalletAddress>,
    ) -> Entity<HyperliquidWalletMonitorWidget> {
        cx.executor().allow_parking();
        // The market is installed and its commit is flushed before the pane is built, which is the
        // order production has: the user loads the market, and a pane opened afterwards reads the
        // generation that load published rather than a commit about a market it was not reading.
        cx.update(install);
        cx.run_until_parked();
        cx.update(|cx| {
            // What a pane a test keeps quiet records as the markets it reads is recorded by the
            // fixture itself: see [`pane_with_source`].
            cx.new(|cx| {
                pane_with_source(addresses, PendingSource, WidgetConfigChange::default(), cx)
            })
        })
    }

    /// A pane reading its addresses.
    fn pane(
        cx: &mut TestAppContext,
        addresses: Vec<WalletAddress>,
    ) -> Entity<HyperliquidWalletMonitorWidget> {
        let pane = quiet_pane(cx, addresses);
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A pane over a venue nobody has loaded, holding the same address list.
    fn pane_over_unloaded_venue(
        cx: &mut TestAppContext,
        addresses: Vec<WalletAddress>,
    ) -> Entity<HyperliquidWalletMonitorWidget> {
        cx.executor().allow_parking();
        cx.update(|cx| install_market(cx, crate::test_support::test_routes(), false));
        cx.run_until_parked();
        let pane = cx.update(|cx| {
            cx.new(|cx| {
                pane_with_source(addresses, PendingSource, WidgetConfigChange::default(), cx)
            })
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A market the user has not loaded leaves the pane waiting for it (⑮): the
    /// notice names the venue, and the wallets it follows are still the user's.
    #[gpui_kit::test]
    fn an_unloaded_venue_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let pane = pane_over_unloaded_venue(cx, vec![wallet()]);

        cx.update(|cx| {
            assert!(!pane.read(cx).has_content(), "there is nothing to draw");
            assert_eq!(addresses_of_pane(pane.read(cx)), vec![wallet()]);
            let status = pane.read(cx).status.state(cx);
            assert!(
                status.failure().is_none(),
                "an unloaded venue is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![VenueId::Hyperliquid],
                    supported: market_participants_connector::venue::definition::venues_supporting(
                        CapabilityId::WalletMonitor,
                    ),
                }),
                "waiting is not working"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Hyperliquid"),
                "the notice names the market: {sentence}"
            );
        });
    }

    /// The market is the pane's precondition, so it is named before the empty list: with no
    /// wallets to follow the app still has exactly one thing to ask of the user first.
    #[gpui_kit::test]
    fn an_unloaded_venue_should_be_reported_before_an_empty_address_list(cx: &mut TestAppContext) {
        let pane = pane_over_unloaded_venue(cx, Vec::new());

        cx.update(|cx| {
            assert!(pane.read(cx).addresses().is_empty());
            assert!(
                pane.read(cx).awaiting_market,
                "the pane waits for the market even with no wallets configured"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![VenueId::Hyperliquid],
                    supported: market_participants_connector::venue::definition::venues_supporting(
                        CapabilityId::WalletMonitor,
                    ),
                }),
                "the market sentence is what the pane waits with, not the empty-list one"
            );
        });
    }

    /// A wallet added while the market is not loaded is part of the next load, not a reason to
    /// start reading a generation that is not in memory: the pane keeps waiting for the market
    /// instead of reporting `Connecting` over streams that cannot answer.
    #[gpui_kit::test]
    fn adding_a_wallet_while_the_market_is_not_loaded_should_keep_waiting(cx: &mut TestAppContext) {
        let pane = pane_over_unloaded_venue(cx, Vec::new());

        cx.update(|cx| {
            assert_eq!(
                pane.update(cx, |pane, cx| pane.add_address(wallet(), cx)),
                Ok(())
            );

            let status = pane.read(cx).status.state(cx);
            assert!(
                !status.is_busy(),
                "the pane must not report Connecting for a market it does not have: {status:?}"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![VenueId::Hyperliquid],
                    supported: market_participants_connector::venue::definition::venues_supporting(
                        CapabilityId::WalletMonitor,
                    ),
                }),
                "the pane keeps the neutral notice"
            );
            assert_eq!(addresses_of_pane(pane.read(cx)), vec![wallet()]);
        });
    }

    /// The notice is a reference into the catalogue rather than a sentence, so the pane names its
    /// market in the language showing now — without a reload and with nothing re-reporting on its
    /// behalf.
    #[gpui_kit::test]
    fn a_waiting_pane_should_name_its_market_in_the_language_showing_now(cx: &mut TestAppContext) {
        let pane = pane_over_unloaded_venue(cx, vec![wallet()]);

        let (notice, english) = cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("the pane waits for its market");
            let english = notice.resolve(cx);
            assert!(
                english.contains("Hyperliquid"),
                "the notice names the market: {english}"
            );

            (notice, english)
        });
        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Hyperliquid"),
                "the notice names the market in the language showing now: {sentence}"
            );
            assert_ne!(sentence, english, "the sentence follows the language");
            assert_eq!(
                pane.read(cx).status.state(cx).notice(),
                Some(&notice),
                "the reference is what the pane reported; nothing re-reported it"
            );
        });
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without being reopened (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_pane_read(cx: &mut TestAppContext) {
        let pane = pane_over_unloaded_venue(cx, vec![wallet()]);
        cx.update(|cx| {
            assert!(
                pane.read(cx).status.state(cx).notice().is_some(),
                "the pane waits for a venue nobody loaded"
            );

            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        crate::test_support::venue(),
                    ]),
                    &[],
                    &[],
                )
                .expect("the venue's routes should load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            market_participants_connector::catalog::test_support::publish_fixture_catalogs(
                cx,
                &crate::test_support::test_routes(),
            );
        });
        // The directory signals its publications through its own task, so the event
        // that re-runs the load arrives on the executor.
        cx.run_until_parked();

        cx.update(|cx| {
            let status = pane.read(cx).status.state(cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(status.failure().is_none());
            assert_eq!(status, WidgetState::Connecting);
        });
    }

    /// A refresh of the market this pane reads replaces the catalogue generation its live streams
    /// resolved the display terms they publish with, while those streams keep what they resolved
    /// when they were opened. So the pane starts over — the readings go, because nothing on screen
    /// can be vouched for — and the request it sends is the one that reopens the streams of every
    /// wallet it follows, at the generation this load published.
    ///
    /// What proves the streams were really reopened is the fake venue's own record: a stream reads
    /// an address's spot state when it is opened, so a second read is a second stream.
    #[gpui_kit::test]
    fn a_refresh_of_the_market_the_pane_reads_should_reopen_its_streams(cx: &mut TestAppContext) {
        let routes = crate::test_support::test_routes();
        let source = TestSource::new(&routes);
        let pane = pane_with_fake(cx, &source, vec![wallet()]);
        // The reading reaching the pane is what this test starts from, not the fake's count of
        // reads: a stream reads the address before what it read is published, so the count is
        // reached while the pane is still waiting for the snapshot it describes.
        settle_until(cx, "the wallet to be read", |cx| {
            source.counters().spot_states == 1 && pane.read(cx).has_content()
        });
        cx.update(|cx| {
            assert!(
                pane.read(cx).has_content(),
                "the pane is reading the wallet"
            );
        });
        // The same venue loads again and the catalog it republishes restates the listing the
        // reading is displayed at, which is the shape a production load has: it re-fetches the
        // catalogs.
        let restated = restated_routes();
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        crate::test_support::venue(),
                    ]),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
            source.restate(&restated);
        });
        // Both the second read and the reading it produced are waited for: the count says the
        // stream was opened, and the snapshot says what it read has reached the pane.
        settle_until(cx, "the wallet to be read again", |cx| {
            source.counters().spot_states == 2
                && pane.read(cx).overview[&wallet()].snapshot.is_some()
        });

        cx.update(|cx| {
            assert!(
                !pane.read(cx).status.is_failed(cx),
                "the request a reload sends is one the service accepts: {:?}",
                pane.read(cx).status.state(cx)
            );
            let reading = pane.read(cx).overview[&wallet()]
                .snapshot
                .clone()
                .expect("the stream that was opened published a reading");
            assert_eq!(
                reading.precision,
                crate::test_support::precision_for(restated),
                "the stream that was opened resolved at the generation this load published"
            );
        });
    }

    /// The test routes with the perpetual listing restated at another price tick.
    fn restated_routes() -> Vec<(VenueRoute, Vec<CatalogEntry>)> {
        let mut routes = crate::test_support::test_routes();
        let perpetual = crate::test_support::perpetual_route();
        for (route, entries) in &mut routes {
            if route != &perpetual {
                continue;
            }
            for entry in entries {
                entry.snapshot.rules.common.price.tick =
                    "0.5".parse().expect("the restated price tick should parse");
            }
        }
        routes
    }

    /// A failure is terminal: a venue commit does not revive a failed pane, even when the commit
    /// takes the markets it reads away and puts them back.
    ///
    /// The first commit is the one a pane without the terminal guard would act on: it takes the
    /// pane's own market away, so an unguarded handler would replace the failure with the neutral
    /// notice. The second puts the market back, which a handler that judged "did what I read
    /// change" without the guard would read as a scope that appeared and would start over for.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let pane = pane(cx, vec![wallet()]);
        // The pane fails the way a stream reports it, and a failure is the state only the user's
        // own recovery leaves.
        let failure = cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&wallet()),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            );

            let state = pane.read(cx).status.state(cx);
            assert!(state.is_failed(), "the pane is in its terminal failure");
            state
        });

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The market the pane reads leaves the loaded venues...
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(crate::test_support::venue(), Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
            deliveries.load(Ordering::Acquire) >= 1
        });
        // ...and comes back.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        crate::test_support::venue(),
                    ]),
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
            assert_eq!(
                pane.read(cx).status.state(cx),
                failure,
                "a venue commit must not revive a failed pane"
            );
            assert!(
                !pane.read(cx).awaiting_market,
                "a pane in its terminal failure is not a pane waiting for a market"
            );
        });
    }

    /// A stream that is still alive after its market went away must not put a reading back
    /// over the notice: the pane waits, and a reading already in flight belongs to a venue
    /// the pane no longer reports reading. Without the guard the reading is accepted, the
    /// overview has content again, and the framework draws the wallets instead of the
    /// notice the user is supposed to act on.
    #[gpui_kit::test]
    fn a_late_reading_after_the_market_went_away_should_not_clear_the_notice(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, vec![wallet()]);

        cx.update(|cx| {
            // The pane is reading, which is the state a revocation interrupts.
            report_overview(&pane, &wallet(), reading(), cx);
            assert!(pane.read(cx).has_content(), "the reading is what arrived");

            // The venue loaded again and published a scope set without them: the pane's own gate
            // is what notices, and it reports the neutral notice.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(crate::test_support::venue(), Vec::new())], &[])
                .expect("an empty scope set unloads the venue's slot");
            pane.update(cx, |pane, cx| pane.load(cx));

            let waiting = pane.read(cx).status.state(cx);
            assert!(
                waiting.notice().is_some(),
                "the pane reports the neutral notice"
            );
            assert!(waiting.failure().is_none(), "waiting is not a failure");

            report_overview(&pane, &wallet(), reading(), cx);

            assert!(
                !pane.read(cx).has_content(),
                "a reading from the market that went away must not become content again"
            );
            assert!(
                pane.read(cx).status.state(cx).notice().is_some(),
                "the notice is still what the pane reports"
            );
        });
    }

    /// The directory answering "this route's catalog is not in memory" is a wait, not a
    /// failure: the pane's own gate asks the same question before it subscribes, so
    /// this is the answer changing underneath the read — a venue re-discovering its scopes
    /// is what does it — and loading the market again is what brings the wallets back.
    /// Reporting it as `failed` would leave the pane terminal over a route the next
    /// publication brings back, and no later event may revive a failed pane.
    #[gpui_kit::test]
    fn a_wallet_read_the_directory_has_not_loaded_should_leave_the_pane_waiting(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, vec![wallet()]);

        cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&wallet()),
                WalletStatus::Failed(WalletStreamError::Account(AccountError::Catalog(
                    AccountCatalogError::Directory(Box::new(
                        market_participants_connector::catalog::error::CatalogError::CatalogNotLoaded {
                            route: perpetual_route(),
                        },
                    )),
                ))),
                cx,
            );

            let status = pane.read(cx).status.state(cx);
            assert!(
                status.failure().is_none(),
                "a catalog that has not loaded is a wait, not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![VenueId::Hyperliquid],
                    supported: market_participants_connector::venue::definition::venues_supporting(
                        CapabilityId::WalletMonitor,
                    ),
                }),
                "waiting is not working"
            );
            let notice = status
                .notice()
                .expect("the pane reports the neutral notice");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Hyperliquid"),
                "the notice names the market: {sentence}"
            );
            assert!(!pane.read(cx).has_content());
        });
    }

    /// A pane reading its addresses through a fake venue a test can inspect.
    fn pane_with_fake(
        cx: &mut TestAppContext,
        source: &TestSource,
        addresses: Vec<WalletAddress>,
    ) -> Entity<HyperliquidWalletMonitorWidget> {
        cx.executor().allow_parking();
        cx.update(install);
        cx.run_until_parked();
        let pane = cx.update(|cx| {
            cx.new(|cx| {
                pane_with_source(addresses, source.clone(), WidgetConfigChange::default(), cx)
            })
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// The addresses a pane is following.
    fn addresses_of_pane(pane: &HyperliquidWalletMonitorWidget) -> Vec<WalletAddress> {
        pane.addresses().to_vec()
    }

    /// Reports one wallet's totals as its stream would.
    fn report_overview(
        pane: &Entity<HyperliquidWalletMonitorWidget>,
        address: &WalletAddress,
        snapshot: Arc<WalletOverviewSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                overview_key(address),
                message(WalletStreamMessage::Overview(snapshot)),
                cx,
            )
        })
    }

    /// Reports one wallet's timeline as its stream would.
    fn report_timeline(
        pane: &Entity<HyperliquidWalletMonitorWidget>,
        address: &WalletAddress,
        snapshot: Arc<crate::model::TimelineSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                timeline_key(address),
                message(WalletStreamMessage::Timeline(snapshot)),
                cx,
            )
        })
    }

    /// Reports one wallet's tab as its stream would.
    fn report_detail(
        pane: &Entity<HyperliquidWalletMonitorWidget>,
        address: &WalletAddress,
        tab: DetailTab,
        snapshot: Arc<DetailSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                detail_key(address, tab),
                message(WalletStreamMessage::Detail(snapshot)),
                cx,
            )
        })
    }

    /// Reports what one stream says about itself.
    fn report_status(
        pane: &Entity<HyperliquidWalletMonitorWidget>,
        key: WalletStreamKey,
        reported: WalletStatus,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(key, status(reported), cx)
        })
    }

    /// Counts the venue commits it is told about.
    ///
    /// A test that asserts a pane did *not* act on a commit still has to know the commit reached
    /// it: this probe subscribes the same way and after the pane did, so the delivery it counts is
    /// one the pane's own handler has already seen.
    struct CommitProbe {
        deliveries: Arc<AtomicUsize>,
    }

    impl CommitProbe {
        fn new(
            cx: &mut Context<Self>,
            events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
            deliveries: Arc<AtomicUsize>,
        ) -> Self {
            cx.subscribe(events, |probe, _, _: &VenuesUpdated, _| {
                probe.deliveries.fetch_add(1, Ordering::AcqRel);
            })
            .detach();
            Self { deliveries }
        }
    }

    /// A list of addresses is what the pane follows, and nothing else about a wallet is its
    /// identity: the same address twice is the same wallet.
    #[test]
    fn a_config_should_hold_the_addresses_it_was_given_in_order() {
        let configured = config(vec![address("1"), address("2")]);

        assert_eq!(
            VersionedWidgetConfig::encode(&configured)
                .expect("a wallet monitor config should encode")
                .decode::<HyperliquidWalletMonitorConfig>()
                .addresses,
            vec![address("1"), address("2")]
        );
    }

    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);

            for key in [
                "widget.names.hyperliquid_wallet_monitor",
                "widget.hyperliquid_wallet_monitor.add",
                "widget.hyperliquid_wallet_monitor.age_days",
                "widget.hyperliquid_wallet_monitor.age_hours",
                "widget.hyperliquid_wallet_monitor.age_minutes",
                "widget.hyperliquid_wallet_monitor.age_seconds",
                "widget.hyperliquid_wallet_monitor.already_monitored",
                "widget.hyperliquid_wallet_monitor.asset",
                "widget.hyperliquid_wallet_monitor.asset_share",
                "widget.hyperliquid_wallet_monitor.back",
                "widget.hyperliquid_wallet_monitor.buy",
                "widget.hyperliquid_wallet_monitor.close_long",
                "widget.hyperliquid_wallet_monitor.close_short",
                "widget.hyperliquid_wallet_monitor.coin",
                "widget.hyperliquid_wallet_monitor.collateral",
                "widget.hyperliquid_wallet_monitor.detail_tabs",
                "widget.hyperliquid_wallet_monitor.empty_addresses",
                "widget.hyperliquid_wallet_monitor.empty_monitored",
                "widget.hyperliquid_wallet_monitor.entry",
                "widget.hyperliquid_wallet_monitor.error_codec",
                "widget.hyperliquid_wallet_monitor.error_dex_read",
                "widget.hyperliquid_wallet_monitor.error_listing_missing",
                "widget.hyperliquid_wallet_monitor.error_route_mismatch",
                "widget.hyperliquid_wallet_monitor.error_stream_closed",
                "widget.hyperliquid_wallet_monitor.error_subscription_buffer_full",
                "widget.hyperliquid_wallet_monitor.error_subscription_closed",
                "widget.hyperliquid_wallet_monitor.fee",
                "widget.hyperliquid_wallet_monitor.funding",
                "widget.hyperliquid_wallet_monitor.invalid_address",
                "widget.hyperliquid_wallet_monitor.kind",
                "widget.hyperliquid_wallet_monitor.limit_reached",
                "widget.hyperliquid_wallet_monitor.liquidation",
                "widget.hyperliquid_wallet_monitor.long",
                "widget.hyperliquid_wallet_monitor.manage_addresses",
                "widget.hyperliquid_wallet_monitor.margin",
                "widget.hyperliquid_wallet_monitor.mark",
                "widget.hyperliquid_wallet_monitor.monitored_count",
                "widget.hyperliquid_wallet_monitor.open_long",
                "widget.hyperliquid_wallet_monitor.open_short",
                "widget.hyperliquid_wallet_monitor.perpetual_value",
                "widget.hyperliquid_wallet_monitor.placeholder",
                "widget.hyperliquid_wallet_monitor.position",
                "widget.hyperliquid_wallet_monitor.position_value",
                "widget.hyperliquid_wallet_monitor.quantity",
                "widget.hyperliquid_wallet_monitor.realized_pnl",
                "widget.hyperliquid_wallet_monitor.reduce_only",
                "widget.hyperliquid_wallet_monitor.refresh",
                "widget.hyperliquid_wallet_monitor.remove_address",
                "widget.hyperliquid_wallet_monitor.retry_failed",
                "widget.hyperliquid_wallet_monitor.scroll_addresses",
                "widget.hyperliquid_wallet_monitor.scroll_rows",
                "widget.hyperliquid_wallet_monitor.sell",
                "widget.hyperliquid_wallet_monitor.short",
                "widget.hyperliquid_wallet_monitor.side",
                "widget.hyperliquid_wallet_monitor.spot",
                "widget.hyperliquid_wallet_monitor.spot_value",
                "widget.hyperliquid_wallet_monitor.status",
                "widget.hyperliquid_wallet_monitor.tab_historical_orders",
                "widget.hyperliquid_wallet_monitor.tab_ledger",
                "widget.hyperliquid_wallet_monitor.tab_open_orders",
                "widget.hyperliquid_wallet_monitor.tab_perpetual",
                "widget.hyperliquid_wallet_monitor.tab_spot",
                "widget.hyperliquid_wallet_monitor.tab_trades",
                "widget.hyperliquid_wallet_monitor.timeline_end",
                "widget.hyperliquid_wallet_monitor.timeline_loading",
                "widget.hyperliquid_wallet_monitor.timeline_marker",
                "widget.hyperliquid_wallet_monitor.timeline_start",
                "widget.hyperliquid_wallet_monitor.timeline_title",
                "widget.hyperliquid_wallet_monitor.trigger_price",
                "widget.hyperliquid_wallet_monitor.unknown",
                "widget.hyperliquid_wallet_monitor.unrealized_pnl",
                "widget.hyperliquid_wallet_monitor.value",
                "common.price",
                "common.time",
                "common.yes",
                "common.no",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
            }

            for tab in DetailTab::ALL {
                assert!(!tab_label(tab, cx).is_empty(), "{tab:?} has no label");
            }
            for address in [wallet(), address("2")] {
                assert!(
                    !SharedString::new(address.as_str()).is_empty(),
                    "{address:?} has no label"
                );
            }
            for failure in [
                HyperliquidWalletMonitorFailure::Stream(WalletStreamError::SubscriptionClosed),
                HyperliquidWalletMonitorFailure::Stream(WalletStreamError::StreamClosed),
                HyperliquidWalletMonitorFailure::Stream(WalletStreamError::Data(
                    crate::model::WalletDataError::Codec { operation: "test" },
                )),
                HyperliquidWalletMonitorFailure::Stream(WalletStreamError::Account(
                    AccountError::Protocol { operation: "test" },
                )),
                HyperliquidWalletMonitorFailure::Stream(WalletStreamError::DexRead {
                    route: perpetual_route(),
                    source: AccountError::Protocol { operation: "test" },
                }),
                HyperliquidWalletMonitorFailure::Rpc(RpcCallError::DeadlineExceeded {
                    request_id: RequestId::new(1),
                }),
            ] {
                assert!(
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }
            for error in [
                AddressDialogError::Invalid,
                AddressDialogError::Duplicate,
                AddressDialogError::Limit,
            ] {
                assert!(
                    !error.localized_message(cx).is_empty(),
                    "{error:?} has no copy"
                );
            }
        });
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            HyperliquidWalletMonitorWidget::ID.as_str(),
            "kytrix.market.hyperliquid_wallet_monitor"
        );
        assert_eq!(
            HyperliquidWalletMonitorWidget::DISPLAY_NAME,
            "widget.names.hyperliquid_wallet_monitor"
        );
        assert_eq!(
            HyperliquidWalletMonitorConfig::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let stored = VersionedWidgetConfig::encode(&config(vec![address("1")]))
            .expect("a wallet monitor config should encode");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
        assert_eq!(
            stored.decode::<HyperliquidWalletMonitorConfig>(),
            config(vec![address("1")])
        );
        assert_eq!(
            decode_or_default::<HyperliquidWalletMonitorConfig>(None),
            HyperliquidWalletMonitorConfig::default()
        );
    }

    /// A payload this build cannot use is refused by the typed decode, which is the mechanism that
    /// sends the pane back to its default list.
    ///
    /// The refusal has to be observed at `from_value`, not only through `decode_or_default`: the
    /// framework's entry is documented never to fail, so comparing "what the pane ends up with"
    /// against the default cannot tell a refused payload from one that decoded into an empty list —
    /// a `#[serde(default)]` on the field would satisfy that comparison while accepting exactly the
    /// payloads this build must refuse.
    #[test]
    fn a_payload_this_build_cannot_use_should_decode_as_the_default() {
        let duplicated = serde_json::json!({
            "addresses": [
                "0x1111111111111111111111111111111111111111",
                "0x1111111111111111111111111111111111111111",
            ],
        });
        let past_the_limit = serde_json::json!({
            "addresses": (0..=MAX_WALLET_ADDRESSES)
                .map(|index| format!("0x{index:0>40}"))
                .collect::<Vec<_>>(),
        });
        let the_old_watch_list = serde_json::json!({
            "watches": [
                {
                    "access": "public",
                    "route": { "venue": "hyperliquid", "scope": "perp" },
                    "addresses": ["0x1111111111111111111111111111111111111111"],
                },
            ],
        });
        let a_bare_address_list = serde_json::json!(["0x1111111111111111111111111111111111111111"]);
        let addresses_that_are_not_a_list = serde_json::json!({
            "addresses": "0x1111111111111111111111111111111111111111",
        });

        for payload in [
            duplicated,
            past_the_limit,
            the_old_watch_list,
            a_bare_address_list,
            addresses_that_are_not_a_list,
        ] {
            assert!(
                serde_json::from_value::<HyperliquidWalletMonitorConfig>(payload.clone()).is_err(),
                "{payload} must be refused rather than decoded into a list this build can use"
            );
            assert_eq!(
                serde_json::from_value::<HyperliquidWalletMonitorConfig>(payload.clone())
                    .unwrap_or_default(),
                HyperliquidWalletMonitorConfig::default(),
                "{payload} must leave the pane on its default list"
            );
        }
    }

    #[gpui_kit::test]
    fn a_scope_with_no_wallets_should_say_what_it_is_waiting_for(cx: &mut TestAppContext) {
        let pane = pane(cx, Vec::new());
        // The pane reads the registry before it can say what it is waiting for, so the frame that
        // reports it runs once that answer is in.
        settle_until(cx, "the pane to say it is waiting", |cx| {
            pane.read(cx)
                .status
                .state(cx)
                .notice()
                .is_some_and(|notice| {
                    notice == &Notice::message("widget.hyperliquid_wallet_monitor.empty_addresses")
                })
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert!(!pane.status.state(cx).is_failed());
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message(
                    "widget.hyperliquid_wallet_monitor.empty_addresses"
                )),
                "a pane with nothing to monitor is not a failure"
            );
        });
    }

    /// The same for the other sentence this pane can be waiting with: an account that follows
    /// no wallets says so in the language showing now.
    #[gpui_kit::test]
    fn a_pane_with_no_wallets_should_say_so_in_the_language_showing_now(cx: &mut TestAppContext) {
        let pane = pane(cx, Vec::new());
        settle_until(cx, "the pane to say it is waiting", |cx| {
            pane.read(cx).status.state(cx).notice().is_some()
        });

        let (notice, english) = cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("the pane waits for wallets to follow");
            let english = notice.resolve(cx);

            (notice, english)
        });
        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            assert_eq!(
                notice.resolve(cx),
                t!(cx, "widget.hyperliquid_wallet_monitor.empty_addresses"),
                "the sentence is the one the catalogue answers with now"
            );
            assert_ne!(
                notice.resolve(cx),
                english,
                "the sentence follows the language"
            );
        });
    }

    #[gpui_kit::test]
    fn a_wallet_reading_should_become_content_and_readiness_should_follow_it(
        cx: &mut TestAppContext,
    ) {
        let pane = quiet_pane(cx, vec![address("1")]);

        // The totals arrive before the stream says it is live, which is the order the service
        // publishes them in: the pane has something to show, and the wallet it belongs to is
        // still connecting until its own stream says otherwise.
        assert!(cx.update(|cx| report_overview(&pane, &address("1"), reading(), cx)));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                pane.has_content(),
                "a wallet with no trades is still a wallet"
            );
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert_eq!(pane.overview[&address("1")].status, ScopeStatus::Loading);
        });

        cx.update(|cx| report_status(&pane, overview_key(&address("1")), WalletStatus::Ready, cx));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.overview[&address("1")].status, ScopeStatus::Ready);
            assert!(!pane.status.state(cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn a_wallet_that_failed_beside_a_working_one_should_not_fail_the_pane(cx: &mut TestAppContext) {
        let addresses = vec![address("1"), address("2")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| report_overview(&pane, &address("1"), reading(), cx));
        cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&address("2")),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content(), "one wallet is still readable");
            assert!(
                !pane.status.state(cx).is_failed(),
                "a wallet that failed beside a working one is a row, not a pane"
            );
            assert_eq!(pane.failed_addresses(), vec![address("2")]);
        });
    }

    #[gpui_kit::test]
    fn a_scope_whose_wallets_all_failed_should_report_the_failure(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&address("1")),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            let state = pane.status.state(cx);
            assert!(state.is_failed());
            let failure = state.failure().expect("the failure should be published");
            assert_eq!(
                failure.text(cx),
                WalletStreamError::SubscriptionClosed.localized_message(cx)
            );
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "every failure offers the framework's recovery"
            );
        });
    }

    #[gpui_kit::test]
    fn a_message_after_a_failure_should_be_refused(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&address("1")),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });

        let accepted = cx.update(|cx| report_overview(&pane, &address("1"), reading(), cx));

        assert!(!accepted, "a failure is terminal until the user recovers");
        cx.update(|cx| assert!(!pane.read(cx).has_content()));
    }

    /// A reading for an address the user removed is not this pane's: the stream may still have one
    /// in flight when the address leaves, and drawing it would put a wallet back on a list the user
    /// took it off.
    ///
    /// Which of the two layers refuses it is worth being exact about. The **effective** one is row
    /// membership: removing an address reseeds the three maps without it, so the message finds no
    /// row to write to and `apply_latest` answers `false` before it ever looks at the list.
    /// `follows` is a second layer that catches the same message a step earlier; the review's
    /// experiment (delete all six `follows` guards) leaves the suite green, so this case pins the
    /// behaviour rather than that guard. Keeping the guard is deliberate — it states the invariant
    /// at the entry instead of relying on the maps staying in step — but it is not what this test
    /// measures.
    #[gpui_kit::test]
    fn a_message_for_an_address_the_pane_no_longer_follows_should_be_refused(
        cx: &mut TestAppContext,
    ) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1"), address("2")]);
        settle_until(cx, "both wallets to be read", |_| {
            source.counters().streams() == 6
        });

        // The user drops the first wallet, and its stream may still have a reading in flight.
        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));

        let accepted = cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.apply_latest(
                    overview_key(&address("1")),
                    message(WalletStreamMessage::Overview(reading())),
                    cx,
                )
            })
        });

        assert!(
            !accepted,
            "a reading of an address the user removed is not this pane's"
        );
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                !pane.overview.contains_key(&address("1")),
                "the wallet that left has no row left to draw into"
            );
            assert!(pane.overview[&address("2")].snapshot.is_none());
        });
    }

    #[gpui_kit::test]
    fn opening_a_wallet_should_show_its_detail_and_report_its_state(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1")]);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Detail(address("1"), DetailTab::Spot));
            assert!(
                pane.has_content(),
                "an open wallet is content before its first reading arrives, so the tab group \
                 stays on screen while the tab it opened is being read"
            );
            assert_eq!(
                pane.status.state(cx),
                WidgetState::Loading,
                "the tab that was opened is being read, and that is said in the table area \
                 rather than by the framework's spinner"
            );
            assert!(
                !pane.status.state(cx).is_failed(),
                "a tab that has not answered yet is not a failure"
            );
        });

        let snapshot = spot_snapshot(
            vec![wallet_balance("USDC", "10")],
            precision_of(
                &spot_route(),
                vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
            ),
        );
        assert!(cx.update(|cx| report_detail(&pane, &address("1"), DetailTab::Spot, snapshot, cx)));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content());
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn a_failed_tab_should_fail_the_pane_and_its_recovery_should_read_it_again(
        cx: &mut TestAppContext,
    ) {
        let pane = quiet_pane(cx, vec![address("1")]);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        cx.update(|cx| {
            report_status(
                &pane,
                detail_key(&address("1"), DetailTab::Spot),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            );
        });

        // A status reaches the pane through its own stream task, so the frame that reports it runs
        // before the failure the framework offers a recovery for is read.
        cx.run_until_parked();
        let recovery = cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                pane.has_content(),
                "the tab group stays drawn over a tab that failed"
            );
            let state = pane.status.state(cx);
            assert!(state.is_failed());
            state
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failed tab offers the framework's recovery")
        });

        // The recovery the framework offers runs the pane's own load, which reads the tab the
        // pane is showing rather than the scope behind it.
        cx.update(|cx| recovery.run(cx));
        // The failure the tab reported is a message on the pane's own stream, and a message that
        // was already in flight lands after the recovery has asked for the tab again: the frame that
        // asks for it is what the pane is doing, so the assertion waits for the pane to settle.
        settle_until(cx, "the tab to be read again", |cx| {
            pane.read(cx)
                .detail
                .get(&(address("1"), DetailTab::Spot))
                .is_some_and(|entry| entry.status == ScopeStatus::Loading)
        });
        // The request the recovery made is answered on the pane's own stream, and the answer is
        // what stops the pane waiting for it: that the pane has been answered is the settled
        // state, while which request it was waiting for is a frame that passes as the answer
        // lands. Waiting for it is what makes the assertions below about a pane that is done
        // asking rather than one that is still asking.
        settle_until(cx, "the tab to have been asked for again", |cx| {
            pane.read(cx).pending.is_none()
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.selected, Some(address("1")));
            assert!(!pane.status.state(cx).is_failed());
            assert_eq!(
                pane.detail
                    .get(&(address("1"), DetailTab::Spot))
                    .expect("the tab the recovery read is the one the pane is showing")
                    .status,
                ScopeStatus::Loading,
                "the recovery is what asks for the tab again, so the tab waits for a reading"
            );
        });

        // The reading the recovery asked for is a reading like any other: it is what the pane
        // draws once it arrives, which is what makes the failed tab a tab that was read again.
        let reading = spot_snapshot(
            vec![wallet_balance("USDC", "10")],
            precision_of(
                &spot_route(),
                vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
            ),
        );
        cx.update(|cx| {
            assert!(
                report_detail(&pane, &address("1"), DetailTab::Spot, reading, cx),
                "the reading of the tab the recovery asked for is one the pane accepts"
            );
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content());
            assert!(!pane.status.state(cx).is_failed());
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn another_tab_should_report_its_own_state(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        cx.update(|cx| {
            report_status(
                &pane,
                detail_key(&address("1"), DetailTab::Spot),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });
        cx.update(|cx| assert!(pane.read(cx).status.state(cx).is_failed()));

        // The tab that was read successfully is content, and switching to it clears the
        // failure of the tab that was not: the pane reports what it is showing.
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::OpenOrders, cx));
        cx.update(|cx| {
            report_detail(
                &pane,
                &address("1"),
                DetailTab::OpenOrders,
                open_orders_snapshot(vec![open_order(&perpetual_listing(), "1", 1)], precision()),
                cx,
            )
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content());
            assert!(!pane.status.state(cx).is_failed());
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
        });
    }

    /// A failed tab keeps its stream parked, so selecting it again has to ask for a replacement
    /// rather than an idempotent load: the latter is accepted without opening anything and the
    /// tab stays on its loading line forever.
    #[gpui_kit::test]
    fn selecting_a_failed_tab_again_should_read_it_again(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1")]);
        settle_until(cx, "the scope to be read", |_| {
            source.counters().spot_states == 1
        });

        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        // The perpetual tab is the first read to fail: the spot tab the wallet opened on has
        // already answered, so the failure the tab reports is the one the pane accepts.
        source.fail_state_of(&perpetual_route());
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::Perpetual, cx));
        settle_until(cx, "the perpetual tab to fail", |cx| {
            pane.read(cx)
                .detail
                .get(&(address("1"), DetailTab::Perpetual))
                .is_some_and(|entry| matches!(entry.status, ScopeStatus::Failed(_)))
        });
        let reads_before = source.read_addresses().len();

        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::Perpetual, cx));

        settle_until(cx, "the failed tab to be read again", |_| {
            source.read_addresses().len() > reads_before
        });
        settle_until(cx, "the retried tab to report its failure", |cx| {
            pane.read(cx)
                .detail
                .get(&(address("1"), DetailTab::Perpetual))
                .is_some_and(|entry| matches!(entry.status, ScopeStatus::Failed(_)))
        });
    }

    #[gpui_kit::test]
    fn a_tab_that_was_read_should_not_be_read_again(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let addresses = vec![address("1")];
        let pane = pane_with_fake(cx, &source, addresses);
        settle_until(cx, "the scope to be read", |_| {
            source.counters().spot_states == 1
        });

        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        settle_until(cx, "the spot tab to be read", |_| {
            source.counters().spot_states == 2
        });
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::OpenOrders, cx));
        settle_until(cx, "the open orders to be read", |_| {
            source.counters().open_orders == 1
        });

        // Switching back to a tab that was already read does not read it again.
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::Spot, cx));
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::OpenOrders, cx));
        cx.run_until_parked();
        assert_eq!(source.counters().open_orders, 1);
        assert_eq!(source.counters().spot_states, 2);
    }

    #[gpui_kit::test]
    fn reconciling_a_scope_should_drop_the_wallets_that_left(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1"), address("2")]);
        cx.update(|cx| {
            report_overview(&pane, &address("1"), reading(), cx);
            report_overview(&pane, &address("2"), reading(), cx);
        });
        cx.update(|cx| {
            report_detail(
                &pane,
                &address("2"),
                DetailTab::Spot,
                spot_snapshot(
                    Vec::new(),
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            )
        });
        pane.update(cx, |pane, cx| pane.select_target(address("2"), cx));

        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.overview.contains_key(&address("1")));
            assert!(
                pane.overview.contains_key(&address("2")),
                "the wallet that stayed is read again rather than forgotten"
            );
            assert_eq!(
                pane.selected,
                Some(address("2")),
                "a wallet that is still monitored keeps its detail open"
            );
        });
    }

    #[gpui_kit::test]
    fn a_wallet_that_left_the_scope_should_leave_the_detail_view(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1"), address("2")]);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));

        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.selected, None);
            assert_eq!(pane.shown(), Shown::Overview);
            assert!(pane.detail.is_empty());
        });
    }

    #[gpui_kit::test]
    fn an_address_should_only_be_added_when_it_is_new_and_there_is_room(cx: &mut TestAppContext) {
        let addresses = (0..MAX_WALLET_ADDRESSES)
            .map(|index| address(&format!("{index}")))
            .collect::<Vec<_>>();
        let pane = quiet_pane(cx, addresses.clone());

        cx.update(|cx| {
            assert_eq!(
                pane.update(cx, |pane, cx| pane.add_address(addresses[0].clone(), cx)),
                Err(AddressDialogError::Duplicate)
            );
            assert_eq!(
                pane.update(cx, |pane, cx| pane.add_address(address("f"), cx)),
                Err(AddressDialogError::Limit)
            );
        });

        // One that is already monitored leaves, which is what makes room for another.
        pane.update(cx, |pane, cx| {
            pane.remove_address(&addresses[0], cx);
        });
        let added = address("f");
        cx.update(|cx| {
            assert_eq!(
                pane.update(cx, |pane, cx| pane.add_address(added.clone(), cx)),
                Ok(())
            );
            assert!(
                pane.read(cx).addresses().contains(&added),
                "the pane's own list is what the dialog adds to"
            );
        });
    }

    /// A wallet the dialog adds is read from every partition the registry publishes, and what the
    /// pane shows is the union of their catalogs.
    ///
    /// This is the whole path rather than a piece of it: a change to the pane's list (which is what
    /// the dialog does), the fan-out the service makes for one address, and the reading that lands
    /// in the row. The routes are a real venue's shape — core spot, core perpetual and a deployed
    /// DEX — because the two-route fixture cannot show a fan-out that reads one route differently
    /// from another, nor three catalogs being merged.
    #[gpui_kit::test]
    fn a_wallet_added_from_the_dialog_should_be_read_across_every_partition(
        cx: &mut TestAppContext,
    ) {
        let routes = crate::test_support::deployed_routes();
        let source = TestSource::new(&routes);
        let core = perpetual_route();
        let deployed = crate::test_support::deployed_route();
        let core_listing = crate::test_support::listing_key(&core, "ETH");
        let deployed_listing = crate::test_support::listing_key(&deployed, "SOL");
        source
            .spot(spot_state(vec![wallet_balance("HYPE", "5")]))
            .pairs(vec![crate::test_support::spot_pair("HYPE", "USDC")])
            .perpetual_on(
                &core,
                perpetual_state(vec![position(&core_listing, "2", Some("100"))]),
            )
            .perpetual_on(
                &deployed,
                perpetual_state(vec![position(&deployed_listing, "3", Some("10"))]),
            );
        cx.executor().allow_parking();
        cx.update(|cx| install_with(cx, routes));
        cx.run_until_parked();
        let pane = cx.update(|cx| {
            cx.new(|cx| {
                pane_with_source(
                    Vec::new(),
                    source.clone(),
                    WidgetConfigChange::default(),
                    cx,
                )
            })
        });

        // The dialog adds one wallet.
        pane.update(cx, |pane, cx| {
            assert_eq!(pane.add_address(address("1"), cx), Ok(()));
        });
        settle_until(cx, "the wallet to be read", |_| {
            source.counters().spot_states == 1
        });

        // The fan-out follows what each catalog publishes: a catalog per route once, the balance
        // once (it is a property of the account, not of a partition), one position read per
        // perpetual partition, spot pairs from the partition that published them — and no orders,
        // because the overview does not read them.
        let counters = source.counters();
        assert_eq!(counters.catalogs, 3, "one catalog per route");
        assert_eq!(counters.spot_states, 1, "one balance read per address");
        assert_eq!(
            counters.perpetual_states, 2,
            "core perpetual and the deployed DEX"
        );
        assert_eq!(counters.pairs, 1, "the spot partition's pairs");
        assert_eq!(counters.fills, 1, "a fill read names only the user");
        assert_eq!(counters.ledger, 1);
        assert_eq!(counters.open_orders, 0, "the overview reads no orders");
        assert_eq!(counters.streams(), 3, "one subscription set per address");

        // The reading is the union of the three catalogs: each partition answers about its own
        // listings, and both positions are in the tab that shows them.
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        pane.update(cx, |pane, cx| pane.select_tab(DetailTab::Perpetual, cx));
        settle_until(cx, "the perpetual reading", |cx| {
            pane.read(cx)
                .detail
                .get(&(address("1"), DetailTab::Perpetual))
                .is_some_and(|entry| entry.snapshot.is_some())
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            let entry = pane
                .detail
                .get(&(address("1"), DetailTab::Perpetual))
                .expect("the tab that was opened");
            let snapshot = entry.snapshot.as_ref().expect("the reading");
            let crate::model::DetailData::Perpetual(state) = &snapshot.data else {
                panic!("the perpetual tab carries a perpetual reading");
            };
            let listings = state
                .positions
                .iter()
                .map(|position| position.listing.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                listings.len(),
                2,
                "one position per partition, each read once: {listings:?}"
            );
            assert!(
                listings.contains(&core_listing),
                "the core position: {listings:?}"
            );
            assert!(
                listings.contains(&deployed_listing),
                "the deployed DEX's position: {listings:?}"
            );
        });
    }

    /// A deployed DEX that cannot answer fails the address its reading was for, and names itself.
    ///
    /// The failure belongs to the address rather than to the panel, which is why the row is what is
    /// asserted: with one address the panel does report it too — every wallet failed — but a
    /// partition failure is not address-specific, so a second address would report it as well.
    #[gpui_kit::test]
    fn a_deployed_dex_that_cannot_be_read_should_fail_the_address(cx: &mut TestAppContext) {
        let routes = crate::test_support::deployed_routes();
        let source = TestSource::new(&routes);
        let deployed = crate::test_support::deployed_route();
        source.fail_state_of(&deployed);
        cx.executor().allow_parking();
        cx.update(|cx| install_with(cx, routes));
        cx.run_until_parked();
        let pane = cx.update(|cx| {
            cx.new(|cx| {
                pane_with_source(
                    vec![address("1")],
                    source.clone(),
                    WidgetConfigChange::default(),
                    cx,
                )
            })
        });
        pane.update(cx, |pane, cx| pane.load(cx));

        settle_until(cx, "the address to fail", |cx| {
            matches!(
                pane.read(cx).overview[&address("1")].status,
                ScopeStatus::Failed(_)
            )
        });
        cx.update(|cx| {
            let ScopeStatus::Failed(failure) = &pane.read(cx).overview[&address("1")].status else {
                panic!("the row reports the failure");
            };
            assert!(
                matches!(
                    failure.as_ref(),
                    HyperliquidWalletMonitorFailure::Stream(WalletStreamError::DexRead {
                        route,
                        ..
                    }) if route == &deployed
                ),
                "the failure names the partition that could not answer: {failure:?}"
            );
        });
    }

    #[gpui_kit::test]
    fn a_timeline_that_moved_should_rebuild_the_row(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1")]);
        let reading = empty_timeline();

        assert!(cx.update(|cx| report_timeline(&pane, &address("1"), reading.clone(), cx)));
        // The same reading again is not a change: it is the same snapshot by identity.
        assert!(!cx.update(|cx| report_timeline(&pane, &address("1"), reading, cx)));
        // A new window is a change, because the axis it is drawn on moved.
        assert!(cx.update(|cx| report_timeline(&pane, &address("1"), empty_timeline(), cx)));
    }

    #[gpui_kit::test]
    fn a_list_change_from_the_dialog_should_be_followed(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1")]);
        settle_until(cx, "the address list to be read", |_| {
            source.counters().spot_states == 1
        });

        pane.update(cx, |pane, cx| {
            pane.remove_address(&address("1"), cx);
            assert_eq!(
                pane.add_address(address("2"), cx),
                Ok(()),
                "the wallet the dialog adds is the one the pane follows"
            );
        });
        settle_until(cx, "the new address list to be read", |cx| {
            pane.read(cx).addresses().contains(&address("2"))
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.overview.contains_key(&address("1")));
            assert!(!pane.timelines.contains_key(&address("1")));
        });
    }

    /// The dialog a pane opens, held by the pane as the real one is.
    ///
    /// The dialog holds no watches of its own, so what it draws is what the pane answers with: a
    /// test drives the pane the way the dialog's own request does.
    struct AttachedDialog {
        dialog: Entity<WalletAddressDialog>,
        _address: Entity<base_ui::input::InputState>,
    }

    /// A dialog attached to `pane`, which is what the pane reaches to repaint it.
    fn open_dialog(
        pane: &Entity<HyperliquidWalletMonitorWidget>,
        cx: &mut TestAppContext,
    ) -> AttachedDialog {
        let window = cx.add_empty_window();
        let (dialog, address) = window.update(|window, cx| {
            let address = cx.new(|cx| {
                base_ui::input::InputStateBuilder::new("hyperliquid-test-address").build(window, cx)
            });
            let dialog =
                cx.new(|cx| WalletAddressDialog::new(pane.downgrade(), address.clone(), cx));
            (dialog, address)
        });
        pane.update(cx, |pane, _| pane.dialog = Some(dialog.downgrade()));

        AttachedDialog {
            dialog,
            _address: address,
        }
    }

    #[gpui_kit::test]
    fn a_dialog_that_asked_for_a_wallet_should_not_be_taken_again(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1")]);
        let attached = open_dialog(&pane, cx);

        // A wallet is added and dropped from inside an update of the dialog that asked for
        // it: a pane that repaints that dialog by taking it again is updating an entity that
        // is already being updated, which panics.
        cx.update(|cx| {
            attached.dialog.update(cx, |_, cx| {
                assert_eq!(
                    pane.update(cx, |pane, cx| pane.add_address(address("2"), cx)),
                    Ok(())
                );
                pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));
            });
        });

        cx.update(|cx| {
            let watched = pane.read(cx).addresses();
            assert!(
                watched.contains(&address("2")),
                "the pane follows the wallet it was asked for"
            );
            assert!(
                !watched.contains(&address("1")),
                "the pane dropped the wallet it was asked to remove"
            );
        });
    }

    #[gpui_kit::test]
    fn the_pane_should_draw_its_wallets_and_their_detail(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| {
            report_overview(&pane, &address("1"), reading(), cx);
            report_timeline(&pane, &address("1"), empty_timeline(), cx);
            report_detail(
                &pane,
                &address("1"),
                DetailTab::Spot,
                spot_snapshot(
                    Vec::new(),
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
            report_status(&pane, overview_key(&address("1")), WalletStatus::Ready, cx);
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        // The wallet rows and the detail below them are drawn inside the body, which is the one
        // cached layer, so each reading below is of a frame that could not have hit the cache
        // (AGENTS.md §10).
        repaint(window);
        let account = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_ACCOUNT")
            .expect("the body should draw the wallets it holds");
        let title = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_TIMELINE_TITLE")
            .expect("the label above the timeline is part of the row");
        assert!(
            title.bottom() <= account.bottom(),
            "the label is laid out inside the account line rather than past it"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        });
        window.run_until_parked();
        // The detail arrives as a page over the list, so it is where it is drawn once the change
        // has run rather than on the frame that pushed it.
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL")
                .is_some(),
            "an open wallet is drawn as its detail"
        );
    }

    /// The list is the root of the stack and an open wallet is a page over it, so a wallet a
    /// reader opens arrives the way a page does: the page that was showing drifts away under it
    /// while it comes in from the side the reader is going to.
    ///
    /// Both pages are read while the change runs — the frame that pushed it, where the arriving
    /// page is still beyond the one it covers, and a frame halfway through, where both pages have
    /// moved and neither has been resized. A body that swapped its two views in place would be
    /// settled on the first of those frames, and a page whose width came from the edge it was
    /// placed by would be squeezed as it travelled rather than carried across.
    #[gpui_kit::test]
    fn an_open_wallet_should_arrive_as_a_page_over_the_list(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        repaint(window);
        let body = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the list is the page the pane opens on");
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "a wallet nobody has opened has no page on the stack"
        );
        assert_eq!(
            body.left(),
            px(0.0),
            "the page on top of a settled stack is the body it is drawn in"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });
        window.run_until_parked();
        repaint(window);

        let covered = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the page a reader was on is still drawn while the next one arrives");
        let arriving = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the wallet's page is drawn as it arrives");
        assert_eq!(
            (arriving.size, covered.size),
            (body.size, body.size),
            "a page is the size of the body it is drawn in, wherever it is on its way in from: \
             {arriving:?} and {covered:?} against {body:?}"
        );
        assert!(
            arriving.left() >= covered.right(),
            "the arriving page comes from the side a reader is going to: {arriving:?} over \
             {covered:?}"
        );

        // Halfway through: the wallet's page is on its way in, and the list it covers has drifted
        // the other way — which is what reads as one page lying under another.
        window.executor().advance_clock(PAGE_TRANSITION / 2);
        repaint(window);
        let halfway_covered = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the page being covered is drawn until the change is over");
        let halfway_arriving = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the page arriving is drawn until it has arrived");
        assert!(
            halfway_arriving.left() > body.left() && halfway_arriving.left() < body.right(),
            "the arriving page is on its way across the body: {halfway_arriving:?} of {body:?}"
        );
        assert!(
            halfway_covered.left() < body.left(),
            "the page being covered drifts the other way: {halfway_covered:?} of {body:?}"
        );
        // A fifth of the body is a floor rather than the drift itself: what the transition calls
        // for is a number of its own, and what this says is that the page underneath is properly
        // out of the way by halfway — not that it has been nudged out of it.
        assert!(
            halfway_covered.left() < -0.2 * body.size.width,
            "the page being covered is well out of the way by halfway: {halfway_covered:?} of \
             {body:?}"
        );
        assert_eq!(
            (halfway_arriving.size, halfway_covered.size),
            (body.size, body.size),
            "a page keeps the size of the body while it travels: {halfway_arriving:?} and \
             {halfway_covered:?} against {body:?}"
        );
        // Two pages are drawn at once for the length of a change, so the page on top carries the
        // body's own surface: without it, what is under it reads through the tab group and the
        // table area of the page over it.
        let body_surface = window.cx.update(|cx| cx.theme().roles.surface.sunken.hsla);
        assert_eq!(
            painted_surface(window, "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE"),
            Some(body_surface),
            "the page on top carries the body's own surface"
        );
        assert_eq!(
            painted_surface(window, "TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE"),
            Some(body_surface),
            "and so does the page it is covering"
        );

        // And the drift carries on to the end of the change rather than turning back partway: the
        // page underneath is further out of the way the closer the page over it is to arriving.
        window.executor().advance_clock(PAGE_TRANSITION / 4);
        repaint(window);
        let later_covered = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the page being covered is drawn until the change is over");
        assert!(
            later_covered.left() < halfway_covered.left(),
            "the page being covered is further out of the way as the change runs: \
             {later_covered:?} against {halfway_covered:?}"
        );

        settle_pages(window);
        let arrived = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the page that arrived is the page showing once the change is over");
        assert_eq!(
            (arrived.origin, arrived.size),
            (body.origin, body.size),
            "the page that arrived is drawn where the page it covered was"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_none(),
            "the page that was covered is off the top of the stack rather than drawn under it"
        );
    }

    /// A change walks its pages across the frames it asks for, and stops asking once it has
    /// arrived.
    ///
    /// This is the one thing the pane's body cannot take for granted: the body is the framework's
    /// cached subtree, drawn again only when its own view is notified (AGENTS.md §7), so a change
    /// that did not get the pane notified would leave its page where it was pushed from for as
    /// long as the clock ran. Every frame below is therefore drawn without refreshing the window —
    /// the pane is embedded here the way the framework embeds it — and nothing else is allowed to
    /// move the page: what a frame draws is what the change asked for through the frame before it.
    #[gpui_kit::test]
    fn a_change_should_move_on_the_frames_it_asks_for_and_stop_when_it_arrives(
        cx: &mut TestAppContext,
    ) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        repaint(window);

        // The box the list is drawn in before the wallet is opened, which is what the journey below
        // is measured against.
        let body = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the list is the page the pane opens on");

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });
        window.run_until_parked();
        draw(window);
        let pushed = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the wallet's page is drawn as it arrives");
        assert!(
            pushed.left() > body.left(),
            "the wallet's page is not where it is going yet: {pushed:?} of {body:?}"
        );

        // The page's journey, one frame at a time: each frame is asked for by the one before it,
        // and the page is drawn further along on each — until it has arrived, where it stays.
        let mut previous = pushed;
        for _ in 0..6 {
            if window.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                break;
            }
            window.executor().advance_clock(PAGE_TRANSITION / 4);
            draw(window);
            let now = window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .expect("the wallet's page is drawn until it has arrived");
            assert!(
                now.left() <= previous.left(),
                "the frame the change asked for draws the page towards the side it is going to: \
                 {now:?} against {previous:?}"
            );
            previous = now;
        }

        assert_eq!(
            previous.left(),
            px(0.0),
            "the page that was pushed is the page showing once the change is over"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_none(),
            "the page that was covered is off the top of the stack rather than drawn under it"
        );

        // And it stops: a page that has arrived asks for nothing more, so the pane is not left
        // drawing for a change that is over. A frame is drawn between the two reads because what
        // is asked for is asked for by the frame before it.
        window.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "a page that has arrived stops asking for frames"
        );
    }

    /// The control a wallet is opened with is the wallet's own row, and what it opens is a page
    /// over the list rather than a body the pane redraws in place.
    ///
    /// The row is clicked twice over, which is what a reader's double click is to the pane: one
    /// wallet is one page, whatever else the second click of the pair was taken by.
    #[gpui_kit::test]
    fn clicking_a_wallet_should_open_its_page(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        repaint(window);

        let row = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_ACCOUNT")
            .expect("the list draws a row for every wallet it follows")
            .center();
        window.simulate_click(row, Modifiers::none());
        window.simulate_click(row, Modifiers::none());
        window.run_until_parked();

        window.cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Detail(target.clone(), DetailTab::Spot));
            assert_eq!(
                pane.stack.read(cx).depth(),
                2,
                "one wallet is one page over the list, however many times its row is clicked"
            );
        });
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some(),
            "the wallet the reader opened is the page that arrived"
        );
    }

    /// Asking twice for one wallet is one page, however the two asks arrive.
    ///
    /// A window hit-tests against the frame it last drew, so a second click dispatched before the
    /// first has been drawn lands on the row the reader pressed a second time — which is a pane
    /// asked to open one wallet twice. What it opens is one page: the list and the wallet over it
    /// is the whole of this pane's stack.
    #[gpui_kit::test]
    fn asking_twice_for_one_wallet_should_leave_one_page(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.select_target(target.clone(), cx);
                pane.select_target(target.clone(), cx);
            });
            assert_eq!(
                pane.read(cx).stack.read(cx).depth(),
                2,
                "the wallet asked for twice is one page over the list"
            );
            assert_eq!(
                pane.read(cx).shown(),
                Shown::Detail(target, DetailTab::Spot)
            );
        });
    }

    /// A wallet that is opened again is the wallet the pane has already read: coming back does not
    /// drop what it read, and the control that refreshes a tab is what asks the venue for a newer
    /// reading.
    #[gpui_kit::test]
    fn reopening_a_wallet_should_show_the_reading_the_pane_already_has(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let target = address("1");
        let pane = pane_with_fake(cx, &source, vec![target.clone()]);
        settle_until(cx, "the list to be read", |_| {
            source.counters().spot_states == 1
        });

        pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        settle_until(cx, "the spot tab to be read", |cx| {
            pane.read(cx)
                .detail
                .get(&(target.clone(), DetailTab::Spot))
                .is_some_and(|entry| entry.snapshot.is_some() && entry.status == ScopeStatus::Ready)
        });
        let reads = source.counters().spot_states;

        pane.update(cx, |pane, cx| pane.back(cx));
        pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            let entry = pane.detail.get(&(target.clone(), DetailTab::Spot));
            assert_eq!(
                entry.map(|entry| entry.status.clone()),
                Some(ScopeStatus::Ready),
                "a wallet opened again is not put back to waiting for a reading it has"
            );
            assert!(
                entry.is_some_and(|entry| entry.snapshot.is_some()),
                "the reading the wallet was opened at is the one it still has"
            );
        });
        cx.run_until_parked();
        assert_eq!(
            source.counters().spot_states,
            reads,
            "the venue is not asked for the wallet again when it is opened again"
        );
    }

    /// Going back is the wallet's page leaving, and what a reader had read of it is kept: the list
    /// is the page the stack is left holding, and the reading goes on being the one the wallet
    /// shows rather than being read again.
    #[gpui_kit::test]
    fn going_back_should_take_the_wallets_page_off_the_stack(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
            report_detail(
                &pane,
                &target,
                DetailTab::Spot,
                spot_snapshot(
                    vec![wallet_balance("USDC", "10")],
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        let open = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the wallet is open before it is closed again");
        assert_eq!(
            open.left(),
            px(0.0),
            "an open wallet is the page on top of a settled stack"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
        });
        window.run_until_parked();
        // The frame the control was clicked on is the frame the change starts on: the wallet's page
        // is still over the list, and the list is already on its way back into place under it. A
        // control that switched the pages on the spot would have nothing on its way out here.
        repaint(window);
        let leaving = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the wallet's page is drawn while it slides back out");
        let returning = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the list is drawn while the wallet's page slides off it");
        assert_eq!(
            (leaving.origin, leaving.size),
            (open.origin, open.size),
            "the page that is leaving starts where it was: {leaving:?} against {open:?}"
        );
        assert!(
            returning.left() < open.left(),
            "the list comes back from the side the wallet's page is leaving for: {returning:?} \
             against {open:?}"
        );

        window.executor().advance_clock(PAGE_TRANSITION);
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the wallet's page is off the stack once the change is over"
        );

        window.cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Overview);
            assert_eq!(
                pane.stack.read(cx).depth(),
                1,
                "the list is the whole stack again"
            );
            assert!(
                pane.detail.contains_key(&(target.clone(), DetailTab::Spot)),
                "the reading the wallet was showing is kept rather than dropped on the way back"
            );
        });
        assert_eq!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .expect("coming back leaves the list drawn"),
            open,
            "the list is drawn where the wallet's page was"
        );
    }

    /// A page on its way out draws the tab it was left on, rather than the tab the pane resets to
    /// for the next wallet.
    ///
    /// The two are read apart on purpose. Only one of the tabs has a reading, and it is not the one
    /// the pane's own tab names once the wallet has been closed — so a page that took its tab from
    /// the pane would draw the wait for a reading it never asked for, and the table it was left on
    /// would have gone for the whole slide.
    #[gpui_kit::test]
    fn a_page_on_its_way_out_should_draw_the_tab_it_was_left_on(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
            // The reader moves to another tab, which is the tab that gets an answer: the first tab
            // the wallet was opened at was asked for and never answered.
            pane.update(cx, |pane, cx| pane.select_tab(DetailTab::OpenOrders, cx));
            report_detail(
                &pane,
                &target,
                DetailTab::OpenOrders,
                open_orders_snapshot(vec![open_order(&perpetual_listing(), "1", 1)], precision()),
                cx,
            );
            report_status(
                &pane,
                detail_key(&target, DetailTab::OpenOrders),
                WalletStatus::Ready,
                cx,
            );
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TABLE")
                .is_some(),
            "the tab the reader is on is the one with a reading, and it draws its table"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
        });
        window.run_until_parked();
        repaint(window);

        window.cx.update(|cx| {
            assert_eq!(
                pane.read(cx).selected_tab,
                DetailTab::Spot,
                "the pane is left ready to open the next wallet at its first tab"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TABLE")
                .is_some(),
            "the page on its way out draws the tab it was left on rather than the one the pane \
             reset to"
        );
    }

    /// A reseed of the list takes a page that is on its way out with it.
    ///
    /// The rows a list is drawn from are seeded again whenever the addresses change, so a change
    /// still running when that happens has nothing left to land on: the list it was sliding back
    /// to is not what the pane is drawing any more. A change left there would run over whatever the
    /// pane draws next — the list, once the venue has answered for it.
    #[gpui_kit::test]
    fn a_change_still_running_when_the_list_is_seeded_again_should_go_with_it(
        cx: &mut TestAppContext,
    ) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some(),
            "the wallet is open before the reader comes back"
        );

        // The reader comes back and the addresses change on the frame that starts the page out —
        // a wallet added from the dialog over the pane — so the change has nothing to land on.
        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
            pane.update(cx, |pane, cx| {
                assert_eq!(pane.add_address(address("2"), cx), Ok(()));
            });
        });
        window.run_until_parked();
        repaint(window);
        window.cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.stack.read(cx).depth(),
                1,
                "the list is the whole stack again"
            );
            assert_eq!(pane.shown(), Shown::Overview);
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the page that was on its way out is gone with the change it belonged to"
        );

        // And when the venue answers for the seeded rows, the list is what comes back: nothing of
        // the wallet the reader closed is drawn with it.
        window.update(|_, cx| {
            report_overview(&pane, &target, reading(), cx);
        });
        window.run_until_parked();
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_some(),
            "the list is what the pane draws once it has a reading again"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "a page whose change went with the reseed is not drawn over the list"
        );
    }

    /// A list with nothing to draw of its own is returned to without a change being left behind it.
    ///
    /// A pane that cannot draw its list reports no content, and the framework draws its placeholder
    /// *instead of* the body — so a page put on its way out on such a frame is never seen. Left
    /// there, it would still be waiting when the list had a reading again: it would come back over
    /// the list the reader had already returned to, and block the pane while it did.
    #[gpui_kit::test]
    fn a_list_with_nothing_to_draw_should_be_returned_to_without_a_change_left_waiting(
        cx: &mut TestAppContext,
    ) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone(), address("2")]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some(),
            "the wallet is open before the list it was opened from is seeded again"
        );

        // A wallet is added from the dialog, which gives every row the state it is drawn with: what
        // the list had to draw goes with it until the venue answers, while the open wallet's page
        // is still the page the pane is showing.
        window.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                assert_eq!(pane.add_address(address("3"), cx), Ok(()));
            });
        });
        window.run_until_parked();

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
        });
        window.run_until_parked();
        repaint(window);
        window.cx.update(|cx| {
            assert!(
                !pane.read(cx).has_content(),
                "the list has nothing of its own to draw yet"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the wallet's page is off the stack rather than on its way out of a body that is not \
             being drawn"
        );

        // The venue answers, and the list is what comes back: nothing of the wallet the reader
        // closed is mounted with it. A page left waiting would be here — off the side it was
        // leaving for — and would still be asking for the frames of a change nobody can see.
        window.update(|_, cx| {
            report_overview(&pane, &target, reading(), cx);
        });
        window.run_until_parked();
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_some(),
            "the list is what the pane draws again"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the page that was never drawn is not mounted over the list"
        );

        // And nothing is left asking: the frames the list's own drawing asks for are delivered and
        // run out, after which the pane asks for nothing more.
        for _ in 0..3 {
            window.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            window.executor().advance_clock(PAGE_TRANSITION);
            draw(window);
        }
        window.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "a change that was never started asks for no frames"
        );
    }

    /// A page that is on its way out when the pane is put back on its list goes with the reset.
    ///
    /// The reset is not a navigation — a pane that reads its market again is starting over, not
    /// taking a reader anywhere — and the list it leaves the pane on is one it may not be able to
    /// draw. A page left on its way out through that would be mounted again the moment the list had
    /// a reading, coming back over a list a reader had already returned to.
    #[gpui_kit::test]
    fn a_reset_while_a_page_is_leaving_should_take_the_page_with_it(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some(),
            "the wallet is open before the reader comes back"
        );

        // The reader comes back and, on the frame that starts the page out, the pane reads its
        // market again: what the list was drawn from goes with it, so the pane has no body to draw
        // the page on its way out in.
        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
            pane.update(cx, |pane, cx| pane.load(cx));
        });
        window.run_until_parked();
        repaint(window);
        window.cx.update(|cx| {
            assert!(
                !pane.read(cx).has_content(),
                "the load dropped what the list was drawn from"
            );
            assert_eq!(
                pane.read(cx).stack.read(cx).depth(),
                1,
                "the page the reader was on is off the stack"
            );
        });

        // The venue answers again: the list is what comes back, with nothing on its way out of it.
        window.update(|_, cx| {
            report_overview(&pane, &target, reading(), cx);
        });
        window.run_until_parked();
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_some(),
            "the list is what the pane draws again"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "a page whose change was reset away is not mounted over the list"
        );
    }

    /// A reader who has asked for less motion is moved between the pages on the spot.
    ///
    /// That is the framework's answer to reduced motion rather than something the pane arranges:
    /// the page a wallet is opened on is the page drawn on the frame it is opened, and the list is
    /// back on the frame it is asked for — with no frame delivered and no time run on in between.
    #[gpui_kit::test]
    fn a_change_should_be_made_on_the_spot_when_motion_is_reduced(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        repaint(window);
        let body = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the list is drawn");

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });
        window.run_until_parked();
        // No frame is delivered and no time is run on: the page is where it belongs on the frame
        // the wallet is opened on, which is what a change with no slide to run looks like.
        repaint(window);
        let open = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the wallet's page is drawn");
        assert_eq!(
            (open.origin, open.size),
            (body.origin, body.size),
            "the page a wallet is opened on is drawn where it belongs rather than on its way in"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_none(),
            "the page a reader left is not still being drawn behind the one that replaced it"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.back(cx));
        });
        window.run_until_parked();
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_some(),
            "the list is what is drawn once the control is used"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the wallet's page is off the stack rather than on its way out"
        );
    }

    /// A wallet the dialog removed takes its page with it, and it does so without a transition:
    /// that change was made in the dialog over the pane, so what is behind it is the list again
    /// rather than a page sliding back out under a reader's attention.
    ///
    /// The frame the assertions below are made on is drawn without the clock being moved on, which
    /// is what tells the two apart: a page going out with a transition to run is still mounted with
    /// the page that replaces it, so those assertions would find it.
    #[gpui_kit::test]
    fn a_wallet_the_dialog_removed_should_leave_the_list_without_a_transition(
        cx: &mut TestAppContext,
    ) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone(), address("2")]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        settle_pages(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some(),
            "the wallet is open before it is removed"
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.remove_address(&target, cx));
        });
        window.run_until_parked();
        repaint(window);

        window.cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Overview);
            assert_eq!(
                pane.stack.read(cx).depth(),
                1,
                "the wallet that left took its page with it"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
                .is_some(),
            "the list is what is drawn once the wallet it was showing is gone"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_none(),
            "the removed wallet's page is gone on the frame that removed it"
        );
    }

    /// The tab group is buttons, so the tab being shown is whatever the control that was
    /// clicked opens rather than the position of a click on a track.
    #[gpui_kit::test]
    fn clicking_a_tab_should_show_that_tab(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            // A tab is read once it is open, so the wallet has to be showing before its stream
            // can report anything about a tab of it.
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
            report_detail(
                &pane,
                &target,
                DetailTab::Spot,
                spot_snapshot(
                    Vec::new(),
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
            report_status(
                &pane,
                detail_key(&target, DetailTab::Spot),
                WalletStatus::Ready,
                cx,
            );
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        // The tab controls are part of the cached body, so the bounds they are clicked at are read
        // off a frame that could not have hit the cache (AGENTS.md §10) — and off a frame
        // the detail has finished arriving on, because a page on its way in is still outside the
        // body and a click where it is drawn is a click somewhere else.
        settle_pages(window);

        let trades = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TAB_3")
            .expect("an open wallet should draw a control for every tab")
            .center();
        window.simulate_click(trades, Modifiers::none());
        window.run_until_parked();

        assert_eq!(
            window.cx.update(|cx| pane.read(cx).shown()),
            Shown::Detail(target, DetailTab::Trades),
            "the tab whose control was clicked should be the open one"
        );
    }

    /// The framework only embeds a body it is told is content, so a tab that is still being
    /// read is waited for *under* the tab group rather than by taking the group off screen.
    ///
    /// Both halves are the point: the group is what says which reading is being waited for, and
    /// a pane that dropped it would blink on every switch to a tab that had not been read.
    #[gpui_kit::test]
    fn the_tab_group_should_stay_while_a_tab_that_was_never_read_is_read(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let target = address("1");
        let pane = pane_with_fake(cx, &source, vec![target.clone()]);
        settle_until(cx, "the scope to be read", |_| {
            source.counters().spot_states == 1
        });

        pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        settle_until(cx, "the spot tab to be read", |_| {
            source.counters().spot_states == 2
        });
        // The venue has not been asked for the open orders yet, so the tab the pane is about to
        // show is one whose reading is still on its way.
        assert!(
            cx.update(|cx| pane
                .read(cx)
                .detail
                .get(&(target.clone(), DetailTab::OpenOrders))
                .is_none()),
            "a tab that was never opened has no reading to draw"
        );

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_tab(DetailTab::OpenOrders, cx));
        });
        window.run_until_parked();
        // The tab group is the top of the cached body, so the reading below is of a frame that could
        // not have hit the cache (AGENTS.md §10) — and of a frame the wallet's page has
        // finished arriving on, which is where the group is drawn where a reader sees it.
        settle_pages(window);

        assert!(
            window.cx.update(|cx| pane.read(cx).has_content()),
            "the pane still reports content, which is what makes the framework embed the body"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TAB_3")
                .is_some(),
            "the tab group is drawn while the tab it opened is still being read"
        );
    }

    /// Draws one frame with every cache missed, so what a debug selector reports is what this
    /// frame painted rather than what the frame before it happened to leave behind.
    ///
    /// A paint action is what fills a selector, and a frame that hits the body cache reuses the
    /// last paint range without drawing (AGENTS.md §10) — `window.refresh()` is the one frame
    /// that cannot hit it, which is what makes reading a selector straight afterwards a
    /// question about this frame.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// Draws one frame the way the window draws its own, caches and all.
    ///
    /// Where [`repaint`] is a frame that cannot hit a cache, this is one that does: it is what a
    /// widget's own invalidation is measured against, because a body that was not notified is
    /// drawn again from the frame before it.
    fn draw(window: &mut VisualTestContext) {
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
    }

    /// What one page is painted on: the colour of the quad the page's own box was painted with.
    ///
    /// Where a page is drawn is read off the frame's selectors, what it is drawn *on* is read off
    /// the frame's quads — the one thing a page can cover the page under it with.
    fn painted_surface(window: &mut VisualTestContext, selector: &'static str) -> Option<Hsla> {
        let bounds = window.debug_bounds(selector)?;
        let scale = window.update(|window, _| window.scale_factor());
        window
            .update(|window, _| window.painted_quads())
            .into_iter()
            .find(|quad| quad.bounds == bounds.scale(scale))
            .and_then(|quad| quad.background.as_solid())
    }

    /// Lets one page finish sliding over the other, and repaints.
    ///
    /// The page over the other is the page showing once the change is over; until then both of
    /// them are mounted, and the one arriving is still placed outside the body — so a page a
    /// reader has just been taken to is not where it is drawn until the transition has run. The
    /// refresh at the end is the same one [`repaint`] makes: the frame a page is read off has to
    /// be a frame that could not have hit the body's cache (AGENTS.md §10).
    fn settle_pages(window: &mut VisualTestContext) {
        window.executor().advance_clock(PAGE_TRANSITION * 2);
        repaint(window);
    }

    /// A reading that is replaced keeps the tab it belongs to on screen: the tab group is drawn
    /// while the newer reading is waited for in the table area, and the table comes back when the
    /// newer reading lands.
    ///
    /// Which body a frame draws is asked of the pane's own decision — [`HyperliquidWalletMonitorWidget::detail_arm`],
    /// the one `detail_body` matches — rather than of a rule restated here: a copy of the rule
    /// would keep passing after the rule moved, which is exactly what this case is here to catch.
    /// What that decision led to being *painted* is then read off a frame whose cache was
    /// invalidated, so the selectors below are this frame's paint actions rather than the last
    /// frame's leftovers.
    #[gpui_kit::test]
    fn refreshing_a_tab_should_keep_its_table_area_and_say_it_is_loading(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
            report_detail(
                &pane,
                &target,
                DetailTab::Spot,
                spot_snapshot(
                    vec![wallet_balance("USDC", "10")],
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
            report_status(
                &pane,
                detail_key(&target, DetailTab::Spot),
                WalletStatus::Ready,
                cx,
            );
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        // The wallet's page arrives before anything on it is read, so the first frame a table is
        // read off is a frame the change has run out on.
        settle_pages(window);
        window.update(|_, cx| {
            let open = pane.read(cx);
            assert!(
                open.has_content(),
                "an open wallet with a reading is content, so the framework draws its body — the \
                 tab group above the table is part of it"
            );
            assert!(
                matches!(
                    open.detail_arm(&target, DetailTab::Spot),
                    DetailArm::Table(_)
                ),
                "a tab with a reading draws its table"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TABLE")
                .is_some(),
            "the frame that was just painted drew the reading's table"
        );

        // A refresh asks for the reading again; the old one is kept until the new one lands.
        window.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.load_detail(target.clone(), DetailTab::Spot, true, cx)
            });
        });
        window.run_until_parked();
        repaint(window);

        window.update(|_, cx| {
            let open = pane.read(cx);
            assert!(
                open.has_content(),
                "the tab group stays drawn while a refresh is in flight"
            );
            assert!(
                matches!(
                    open.detail_arm(&target, DetailTab::Spot),
                    DetailArm::Loading
                ),
                "the table area says it is loading rather than drawing the reading being replaced"
            );
            assert!(
                open.detail
                    .get(&(target.clone(), DetailTab::Spot))
                    .is_some_and(|entry| entry.snapshot.is_some()),
                "the reading being replaced is kept rather than dropped while the next one is \
                 asked for"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TAB_0")
                .is_some(),
            "the frame that was just painted drew the tab group over the wait"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TABLE")
                .is_none(),
            "the frame that was just painted did not draw the reading being replaced"
        );

        // And the reading that answers the refresh is what puts the table back, on the two
        // messages the service publishes for it: the newer reading, then the tab saying it is live.
        window.update(|_, cx| {
            report_detail(
                &pane,
                &target,
                DetailTab::Spot,
                spot_snapshot(
                    vec![wallet_balance("USDC", "20")],
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
            report_status(
                &pane,
                detail_key(&target, DetailTab::Spot),
                WalletStatus::Ready,
                cx,
            );
        });
        window.run_until_parked();
        repaint(window);
        window.update(|_, cx| {
            let open = pane.read(cx);
            assert!(
                matches!(
                    open.detail_arm(&target, DetailTab::Spot),
                    DetailArm::Table(_)
                ),
                "the table is drawn again once the refresh has been answered"
            );
            assert!(
                open.detail
                    .get(&(target.clone(), DetailTab::Spot))
                    .is_some_and(|entry| entry.status == ScopeStatus::Ready),
                "the reading that answers the refresh is the one the table is drawn from"
            );
        });
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DETAIL_TABLE")
                .is_some(),
            "the frame that was just painted drew the new reading's table"
        );
    }

    /// A wallet that holds none of what a tab lists is still a reading of that tab.
    ///
    /// The pane is content either way, so the tab has to say it is empty under its own headers;
    /// drawing nothing there leaves the room below the tab bar blank, which reads as a pane that
    /// failed to draw rather than as a wallet with nothing on this tab.
    #[gpui_kit::test]
    fn an_empty_detail_reading_should_say_it_is_empty(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        let target = address("1");
        cx.update(|cx| {
            // A tab is read once it is open, so the pane has to be showing the wallet before its
            // stream can report anything about a tab of it.
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
            report_detail(
                &pane,
                &target,
                DetailTab::Spot,
                spot_snapshot(
                    Vec::new(),
                    precision_of(
                        &spot_route(),
                        vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
                    ),
                ),
                cx,
            );
            report_status(
                &pane,
                detail_key(&target, DetailTab::Spot),
                WalletStatus::Ready,
                cx,
            );
        });
        assert!(
            cx.update(|cx| pane.read(cx).has_content()),
            "an empty reading is content, so the framework embeds the tab's body"
        );

        let (_, window) = cx.add_window_view(move |_, _| PaneHarness { pane });
        window.run_until_parked();
        // The empty state is drawn inside the body, which is the one cached layer, so it is read off
        // a frame that could not have hit the cache (AGENTS.md §10) — the wallet's page
        // having finished arriving being part of what that frame is.
        settle_pages(window);
        assert!(
            window.debug_bounds("EMPTY_STATE").is_some(),
            "a tab with no rows should draw its no-data state rather than a blank body"
        );
    }

    /// A frame of its own, so a pane can be drawn without the pane the framework builds.
    ///
    /// The pane is embedded the way the framework embeds it: as a cached body laid out in the box
    /// that is left for it (`widget::surface`). That is not a detail of the harness — a cached
    /// body is drawn again only when the pane itself is notified, so it is what makes a frame
    /// drawn without refreshing the window a question about what the pane asked for.
    struct PaneHarness {
        pane: Entity<HyperliquidWalletMonitorWidget>,
    }

    impl Render for PaneHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(
                self.pane
                    .clone()
                    .cached(StyleRefinement::default().flex_1().min_w_0().min_h_0()),
            )
        }
    }

    /// The window a dialog is really opened over: the layer holds it, draws it, and is what
    /// drops it once its exit transition is over.
    fn dialog_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<HyperliquidWalletMonitorWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(move |_| PaneHarness { pane })));
        window.run_until_parked();

        window
    }

    /// Lets the dialog's own transition finish and repaints, so that one just opened is past
    /// its first frames and one just dismissed is gone rather than still fading out.
    fn settle_dialog(window: &mut VisualTestContext) {
        window
            .executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The control at the foot of the dialog closes it by emitting the event the layer listens
    /// for, rather than by asking the window to dismiss whatever happens to be on top — and the
    /// pane is left without it once the layer has let it go.
    ///
    /// Both halves are the point: the dialog leaving the tree is the emit having reached the
    /// layer that hosts it, and the pane losing its handle is the same thing seen from the side
    /// that has to open the next one. A pane that went on believing a dialog was open would
    /// refuse the next one, which the end of this test asks for.
    #[gpui_kit::test]
    fn a_control_at_the_foot_of_the_dialog_should_close_it_and_leave_the_pane_without_one(
        cx: &mut TestAppContext,
    ) {
        let pane = quiet_pane(cx, vec![address("1")]);
        let window = dialog_window(cx, &pane);
        window.update(|window, cx| WalletAddressDialog::show(pane.downgrade(), window, cx));
        settle_dialog(window);

        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DIALOG_CLOSE")
                .is_some(),
            "the dialog should be drawn over the layer once it is opened"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the pane should hold the dialog it opened"
        );

        let dialog = window.cx.update(|cx| {
            pane.read(cx)
                .dialog
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .expect("the pane should hold the dialog it opened")
        });
        let close = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_DIALOG_CLOSE")
            .expect("the dialog should draw the control at its foot")
            .center();
        // The handle the test holds goes first: the layer is what holds the last strong
        // reference to the dialog, and one kept here would keep it — and everything it owns —
        // alive after it has left, which is the very thing asserted below.
        drop(dialog);

        window.simulate_click(close, Modifiers::none());
        for _ in 0..4 {
            settle_dialog(window);
            if !window.cx.update(|cx| pane.read(cx).dialog_open()) {
                break;
            }
        }

        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the pane should be left without the dialog its own control closed"
        );

        // And the pane opens another one: what it was holding was the dialog that left rather
        // than one kept alive by the pane itself.
        window.update(|window, cx| WalletAddressDialog::show(pane.downgrade(), window, cx));
        settle_dialog(window);
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_DIALOG_CLOSE")
                .is_some(),
            "a pane whose dialog has left should open another one"
        );
    }

    #[gpui_kit::test]
    fn a_stream_that_came_back_should_report_itself_again(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| {
            report_status(
                &pane,
                timeline_key(&address("1")),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });

        // A timeline that failed is a row: the pane itself is still connecting.
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert!(!pane.status.state(cx).is_failed());
        });

        cx.update(|cx| report_status(&pane, timeline_key(&address("1")), WalletStatus::Ready, cx));
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).timelines[&address("1")].status,
                ScopeStatus::Ready
            );
        });
    }

    #[gpui_kit::test]
    fn a_wallet_the_venue_answered_nothing_for_should_still_be_drawn(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1")]);
        settle_until(cx, "the scope to be read", |_| {
            source.counters().spot_states == 1
        });

        // The venue publishes totals whose spot and perpetual values are both absent, which is
        // a wallet with nothing in it rather than a failure.
        let reading = overview_snapshot(
            spot_state(Vec::new()),
            perpetual_state(Vec::new()),
            Vec::new(),
            precision(),
        );
        cx.update(|cx| report_overview(&pane, &address("1"), reading, cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content());
            let row = &pane.overview[&address("1")];
            let snapshot = row.snapshot.as_ref().expect("the reading should be held");
            assert!(snapshot.summary.spot_value.is_some());
            assert!(snapshot.summary.perpetual_value.is_some());
            assert!(snapshot.summary.unrealized_pnl.is_some());
        });
    }

    #[gpui_kit::test]
    fn the_timeline_of_a_wallet_should_be_retried_from_its_row(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1")]);
        settle_until(cx, "the timeline to be read", |_| {
            source.counters().fills_by_time == 1
        });
        let reads = source.counters().fills_by_time;

        pane.update(cx, |pane, cx| pane.retry_timeline(address("1"), cx));
        settle_until(cx, "the timeline to be read again", |_| {
            source.counters().fills_by_time == reads + 1
        });
        settle_until(cx, "the retried timeline to be live", |cx| {
            pane.read(cx).timelines[&address("1")].status == ScopeStatus::Ready
        });
    }

    #[gpui_kit::test]
    fn a_subscription_that_reported_nothing_should_be_a_status_not_a_reading(
        cx: &mut TestAppContext,
    ) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);

        // A status is not content, so a pane that only heard from its streams is still empty.
        let changed = cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&address("1")),
                WalletStatus::Connecting,
                cx,
            )
        });
        cx.update(|cx| {
            assert!(!changed, "a repeated status is not a change");
            assert!(!pane.read(cx).has_content());
        });

        // An update that is not the reading this key carries is refused.
        let accepted = cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.apply_latest(
                    overview_key(&address("1")),
                    message(WalletStreamMessage::Overview(reading())),
                    cx,
                )
            })
        });
        assert!(accepted);

        let stale = cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.apply_latest(overview_key(&address("1")), status(WalletStatus::Ready), cx)
            })
        });
        assert!(!stale, "the latest channel never carries a status");
    }

    /// The pane follows the list it behaves on: what it reads, what it draws and what it reports to
    /// the framework are the same list, and adding or removing one address moves all of them.
    ///
    /// The equality between the followed list and `config()` is the write-back contract, and it is
    /// structural rather than behavioural here: the pane has one list, so `addresses()` and
    /// `config()` read the same field and no test can pull them apart. What is falsifiable is the
    /// **value** of that list after each change — a mutation that forgot to store the new list
    /// would leave both sides reading the old one and the value assertions below would fail.
    /// `a_change_to_the_list_should_be_reported_to_the_framework` covers the other half of the
    /// contract, which is that the framework is told.
    #[gpui_kit::test]
    fn the_list_should_be_the_wallets_the_pane_follows_and_reports(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1")]);

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(addresses_of_pane(pane), vec![address("1")]);
            assert_eq!(
                addresses_of_pane(pane),
                pane.config().expect("a persisted list").addresses
            );
        });

        pane.update(cx, |pane, cx| {
            assert_eq!(pane.add_address(address("2"), cx), Ok(()));
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(addresses_of_pane(pane), vec![address("1"), address("2")]);
            assert_eq!(
                addresses_of_pane(pane),
                pane.config().expect("a persisted list").addresses
            );
            assert!(
                pane.timelines.contains_key(&address("2")),
                "the new wallet has a row"
            );
        });

        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(addresses_of_pane(pane), vec![address("2")]);
            assert_eq!(
                addresses_of_pane(pane),
                pane.config().expect("a persisted list").addresses
            );
            assert!(
                !pane.timelines.contains_key(&address("1")),
                "a wallet that left is not drawn any more"
            );
        });
    }

    /// Every change to the list is reported, which is what makes the framework write it back: a
    /// pane that changed a field without reporting it would keep the change in memory only.
    #[gpui_kit::test]
    fn a_change_to_the_list_should_be_reported_to_the_framework(cx: &mut TestAppContext) {
        let reports = Rc::new(Cell::new(0));
        cx.executor().allow_parking();
        cx.update(install);
        cx.run_until_parked();
        let pane = cx.update(|cx| {
            let change = WidgetConfigChange::new({
                let reports = reports.clone();
                move |_| reports.set(reports.get() + 1)
            });
            cx.new(|cx| pane_with_source(vec![address("1")], PendingSource, change, cx))
        });
        assert_eq!(
            reports.get(),
            0,
            "a pane that was only built reports nothing"
        );

        pane.update(cx, |pane, cx| {
            assert_eq!(pane.add_address(address("2"), cx), Ok(()));
        });
        assert_eq!(reports.get(), 1, "the address that was added is reported");

        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));
        assert_eq!(reports.get(), 2, "the address that was removed is reported");
    }

    #[gpui_kit::test]
    fn a_reading_that_lost_its_wallet_should_be_refused(cx: &mut TestAppContext) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let pane = pane_with_fake(cx, &source, vec![address("1")]);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
        settle_until(cx, "the detail to be opened", |cx| {
            pane.read(cx)
                .detail
                .contains_key(&(address("1"), DetailTab::Spot))
        });

        // The wallet is not monitored any more, so a reading of it finds nothing to update.
        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));
        let accepted = cx.update(|cx| {
            report_detail(
                &pane,
                &address("1"),
                DetailTab::Spot,
                spot_snapshot(Vec::new(), precision()),
                cx,
            )
        });

        assert!(!accepted);
    }

    #[gpui_kit::test]
    fn a_wallet_removed_while_its_detail_is_open_should_close_it(cx: &mut TestAppContext) {
        let pane = quiet_pane(cx, vec![address("1")]);
        pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));

        pane.update(cx, |pane, cx| pane.remove_address(&address("1"), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Overview);
            assert_eq!(pane.selected, None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message(
                    "widget.hyperliquid_wallet_monitor.empty_addresses"
                ))
            );
        });
    }

    #[gpui_kit::test]
    fn the_header_should_follow_what_the_pane_is_showing(cx: &mut TestAppContext) {
        let addresses = vec![address("1")];
        let pane = quiet_pane(cx, addresses);
        cx.update(|cx| {
            report_status(
                &pane,
                overview_key(&address("1")),
                WalletStatus::Failed(WalletStreamError::SubscriptionClosed),
                cx,
            )
        });

        let window = cx.add_empty_window();
        // A scope with a failed wallet offers the way to read it again.
        window.update(|window, cx| {
            let header = pane.update(cx, |pane, cx| pane.header(window, cx));
            assert!(header.left.is_some(), "the pane has a manage control");
            assert!(
                header.right.is_some(),
                "a failed wallet is offered a retry in the header"
            );
        });

        window.update(|window, cx| {
            // The wallet is opened the way a reader opens it, so what the header is read against
            // is the pane they would be looking at rather than a selection set behind its back.
            pane.update(cx, |pane, cx| pane.select_target(address("1"), cx));
            let header = pane.update(cx, |pane, cx| pane.header(window, cx));
            assert!(header.left.is_some(), "an open wallet has a back control");
            assert!(header.right.is_some(), "an open tab has a refresh control");
        });
    }

    /// The control a reader comes back with is in the pane's header, which the framework draws
    /// around the body and rebuilds every frame — so the stack is the body's alone, and coming
    /// back is the same control in the same place it has always been.
    #[gpui_kit::test]
    fn the_control_in_the_header_should_bring_a_reader_back_to_the_list(cx: &mut TestAppContext) {
        let target = address("1");
        let pane = quiet_pane(cx, vec![target.clone()]);
        cx.update(|cx| {
            report_overview(&pane, &target, reading(), cx);
            report_timeline(&pane, &target, empty_timeline(), cx);
            pane.update(cx, |pane, cx| pane.select_target(target.clone(), cx));
        });

        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| HeaderHarness { pane }
        });
        window.run_until_parked();

        let back = window
            .debug_bounds("TEST_HYPERLIQUID_WALLET_MONITOR_BACK")
            .expect("an open wallet offers the way back in the header's own corner")
            .center();
        window.simulate_click(back, Modifiers::none());
        window.run_until_parked();

        window.cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.shown(), Shown::Overview);
            assert_eq!(
                pane.stack.read(cx).depth(),
                1,
                "the control in the header is what takes the wallet's page off the stack"
            );
        });
    }

    /// The pane's header on its own, drawn in a window so that a control in it is somewhere a
    /// click can land: the header belongs to the framework's pane, which a test does not build.
    ///
    /// It is built again on every frame here, which is what the framework's pane does with it: a
    /// header is never cached, so a control in it is as fresh as the state behind it. It is mounted
    /// under the layer the application draws dialogs in, because the address control opens one.
    struct HeaderHarness {
        pane: Entity<HyperliquidWalletMonitorWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let left = self
                .pane
                .update(cx, |pane, cx| pane.header(window, cx))
                .left;
            div().size_full().children(left)
        }
    }

    /// A window over a pane's own header, under the layer dialogs are drawn in.
    fn header_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<HyperliquidWalletMonitorWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(move |_| HeaderHarness { pane })));
        window.run_until_parked();

        window
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes down, the
    /// frame that follows is drawn, and only then does it come up.
    fn press_control(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .expect("the harness draws what it is asked to press")
            .center();
        window.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
        window.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
    }

    /// The address control is what opens the address dialog while the pane has a market to read.
    #[gpui_kit::test]
    fn the_address_control_should_open_the_dialog(cx: &mut TestAppContext) {
        let pane = pane(cx, vec![wallet()]);
        let window = header_window(cx, &pane);

        press_control(window, "TEST_HYPERLIQUID_WALLET_MONITOR_MANAGE");
        settle_dialog(window);

        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_ADDRESS_LIST")
                .is_some(),
            "the control in the header is what opens the addresses the pane follows"
        );
    }

    /// The address control refuses input while the pane waits for its market: what it manages is
    /// the readings of a market, and the sentence the pane is waiting with is what says to load one.
    #[gpui_kit::test]
    fn the_address_control_should_refuse_input_while_the_market_is_not_loaded(
        cx: &mut TestAppContext,
    ) {
        let pane = pane_over_unloaded_venue(cx, vec![wallet()]);
        let window = header_window(cx, &pane);

        assert!(
            window.cx.update(|cx| pane.read(cx).awaiting_market),
            "the fixture pane really is waiting for its market"
        );
        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_MONITOR_MANAGE")
                .is_some(),
            "the control is still what the header draws"
        );

        press_control(window, "TEST_HYPERLIQUID_WALLET_MONITOR_MANAGE");
        settle_dialog(window);

        assert!(
            window
                .debug_bounds("TEST_HYPERLIQUID_WALLET_ADDRESS_LIST")
                .is_none(),
            "a control that refuses input opens nothing"
        );
    }

    #[gpui_kit::test]
    fn a_fill_that_arrived_while_the_scope_was_loading_should_still_be_drawn(
        cx: &mut TestAppContext,
    ) {
        let source = TestSource::new(&crate::test_support::test_routes());
        let addresses = vec![address("1")];
        let pane = pane_with_fake(cx, &source, addresses);
        settle_until(cx, "the scope to be read", |_| {
            source.counters().spot_states == 1
        });

        let listing = perpetual_listing();
        publish(
            cx,
            &source,
            SubscriptionKind::FastContexts,
            HyperliquidWalletUpdate::FastAssetContexts {
                contexts: vec![context(&listing, Some("100"), None)],
            },
        );
        settle_until(cx, "the totals to follow the venue", |cx| {
            pane.read(cx).overview[&address("1")]
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.summary.perpetual_value.is_some())
        });
    }
}
