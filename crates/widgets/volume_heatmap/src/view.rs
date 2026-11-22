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
//! offers exactly the loaded venues with a scope this map can be drawn from: a venue
//! that is not loaded publishes no market to read, and one that answers tickers a
//! listing at a time cannot answer the whole-scope read the map is made of.
//!
//! # What the pane reports, and what it does not
//!
//! It reports the poll through the framework's status handle and nothing else: a
//! failure is a failure, and the framework decides where that is shown and what
//! the recovery control looks like. The pane keeps no banner, no placeholder and
//! no "unexpected response" state of its own; a response that does not match the
//! request cannot happen, because both are typed.
//!
//! # The body is one canvas and its empty state
//!
//! The pane's whole body is the map, and the engine's only self-drawing entry point is a
//! `Canvas` node, so [`WidgetView::body`] builds one host node, the chart's canvas into it,
//! and the stand-in that says a published map covering no instrument is still a map
//! ([`VolumeHeatMapWidget::sync_empty_overlay`]). After that the chart owns its drawing:
//! `sync` rebuilds the canvas when what it would draw moved
//! ([`VolumeHeatMapChart`]), and nothing rebuilds the body.
//!
//! # Where the countdown lives
//!
//! The countdown is the refresh control's own label beside the button rather than a
//! word elsewhere. The two are one control's worth of information — when the next read
//! is due, and the way to bring it forward — so they are one group. (The old control
//! drew the time left *inside* the button; a kit button's label is chosen when it is
//! built and a countdown moves once a second, so the label is a `bind_text` node beside
//! the button — registered as a W4a difference.)
//!
//! # The header controls
//!
//! The picker is the pane's scope rather than a filter drawn over a finished
//! map: what it states is the market participant the tiles below it are made of. Every
//! control's key comes from [`WidgetCx::unique_key`], so two panes of this widget never
//! share one handler (AGENTS §1 rule 11). A picker's and a button's copy is chosen when it
//! is built, so a language switch rebuilds the button and re-states the picker's items;
//! what the picker *offers* is its item list (`set_items`), which a commit moves without a
//! rebuild. The choice a control reports is written into a request signal and applied by
//! the next `sync`, because a control's handler owns no widget context (AGENTS §4.2).
//!
//! # Why a reply is matched by request identity
//!
//! Two commands share one inbox, and a load or a venue switch replaces the command before
//! it. Draining happens before anything in the frame can replace a command, so an answer
//! that was delivered before the replacement is applied to the command it belongs to; on
//! top of that every reply is attributed by the framework `RequestId` it answers
//! ([`widget::binding::Pending`]), which is what keeps a replaced command's failure from
//! being reported over the command that replaced it.
//!
//! # The directory
//!
//! The pane never loads the directory and never polls the catalogue: it re-reads the
//! answers its [`MarketDirectory`] gives on every load, and re-runs that load when the
//! injected [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3). A venue the directory stopped offering — the market was
//! unloaded, or the venue no longer publishes a scope this map reads — is not a choice
//! that holds: it goes back to "not chosen" and the pane reads the market that is there.

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::role::Role,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
    connector::MarketParticipantsConnector,
    venue::definition::{ScopeEntry, venues_supporting},
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::{Event, InputEvent},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    util::{format_countdown, unix_millis},
    venue::{VenueDisplay, route_display_string},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

#[cfg(test)]
use crate::service::VolumeHeatMapSource;
use crate::{
    chart::VolumeHeatMapChart,
    model::HeatMapModelError,
    service::{
        VolumeHeatMapFetchError, VolumeHeatMapProtocol, VolumeHeatMapRequest, VolumeHeatMapService,
        VolumeHeatMapServiceError, VolumeHeatMapStatus, VolumeHeatMapStreamKey,
        VolumeHeatMapStreamMessage,
    },
};

/// How often the pane re-reads the clock behind the countdown.
///
/// It is not a fetch: it only re-checks the deadline the service published. A
/// quarter of a second is faster than the label's own resolution and cheaper than
/// a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// The widget-local names of the header controls' kit keys.
///
/// A kit key is **application-level** — a button derives its click action from it and two
/// panes that build the same constant key share one handler — so the pane never spells one
/// out: [`WidgetCx::unique_key`] stamps each suffix with the pane's own instance scope
/// (AGENTS §1 rule 11).
const REFRESH_KEY_SUFFIX: &str = "volume_heatmap.header.refresh";
const VENUE_KEY_SUFFIX: &str = "volume_heatmap.header.venue";

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

/// The market this map reads: one venue, and the scopes of it that answer the
/// route-level bulk ticker read and publish a catalog.
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

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VolumeHeatMapFailure {
    Fetch(VolumeHeatMapFetchError),
    Rpc(RpcCallError<VolumeHeatMapServiceError>),
}

impl TranslatedError for VolumeHeatMapFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for VolumeHeatMapFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::MarketParticipant(error) => error.translated_message(translate),
            Self::Model(error) => error.translated_message(translate),
            Self::TooManyTickers {
                route,
                count,
                limit,
            } => t_smol!(
                translate,
                "error.volume_heatmap.too_many_tickers",
                route = route_display_string(route, translate),
                count = count,
                limit = limit
            ),
            Self::Timeout(timeout) => t_smol!(
                translate,
                "error.volume_heatmap.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl TranslatedError for HeatMapModelError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            // Neither arm names the identity its own field carries: the message shows a
            // reader a localized message, and a canonical `InstrumentId` or `AssetId`
            // digest is an identity rather than a name, so it stays in the field.
            Self::NotionalOverflow { .. } => {
                t_smol!(translate, "error.volume_heatmap.notional_overflow")
            }
            Self::AssetNotNamed { .. } => {
                t_smol!(translate, "error.volume_heatmap.asset_not_named")
            }
        }
    }
}

impl TranslatedError for VolumeHeatMapServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t_smol!(translate, "error.volume_heatmap.stream_unavailable")
            }
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

/// Whether one loaded scope is a market this map is drawn from.
///
/// The scope has to answer the route-level bulk ticker read: the map folds the
/// tickers of a whole market, so a scope that answers one listing at a time cannot
/// produce one, however many tickers it can serve one by one. It also has to publish
/// a catalog, because a ticker carries a listing key and the catalog is the only
/// thing that says which listing and instrument that key names.
fn reads_the_whole_market(entry: &ScopeEntry) -> bool {
    entry.catalog_source.is_some()
        && entry
            .capabilities
            .iter()
            .any(|support| support.capability == CapabilityId::AllTickers)
}

/// Every loaded venue with a market this map may be drawn from, in the order the
/// interface states venues.
///
/// A venue is a candidate exactly when one of its loaded scopes is one this map reads
/// ([`reads_the_whole_market`]): a loaded market participant that answers tickers one
/// listing at a time — Longbridge, today — publishes no route-level read this map is
/// made of, so it is not offered even though its market is in memory. A venue no
/// longer loaded is no longer offered either, and a map of it is replaced by the
/// market that is there.
///
/// The two inputs are arguments rather than a connector so the rule is a pure function of
/// the directory's own answer: one reading of one commit decides both, and a test can ask
/// the question without a venue table.
fn readable_venues(
    supported: &[VenueId],
    entries: &[(VenueRoute, Arc<ScopeEntry>)],
) -> Vec<VenueId> {
    supported
        .iter()
        .copied()
        .filter(|venue| {
            entries
                .iter()
                .any(|(route, entry)| route.venue == *venue && reads_the_whole_market(entry))
        })
        .collect()
}

/// The scopes of one loaded venue that this map is drawn from.
///
/// One question is asked of the loaded venues — which scopes carry the whole-scope
/// ticker read and publish a catalog — so a pane that reads one market participant never
/// enumerates scopes of its own. The route and the entry it is judged by come from
/// one reading of one commit:
/// [`market_participants_connector::venue::loaded::LoadedVenues::entries`] takes the
/// venue table's lock once, so a load landing while this runs cannot put one
/// commit's route beside another's catalog source.
fn routes_of(entries: &[(VenueRoute, Arc<ScopeEntry>)], venue: VenueId) -> Vec<VenueRoute> {
    entries
        .iter()
        .filter(|(route, entry)| route.venue == venue && reads_the_whole_market(entry))
        .map(|(route, _)| route.clone())
        .collect()
}

/// The markets a notice names while nothing is loaded: the venues whose loader can
/// answer the route-level bulk ticker read, in the order supported_venues() answers.
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
    venues_supporting(CapabilityId::AllTickers)
}

/// The loaded market directory one pane reads.
///
/// Production wraps the injected connector; a test drives a fixture, because the
/// framework's headless host does not build a connector. The pane only asks the questions
/// a load needs: which loaded venues publish a market this map can be drawn from, which
/// scopes of one of them answer the whole-scope read, and whether a scope's catalog is in
/// memory.
trait MarketDirectory {
    /// The loaded venues this map can be drawn from, in the directory's order.
    fn readable_venues(&self) -> Vec<VenueId>;

    /// The scopes of one venue this map reads, as the directory holds them now.
    fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute>;

    /// Whether one route's market is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;
}

/// The application directory: the injected market-participants connector.
struct ConnectorDirectory(MarketParticipantsConnector);

impl MarketDirectory for ConnectorDirectory {
    fn readable_venues(&self) -> Vec<VenueId> {
        readable_venues(&self.0.supported_venues(), &self.0.venues().entries())
    }

    fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute> {
        routes_of(&self.0.venues().entries(), venue)
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }
}

/// One command this pane sent and has not seen the answer to.
///
/// Two commands share the binding, so the tag names which one a reply answers: a reply
/// whose identity is not tracked answers a call the pane already replaced (a venue switch,
/// a reload, or a failure the user recovered from) and is dropped rather than applied to
/// whichever command happens to be pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The load that starts a map's stream.
    Start,
    /// A refresh of the map on screen.
    Refresh,
}

/// What a header control asks the pane to do.
///
/// A control's handler runs with an application context and no widget context, so it
/// cannot change the pane: it writes the request and asks for a frame, and the next
/// `sync` applies it (AGENTS §4.2 — a control asks for the next `sync` instead of
/// re-entering the widget).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Venue(VenueId),
}

/// The countdown the header draws, and the one-shot timer that keeps it moving.
///
/// It is shared with the timer callback (`Rc`), which has only `&mut App`: the deadline
/// the label counts to lives in a `Cell`, the label itself is a signal, and the callback
/// re-arms the timer. Every deadline change goes through `update`, which cancels the
/// pending tick before it writes the label and arms the next chain — a deadline that
/// flickers cannot leave two chains ticking (I3: the pane is woken once per tick, not
/// twice), and `release` cancels through the same path.
struct Countdown {
    /// The window the timer belongs to.
    window: WindowId,
    /// The wall-clock millisecond the next read is due at, or nothing worth counting
    /// to (a read in flight, a failure, a schedule that stopped).
    deadline: Cell<Option<i64>>,
    /// The time left, as the header's text node draws it.
    label: Signal<Option<SmolStr>>,
    /// The wall clock, which is what a deadline published in Unix milliseconds is
    /// comparable against; a test hands in a clock it can move.
    now: Rc<dyn Fn() -> i64>,
    /// The tick this pane is waiting for, if any. Holding the id is what makes a
    /// deadline change and `release` able to cancel it instead of leaving it to fire.
    timer: std::cell::RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: Cell::new(None),
            label: cx.signal(None),
            now,
            timer: std::cell::RefCell::new(None),
        }
    }

    /// The deadline the label is counting to.
    fn deadline(&self) -> Option<i64> {
        self.deadline.get()
    }

    /// Moves the countdown to a new deadline (or stops it), and re-arms the timer.
    ///
    /// The label is written here rather than only on the next tick: a new answer
    /// carries a new deadline, and the header has to state it on the frame that shows
    /// the answer. The pending tick is cancelled first, so a deadline that comes back
    /// before the old tick was due cannot leave a second chain running.
    fn update(self: &Rc<Self>, app: &mut App, deadline: Option<i64>) {
        self.deadline.set(deadline);
        if let Some(id) = self.timer.borrow_mut().take() {
            app.cancel_timer(&id);
        }
        self.write_label(app);
        if deadline.is_some() {
            self.arm(app);
        }
    }

    /// Recomputes the label from the clock; returns whether it changed.
    ///
    /// The write only requests a frame when the label really moved (`App::write`
    /// short-circuits equal values), which is what keeps a quiet 250 ms tick from
    /// repainting the pane four times a second.
    fn write_label(&self, app: &mut App) -> bool {
        app.write(
            &self.label,
            countdown_label(self.deadline.get(), (self.now)()),
        )
    }

    /// Arms the one-shot tick for the next interval, if there is anything to count to.
    fn arm(self: &Rc<Self>, app: &mut App) {
        if self.deadline.get().is_none() {
            return;
        }
        let this = Rc::clone(self);
        match app.after(self.window, COUNTDOWN_TICK, move |app| this.tick(app)) {
            Ok(id) => *self.timer.borrow_mut() = Some(id),
            Err(error) => {
                log::error!("the volume heat map countdown could not arm its next tick: {error}");
            }
        }
    }

    /// One tick: write the label and re-arm the next one.
    ///
    /// The timer it just served is already spent, so the field is cleared first; the
    /// arm then owns the only pending tick this chain has.
    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

/// The venue picker this pane built, and the copy it was built with.
///
/// A kit picker's item labels are chosen when the items are handed to it (a language
/// switch re-states them, AGENTS §15.1); its *selection* is not — a picker's trigger text
/// is a `bind_text` over the picker's own signals, so a commit that offers another venue
/// is `set_items`.
struct VenueControl {
    /// The widget's own row inside the group the pane made, which the widget removes
    /// itself.
    root: NodeId,
    select: Select,
    /// The language the items were stated in.
    language: LanguageCode,
    /// The venues the items were built from, so a commit that changed them is compared
    /// rather than re-stated every frame.
    venues: Vec<VenueId>,
}

/// The refresh control this pane built: the button, the countdown beside it, and the
/// language the button's copy was built in.
struct RefreshControl {
    root: NodeId,
    /// The node the button lives in.
    ///
    /// A kit button's label is chosen when it is built, so a language switch replaces it;
    /// the slot is what keeps the replaced button in the row's first position without the
    /// widget having to move nodes.
    slot: NodeId,
    button: Button,
    language: LanguageCode,
}

/// One pane: the market participant it draws, the countdown it counts, and its two controls.
pub struct VolumeHeatMapWidget {
    pub config: VolumeHeatMapConfig,
    config_change: WidgetConfigChange,
    binding: Option<WidgetBinding<VolumeHeatMapProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    /// The pane's only channel for reporting state.
    status: WidgetStatus,
    pub recovery: WidgetAction,
    /// The loaded market directory, when the host has one.
    directory: Option<Rc<dyn MarketDirectory>>,
    /// The canvas the body built, and the map it draws.
    chart: VolumeHeatMapChart,
    /// The body root, once the body exists.
    body_root: Option<NodeId>,
    /// The stand-in drawn over a published map that covers no instrument.
    empty: Option<NodeId>,
    /// The language the stand-in's copy was built in.
    empty_language: Option<LanguageCode>,
    /// The venues with a loaded market this map can be drawn from, in the order the
    /// interface states them.
    ///
    /// Which venues may be read is the loaded venues' answer together with the
    /// capability they publish, and the user may load one at any time, so it is
    /// re-read on every load rather than taken once. Empty means no market this pane
    /// reads is loaded, which is what keeps [`Self::has_content`] false while the pane
    /// waits.
    venues: Vec<VenueId>,
    /// The market the map on screen was read from, or nothing while the pane waits.
    ///
    /// It is the venue *and* its scopes: a venue the user just picked is a different
    /// map even when the two market participants declare the same scope names, and a market
    /// that reloaded with a different scope set is a different stream rather than a
    /// repeat of the one being read.
    reading: Option<Reading>,
    /// The revision of the map the pane is showing, which is what makes it content
    /// and what readiness is measured against.
    accepted: Option<u64>,
    /// The revision the service has stated complete. It arrives on the other
    /// channel, so it can arrive before the map it describes.
    ready: Option<u64>,
    /// When the next automatic read is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
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
    /// The generation this pane has already reacted to.
    seen_generation: u64,
    /// The directory generation the composition root publishes.
    generation: Signal<u64>,
    /// Where a header control hands over what the user did.
    requests: Signal<Option<HeaderRequest>>,
    /// The venue picker this pane built, if the pane has built its header row.
    venue_control: Option<VenueControl>,
    /// The refresh control this pane built, if the pane has built its header row.
    refresh_control: Option<RefreshControl>,
}

impl WidgetView for VolumeHeatMapWidget {
    type Config = VolumeHeatMapConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.volume_heatmap");
    const DISPLAY_NAME: &'static str = "widget.names.volume_heatmap";
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.volume_heatmap");
    const ICON: Option<&'static str> = Some("widgets/volume_heatmap.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        // A host without a directory has nothing to ask and nothing to start; the pane
        // reports the neutral notice and waits. Every application supplies one.
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                config,
                None,
                None,
                status,
                config_change,
                recovery,
                now,
                cx,
            ));
        };
        let service = VolumeHeatMapService::new(connector.market());
        let binding = WidgetBinding::spawn(cx, service);
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory(connector));
        Ok(Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            now,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The markets this map reads have to be loaded before it reads
        // anything: a venue the registry does not hold, or one whose catalog was
        // never published, is a market the user has not loaded, and the pane
        // waits for one instead of drawing a map of nothing. The notice names
        // every venue that could answer, which is the only honest thing to say
        // while none of them is in memory.
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let venues = directory.readable_venues();
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
        // A venue this build no longer reads, one whose market is not loaded any
        // more, or one that publishes no route-level read this map is made of, is a
        // choice that does not hold: it goes back to "not chosen" and the pane picks
        // from the markets that are there, so a stored venue cannot pin the map to a
        // market participant the user can no longer see.
        if self
            .config
            .venue
            .is_some_and(|venue| !self.venues.contains(&venue))
        {
            self.config.venue = None;
            log::warn!(
                "the volume heat map's venue is not one this map can read; the pane picks a readable one"
            );
            self.config_change.publish(cx.app_mut());
        }
        let venue = match self.config.venue {
            Some(venue) => venue,
            None => {
                let venue = self.venues[0];
                self.config.venue = Some(venue);
                self.config_change.publish(cx.app_mut());
                venue
            }
        };
        let routes = directory.routes_of(venue);
        if routes.is_empty() {
            // The venue is loaded and publishes no scope this map can read. That is
            // the directory answering about a market the user has just loaded, so it
            // is a wait rather than a map of nothing.
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }

        let retry = self.status.is_failed(cx.app());
        self.reading = Some(Reading {
            venue,
            routes: routes.into(),
        });
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

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
            // The framework builds a body once; a second call is a caller that did not
            // read the contract, and building the canvas node twice would double it.
            return Ok(());
        }
        let root = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_BODY")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.chart.build(cx, root)?;
        self.body_root = Some(root);
        self.sync_empty_overlay(cx)?;
        // The canvas node has no box until the frame that lays this body out, so the map
        // cannot be painted on the frame that builds it: ask for the one that can.
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The directory generation is the one signal the framework's surface root also
        // reads, so a write schedules the frame that runs this hook — including for a
        // pane that is only waiting and has no body. Nothing else may poll the directory
        // (AGENTS §8.3).
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.reload_market(cx);
            }
        }

        // The binding delivers messages instead of calling back into the widget; this is
        // the one place they are applied, where the widget has a context again.
        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        // A header control owns no `&mut self`; it asks for the transition it was used
        // for, and this is where the request becomes one. The read is taken out of the
        // signal before the write: an engine signal is not copied on read, so a write
        // while a read guard is alive would alias the value it is reading.
        let request = *cx.read(&self.requests);
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is
        // one frame away, so ask for that frame.
        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        self.chart.sync(cx)?;
        self.sync_empty_overlay(cx)?;

        // The countdown follows the schedule the state now states. Comparing first is
        // what keeps a steady frame from cancelling and re-arming the same tick.
        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }
        self.sync_header(cx)?;

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match group {
            HeaderGroup::Left => {
                // The pane is building its row; the control this widget built into the
                // previous one went with it. Only the components are the widget's to take
                // down.
                self.release_venue_control(cx, true);
                let control = self.build_venue_control(cx, parent)?;
                self.venue_control = Some(control);
            }
            HeaderGroup::Right => {
                self.release_refresh_control(cx, true);
                let control = self.build_refresh_control(cx, parent)?;
                self.refresh_control = Some(control);
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        // A pointer that is over the map follows the tile under it; one that left
        // says so. Either way only a *tile* that moved is a change worth a frame: a
        // pointer that moved a pixel inside one tile is the same drawing.
        let moved = match input {
            InputEvent::PointerMove { position, .. }
            | InputEvent::PointerEnter { position, .. } => self.chart.point_at(*position),
            InputEvent::PointerLeave { .. } => self.chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        // The header's group nodes belong to the pane, which removes its row: only the
        // components' signal scopes and action handlers are this widget's to release.
        self.release_venue_control(cx, false);
        self.release_refresh_control(cx, false);
        // The chart's canvas node lives in the body, which the framework removes after
        // this hook; the node and its cache are the chart's own, so they go here.
        self.chart.release(cx);
        // The countdown tick is this widget's: the framework's subtree teardown does not
        // own it, so release is where it is cancelled.
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.empty = None;
        self.empty_language = None;
        self.body_root = None;
        self.reading = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.pending.clear();
    }
}

impl VolumeHeatMapWidget {
    /// Builds a widget over the collaborators a caller hands it.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        config: VolumeHeatMapConfig,
        binding: Option<WidgetBinding<VolumeHeatMapProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        now: Rc<dyn Fn() -> i64>,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let requests = cx.signal(None);
        let countdown = Rc::new(Countdown::new(cx, Rc::clone(&now)));

        Self {
            config,
            config_change,
            binding,
            pending: Pending::new(),
            status,
            recovery,
            directory,
            chart: VolumeHeatMapChart::new(),
            body_root: None,
            empty: None,
            empty_language: None,
            venues: Vec::new(),
            reading: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            awaiting_market: false,
            seen_generation,
            generation,
            requests,
            venue_control: None,
            refresh_control: None,
        }
    }

    /// Builds a pane over a test's own source, directory and clock.
    ///
    /// The framework's headless host composes the services without a directory (the
    /// connector still needs the application's own runtime to build), so an interface
    /// test constructs the widget directly with the fixture it wants, and drives the
    /// widget hooks itself.
    #[cfg(test)]
    fn with_source<S: VolumeHeatMapSource>(
        config: VolumeHeatMapConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        now: Rc<dyn Fn() -> i64>,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = VolumeHeatMapService::new(source);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            now,
            cx,
        )
    }

    /// Runs the load a new directory generation may have unblocked.
    ///
    /// The pane compares the market it is reading — the venue and the scopes of it
    /// that answer the route-level bulk ticker read — with the one that is loaded now: a
    /// commit that leaves both as they are is a publication behind a market the pane
    /// already has, so it only re-checks its precondition and never starts over what the
    /// user is watching. That is this pane's answer even when the commit is a refresh of
    /// its own market, which is the one place it differs from the panes whose streams
    /// resolve a listing when they are opened: every fetch here reads the catalogue
    /// generation in memory at the moment it is taken
    /// ([`crate::service::stream::VolumeHeatMapSource`]), so the map that follows such a
    /// commit is already drawn from what that commit published and there is no stream to
    /// reopen.
    ///
    /// A market that changed is a different map rather than a publication of the one
    /// being read: a venue loaded while this pane was open becomes a choice the
    /// picker offers, a shrinking commit takes one away, and a venue the user is on
    /// that is gone sends the pane to the market that is there. That is what lets the
    /// map pick up a market loaded after it was opened — the protection is against
    /// restarting a stream that reads the same map, not against reading a market that
    /// changed. A failure is terminal: the generation does not revive a failed pane, only
    /// the user's own recovery does.
    fn reload_market(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let venues = directory.readable_venues();
        if venues.is_empty() {
            // Nothing this pane reads is loaded any more, so nothing on screen is
            // backed by a market: the venue it was showing goes with it.
            self.venues = venues;
            self.config.venue = None;
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }
        // The picker offers the markets that are loaded and readable, and a commit is
        // the only thing that can change which those are: the list is refreshed before
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
        let routes = directory.routes_of(venue);
        let next = (!routes.is_empty()).then(|| Reading {
            venue,
            routes: routes.into(),
        });
        if next == self.reading {
            if !self.market_ready(directory.as_ref()) {
                self.wait_for_market(cx);
            }
            return;
        }
        self.load(cx);
    }

    /// Whether the market the pane reads is in memory: every scope of it is
    /// registered and its catalog has been published.
    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        self.reading.as_ref().is_some_and(|reading| {
            !reading.routes.is_empty()
                && reading
                    .routes
                    .iter()
                    .all(|route| directory.is_loaded(route))
        })
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
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.reading = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.refresh_pending = false;
        // The commands in flight name a market the pane is no longer reading; their
        // answers are late, not this pane's (AGENTS §8.2).
        self.pending.clear();
        self.chart.set_entries(Arc::from([]));
        self.report_markets_not_loaded(cx);
        let _ = cx.request_frame();
    }

    /// Reports the notice naming the markets this map reads.
    ///
    /// This is the one place that reference is produced: every path that finds no
    /// market to read reports it. The sentence is the framework's to resolve, in the
    /// language showing on the frame that draws it.
    ///
    /// The pane waits for every venue that answers the route-level bulk ticker read and
    /// lists every one of them: it reads the whole market, so the venues it can read at
    /// all and the venues it waits for are the same answer.
    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let venues = market_venues();
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
        );
    }

    /// Starts reading, or restarts for a map the user is retrying.
    ///
    /// A retry keeps the map the user is still reading — blanking it to reconnect
    /// would flash a screen of data away — while an explicit load starts over,
    /// because the pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.ready = None;
        self.next_refresh_at = None;
        // The command that was in flight belongs to the generation this call is
        // replacing, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        self.pending.clear();
        if !retry {
            self.accepted = None;
            self.chart.set_entries(Arc::from([]));
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(reading) = self.reading.clone() else {
            return;
        };
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(
            cx,
            VolumeHeatMapRequest::Load {
                venue: reading.venue,
                routes: reading.routes,
            },
        );
        self.pending.track(request_id, PendingCommand::Start);
        let _ = cx.request_frame();
    }

    /// Applies what the binding delivered, in arrival order.
    fn apply(&mut self, inbound: Inbound<VolumeHeatMapProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, message, cx),
            Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
        }
    }

    /// Applies one RPC reply, once its request identity says which command it answers.
    ///
    /// Only one call is in flight at a time (the binding replaces and aborts the previous
    /// one), so the table says which command an answer belongs to. A reply whose identity
    /// is not there answers a call the pane has already replaced, and applying it would
    /// report the replaced command's failure over the one that replaced it.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A failure is terminal, and every entry point has to say so: the reply to a
        // command the recovery replaced would otherwise report the same failure again —
        // under another error object, over the banner the user is reading.
        if self.status.is_failed(cx.app()) {
            return;
        }
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_rpc_result(result, cx),
            Some(PendingCommand::Refresh) => self.apply_rpc_result(result, cx),
            None => {
                log::debug!(
                    "the volume heat map pane ignored reply {request_id}: it answers no command in flight"
                );
            }
        }
    }

    /// Reported one way, whichever request it answers: the two differ in what they asked
    /// for, not in what a failure means to the reader.
    ///
    /// An accepted refresh stays pending until the round it started is reported: the
    /// acknowledgement and the round are two different messages, and the gate the service
    /// holds spans both.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(VolumeHeatMapFailure::Rpc(error)), recovery),
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// A status about another venue is not this pane's to apply: the pane shows one
    /// market participant, and the key is what says which one the message is about.
    fn apply_reliable(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_map_on_screen(key, cx.app()) {
            return;
        }
        let VolumeHeatMapStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            VolumeHeatMapStatus::Connecting { .. } => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            VolumeHeatMapStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            VolumeHeatMapStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between
                // them, so this can arrive before the map it describes: claiming
                // readiness now would be claiming a map the pane does not have.
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
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
                    return;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(failure), recovery),
                );
            }
        }
    }

    /// Applies a map the stream published.
    fn apply_latest(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_map_on_screen(key, cx.app()) {
            return;
        }
        let VolumeHeatMapStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return;
        };
        // A map the pane has already overtaken would only make the tiles jump
        // backwards: within one generation the service's revision only moves
        // forwards, so a smaller one can only be a map a status has replaced.
        if self.accepted.is_some_and(|held| snapshot.revision < held) {
            return;
        }
        let changed = self.chart.set_entries(Arc::clone(&snapshot.entries));
        self.accepted = Some(snapshot.revision);
        // Readiness is published at the protocol's own boundary: a complete map is
        // not the same thing as a map a status said was complete, and this is
        // where the two meet.
        if self.ready.is_some_and(|ready| snapshot.revision >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        if changed {
            let _ = cx.request_frame();
        }
    }

    /// Whether a message belongs to the map the pane is showing.
    ///
    /// Three things have to hold, and none of them is implied by the others: the
    /// message has to be about the venue the pane reads, the pane has to be reading
    /// rather than waiting for a market, and the pane must not have failed — a
    /// failure is terminal until the user recovers, and a message that arrives after
    /// it is about a stream that no longer exists.
    fn is_about_the_map_on_screen(&self, key: VolumeHeatMapStreamKey, app: &App) -> bool {
        if self.status.is_failed(app) || self.awaiting_market {
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
    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
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
    /// icon alone and refuses input the rest of the time.
    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    /// Shows another market participant's map.
    ///
    /// The map on screen belongs to the market participant being left, so it is dropped rather
    /// than carried over: the pane cannot vouch for tiles the market it now reads did
    /// not produce. Picking the venue that is already shown is not a change — the
    /// picker refuses it, and this guard says the same thing one step later.
    fn select_venue(&mut self, venue: VenueId, cx: &mut WidgetCx<'_>) {
        if self.config.venue == Some(venue) {
            return;
        }
        self.config.venue = Some(venue);
        self.config_change.publish(cx.app_mut());
        self.reading = None;
        self.accepted = None;
        self.chart.set_entries(Arc::from([]));
        self.load(cx);
    }

    /// Reads the market again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers:
    /// a failure is recovered through the framework's own control, above the body.
    /// The command names the venue on screen, which is what makes it a read of the
    /// map the user is looking at rather than of whichever market participant the loop happens
    /// to be serving.
    fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        let Some(reading) = self.reading.as_ref() else {
            return;
        };
        let venue = reading.venue;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        self.refresh_pending = true;
        let request_id = binding.call(cx, VolumeHeatMapRequest::Refresh { venue });
        self.pending.track(request_id, PendingCommand::Refresh);
        let _ = cx.request_frame();
    }

    /// Applies what a header control handed over.
    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => self.refresh(cx),
            HeaderRequest::Venue(venue) => self.select_venue(venue, cx),
        }
    }

    /// Builds the venue picker into the pane's left header group.
    fn build_venue_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<VenueControl, EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_VENUE")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let select = self.build_venue_select(cx, row)?;
        Ok(VenueControl {
            root: row,
            select,
            language,
            venues: self.venues.clone(),
        })
    }

    /// Builds the picker itself, with the venues the directory offers and the choice the
    /// pane is showing.
    fn build_venue_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let translate = cx.translate().clone();
        let label = t_smol!(translate, "common.market_participants");
        let placeholder = if self.config.venue.is_some() {
            SmolStr::default()
        } else {
            t_smol!(translate, "widget.volume_heatmap.choose_venue")
        };
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(label)
            .placeholder(placeholder);
        let window = cx.window();
        let key = cx.unique_key(VENUE_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let Ok(venue) = VenueId::from_name(id.as_str()) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Venue(venue)));
                let _ = app.request_repaint(window);
            },
        );
        // The items are stated in the language showing now; a language switch re-states
        // them (`sync_header`), which is the kit's static-copy rebuild rule (§15.1).
        let items = self.venue_items(&translate);
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the volume heat map picker could not state its venues: {error}");
            });
        if let Some(venue) = self.config.venue {
            select.set_chosen(cx.app_mut(), [venue.as_str()], false);
        }
        let _ = select.set_disabled(cx.app_mut(), self.venue_picker_is_disabled());
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    /// The picker's items: the loaded venues this map can read, in the order the
    /// interface states them.
    ///
    /// The choice is identified by the venue's canonical identity and *labelled* with its
    /// name in the language showing now, so the same choice survives a language switch
    /// instead of being looked up by a translated string.
    fn venue_items(&self, translate: &Translate) -> Vec<SelectItem> {
        self.venues
            .iter()
            .map(|venue| {
                SelectItem::new(
                    SmolStr::new(venue.as_str()),
                    venue.display_string(translate),
                )
            })
            .collect()
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

    /// Builds the refresh control into the pane's right header group.
    fn build_refresh_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<RefreshControl, EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_REFRESH_ROW")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let slot = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), row)?;
        let button = self.build_refresh_button(cx, slot)?;
        let countdown = self.countdown.label;
        let theme = cx.theme().clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_VOLUME_HEATMAP_COUNTDOWN")
        .bind_text(move |read| {
            let label: Option<SmolStr> = read.read(&countdown).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), slot)?;
        Ok(RefreshControl {
            root: row,
            slot,
            button,
            language,
        })
    }

    /// Builds the refresh button, in the state the pane is in now.
    fn build_refresh_button(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Button, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.volume_heatmap.refresh_now");
        let disabled = self.refresh_is_disabled(cx.app());
        let key = cx.unique_key(REFRESH_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let window = cx.window();
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .disabled(disabled)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                // A button handler owns no widget: it asks the next sync for the refresh.
                ctx.update(&requests, |request| *request = Some(HeaderRequest::Refresh));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    /// Keeps the header controls in step with what they should state.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_venue_control(cx)?;
        self.sync_refresh_control(cx)?;
        Ok(())
    }

    /// Keeps the picker's items, choice and availability current.
    ///
    /// The picker's item labels are resolved text, so a language switch re-states them; a
    /// commit that changed the readable venues does the same. What the picker *states* is
    /// its `chosen` signal, which a venue switch moves without a rebuild.
    fn sync_venue_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, language, venues)) = self
            .venue_control
            .as_ref()
            .map(|control| (control.root, control.language, control.venues.clone()))
        else {
            return Ok(());
        };
        // The pane rebuilds the row it calls `header` for, so the row this widget built
        // into may already be gone — and it took the control with it. Touching a stale
        // node would fail the whole synchronization, so a row that is missing is a
        // release and nothing else.
        if cx.tree().get(root).is_err() {
            self.release_venue_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        let items_moved = language != language_now || venues != self.venues;
        if items_moved {
            let translate = cx.translate().clone();
            let items = self.venue_items(&translate);
            if let Some(control) = self.venue_control.as_mut() {
                control
                    .select
                    .set_items(cx.app_mut(), items)
                    .unwrap_or_else(|error| {
                        log::warn!(
                            "the volume heat map picker could not replace its venues: {error}"
                        );
                    });
                control.language = language_now;
                control.venues = self.venues.clone();
            }
        }
        let Some(control) = self.venue_control.as_ref() else {
            return Ok(());
        };
        if let Some(venue) = self.config.venue {
            let stated = control.select.chosen(cx.app());
            if stated.len() != 1 || stated.first().map(SmolStr::as_str) != Some(venue.as_str()) {
                control
                    .select
                    .set_chosen(cx.app_mut(), [venue.as_str()], false);
            }
        }
        let _ = control
            .select
            .set_disabled(cx.app_mut(), self.venue_picker_is_disabled());
        control.select.sync(cx.app_mut())?;
        Ok(())
    }

    /// Keeps the refresh button's copy and availability current.
    ///
    /// A kit button's label and tooltip are chosen when it is built, so a language switch
    /// rebuilds it — in the slot it already occupies, so nothing else in the row moves.
    fn sync_refresh_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, slot, language)) = self
            .refresh_control
            .as_ref()
            .map(|control| (control.root, control.slot, control.language))
        else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.release_refresh_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        if language != language_now {
            if let Some(control) = self.refresh_control.as_mut() {
                if let Some(node) = control.button.node() {
                    remove_own_node(cx, node);
                }
                let _ = control.button.release(cx.app_mut());
            }
            let button = self.build_refresh_button(cx, slot)?;
            if let Some(control) = self.refresh_control.as_mut() {
                control.button = button;
                control.language = language_now;
            }
        }
        let disabled = self.refresh_is_disabled(cx.app());
        let Some(control) = self.refresh_control.as_ref() else {
            return Ok(());
        };
        control.button.sync(cx.app_mut())?;
        let _ = control.button.set_disabled(cx.app_mut(), disabled);
        Ok(())
    }

    /// Shows `common.no_data` over a map that holds no tile.
    ///
    /// The stand-in is the widget's, not the framework's: a published map that covers no
    /// instrument is *content*, so this is the body saying there is nothing in it rather
    /// than a placeholder saying there is no body. Its copy is static, so it is built for
    /// the language showing now and rebuilt when that moves (the same rule the surface
    /// follows for its notices).
    fn sync_empty_overlay(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self.has_content() && self.chart.is_empty();
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node);
                self.empty_language = None;
            }
            return Ok(());
        }
        let language = *cx.read(&cx.language());
        if self.empty.is_some() && self.empty_language == Some(language) {
            return Ok(());
        }
        if let Some(node) = self.empty.take() {
            remove_own_node(cx, node);
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.body_root = None;
            return Ok(());
        }
        let overlay = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let text = t_smol!(cx.translate(), "common.no_data");
        EmptyState::new(cx.theme().clone(), "volume_heatmap.empty")
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);
        Ok(())
    }

    /// Releases the picker's component, and the widget's own row when asked to.
    fn release_venue_control(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(control) = self.venue_control.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, control.root);
        }
        let _ = control.select.release(cx.app_mut());
    }

    /// Releases the refresh button, and the widget's own row when asked to.
    fn release_refresh_control(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(control) = self.refresh_control.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, control.root);
        }
        let _ = control.button.release(cx.app_mut());
    }
}

/// Removes a node this widget built, if it is still in the tree.
///
/// A node that is already gone is not a failure: the pane rebuilds its rows, and a node
/// this widget built into the old one went with it.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) {
    if cx.tree().get(node).is_err() {
        return;
    }
    let window = cx.window();
    let _ = cx.app_mut().remove_node(window, node);
}

/// The label beside the refresh button for `deadline` at `now`.
///
/// A deadline that has already passed counts as `00:00` rather than as a negative wait,
/// which is the same rule [`format_countdown`] applies: the pane never shows a countdown
/// that has run out as anything but zero.
fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

#[cfg(test)]
mod tests;
