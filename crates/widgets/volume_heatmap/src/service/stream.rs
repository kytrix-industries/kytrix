//! The background service one heat map talks to.
//!
//! A map is a poll, not a subscription: no venue streams "quote volume of every
//! spot pair" as an event, so the answer is re-fetched on an interval and the pane
//! counts down to the next one. One stream task owns that loop and does three
//! things, in this order, every time it runs:
//!
//! 1. report that it is fetching, and when the next fetch is due, so the pane can
//!    count down to something it can see;
//! 2. fetch every spot scope of **one venue**, in parallel, and fold them into the
//!    tiles the map shows — a map that did not change keeps its allocation, and the
//!    pane above compares by pointer to decide whether there is anything to redraw;
//! 3. report readiness *after* the map it describes, never before, because the two
//!    channels have no order between them and a status that ran ahead of its own
//!    snapshot would put a map on screen that is not there yet.
//!
//! A venue is the scope of a stream, not a parameter of a round. The map shows one
//! market participant at a time, so nothing here ever holds two venues' answers at once and
//! no round can mix them; a pane that switches venue replaces the stream, and the
//! stream it replaced is gone with every message it had not sent yet. A refresh
//! command names the venue it belongs to and is refused by a stream serving
//! another one, so a click that arrives after the user moved on cannot read the
//! market participant the user is no longer looking at.
//!
//! A failed fetch stops the clock without ending the scope: the task stays alive
//! and waits for a manual refresh, so a venue that is briefly unreachable does not
//! turn into a request every ten minutes until someone notices. Only an explicit
//! load or refresh starts the clock again.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ahash::AHashMap;
use futures::future::join_all;
use market_participants_connector::{
    catalog::{generation::CatalogGeneration, registry::asset::AssetTerms},
    market::{error::MarketError, provider::MarketCapability},
};
use market_participants_model::{
    asset::AssetId, identifier::VenueId, instrument::InstrumentTerms, listing::route::VenueRoute,
    market::selection::ListingTicker,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};
use widget::{
    protocol::{StreamGeneration, WidgetProtocol},
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    util::deadline_millis,
};

use crate::model::{
    HeatMapEntry, HeatMapModelError, RouteListing, RouteTickerAnswer, combine_market_tickers,
};

/// How often a map is re-fetched on its own.
///
/// Ten minutes is the old component's interval, and it is a statement about the
/// data rather than about this map: rolling 24-hour volumes move slowly, so a
/// shorter interval would redraw the same picture and spend requests doing it.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long one round of venue fetches may take before it is treated as a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most tickers one route may answer with.
///
/// A venue that answers with more than this is not answering the question that was
/// asked — the spot catalog of the supported venues is a few thousand pairs — and
/// the map is built from the answer as a whole, so folding a truncated one would
/// silently leave out whatever did not fit.
const MAX_TICKERS_PER_ROUTE: usize = 10_000;

/// The refresh command queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is pending is answered as
/// busy rather than queued.
const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapRequest {
    /// Starts the map over, replacing whatever generation was there, over the
    /// venue the user is looking at and the scopes it publishes now.
    ///
    /// One request serves the framework's first load, its recovery and a venue the
    /// user just picked: an explicit load is what starts a generation, and a
    /// generation reads one venue. The scopes travel with the request because the
    /// user may load a market after the pane was opened, and a load of that venue
    /// publishes the scopes this map reads.
    Load {
        venue: VenueId,
        routes: Arc<[VenueRoute]>,
    },
    /// Fetches the current venue's map once, without waiting for the next
    /// interval.
    ///
    /// The venue is part of the command because the answer is: a stream reads one
    /// market participant, and a refresh that named no venue would have to be trusted to
    /// still be about the one the pane has on screen.
    Refresh { venue: VenueId },
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapResponse {
    StreamStarted,
    RefreshQueued,
}

/// The scope of one map: the venue it is drawn from.
///
/// It is a key rather than a bare `VenueId` because the protocol needs one, and
/// because it is what states the rule the pane and the service share: only a
/// message about the venue the pane is showing may be acted on. A pane that
/// switched venue is a different scope, so the map of the one it left is not a map
/// it may draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VolumeHeatMapStreamKey {
    venue: VenueId,
}

impl VolumeHeatMapStreamKey {
    /// The key of one venue's map.
    pub(crate) const fn new(venue: VenueId) -> Self {
        Self { venue }
    }

    /// The venue this map is drawn from.
    pub(crate) const fn venue(self) -> VenueId {
        self.venue
    }
}

/// Where the poll loop is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapStatus {
    /// The stream is starting.
    Connecting { venue: VenueId },
    /// Venue fetches are in flight; `next_refresh_at` is the Unix millisecond the
    /// following automatic fetch is due.
    Refreshing {
        venue: VenueId,
        next_refresh_at: i64,
    },
    /// The map of `revision` has been published and the loop is idle until
    /// `next_refresh_at`.
    Ready {
        venue: VenueId,
        revision: u64,
        next_refresh_at: i64,
    },
    /// The last round produced no map.
    Failed {
        venue: VenueId,
        error: VolumeHeatMapFetchError,
    },
}

impl VolumeHeatMapStatus {
    /// When the next automatic fetch is due, if one is scheduled.
    pub(crate) const fn next_refresh_at(&self) -> Option<i64> {
        match self {
            Self::Connecting { .. } | Self::Failed { .. } => None,
            Self::Refreshing {
                next_refresh_at, ..
            }
            | Self::Ready {
                next_refresh_at, ..
            } => Some(*next_refresh_at),
        }
    }
}

/// Why a round produced no map.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum VolumeHeatMapFetchError {
    #[error("a venue answer for the volume heat map failed: {0}")]
    MarketParticipant(#[source] MarketError),
    #[error("the volume heat map could not combine the venue answers: {0}")]
    Model(#[source] HeatMapModelError),
    #[error("the {route} ticker answer contains {count} entries; the maximum is {limit}")]
    TooManyTickers {
        route: VenueRoute,
        count: usize,
        limit: usize,
    },
    #[error("the volume heat map did not answer within {0:?}")]
    Timeout(Duration),
}

/// Everything the service publishes: state on the reliable channel, maps on the
/// latest-value one.
#[derive(Clone, Debug)]
pub enum VolumeHeatMapStreamMessage {
    Status(VolumeHeatMapStatus),
    Snapshot(Arc<VolumeHeatMapSnapshot>),
}

/// One complete map of one venue, shared rather than copied.
#[derive(Clone, Debug)]
pub struct VolumeHeatMapSnapshot {
    pub revision: u64,
    pub entries: Arc<[HeatMapEntry]>,
}

/// Why a request could not be accepted.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum VolumeHeatMapServiceError {
    #[error("the volume heat map stream is closed")]
    StreamClosed,
    #[error("a volume heat map refresh is already in flight")]
    StreamBusy,
}

pub(crate) struct VolumeHeatMapProtocol;

impl WidgetProtocol for VolumeHeatMapProtocol {
    type Request = VolumeHeatMapRequest;
    type Response = VolumeHeatMapResponse;
    type StreamKey = VolumeHeatMapStreamKey;
    type StreamMessage = VolumeHeatMapStreamMessage;
    type Error = VolumeHeatMapServiceError;
}

/// The seam between this widget and the venues, so a test can answer instead.
///
/// It is `pub` rather than module-private because the pane's own test seam — a
/// constructor that takes a source — is a public item on a public widget, and the
/// trait it names has to be at least as visible as the item that uses it.
///
/// One call is one route: the route's published listings (which is what says
/// whether a ticker belongs to a spot listing of that route) together with the
/// tickers themselves, so a caller never has to resolve a listing twice.
pub trait VolumeHeatMapSource: Clone + Send + Sync + 'static {
    fn fetch_route_tickers(
        &self,
        route: VenueRoute,
    ) -> impl Future<Output = Result<RouteTickerAnswer, MarketError>> + Send;
}

impl VolumeHeatMapSource for MarketCapability {
    fn fetch_route_tickers(
        &self,
        route: VenueRoute,
    ) -> impl Future<Output = Result<RouteTickerAnswer, MarketError>> + Send {
        let capability = self.clone();
        async move {
            // The catalog is what says which listing a ticker belongs to and what
            // its own rules name: a route-level ticker answer carries listing keys,
            // and no product is guessed from a venue.
            let listings = capability.listings(&route)?;
            let tickers = capability
                .market(route.clone())
                .all_tickers()
                .fetch()
                .await?;
            let listings = listings
                .into_iter()
                .map(|listing| RouteListing {
                    listing: listing.listing,
                    instrument: listing.instrument,
                })
                .collect::<Vec<_>>();
            let generation = capability.catalog().generation();
            let (asset_labels, asset_terms) = route_assets(&generation, &listings, &tickers);
            Ok(RouteTickerAnswer {
                listings,
                asset_labels,
                asset_terms,
                tickers,
            })
        }
    }
}

/// The labels and the canonical terms of every asset one route's answer names.
///
/// The display catalog of one generation is the only boundary that turns a
/// canonical [`AssetId`] into a name, and the registry of that same generation
/// states what the asset *is*. Both are read here from one publication, so a label
/// and the terms behind it cannot come from two of them. The labels are what a tile
/// is written with, and nothing here derives a second name for it: the code an asset
/// has is the code the catalog published for it, and a name this module built
/// from the terms instead would be free to disagree with the publication about the
/// very asset. The terms are what decides which role an asset plays, and they never
/// work from a rendered string.
pub(super) fn route_assets(
    generation: &CatalogGeneration,
    listings: &[RouteListing],
    tickers: &[ListingTicker],
) -> (AHashMap<AssetId, SmolStr>, AHashMap<AssetId, AssetTerms>) {
    let registry = generation.assets();
    let display = generation.display_catalog();

    let mut assets = Vec::with_capacity(listings.len() * 2);
    for listing in listings {
        if let InstrumentTerms::Spot(spec) = listing.instrument.terms() {
            assets.push(spec.base().clone());
            assets.push(spec.quote().clone());
        }
    }
    for ticker in tickers {
        if let Some(quantity) = &ticker.traded_notional
            && let Some(asset) = quantity.unit.asset()
        {
            assets.push(asset.clone());
        }
    }
    let mut labels = AHashMap::with_capacity(assets.len());
    let mut terms = AHashMap::with_capacity(assets.len());
    for asset in assets {
        if terms.contains_key(&asset) {
            continue;
        }
        // An asset the registry does not carry is outside the map's scope rather
        // than a name that went missing: an asset's roles are read from canonical
        // terms, so a reading whose money or base has none is not one this map can
        // classify at all.
        let Some(asset_terms) = registry.terms(&asset) else {
            continue;
        };
        // A registered asset without a code is a display invariant breaking: a base a
        // listed market references always has one.
        // It is left unnamed here rather than given a name derived from its terms,
        // and the model refuses the market it cannot name instead of dropping it
        // from the map without a trace.
        if let Some(code) = display.asset_code(&asset) {
            labels.insert(asset.clone(), SmolStr::new(code.as_str()));
        }
        terms.insert(asset, asset_terms);
    }
    (labels, terms)
}

/// The refresh command a manual refresh sends to the poll loop.
///
/// It carries the permit the round it starts will hold, which is what makes the
/// gate span the round rather than the queue: the sender cannot report a refresh
/// as queued and then have the loop run something else instead.
pub(crate) enum StreamCommand {
    Refresh(RefreshPermit),
}

/// One manual refresh, guarded so the command queue cannot fill with duplicates.
#[derive(Clone, Default)]
pub(crate) struct RefreshGate {
    active: Arc<AtomicBool>,
}

impl RefreshGate {
    fn try_acquire(&self) -> Option<RefreshPermit> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RefreshPermit {
                active: self.active.clone(),
            })
    }
}

pub(crate) struct RefreshPermit {
    active: Arc<AtomicBool>,
}

impl Drop for RefreshPermit {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

pub(crate) struct ActiveStream {
    generation: StreamGeneration,
    /// The venue the generation was started for.
    venue: VenueId,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

/// What the poll loop carries between rounds.
#[derive(Default)]
pub(crate) struct StreamState {
    latest: Option<Arc<[HeatMapEntry]>>,
    revision: u64,
}

pub struct VolumeHeatMapService<S = MarketCapability> {
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
}

impl<S> VolumeHeatMapService<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            refresh_interval: REFRESH_INTERVAL,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }

    /// The same service with timings a test can wait out.
    ///
    /// It is crate-visible rather than private to this module because the tests
    /// that use it live in `stream_tests.rs` beside it, which is a module of its
    /// own.
    #[cfg(test)]
    pub(crate) fn with_timing(
        source: S,
        refresh_interval: Duration,
        attempt_timeout: Duration,
    ) -> Self {
        Self {
            source,
            refresh_interval: refresh_interval.max(Duration::from_millis(1)),
            attempt_timeout: attempt_timeout.max(Duration::from_millis(1)),
        }
    }
}

impl<S> WidgetService<VolumeHeatMapProtocol> for VolumeHeatMapService<S>
where
    S: VolumeHeatMapSource,
{
    async fn run(self, mut context: ServiceContext<VolumeHeatMapProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                VolumeHeatMapRequest::Load { venue, routes } => {
                    // The answer is worth less than the request it belongs to: a
                    // caller that has already given up is not waiting for it.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    let gate = RefreshGate::default();
                    let stream_gate = gate.clone();
                    let stream_routes = routes.clone();
                    // Replacing is what makes a recovery start over, and what makes
                    // a venue the user picked a different map: after a failure the
                    // loop is still alive but off the clock, and a stream that
                    // answered `StreamStarted` for a generation nobody replaced
                    // would leave the pane waiting on a schedule that no longer
                    // exists. A load is the only thing that starts a generation, so
                    // it always replaces: a stream that is already reading the same
                    // venue is not left alone, because "already reading" is the one
                    // state this cannot tell from "reported a failure and stopped".
                    let generation = context.replace_stream(
                        VolumeHeatMapStreamKey::new(venue),
                        move |publisher| {
                            run_stream(
                                source,
                                venue,
                                stream_routes,
                                refresh_interval,
                                attempt_timeout,
                                stream_gate,
                                stream_commands,
                                publisher,
                            )
                        },
                    );
                    if reply
                        .respond(Ok(VolumeHeatMapResponse::StreamStarted))
                        .is_ok()
                    {
                        active = Some(ActiveStream {
                            generation,
                            venue,
                            commands,
                            gate,
                        });
                    } else {
                        // Nobody is listening to this stream any more, and the
                        // command channel is gone with the answer.
                        context.cancel_stream(&VolumeHeatMapStreamKey::new(venue));
                        active = None;
                    }
                }
                VolumeHeatMapRequest::Refresh { venue } => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // A refresh is only worth sending to the loop that is serving
                    // the venue it names: the map on screen is the only one there
                    // is, and after a failure the loop is off the clock, so a
                    // command handed to a channel nobody reads would be answered
                    // as queued and never run. The next load starts a generation
                    // again.
                    let running = active.as_ref().filter(|stream| {
                        stream.venue == venue
                            && context
                                .stream_generation(&VolumeHeatMapStreamKey::new(venue))
                                .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(VolumeHeatMapServiceError::StreamClosed));
                        continue;
                    };
                    // The gate spans the whole round trip: a second click while
                    // the first fetch is in flight is refused here rather than
                    // queued behind it.
                    let Some(permit) = stream.gate.try_acquire() else {
                        let _ = reply.respond(Err(VolumeHeatMapServiceError::StreamBusy));
                        continue;
                    };
                    // A command that was refused drops its permit with itself, so
                    // the gate is released whether or not the queue took it.
                    match stream.commands.try_send(StreamCommand::Refresh(permit)) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(VolumeHeatMapResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            let _ = reply.respond(Err(VolumeHeatMapServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            let _ = reply.respond(Err(VolumeHeatMapServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

/// Runs the poll loop of one venue's map until the publisher is cancelled.
///
/// The loop has two states, and a failure is what moves it between them. It is
/// either **scheduled** — sleeping until the next automatic fetch — or **waiting**,
/// which is where a failed round leaves it: the task stays alive so the map does
/// not have to be replaced to be recovered, but nothing is on the clock. Only a
/// manual refresh moves it back to scheduled, and a round that fails while waiting
/// leaves it waiting.
///
/// Every round runs holding the refresh gate, whether it was the schedule or a
/// command that started it. That is what a manual refresh is refused against: from
/// the moment a round starts until it reports, there is no second round to start,
/// and a pane that forwards the click anyway is told so instead of being queued
/// behind it.
async fn run_stream<S>(
    source: S,
    venue: VenueId,
    routes: Arc<[VenueRoute]>,
    refresh_interval: Duration,
    attempt_timeout: Duration,
    gate: RefreshGate,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<VolumeHeatMapProtocol>,
) where
    S: VolumeHeatMapSource,
{
    let mut state = StreamState::default();
    let mut next_refresh = Instant::now() + refresh_interval;
    let _ = publish_status(&publisher, VolumeHeatMapStatus::Connecting { venue }).await;

    // The first round runs as soon as the stream starts rather than after a whole
    // interval: a pane that has just been opened has nothing to show, and a
    // countdown to its first data would be a countdown to nothing.
    let mut scheduled = match gate.try_acquire() {
        Some(permit) => {
            let outcome = run_refresh_round(
                &source,
                venue,
                &routes,
                attempt_timeout,
                next_refresh,
                &mut state,
                &publisher,
            )
            .await;
            drop(permit);
            match outcome {
                RefreshRoundOutcome::Scheduled => true,
                RefreshRoundOutcome::Waiting => false,
                RefreshRoundOutcome::Stop => return,
            }
        }
        // Unreachable: the gate of a stream nobody has asked to refresh yet is
        // free. Waiting is the honest answer if it ever is not, because it is the
        // state that starts no round of its own.
        None => false,
    };

    loop {
        // What wakes the loop, and the permit the round it starts will hold.
        let (attempted_at, permit) = if scheduled {
            // A manual refresh also resets the automatic schedule, so a pane the
            // user just refreshed does not fetch again seconds later.
            let sleep = tokio::time::sleep_until(next_refresh);
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                () = publisher.clone().cancelled_owned() => return,
                command = commands.recv() => {
                    let Some(StreamCommand::Refresh(permit)) = command else {
                        return;
                    };

                    (Instant::now(), permit)
                }
                () = &mut sleep => {
                    // A round the user asked for is in flight. This one is not run
                    // alongside it, and the schedule moves on rather than coming
                    // straight back here: an interval that was skipped is skipped.
                    next_refresh = Instant::now() + refresh_interval;
                    let Some(permit) = gate.try_acquire() else {
                        continue;
                    };

                    (Instant::now(), permit)
                }
            }
        } else {
            // Nothing is on the clock, so the only thing left that can happen is a
            // refresh or the end of the scope.
            tokio::select! {
                biased;
                () = publisher.clone().cancelled_owned() => return,
                command = commands.recv() => {
                    let Some(StreamCommand::Refresh(permit)) = command else {
                        return;
                    };

                    (Instant::now(), permit)
                }
            }
        };
        next_refresh = attempted_at + refresh_interval;
        let outcome = run_refresh_round(
            &source,
            venue,
            &routes,
            attempt_timeout,
            next_refresh,
            &mut state,
            &publisher,
        )
        .await;
        drop(permit);
        scheduled = match outcome {
            RefreshRoundOutcome::Scheduled => true,
            RefreshRoundOutcome::Waiting => false,
            RefreshRoundOutcome::Stop => return,
        };
    }
}

/// What the loop does after one round.
///
/// The three answers are the three things a round can mean, and a failure is the
/// middle one: the loop keeps running so the map does not have to be replaced to
/// be recovered, but nothing is on the clock. Collapsing a failure into "the round
/// reported its result" would put a refused request back on the schedule and turn
/// a venue that is briefly unreachable into a request every ten minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshRoundOutcome {
    /// The round published its result; the next automatic fetch is due.
    Scheduled,
    /// The round reported a failure: nothing is on the clock until a refresh.
    Waiting,
    /// The publisher is gone, so there is nobody left to report to.
    Stop,
}

/// Fetches one venue's map and reports it.
async fn run_refresh_round<S>(
    source: &S,
    venue: VenueId,
    routes: &[VenueRoute],
    attempt_timeout: Duration,
    next_refresh: Instant,
    state: &mut StreamState,
    publisher: &StreamPublisher<VolumeHeatMapProtocol>,
) -> RefreshRoundOutcome
where
    S: VolumeHeatMapSource,
{
    let next_refresh_at = deadline_millis(next_refresh);
    if publish_status(
        publisher,
        VolumeHeatMapStatus::Refreshing {
            venue,
            next_refresh_at,
        },
    )
    .await
    .is_err()
    {
        return RefreshRoundOutcome::Stop;
    }

    let fetch = fetch_snapshot(source, routes, state.latest.as_ref());
    let outcome = tokio::select! {
        biased;
        () = publisher.clone().cancelled_owned() => return RefreshRoundOutcome::Stop,
        result = timeout(attempt_timeout, fetch) => result,
    };
    match outcome {
        Ok(Ok((entries, changed))) => {
            if changed {
                state.revision = state.revision.saturating_add(1);
                if publisher
                    .publish_latest(VolumeHeatMapStreamMessage::Snapshot(Arc::new(
                        VolumeHeatMapSnapshot {
                            revision: state.revision,
                            entries: entries.clone(),
                        },
                    )))
                    .is_err()
                {
                    return RefreshRoundOutcome::Stop;
                }
                state.latest = Some(entries);
            }
            if publish_status(
                publisher,
                VolumeHeatMapStatus::Ready {
                    venue,
                    revision: state.revision,
                    next_refresh_at,
                },
            )
            .await
            .is_err()
            {
                return RefreshRoundOutcome::Stop;
            }
            RefreshRoundOutcome::Scheduled
        }
        Ok(Err(error)) => {
            publish_failure(publisher, venue, error).await;
            RefreshRoundOutcome::Waiting
        }
        Err(_) => {
            publish_failure(
                publisher,
                venue,
                VolumeHeatMapFetchError::Timeout(attempt_timeout),
            )
            .await;
            RefreshRoundOutcome::Waiting
        }
    }
}

/// Publishes a failed round, ignoring the outcome: the caller only needs to know
/// the loop is still alive, and a closed publisher is noticed by the next write.
async fn publish_failure(
    publisher: &StreamPublisher<VolumeHeatMapProtocol>,
    venue: VenueId,
    error: VolumeHeatMapFetchError,
) {
    let _ = publisher
        .publish_reliable(VolumeHeatMapStreamMessage::Status(
            VolumeHeatMapStatus::Failed { venue, error },
        ))
        .await;
}

async fn publish_status(
    publisher: &StreamPublisher<VolumeHeatMapProtocol>,
    status: VolumeHeatMapStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(VolumeHeatMapStreamMessage::Status(status))
        .await
}

/// Fetches every spot scope of one venue in parallel and folds the answers into
/// one map.
pub(crate) async fn fetch_snapshot<S>(
    source: &S,
    routes: &[VenueRoute],
    previous: Option<&Arc<[HeatMapEntry]>>,
) -> Result<(Arc<[HeatMapEntry]>, bool), VolumeHeatMapFetchError>
where
    S: VolumeHeatMapSource,
{
    let answers = join_all(routes.iter().cloned().map(|route| async move {
        let answer = source
            .fetch_route_tickers(route.clone())
            .await
            .map_err(VolumeHeatMapFetchError::MarketParticipant)?;
        bounded_tickers(route, answer)
    }))
    .await
    .into_iter()
    .collect::<Result<Vec<_>, VolumeHeatMapFetchError>>()?;

    combine_market_tickers(answers, previous).map_err(VolumeHeatMapFetchError::Model)
}

/// Refuses a route answer that is larger than this service is willing to fold.
///
/// The bound is checked before anything is combined, so an oversized answer fails
/// the whole round rather than publishing a map built from part of it.
fn bounded_tickers(
    route: VenueRoute,
    answer: RouteTickerAnswer,
) -> Result<RouteTickerAnswer, VolumeHeatMapFetchError> {
    let count = answer.tickers.len();
    if count > MAX_TICKERS_PER_ROUTE {
        return Err(VolumeHeatMapFetchError::TooManyTickers {
            route,
            count,
            limit: MAX_TICKERS_PER_ROUTE,
        });
    }
    Ok(answer)
}
