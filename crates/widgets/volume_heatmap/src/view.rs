//! The volume heat map pane.
//!
//! One map of one market participant's spot market: the venue the user picked, its tickers
//! folded into twenty tiles, coloured by which way each asset's price moved, and
//! re-read every ten minutes. The pane's state is therefore the venue it is showing
//! — persisted, so the map a reader comes back to is the map they left — beside
//! what a frame needs to draw: the map that arrived, how far it has been
//! acknowledged, and the countdown to the next read.
//!
//! # Why the venue is a choice rather than a parameter
//!
//! Two market participants quoting one instrument are two numbers about two order books, and
//! a map that added them would report a market nobody quotes. The venue is
//! therefore the map's scope: the stream is keyed by it, the service reads only it,
//! and every tile of the map is one market participant's money. The control that picks it
//! offers exactly the venues whose market is loaded, because a venue that is not
//! loaded publishes no scope this map may read.
//!
//! # What the pane reports, and what it does not
//!
//! It reports the poll through the framework's status handle and nothing else: a
//! failure is a failure, and the framework decides where that is shown and what
//! the recovery control looks like. The pane keeps no banner, no placeholder and
//! no "unexpected response" state of its own; a response that does not match the
//! request cannot happen, because both are typed.
//!
//! # Where the countdown lives
//!
//! The countdown is the refresh button's own label rather than a word beside it.
//! The two are one control's worth of information — when the next read is due, and
//! the way to bring it forward — so they are one control.
//!
//! # The refresh control
//!
//! It is disabled while the pane has nothing to bring forward: connecting, fetching,
//! or waiting for the map its own refresh asked for. That is not decoration: the
//! service refuses a second refresh while one is in flight, and a control that
//! forwarded the click anyway would turn a fast double click into a failure the
//! user has to dismiss. A *failed* pane is the exception, where the control is a
//! way out and runs the same recovery the framework's banner offers.
//!
//! It reads the venue the pane is showing and nothing else: the command names that
//! venue, so a click that arrives after the user moved to another market participant is
//! refused rather than answered with a read of the one they left.

use std::{sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, base::Disableable, div,
};
use i18n::t;
use market_participants_connector::venue::loaded::VenuesUpdated;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use serde::{Deserialize, Serialize};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{LocalizedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    util::{format_countdown, unix_millis},
    venue::{VenueDisplay, route_display_string},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    chart::VolumeHeatMapChart,
    model::HeatMapModelError,
    service::{
        VolumeHeatMapFetchError, VolumeHeatMapProtocol, VolumeHeatMapRequest, VolumeHeatMapService,
        VolumeHeatMapServiceError, VolumeHeatMapSource, VolumeHeatMapStatus,
        VolumeHeatMapStreamKey, VolumeHeatMapStreamMessage,
    },
};

/// How often the pane re-reads the clock behind the countdown.
///
/// It is not a fetch: it only re-checks the deadline the service published. A
/// quarter of a second is faster than the label's own resolution and cheaper than
/// a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// The pane's configuration: the market participant whose map it shows.
///
/// The field is an `Option` because "nothing has been chosen yet" and "the venue
/// this was saved from is not loaded" are the same state of the world as far as a
/// reader is concerned — the pane picks the first loaded venue either way — and a
/// default that named one would be this widget deciding for the user which market
/// they opened the pane for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeHeatMapConfig {
    /// The venue the user picked, which is the one the map is drawn from.
    #[serde(rename = "venue")]
    pub venue: Option<VenueId>,
}

impl WidgetConfig for VolumeHeatMapConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The market this map reads: one venue, and the scopes of it that answer with
/// tickers and publish a catalog.
///
/// The two travel together because they are one question — "which map is this pane
/// reading" — and every decision below is a comparison of two of these. A venue
/// without its routes would say which market participant the pane chose but not what it is
/// reading from it, and a change in either is a different map.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Reading {
    venue: VenueId,
    routes: Arc<[VenueRoute]>,
}

/// Every loaded venue whose market this map may be drawn from, in the order the
/// interface states venues.
///
/// A venue is a candidate exactly when its market is loaded, which is the one
/// question the loaded-venues view answers ([`market_participants_connector::venue::loaded`]):
/// a load commits a venue's whole scope set, catalogs and metadata in one step, and
/// the built-in venues all declare a spot scope, so a loaded venue always has a
/// market this map can read. A venue no longer loaded is no longer offered, and a
/// map of it is replaced by the market that is there.
fn available_venues(cx: &App) -> Vec<VenueId> {
    let connector =
        cx.global::<market_participants_connector::connector::MarketParticipantsConnector>();
    connector
        .supported_venues()
        .into_iter()
        .filter(|venue| connector.venues().loaded(venue).is_some())
        .collect()
}

/// The scopes of one loaded venue that this map is drawn from.
///
/// A route that answers with tickers but publishes no catalog has no listings to
/// resolve them against, so it is not a scope this map can be drawn from. One
/// question is asked of the loaded venues — which scopes carry the capability and
/// publish a catalog — so a pane that reads one market participant never enumerates scopes of
/// its own. The route and the entry it is judged by come from one reading of one
/// commit: [`market_participants_connector::venue::loaded::LoadedVenues::entries`] takes the
/// venue table's lock once, so a load landing while this runs cannot put one
/// commit's route beside another's catalog source.
fn venue_routes(
    connector: &market_participants_connector::connector::MarketParticipantsConnector,
    venue: VenueId,
) -> Arc<[VenueRoute]> {
    connector
        .venues()
        .entries()
        .iter()
        .filter(|(route, entry)| {
            route.venue == venue
                && entry.catalog_source.is_some()
                && entry.capabilities.iter().any(|support| {
                    support.capability == market_participants_model::routing::CapabilityId::Tickers
                })
        })
        .map(|(route, _)| route.clone())
        .collect()
}

/// Whether one route's market is in memory: the scope is loaded.
///
/// Reading a market is only possible once the user has loaded it, and "not
/// loaded" is a wait rather than a failure.
fn market_ready(cx: &App, route: &VenueRoute) -> bool {
    cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
        .venues()
        .scope(route)
        .is_some()
}

/// The markets a notice names while nothing is loaded: the venues whose loader can
/// answer with tickers, in the order supported_venues() answers.
///
/// This is the one static answer a pane may read: the capability's venue
/// name set, never a scope, an origin or a credential. It is what says which
/// markets can answer at all, which is the only honest answer while nothing is
/// loaded.
///
/// What comes back is the venues themselves rather than their names: the notice is a
/// reference into the catalogue, and the sentence that names them is resolved by the
/// framework, in the language showing on the frame that draws it.
fn market_venues() -> Vec<VenueId> {
    market_participants_connector::venue::definition::venues_supporting(
        market_participants_model::routing::CapabilityId::Tickers,
    )
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VolumeHeatMapFailure {
    Fetch(VolumeHeatMapFetchError),
    Rpc(RpcCallError<VolumeHeatMapServiceError>),
}

impl LocalizedError for VolumeHeatMapFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// Whether this failure is the directory answering "no venue's catalog is in memory
/// yet" rather than reporting something wrong.
///
/// The pane's own gate asks that question before it reads anything, so this is the
/// answer changing underneath the request — a venue re-discovering its scopes is what
/// does it. It is a wait, not a failure: loading the market again is what
/// puts the catalogs back. The pane's own service errors are refusals about the
/// stream it is running, so only the market read can carry the directory's answer.
fn is_waiting_for_a_catalog(failure: &VolumeHeatMapFailure) -> bool {
    matches!(
        failure,
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::MarketParticipant(error))
            if market_is_catalog_not_loaded(error)
    )
}

impl LocalizedError for VolumeHeatMapFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::MarketParticipant(error) => error.localized_message(cx),
            Self::Model(error) => error.localized_message(cx),
            Self::TooManyTickers {
                route,
                count,
                limit,
            } => t!(
                cx,
                "error.volume_heatmap.too_many_tickers",
                route = route_display_string(route, cx),
                count = count,
                limit = limit
            ),
            Self::Timeout(timeout) => t!(
                cx,
                "error.volume_heatmap.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl LocalizedError for HeatMapModelError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            // Neither arm names the identity its own field carries: the message shows a
            // reader a localized message, and a canonical `InstrumentId` or `AssetId`
            // digest is an identity rather than a name, so it stays in the field.
            Self::NotionalOverflow { .. } => t!(cx, "error.volume_heatmap.notional_overflow"),
            Self::AssetNotNamed { .. } => t!(cx, "error.volume_heatmap.asset_not_named"),
        }
    }
}

impl LocalizedError for VolumeHeatMapServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t!(cx, "error.volume_heatmap.stream_unavailable")
            }
        }
    }
}

/// One pane: the market participant it draws, the countdown it counts, and its two controls.
pub struct VolumeHeatMapWidget {
    pub config: VolumeHeatMapConfig,
    config_change: WidgetConfigChange,
    binding: WidgetBinding<VolumeHeatMapProtocol>,
    /// The pane's only channel for reporting state.
    ///
    /// Nothing outside this module reads the cell itself: the framework's surface
    /// presents it, and the tests below read it through the extension trait.
    status: Entity<WidgetStatus>,
    pub recovery: WidgetAction,
    /// The venues whose market is loaded, in the order the interface states them.
    ///
    /// Which venues may be read is the loaded venues' answer, and the user may load
    /// one at any time, so it is re-read on every load rather than taken once. Empty
    /// means no market this pane reads is loaded, which is what keeps
    /// [`Self::has_content`] false while the pane waits.
    venues: Vec<VenueId>,
    /// The market the map on screen was read from, or nothing while the pane waits.
    ///
    /// It is the venue *and* its scopes: a venue the user just picked is a different
    /// map even when the two market participants declare the same scope names, and a market
    /// that reloaded with a different scope set is a different stream rather than a
    /// repeat of the one being read.
    reading: Option<Reading>,
    chart: VolumeHeatMapChart,
    /// The revision of the map the pane is showing, which is what makes it content
    /// and what readiness is measured against.
    accepted: Option<u64>,
    /// The revision the service has stated complete. It arrives on the other
    /// channel, so it can arrive before the map it describes.
    ready: Option<u64>,
    /// When the next automatic read is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    pub countdown: Option<SharedString>,
    /// Whether a refresh command this pane sent has been answered *and its round
    /// reported*.
    ///
    /// It is set by the click and cleared when the service says what it did with
    /// it, which is what closes the window between the two: the service holds the
    /// gate for the whole round, and the round is announced after the command is
    /// acknowledged — so a control that trusted the acknowledgement alone would be
    /// live again for the moment in between.
    refresh_pending: bool,
    /// Whether the pane is waiting for its market rather than reading it.
    ///
    /// A status or a map that was already in flight when the market went away can
    /// still reach the pane, and either one writes a status that takes the notice off
    /// the screen: a `Ready` turns it into the loading placeholder, and the map that
    /// follows the readiness turns it into content. Every message entry refuses while
    /// this is set.
    awaiting_market: bool,
    _countdown_task: Task<()>,
}

impl WidgetView for VolumeHeatMapWidget {
    type Config = VolumeHeatMapConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.volume_heatmap");
    const DISPLAY_NAME: &'static str = "widget.names.volume_heatmap";
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: &'static str = "One market participant's spot quote volume, as a heat map";
    const ICON: Option<&'static str> = Some("widgets/volume_heatmap.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .market();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The markets this map reads have to be loaded before it reads
        // anything: a venue the registry does not hold, or one whose catalog was
        // never published, is a market the user has not loaded, and the pane
        // waits for one instead of drawing a map of nothing. The notice names
        // every venue that could answer, which is the only honest thing to say
        // while none of them is in memory.
        let venues = available_venues(cx);
        if venues.is_empty() {
            self.venues = venues;
            self.wait_for_market(cx);
            return;
        }
        // The pane reached here only past its own gate, so at least one venue is
        // loaded and the map it is waiting for is in memory again: messages belong
        // to a market once more.
        self.awaiting_market = false;
        self.venues = venues;
        // A venue this build no longer reads, or one whose market is not loaded any
        // more, is a choice that does not hold: it goes back to "not chosen" and the
        // pane picks from the markets that are there, so a stored venue cannot pin
        // the map to a market participant the user can no longer see.
        if self
            .config
            .venue
            .is_some_and(|venue| !self.venues.contains(&venue))
        {
            self.config.venue = None;
            log::warn!("the volume heat map's venue is not loaded; the pane picks a loaded one");
            self.config_change.publish(cx);
        }
        let venue = match self.config.venue {
            Some(venue) => venue,
            None => {
                let venue = self.venues[0];
                self.config.venue = Some(venue);
                self.config_change.publish(cx);
                venue
            }
        };
        let routes = self.routes_of(cx, venue);
        if routes.is_empty() {
            // The venue is loaded and publishes no scope this map can read. That is
            // the directory answering about a market the user has just loaded, so it
            // is a wait rather than a map of nothing.
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }

        let retry = self.status.is_failed(cx);
        self.reading = Some(Reading { venue, routes });
        self.start(retry, cx);
    }

    /// The map is content once one has arrived, and an *empty* map is content too:
    /// it is what "this market participant is quoting nothing" looks like, which is a fact
    /// about the market rather than a pane that is still waiting.
    ///
    /// A pane that reads no market at all has nothing to draw however much it once
    /// accepted: the market it was reading is gone, and the neutral notice stands
    /// in its place.
    fn has_content(&self) -> bool {
        self.reading.is_some() && self.accepted.is_some()
    }

    /// The venue picker on the left, the refresh on the right.
    ///
    /// The picker is the pane's scope rather than a filter drawn over a finished
    /// map: what it states is the market participant the tiles below it are made of. Neither
    /// control is usable before the market participant's metadata is — both refuse input while the
    /// pane waits for its market, and the picker has nothing to offer while no venue is
    /// loaded.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
            .left(self.venue_select(cx))
            .right(self.refresh_button(cx))
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }
}

impl VolumeHeatMapWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    pub fn new_with_source<S>(
        config: VolumeHeatMapConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: VolumeHeatMapSource,
    {
        let service = VolumeHeatMapService::new(source);
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            service,
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let countdown_task = cx.spawn(async move |this, cx| {
            loop {
                if this
                    .update(cx, |this, cx| {
                        this.tick_countdown(cx);
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(COUNTDOWN_TICK).await;
            }
        });
        // The pane recovers from the markets it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = cx
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .venues()
            .events();
        cx.subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        })
        .detach();

        Self {
            config,
            config_change,
            binding,
            status,
            recovery,
            venues: Vec::new(),
            reading: None,
            chart: VolumeHeatMapChart::new(Arc::from([])),
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown: None,
            refresh_pending: false,
            awaiting_market: false,
            _countdown_task: countdown_task,
        }
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// The pane compares the market it is reading — the venue and the scopes of it
    /// that answer with tickers — with the one that is loaded now: a commit that
    /// leaves both as they are is a publication behind a market the pane already has,
    /// so it only re-checks its precondition and never starts over what the user is
    /// watching. That is this pane's answer even when the commit is a refresh of its own market,
    /// which is the one place it differs from the panes whose streams resolve a listing when they
    /// are opened: every fetch here reads the catalogue generation in memory at the moment it is
    /// taken ([`crate::service::stream::VolumeHeatMapSource`]), so the map that follows such a
    /// commit is already drawn from what that commit published and there is no stream to reopen.
    ///
    /// A market that changed is a different map rather than a publication of the one
    /// being read: a venue loaded while this pane was open becomes a choice the
    /// picker offers, a shrinking commit takes one away, and a venue the user is on
    /// that is gone sends the pane to the market that is there. That is what lets the
    /// map pick up a market loaded after it was opened — the protection is against
    /// restarting a stream that reads the same map, not against reading a market that
    /// changed. A failure is terminal: the event does not revive a failed pane, only
    /// the user's own recovery does.
    fn on_venues_updated(&mut self, _event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        let venues = available_venues(cx);
        if venues.is_empty() {
            // Nothing this pane reads is loaded any more, so nothing on screen is
            // backed by a market: the venue it was showing goes with it.
            self.venues = venues;
            self.config.venue = None;
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }
        // The picker offers the markets that are loaded, and a commit is the only
        // thing that can change which those are: the list is refreshed before
        // anything is compared, so a venue loaded behind the market on screen joins
        // the picker without the map being disturbed.
        self.venues = venues;
        // The chosen venue, kept or replaced the way a load would decide it: the
        // commit is the same question — which markets may this map be drawn from —
        // asked by an event rather than by the framework's own load.
        let venue = self
            .config
            .venue
            .filter(|venue| self.venues.contains(venue))
            .unwrap_or(self.venues[0]);
        let routes = self.routes_of(cx, venue);
        let next = (!routes.is_empty()).then_some(Reading { venue, routes });
        if next == self.reading {
            if !self.market_ready(cx) {
                self.wait_for_market(cx);
            }
            return;
        }
        self.load(cx);
    }

    /// Whether the market the pane reads is in memory: every scope of it is
    /// registered and its catalog has been published.
    fn market_ready(&self, cx: &App) -> bool {
        self.reading.as_ref().is_some_and(|reading| {
            !reading.routes.is_empty() && reading.routes.iter().all(|route| market_ready(cx, route))
        })
    }

    /// The scopes of one venue this map is drawn from, as the loaded venues answer
    /// them now.
    fn routes_of(&self, cx: &App, venue: VenueId) -> Arc<[VenueRoute]> {
        venue_routes(
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>(),
            venue,
        )
    }

    /// Reports the neutral notice for the markets this map reads, and drops what it
    /// was showing.
    ///
    /// The routes behind that map are gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a status or a map already in flight
    /// would otherwise write over the notice. The stream itself is left running:
    /// cancelling the binding would cancel the service behind it, and a service that
    /// is gone cannot read the market the user loads next.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.reading = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        self.refresh_pending = false;
        self.chart.set_entries(Arc::from([]));
        self.report_markets_not_loaded(cx);
        cx.notify();
    }

    /// Reports the notice naming the markets this map reads.
    ///
    /// This is the one place that reference is produced: every path that finds no
    /// market to read reports it. The sentence is the framework's to resolve, in the
    /// language showing on the frame that draws it.
    ///
    /// The pane waits for every venue that answers with tickers and lists every one of
    /// them: it reads the whole market, so the venues it can read at all and the venues
    /// it waits for are the same answer.
    fn report_markets_not_loaded(&self, cx: &mut Context<Self>) {
        let venues = market_venues();
        self.status.report(
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
            cx,
        );
    }

    /// Messages are reported through the binding, which a test drives by hand: it
    /// is cancelled first so the service behind it cannot answer at the same time.
    #[cfg(test)]
    pub(crate) fn cancel_binding(&self) {
        self.binding.cancel();
    }

    /// Whether the map on screen is showing `revision`.
    #[cfg(test)]
    pub(crate) const fn accepted_revision(&self) -> Option<u64> {
        self.accepted
    }

    /// Starts reading, or restarts for a map the user is retrying.
    ///
    /// A retry keeps the map the user is still reading — blanking it to reconnect
    /// would flash a screen of data away — while an explicit load starts over,
    /// because the pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        // The command that was in flight belongs to the generation this call is
        // replacing, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        if !retry {
            self.accepted = None;
            self.chart.set_entries(Arc::from([]));
        }
        self.status.report(WidgetState::Connecting, cx);
        let Some(reading) = self.reading.clone() else {
            return;
        };
        self.binding.call(
            VolumeHeatMapRequest::Load {
                venue: reading.venue,
                routes: reading.routes,
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut Context<Self>,
    ) {
        self.refresh_pending = false;
        if let Err(error) = result {
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(VolumeHeatMapFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut Context<Self>,
    ) {
        // An accepted command stays pending until the round it started is reported:
        // the acknowledgement and the round are two different messages, and the
        // gate the service holds spans both.
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(VolumeHeatMapFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// framework around the body, so publishing it is enough.
    ///
    /// A status about another venue is not this pane's to apply: the pane shows one
    /// market participant, and the key is what says which one the message is about.
    pub(crate) fn apply_reliable(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_about_the_map_on_screen(key, cx) {
            return false;
        }
        let VolumeHeatMapStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            VolumeHeatMapStatus::Connecting { .. } => {
                self.status.report(WidgetState::Connecting, cx);
            }
            VolumeHeatMapStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(WidgetState::Loading, cx);
            }
            VolumeHeatMapStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between
                // them, so this can arrive before the map it describes: claiming
                // readiness now would be claiming a map the pane does not have.
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            VolumeHeatMapStatus::Failed { error, .. } => {
                self.refresh_pending = false;
                let failure = VolumeHeatMapFailure::Fetch(error.clone());
                // A read the directory answered "not loaded" to is a market that is no
                // longer loaded rather than a failure: the pane goes back to its
                // neutral notice, and the directory's next publication is what brings
                // it out of it.
                if is_waiting_for_a_catalog(&failure) {
                    self.reading = None;
                    self.wait_for_market(cx);
                    return false;
                }
                let recovery = self.recovery.clone();
                self.status
                    .report(WidgetState::failed(Arc::new(failure), recovery), cx);
            }
        }
        false
    }

    /// Applies a map the stream published.
    ///
    /// Returns whether anything a frame draws changed, which is what the framework
    /// turns into a repaint: the body is cached, and a map that folds into the same
    /// pixels is not worth a frame.
    pub(crate) fn apply_latest(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_about_the_map_on_screen(key, cx) {
            return false;
        }
        let VolumeHeatMapStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        // A map the pane has already overtaken would only make the tiles jump
        // backwards: within one generation the service's revision only moves
        // forwards, so a smaller one can only be a map a status has replaced.
        if self.accepted.is_some_and(|held| snapshot.revision < held) {
            return false;
        }
        let changed = self.chart.set_entries(snapshot.entries.clone());
        self.accepted = Some(snapshot.revision);
        // Readiness is published at the protocol's own boundary: a complete map is
        // not the same thing as a map a status said was complete, and this is
        // where the two meet.
        if self.ready.is_some_and(|ready| snapshot.revision >= ready) {
            self.status.report(WidgetState::Ready, cx);
        }
        changed
    }

    /// Whether a message belongs to the map the pane is showing.
    ///
    /// Three things have to hold, and none of them is implied by the others: the
    /// message has to be about the venue the pane reads, the pane has to be reading
    /// rather than waiting for a market, and the pane must not have failed — a
    /// failure is terminal until the user recovers, and a message that arrives after
    /// it is about a stream that no longer exists.
    fn is_about_the_map_on_screen(
        &self,
        key: VolumeHeatMapStreamKey,
        cx: &impl AppContext,
    ) -> bool {
        if self.status.is_failed(cx) || self.awaiting_market {
            return false;
        }
        self.reading
            .as_ref()
            .is_some_and(|reading| reading.venue == key.venue())
    }

    /// The deadline this pane counts down to, if it has one to count down to.
    ///
    /// A countdown belongs to a pane that is *ready*: it counts down the time left
    /// until the next automatic read of the map that is on screen. While a read is in
    /// flight there is nothing to count down to — the deadline the service announced
    /// belongs to the read after the one being waited for — so a number that started
    /// moving then would be counting down to a map the pane has not received.
    fn countdown_deadline(&self, cx: &impl AppContext) -> Option<i64> {
        let status = self.status.state(cx);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, fetching
    /// or failed has nothing to bring forward — the first two are already waiting on
    /// data, and a failure is a state the framework offers its own recovery over,
    /// above the body — and so has one that is waiting for its market, which is not
    /// reading anything and says so with a placeholder of its own. So the control is an
    /// icon alone and refuses input the rest of the time, and carries the countdown only
    /// while there is one.
    fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(cx).is_none()
    }

    /// The picker that decides which market participant the map is drawn from.
    ///
    /// It offers the markets that are loaded and nothing else, because a map can only
    /// be drawn from a market that is in memory. The choice is identified by the
    /// venue's canonical identity and *labelled* with its name in the language
    /// showing now, so the same choice survives a language switch instead of being
    /// looked up by a translated string.
    fn venue_select(&self, cx: &Context<Self>) -> AnyElement {
        let selected = self.config.venue;
        let label = selected.map_or_else(
            || t!(cx, "widget.volume_heatmap.choose_venue"),
            |venue| venue.display_string(cx),
        );
        let disabled = self.venue_picker_is_disabled();
        let select = self.venues.iter().copied().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "VolumeHeatMapVenue",
            )))
            .placeholder(label)
            .filled()
            .small()
            .disabled(disabled)
            .selected_item(selected.map(|venue| SharedString::new(venue.as_str())))
            .aria_label(t!(cx, "common.market_participants")),
            |select, venue| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(venue.as_str()),
                    venue.display_string(cx),
                )))
            },
        );

        select
            .on_change({
                let widget = cx.entity();
                move |venue, _, app| {
                    let Some(venue) = venue
                        .as_deref()
                        .and_then(|value| VenueId::from_name(value).ok())
                    else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_venue(venue, cx));
                }
            })
            .into_any_element()
    }

    /// Whether the venue picker has nothing to choose between.
    ///
    /// Nothing can be chosen while no market is loaded, and there is exactly one market: the control
    /// would be a list of one. It says what is on screen either way, and it is the pane's own header
    /// that draws it, not the notice the framework shows in the body. A pane that is waiting for its
    /// market has nothing to switch to either — the venues it was built from are the ones the last
    /// load answered, and a map cannot be read from any of them yet.
    fn venue_picker_is_disabled(&self) -> bool {
        self.awaiting_market || self.venues.len() < 2
    }

    /// Shows another market participant's map.
    ///
    /// The map on screen belongs to the market participant being left, so it is dropped rather
    /// than carried over: the pane cannot vouch for tiles the market it now reads did
    /// not produce. Picking the venue that is already shown is not a change — the
    /// picker refuses it, and this guard says the same thing one step later.
    fn select_venue(&mut self, venue: VenueId, cx: &mut Context<Self>) {
        if self.config.venue == Some(venue) {
            return;
        }
        self.config.venue = Some(venue);
        self.config_change.publish(cx);
        self.reading = None;
        self.accepted = None;
        self.chart.set_entries(Arc::from([]));
        self.load(cx);
    }

    /// The control that reads the market again, and the time left until the next
    /// automatic read.
    fn refresh_button(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.refresh_is_disabled(cx);
        Button::new(ElementId::Name(SharedString::new_static(
            "VolumeHeatMapRefresh",
        )))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(t!(cx, "widget.volume_heatmap.refresh_now"))
        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
        .children(self.countdown.clone())
        .on_click({
            let widget = cx.entity();
            move |_, _, app| {
                widget.update(app, |this, cx| this.refresh(cx));
            }
        })
        .into_any_element()
    }

    /// Reads the market again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers:
    /// a failure is recovered through the framework's own control, above the body.
    /// The command names the venue on screen, which is what makes it a read of the
    /// map the user is looking at rather than of whichever market participant the loop happens
    /// to be serving.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        let Some(reading) = self.reading.as_ref() else {
            return;
        };
        self.refresh_pending = true;
        self.binding.call(
            VolumeHeatMapRequest::Refresh {
                venue: reading.venue,
            },
            cx,
            Self::apply_refresh_result,
        );
        cx.notify();
    }

    /// Rebuilds the countdown, asking for the frame that draws it.
    ///
    /// The countdown is the only thing this pane draws that moves on its own, and
    /// it moves once a second: the task wakes four times a second so a second is
    /// never missed, and only the write that changed it asks for a frame. The
    /// header is not cached, so nothing else here has to announce itself.
    ///
    /// The label is the deadline of a ready pane and nothing else, so leaving the
    /// ready state — a read starting, a failure, a reload — takes the countdown off
    /// the button in the same tick that it greys it.
    fn tick_countdown(&mut self, cx: &mut Context<Self>) {
        let label = self.countdown_deadline(cx).map(|due| {
            SharedString::new(format_countdown(due.saturating_sub(unix_millis())).as_str())
        });
        if self.countdown == label {
            return;
        }
        self.countdown = label;
        cx.notify();
    }
}

impl Render for VolumeHeatMapWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the
        // ready state and nothing else. Everything the pane is *doing* is drawn by
        // the framework around this.
        let canvas = self.chart.element(ElementId::Name(SharedString::new_static(
            "VolumeHeatMapCanvas",
        )));

        // A map that covers no instrument draws no tile at all, which leaves the
        // canvas and a failed load looking the same. The map is still the pane's
        // content — the venue published it — so the pane says what it holds rather
        // than letting the framework's placeholder speak for it. The canvas stays:
        // the message stands over the map's own box rather than replacing it.
        let empty = self.chart.is_empty().then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "VolumeHeatMapEmpty",
                    t!(cx, "common.no_data"),
                ))
                .into_any_element()
        });

        div()
            .relative()
            .size_full()
            .min_h_0()
            .child(canvas)
            .children(empty)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{App, Entity, TestAppContext};
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{catalog::test_support, market::error::MarketError};
    use market_participants_model::{decimal::Quantity, routing::CapabilityId};
    use smol_str::SmolStr;
    use widget::protocol::RequestId;
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        model::{HeatMapEntry, PriceDirection},
        service::{
            VolumeHeatMapResponse,
            test_support::{FakeSource, binance, okx},
        },
    };

    /// A map of one tile is enough for every assertion here.
    fn entries() -> Arc<[HeatMapEntry]> {
        Arc::from([HeatMapEntry {
            instrument: test_support::spot_token("fixture", "BTC", "USDT"),
            base_asset_label: SmolStr::new_static("BTC"),
            notional_asset: test_support::token("fixture", "USDT"),
            notional: Quantity::from(120_u64),
            notional_label: SmolStr::new_static("120"),
            direction: PriceDirection::Up,
        }])
    }

    fn ready(revision: u64) -> VolumeHeatMapStatus {
        VolumeHeatMapStatus::Ready {
            venue: binance(),
            revision,
            next_refresh_at: unix_millis() + COUNTDOWN,
        }
    }

    fn refreshing() -> VolumeHeatMapStatus {
        VolumeHeatMapStatus::Refreshing {
            venue: binance(),
            next_refresh_at: unix_millis() + COUNTDOWN,
        }
    }

    /// A deadline far enough ahead that the label it produces is a real countdown
    /// rather than one that has already run out.
    const COUNTDOWN: i64 = 600_000;

    fn failed() -> VolumeHeatMapStatus {
        VolumeHeatMapStatus::Failed {
            venue: binance(),
            error: VolumeHeatMapFetchError::MarketParticipant(MarketError::ServiceClosed),
        }
    }

    /// A status about the market participant the pane is *not* showing.
    fn another_venues_status() -> VolumeHeatMapStatus {
        VolumeHeatMapStatus::Ready {
            venue: okx(),
            revision: 9,
            next_refresh_at: unix_millis() + COUNTDOWN,
        }
    }

    /// Installs the market participants root and the directory a pane reads, with every venue
    /// that answers with tickers loaded or not.
    ///
    /// A loaded market is one commit of every one of its scopes, catalogs and metadata
    /// routes, so the fixture publishes an (empty) catalog for every route the
    /// registry holds: what a pane draws comes from the source a test hands it.
    fn install(cx: &mut App, loaded: bool) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let catalog = Arc::new(market_participants_connector::catalog::store::Catalog::new());
        let connector =
            market_participants_connector::test_support::install_root(cx, Arc::clone(&catalog));
        if !loaded {
            return;
        }
        let venues = connector.supported_venues();
        connector
            .load_fixture(
                &market_participants_connector::test_support::definitions(&venues),
                &[],
                &[],
            )
            .expect("the venue's routes should load");
        let routes = connector.venues().scopes();
        let sources = routes
            .iter()
            .map(|route| test_support::binding(&source_id(route), test_support::pending()))
            .collect::<Vec<_>>();
        let registrations = routes
            .iter()
            .map(|route| test_support::registration(route, &source_id(route)))
            .collect::<Vec<_>>();
        test_support::commit_fixture_routes(
            &catalog,
            market_participants_connector::catalog::source::CatalogRouteUpdate::new(
                sources,
                registrations,
            ),
            &test_support::requirements(&routes),
        )
        .expect("the fixture routes should register in the directory");
        let published = routes
            .iter()
            .cloned()
            .map(|route| (route, Vec::new()))
            .collect::<Vec<_>>();
        test_support::publish_fixture_catalogs(cx, &published);
    }

    /// The catalog source id of one fixture route.
    fn source_id(route: &VenueRoute) -> String {
        format!("fixture-{}-{}", route.venue, route.scope)
    }

    /// Loads the venues named with the scopes their own loaders publish.
    fn load_venues(cx: &App, venues: &[market_participants_model::identifier::VenueId]) {
        cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(venues),
                &[],
                &[],
            )
            .expect("the venue's routes should load");
    }

    /// Loads one venue again with no scope at all, the way a shrinking commit takes
    /// every scope away: a venue that publishes nothing is not a loaded venue.
    fn unload_venue(cx: &App, venue: &market_participants_model::identifier::VenueId) {
        cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .load_fixture(&[], &[(*venue, Vec::new())], &[])
            .expect("the shrinking commit lands");
    }

    /// The scopes of one venue that answer with tickers, as the pane reads them.
    fn ticker_routes_of(
        cx: &App,
        venue: market_participants_model::identifier::VenueId,
    ) -> Vec<VenueRoute> {
        venue_routes(
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>(),
            venue,
        )
        .to_vec()
    }

    /// Creates a pane the way the framework does: status first, then one load.
    ///
    /// The fake never answers, so the load the pane starts stays in flight and the
    /// tests below are the only thing that ever reports to it. A fake that answered
    /// would race them: the service publishes on its own task, and a test that
    /// reported a map at the same time would be asserting against whichever of the
    /// two arrived last.
    fn pane_with(cx: &mut TestAppContext) -> Entity<VolumeHeatMapWidget> {
        pane_over(cx, true)
    }

    /// The same, over a fixture whose venues are loaded or not.
    ///
    /// Every fetch of the round stays in flight, so a pane a test leaves alone stays in the
    /// middle of its first round rather than being answered by a fixture that ran out of plans.
    fn pane_over(cx: &mut TestAppContext, loaded: bool) -> Entity<VolumeHeatMapWidget> {
        pane_over_source(cx, FakeSource::pending(), loaded)
    }

    /// The same, over a source a test keeps so it can count what the pane asked for.
    fn pane_over_source(
        cx: &mut TestAppContext,
        source: FakeSource,
        loaded: bool,
    ) -> Entity<VolumeHeatMapWidget> {
        pane_over_config(cx, source, loaded, VolumeHeatMapConfig::default())
    }

    /// The same, over a stored configuration.
    fn pane_over_config(
        cx: &mut TestAppContext,
        source: FakeSource,
        loaded: bool,
        config: VolumeHeatMapConfig,
    ) -> Entity<VolumeHeatMapWidget> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx, loaded);
            let pane = cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: gpui_kit::WeakEntity<VolumeHeatMapWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |pane, cx| pane.load(cx));
                });
                VolumeHeatMapWidget::new_with_source(
                    config,
                    source,
                    status,
                    WidgetConfigChange::default(),
                    recovery,
                    cx,
                )
            });
            pane.update(cx, |pane, cx| pane.load(cx));
            pane
        })
    }

    /// Counts the venue commits it is told about.
    ///
    /// A test that asserts a pane did *not* act on a commit still has to know the commit reached
    /// it: this probe subscribes the same way and after the pane did, so the delivery it counts is
    /// one the pane's own handler has already seen.
    struct CommitProbe {
        deliveries: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CommitProbe {
        fn new(
            cx: &mut Context<Self>,
            events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
            deliveries: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            cx.subscribe(events, |probe, _, _: &VenuesUpdated, _| {
                probe
                    .deliveries
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
            .detach();
            Self { deliveries }
        }
    }

    /// Gives the executor turns until the source has been asked for `expected` markets.
    ///
    /// A round runs on the application runtime rather than on the test's own queue, so waiting
    /// for it means yielding to that runtime the way the load does.
    fn settle_fetches(cx: &mut TestAppContext, source: &FakeSource, expected: usize) {
        settle_until(cx, "the map to ask for its markets", || {
            source.fetch_count() >= expected
        });
    }

    /// Gives the executor turns until `expected` commits have reached their subscribers.
    fn settle_deliveries(
        cx: &mut TestAppContext,
        deliveries: &Arc<std::sync::atomic::AtomicUsize>,
        expected: usize,
    ) {
        settle_until(cx, "the commit to reach its subscribers", || {
            deliveries.load(std::sync::atomic::Ordering::Acquire) >= expected
        });
    }

    /// Polls a condition that becomes true on the application runtime.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Reports a status as the stream would.
    ///
    /// Cancelling the binding first is what makes the reported message the only
    /// input the pane has: the load it was still holding is dropped with the
    /// runtime, so nothing else can answer.
    fn report_status(
        pane: &Entity<VolumeHeatMapWidget>,
        status: VolumeHeatMapStatus,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            pane.cancel_binding();
            pane.apply_reliable(
                VolumeHeatMapStreamKey::new(binance()),
                Arc::new(VolumeHeatMapStreamMessage::Status(status)),
                context,
            )
        })
    }

    /// Reports a map as the stream would.
    fn report_map(pane: &Entity<VolumeHeatMapWidget>, revision: u64, cx: &mut App) -> bool {
        pane.update(cx, |pane, context| {
            pane.cancel_binding();
            pane.apply_latest(
                VolumeHeatMapStreamKey::new(binance()),
                Arc::new(VolumeHeatMapStreamMessage::Snapshot(Arc::new(
                    crate::service::VolumeHeatMapSnapshot {
                        revision,
                        entries: entries(),
                    },
                ))),
                context,
            )
        })
    }

    #[gpui_kit::test]
    fn a_pane_should_ask_for_nothing_until_a_map_arrives(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            assert!(!pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert!(
                pane.read(cx).status.state(cx).failure().is_none(),
                "a pane that is still connecting has nothing to report"
            );
        });
    }

    /// A scope that answers with tickers but publishes no catalog is not a scope this map can
    /// be drawn from: a ticker has no listing to be resolved against, so it is not read —
    /// while a scope beside it that publishes one is.
    #[gpui_kit::test]
    fn a_route_that_publishes_no_catalog_should_not_be_read(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);
        let venue = market_participants_model::identifier::VenueId::Binance;

        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &[],
                    &[(
                        venue,
                        vec![
                            // The scope beside it: it publishes a catalog, so it is read.
                            scope_entry("usd-m-perp", &[CapabilityId::Tickers], true),
                            scope_entry("spot", &[CapabilityId::Tickers], false),
                        ],
                    )],
                    &[],
                )
                .expect("the fixture scopes load");
            let routes = pane.update(cx, |pane, cx| {
                pane.venues = available_venues(cx);
                pane.routes_of(cx, venue)
            });

            assert_eq!(
                routes.as_ref(),
                [test_support::route("binance", "usd-m-perp")],
                "a scope that answers with tickers but publishes no catalog has no listing to \
                 be resolved against"
            );
        });
    }

    /// A loaded scope that does not carry tickers is not a scope this map reads, however
    /// loaded and however catalogued it is: the scopes are the ones that answer with a
    /// ticker, and one that does not is not a column of the map.
    #[gpui_kit::test]
    fn a_route_that_does_not_answer_tickers_should_not_be_read(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);
        let venue = market_participants_model::identifier::VenueId::Binance;

        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &[],
                    &[(
                        venue,
                        vec![
                            // The scope beside it: it answers with tickers, so it is read.
                            scope_entry("usd-m-perp", &[CapabilityId::Tickers], true),
                            scope_entry("spot", &[CapabilityId::Candlesticks], true),
                        ],
                    )],
                    &[],
                )
                .expect("the fixture scopes load");
            let routes = pane.update(cx, |pane, cx| {
                pane.venues = available_venues(cx);
                pane.routes_of(cx, venue)
            });

            assert_eq!(
                routes.as_ref(),
                [test_support::route("binance", "usd-m-perp")],
                "a loaded scope that carries no tickers is not a market this map is drawn from"
            );
            assert!(
                !routes.iter().any(|route| route.scope.as_str() == "spot"),
                "the scope that does not answer with tickers is excluded, not merely outpaced: \
                 {routes:?}"
            );
        });
    }

    /// One scope of a fixture load: it carries `capabilities`, and publishes a catalog when
    /// `publishes` says so.
    fn scope_entry(
        scope: &str,
        capabilities: &[CapabilityId],
        publishes: bool,
    ) -> market_participants_connector::venue::definition::ScopeEntry {
        market_participants_connector::venue::definition::ScopeEntry {
            scope: market_participants_model::identifier::VenueScopeId::new(scope)
                .expect("the fixture scope is canonical"),
            catalog_source: publishes.then(|| {
                market_participants_connector::catalog::source::CatalogSourceId::new(format!(
                    "fixture-{scope}"
                ))
                .expect("the fixture source id is canonical")
            }),
            normalization: Vec::new(),
            capabilities: capabilities
                .iter()
                .map(|capability| {
                    market_participants_connector::venue::definition::CapabilitySupport {
                        capability: *capability,
                        needs_auth: false,
                    }
                })
                .collect(),
        }
    }

    /// A market the user has not loaded is a wait rather than a failure (⑮): the
    /// pane names what to load and has nothing to draw — not even for a map it
    /// once accepted, because nothing on screen is backed by a market any more
    /// (⑰). This is the pane's whole answer while nothing is loaded, and it is the
    /// framework's own sentence that says where to load one.
    #[gpui_kit::test]
    fn an_unloaded_market_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);

        cx.update(|cx| {
            let status = pane.read(cx).status.state(cx);
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
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") && sentence.contains("OKX"),
                "the notice names the markets that answer with tickers: {sentence}"
            );
            assert!(
                sentence.contains("top left corner") && sentence.contains("Markets"),
                "and it points at the control that loads one: {sentence}"
            );
            assert!(!pane.read(cx).has_content());

            report_map(&pane, 1, cx);
            assert!(
                !pane.read(cx).has_content(),
                "a venue the registry does not hold is no market to draw"
            );
        });
    }

    /// A pane that is waiting for its market has no header controls to offer either: the venue
    /// picker chooses between markets that are in memory, and the refresh has no schedule to bring
    /// forward. These are the two answers the controls in the header read.
    #[gpui_kit::test]
    fn waiting_for_the_market_should_disable_the_header_controls(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);

        cx.update(|cx| {
            assert!(
                pane.read(cx).awaiting_market,
                "the fixture pane really is waiting for its market"
            );
            assert!(
                pane.read(cx).venue_picker_is_disabled(),
                "there is no market to switch the map to, so the picker refuses input"
            );
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "and there is no schedule for the refresh to bring forward"
            );
        });
    }

    /// The words a waiting pane shows are the ones the window's language resolves them in: the
    /// notice is a reference into the catalogue, resolved on the frame that draws it, so the
    /// pane reports nothing again — nothing loads again, and nothing re-reports by hand.
    #[gpui_kit::test]
    fn a_pane_waiting_for_its_markets_should_follow_the_language(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);

        let (notice, english) = cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("the pane waits for its markets");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") && sentence.contains("OKX"),
                "the notice names the markets that answer with tickers: {sentence}"
            );

            (notice, sentence.to_string())
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("币安") && sentence.contains("欧易"),
                "the same reference resolves in the language showing now: {sentence}"
            );
            assert_ne!(
                sentence, english,
                "the copy of the language the interface left is gone: {sentence}"
            );
        });
    }

    /// A stream that is still alive after its markets went away must not write a
    /// status over the notice: the pane waits, and a status or a map already in flight
    /// belongs to markets the pane no longer reports reading. Without the guard the
    /// `Ready` turns the notice into the loading placeholder and the map that follows
    /// the readiness turns it into content.
    #[gpui_kit::test]
    fn a_late_message_after_the_market_went_away_should_not_clear_the_notice(
        cx: &mut TestAppContext,
    ) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            // The pane is reading, which is the state a revocation interrupts.
            report_map(&pane, 1, cx);
            assert!(pane.read(cx).has_content(), "the map is what arrived");

            // Every venue loaded again and published no scope at all: the pane's own
            // gate is what notices, and it reports the neutral notice.
            let venues = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .supported_venues();
            for venue in &venues {
                unload_venue(cx, venue);
            }
            pane.update(cx, |pane, cx| pane.load(cx));

            let waiting = pane.read(cx).status.state(cx);
            assert!(
                waiting.notice().is_some(),
                "the pane reports the neutral notice"
            );
            assert!(waiting.failure().is_none(), "waiting is not a failure");

            report_status(&pane, ready(2), cx);
            assert!(
                pane.read(cx).status.state(cx).notice().is_some(),
                "the notice is what the pane reports after a late status"
            );

            report_map(&pane, 2, cx);
            assert!(
                !pane.read(cx).has_content(),
                "a map from the market that went away must not become content again"
            );
            assert!(
                pane.read(cx).status.state(cx).notice().is_some(),
                "the notice is what the pane reports after a late map"
            );
        });
    }

    /// The picker offers exactly the markets that are loaded, in the order the
    /// interface states venues, and a venue loaded after the pane was opened joins
    /// them.
    #[gpui_kit::test]
    fn the_venue_picker_should_offer_every_loaded_market(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);

        cx.update(|cx| {
            assert!(
                pane.read(cx).venues.is_empty(),
                "nothing is loaded, so nothing can be chosen"
            );
        });

        // The probe subscribes after the pane did, so a delivery it counts is one the
        // pane's own handler has already seen: waiting on it is what makes the two
        // commits below deterministic.
        let deliveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .venues()
                .events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });

        cx.update(|cx| load_venues(cx, &[okx()]));
        settle_deliveries(cx, &deliveries, 1);
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).venues,
                [okx()],
                "a loaded market is a choice the picker offers"
            );
            load_venues(cx, &[binance()]);
        });
        settle_deliveries(cx, &deliveries, 2);
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).venues,
                [binance(), okx()],
                "the venues are offered in the order the interface states them"
            );
        });
    }

    /// The choice is the venue's canonical identity rather than its name, which is
    /// what keeps it a choice when the language changes: the picker's labels are
    /// translated and the selection is not looked up by one.
    #[gpui_kit::test]
    fn the_venue_choice_should_survive_a_language_change(cx: &mut TestAppContext) {
        let pane = pane_over_config(
            cx,
            FakeSource::pending(),
            true,
            VolumeHeatMapConfig { venue: Some(okx()) },
        );

        cx.update(|cx| {
            assert_eq!(pane.read(cx).config.venue, Some(okx()));
            assert_eq!(
                pane.read(cx).reading.as_ref().map(|reading| reading.venue),
                Some(okx()),
                "the pane reads the venue its configuration names"
            );
            assert_eq!(
                okx().display_string(cx).as_ref(),
                "OKX",
                "the picker states the venue in the language showing now"
            );

            cx.set_global(LanguageCode::ZhCn);

            assert_eq!(
                pane.read(cx).config.venue,
                Some(okx()),
                "a translated name cannot move the selection"
            );
            assert_eq!(okx().display_string(cx).as_ref(), "欧易");
        });
    }

    /// A stored venue this build no longer reads, or one whose market is not loaded,
    /// is a choice that does not hold: it goes back to "not chosen" and is reported,
    /// and the pane reads a market that is there.
    #[gpui_kit::test]
    fn a_stored_venue_that_is_not_loaded_should_be_reset_and_reported(cx: &mut TestAppContext) {
        let reported = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pane = cx.update(|cx| {
            // Only Binance is loaded, so the stored OKX venue is not one the pane may
            // read: it is the state a stored choice falls into when the market behind
            // it is not there.
            install(cx, false);
            load_venues(cx, &[binance()]);
            let pane = cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: gpui_kit::WeakEntity<VolumeHeatMapWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |pane, cx| pane.load(cx));
                });
                let reported = Arc::clone(&reported);
                VolumeHeatMapWidget::new_with_source(
                    VolumeHeatMapConfig { venue: Some(okx()) },
                    FakeSource::pending(),
                    status,
                    WidgetConfigChange::new(move |_| {
                        reported.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    }),
                    recovery,
                    cx,
                )
            });
            pane.update(cx, |pane, cx| pane.load(cx));
            pane
        });

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).config.venue,
                Some(binance()),
                "the market that is loaded is the one the pane falls back to"
            );
            assert!(
                reported.load(std::sync::atomic::Ordering::Acquire) > 0,
                "a reset that is not reported is a reset the user sees every start"
            );
        });
    }

    /// A commit that leaves the pane's market as it was — the venue it reads loading
    /// again with the same scopes — is a publication behind a market the pane already
    /// has: it re-checks its precondition and keeps the round it is running rather
    /// than starting over on what the user is watching.
    ///
    /// What proves the pane did not start over is the source's own fetch count: a round asks
    /// every scope the map reads once, so a second round is visible as more fetches.
    #[gpui_kit::test]
    fn a_commit_that_leaves_the_panes_market_alone_does_not_restart_its_round(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::pending();
        let pane = pane_over_source(cx, source.clone(), true);
        cx.update(|cx| {
            load_venues(cx, &[binance()]);
            pane.update(cx, |pane, cx| pane.load(cx));
        });
        let reading = cx.read(|cx| {
            pane.read(cx)
                .reading
                .clone()
                .expect("the pane reads a market")
        });
        assert!(
            !reading.routes.is_empty(),
            "the map reads the scopes of the venue whose market is loaded"
        );
        settle_fetches(cx, &source, reading.routes.len());
        let fetches = source.fetch_count();
        assert_eq!(
            fetches,
            reading.routes.len(),
            "a round asks every scope the map reads exactly once"
        );

        // The venue loads again with the same scopes: a commit of a market the pane already has.
        let deliveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .venues()
                .events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        cx.update(|cx| load_venues(cx, &[binance()]));
        settle_deliveries(cx, &deliveries, 1);
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).reading.as_ref(),
                Some(&reading),
                "the market the map reads is the one it was already reading"
            );
            assert_eq!(
                source.fetch_count(),
                fetches,
                "a commit that leaves the map's market as it was must not start its round over"
            );
        });
    }

    /// A venue loaded while the pane is already reading is a market the picker
    /// offers from that moment on, without the map on screen being disturbed: the
    /// commit is not a publication of the market being read.
    #[gpui_kit::test]
    fn a_venue_loaded_after_the_map_arrived_should_join_the_picker(cx: &mut TestAppContext) {
        let pane = pane_over(cx, false);
        cx.update(|cx| {
            load_venues(cx, &[binance()]);
        });
        // A commit is announced on the market participants root's own task, so the pane starts
        // reading the market on the executor.
        cx.run_until_parked();
        let before = cx.read(|cx| {
            pane.read(cx)
                .reading
                .clone()
                .expect("the pane reads the loaded venue")
        });
        assert_eq!(before.venue, binance());

        cx.update(|cx| {
            load_venues(cx, &[okx()]);
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).venues,
                [binance(), okx()],
                "the market loaded after the pane was opened is a choice it offers"
            );
            assert_eq!(
                pane.read(cx).reading.as_ref(),
                Some(&before),
                "and it is not a reason to stop reading the market that is on screen"
            );
        });
    }

    /// The venue the pane is reading going away sends it to the market that is
    /// there: nothing on screen is backed by a market any more, so the pane reads
    /// another one rather than keeping tiles nothing published.
    #[gpui_kit::test]
    fn unloading_the_venue_on_screen_should_move_the_pane_to_the_loaded_one(
        cx: &mut TestAppContext,
    ) {
        let pane = pane_over(cx, true);
        cx.update(|cx| {
            load_venues(cx, &[binance(), okx()]);
            pane.update(cx, |pane, cx| pane.load(cx));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).config.venue,
                Some(binance()),
                "the first loaded venue is what a pane with no stored choice reads"
            );

            unload_venue(cx, &binance());
            for route in ticker_routes_of(cx, okx()) {
                test_support::publish_fixture_catalogs(cx, &[(route, Vec::new())]);
            }
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).config.venue,
                Some(okx()),
                "the pane moves to the market that is loaded"
            );
            assert_eq!(
                pane.read(cx).reading.as_ref().map(|reading| reading.venue),
                Some(okx())
            );
        });
    }

    /// Loading the venues is what a waiting pane recovers from, by itself and
    /// without a poll (⑯).
    ///
    /// What proves it recovered is that the service behind the binding actually ran a
    /// round: a pane that had cancelled its binding while it waited would be told the
    /// stream is closed, and no fetch would ever reach the source.
    #[gpui_kit::test]
    fn loading_the_venues_should_let_the_waiting_pane_start(cx: &mut TestAppContext) {
        let source = FakeSource::pending();
        let pane = pane_over_source(cx, source.clone(), false);
        cx.update(|cx| {
            assert!(
                pane.read(cx).status.state(cx).notice().is_some(),
                "the pane waits for a market nothing has published"
            );

            let venues = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .supported_venues();
            load_venues(cx, &venues);
            let routes = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .venues()
                .scopes();
            // The catalogs a load publishes are what makes each scope a market the pane may
            // read; the commit the load itself makes is what tells the pane to look again.
            for route in routes.iter() {
                test_support::publish_fixture_catalogs(cx, &[(route.clone(), Vec::new())]);
            }
        });
        // The commit is announced on the market participants root's own task, so the event that re-runs
        // the load arrives on the executor, and the load it starts reaches the service the
        // pane already had.
        cx.run_until_parked();
        settle_fetches(cx, &source, 1);

        cx.update(|cx| {
            let status = pane.read(cx).status.state(cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(
                status.failure().is_none(),
                "the pane started reading rather than failing: {:?}",
                status.failure()
            );
            assert_eq!(status, WidgetState::Connecting);
        });
    }

    /// Choosing another market participant drops the map the previous one published: the pane
    /// cannot vouch for tiles the market it now reads did not produce.
    #[gpui_kit::test]
    fn choosing_another_venue_should_drop_the_map_of_the_previous_one(cx: &mut TestAppContext) {
        let pane = pane_with(cx);
        cx.update(|cx| {
            load_venues(cx, &[binance(), okx()]);
            pane.update(cx, |pane, cx| pane.load(cx));
            report_map(&pane, 1, cx);
            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).accepted_revision(), Some(1));

            pane.update(cx, |pane, cx| pane.select_venue(okx(), cx));

            let pane = pane.read(cx);
            assert!(
                !pane.has_content(),
                "the map of the market participant the user left is not content for the one they chose"
            );
            assert_eq!(pane.accepted_revision(), None);
            assert_eq!(
                pane.reading.as_ref().map(|reading| reading.venue),
                Some(okx())
            );
        });
    }

    /// A status about another market participant is not this pane's to apply: what is on
    /// screen is one market's map, and a message about another one would report a
    /// state for tiles that are not there.
    #[gpui_kit::test]
    fn a_status_about_another_venue_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane_with(cx);
        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, ready(1), cx);
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);

            let applied = pane.update(cx, |pane, context| {
                pane.apply_reliable(
                    VolumeHeatMapStreamKey::new(okx()),
                    Arc::new(VolumeHeatMapStreamMessage::Status(another_venues_status())),
                    context,
                )
            });

            assert!(!applied, "a message about another venue is not a change");
            assert_eq!(
                pane.read(cx).accepted_revision(),
                Some(1),
                "the map on screen is the one the pane is reading"
            );
        });
    }

    /// An empty map is content: it is what "this market participant is quoting nothing" looks
    /// like, which is a fact about the market rather than a pane still waiting.
    #[gpui_kit::test]
    fn an_empty_map_should_still_be_content(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
        });

        cx.update(|cx| {
            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).accepted_revision(), Some(1));
        });
    }

    /// Readiness and the map arrive on two channels with no order between them, so
    /// the pane has to wait for the map a status describes rather than believe it.
    #[gpui_kit::test]
    fn readiness_before_the_map_it_describes_should_keep_the_pane_loading(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_status(&pane, ready(1), cx);
            assert!(!pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Loading);
        });
        cx.update(|cx| {
            report_map(&pane, 1, cx);
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn a_map_the_pane_has_overtaken_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 2, cx);
            assert!(
                !report_map(&pane, 1, cx),
                "an older revision is not a change"
            );
            assert_eq!(pane.read(cx).accepted_revision(), Some(2));
        });
    }

    #[gpui_kit::test]
    fn a_failed_map_should_offer_the_recovery_it_was_given(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, failed(), cx);

            let status = pane.read(cx).status.state(cx);
            let failure = status.failure().expect("a failed round is reported");
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "every failure carries the framework's own way out"
            );
            assert!(!failure.text(cx).is_empty());
        });
    }

    /// A failure is terminal for the pane: what arrives after it is a message about
    /// a scope that no longer exists, and acting on it would show a map over the
    /// recovery the user is looking at.
    #[gpui_kit::test]
    fn messages_after_a_failure_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, failed(), cx);

            assert!(!report_map(&pane, 2, cx), "a late map is not a change");
            assert!(!report_status(&pane, ready(2), cx));
            assert!(pane.read(cx).status.is_failed(cx));
            assert_eq!(pane.read(cx).accepted_revision(), Some(1));
            assert!(pane.read(cx).countdown.is_none());
        });
    }

    /// The countdown belongs to a ready pane, and only to a ready pane.
    ///
    /// A round in flight has a deadline too — the one it will arm when it lands — but
    /// counting down to it before the map is on screen would be counting down to a map
    /// the pane does not have yet. So the label appears with readiness and is taken
    /// away by anything that is not it, and the control is greyed with a bare icon for
    /// the same stretch.
    #[gpui_kit::test]
    fn the_countdown_should_only_run_while_the_pane_is_ready(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, ready(1), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("10:00")),
                "a ready pane counts down to the deadline the service published"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });

        cx.update(|cx| {
            report_status(&pane, refreshing(), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).countdown.is_none(),
                "a round in flight has no countdown to show"
            );
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "and nothing for the control to bring forward"
            );
        });

        cx.update(|cx| {
            report_status(
                &pane,
                VolumeHeatMapStatus::Ready {
                    venue: binance(),
                    revision: 2,
                    next_refresh_at: unix_millis() + 60_000,
                },
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).countdown.is_none(),
                "readiness without the map it describes is not something to count down from"
            );
            assert!(pane.read(cx).refresh_is_disabled(cx));

            report_map(&pane, 2, cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("01:00")),
                "and with the map in hand the countdown starts on the new deadline"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });
    }

    /// A failure is the framework's to recover from, not the header's.
    ///
    /// A failed pane takes no more of its stream's messages, so a refresh command sent
    /// from here would be accepted by the loop and its answer dropped by the pane. The
    /// control is therefore greyed with the rest of the states that are not ready, and
    /// the way out is the recovery the banner above the body offers.
    #[gpui_kit::test]
    fn a_failed_map_should_offer_its_recovery_rather_than_a_refresh(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, ready(1), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(pane.read(cx).countdown.is_some());

            report_status(&pane, failed(), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "the header has nothing to bring forward, so it offers nothing"
            );
            assert!(
                pane.read(cx).countdown.is_none(),
                "and a pane whose schedule stopped has nothing to count down to"
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
                "the framework's own recovery is the way out of it"
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

    /// The double-click this control exists to absorb.
    ///
    /// A refresh is refused by the service while one is in flight, and the
    /// framework shows a refusal as a failure — so a control that forwarded a
    /// second click inside that window would answer a fast double click with a
    /// banner. Each step below is one way the window is open, and the last is the
    /// one an acknowledgement alone would leave open.
    #[gpui_kit::test]
    fn a_refresh_should_not_be_forwarded_while_a_map_is_being_fetched(cx: &mut TestAppContext) {
        let pane = pane_with(cx);

        cx.update(|cx| {
            report_map(&pane, 1, cx);
            report_status(&pane, ready(1), cx);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "an idle pane with a map can read again on demand"
            );
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(
                    pane.refresh_is_disabled(cx),
                    "the click closes the window itself, before anything has answered"
                );
                // The service acknowledges the command before it runs the round it
                // started, so the acknowledgement is not the end of the window.
                pane.apply_refresh_result(Ok(VolumeHeatMapResponse::RefreshQueued), cx);
                assert!(
                    pane.refresh_is_disabled(cx),
                    "a queued refresh is still a refresh in flight"
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
                "readiness without the map it describes is not a reason to read again"
            );
            report_map(&pane, 2, cx);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "and with the map in hand there is nothing left to wait for"
            );
        });
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            VolumeHeatMapWidget::ID.as_str(),
            "kytrix.market.volume_heatmap"
        );
        assert_eq!(
            VolumeHeatMapWidget::DISPLAY_NAME,
            "widget.names.volume_heatmap"
        );
        assert_eq!(
            VolumeHeatMapConfig::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    /// Every message this pane can show, in one place.
    ///
    /// A key that is missing from a locale translates to an empty string rather
    /// than to an error, so a gap in the interface is what a typo looks like: this
    /// is the test that turns it into a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for key in [
                "widget.names.volume_heatmap",
                "widget.volume_heatmap.refresh_now",
                "widget.volume_heatmap.choose_venue",
                "common.market_participants",
                "error.volume_heatmap.notional_overflow",
                "error.volume_heatmap.too_many_tickers",
                "error.volume_heatmap.timeout",
                "error.volume_heatmap.stream_unavailable",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
            }

            let failures = [
                VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::MarketParticipant(
                    MarketError::ServiceClosed,
                )),
                VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::Model(
                    HeatMapModelError::NotionalOverflow {
                        instrument: Box::new(test_support::spot_token("fixture", "BTC", "USDT")),
                    },
                )),
                VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::TooManyTickers {
                    route: test_support::route("binance", "spot"),
                    count: 10_001,
                    limit: 10_000,
                }),
                VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::Timeout(Duration::from_secs(
                    5,
                ))),
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
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }
        });
    }
}
