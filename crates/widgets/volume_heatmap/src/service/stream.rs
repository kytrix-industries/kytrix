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

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_TICKERS_PER_ROUTE: usize = 10_000;

const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapRequest {
    Load {
        venue: VenueId,
        routes: Arc<[VenueRoute]>,
    },
    Refresh {
        venue: VenueId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapResponse {
    StreamStarted,
    RefreshQueued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VolumeHeatMapStreamKey {
    venue: VenueId,
}

impl VolumeHeatMapStreamKey {
    pub(crate) const fn new(venue: VenueId) -> Self {
        Self { venue }
    }

    pub(crate) const fn venue(self) -> VenueId {
        self.venue
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeHeatMapStatus {
    Connecting {
        venue: VenueId,
    },
    Refreshing {
        venue: VenueId,
        next_refresh_at: i64,
    },
    Ready {
        venue: VenueId,
        revision: u64,
        next_refresh_at: i64,
    },
    Failed {
        venue: VenueId,
        error: VolumeHeatMapFetchError,
    },
}

impl VolumeHeatMapStatus {
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

#[derive(Clone, Debug)]
pub enum VolumeHeatMapStreamMessage {
    Status(VolumeHeatMapStatus),
    Snapshot(Arc<VolumeHeatMapSnapshot>),
}

#[derive(Clone, Debug)]
pub struct VolumeHeatMapSnapshot {
    pub revision: u64,
    pub entries: Arc<[HeatMapEntry]>,
}

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
        let Some(asset_terms) = registry.terms(&asset) else {
            continue;
        };
        if let Some(code) = display.asset_code(&asset) {
            labels.insert(asset.clone(), SmolStr::new(code.as_str()));
        }
        terms.insert(asset, asset_terms);
    }
    (labels, terms)
}

pub(crate) enum StreamCommand {
    Refresh(RefreshPermit),
}

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
    venue: VenueId,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

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
                        context.cancel_stream(&VolumeHeatMapStreamKey::new(venue));
                        active = None;
                    }
                }
                VolumeHeatMapRequest::Refresh { venue } => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
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
                    let Some(permit) = stream.gate.try_acquire() else {
                        let _ = reply.respond(Err(VolumeHeatMapServiceError::StreamBusy));
                        continue;
                    };
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
        None => false,
    };

    loop {
        let (attempted_at, permit) = if scheduled {
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
                    next_refresh = Instant::now() + refresh_interval;
                    let Some(permit) = gate.try_acquire() else {
                        continue;
                    };

                    (Instant::now(), permit)
                }
            }
        } else {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshRoundOutcome {
    Scheduled,
    Waiting,
    Stop,
}

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
