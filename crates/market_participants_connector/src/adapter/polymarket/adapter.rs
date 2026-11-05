//! The Polymarket adapter: Gamma event directory and CLOB probability history.
//!
//! Polymarket is one venue with two HTTP hosts. Gamma publishes the event
//! directory and CLOB publishes prices and history; the execution context carries
//! both, and this adapter chooses the host by the request it is answering rather
//! than by a second venue identity.
//!
//! Gamma can localize event content with a `locale` query parameter. The adapter
//! always keeps the canonical English answer as the semantic source and merges the
//! localized answer into display fields only. Public search does not localize, so
//! its results are re-read by event id when the caller supplies a locale.

use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::{
    Stream, StreamExt,
    future::{BoxFuture, join_all},
    stream,
};
use market_participants_model::{
    asset::{ContractAddress, CryptoTokenOrigin, CryptoTokenTerms, TokenReference},
    decimal::{Fraction, Quantity, Ratio, Tick, UnixMillis},
    identifier::{ChainId, IdentifierValue, VenueId, VenueScopeId},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        route::VenueRoute,
        venue::{PolymarketAssetId, PolymarketProtocolVersion},
    },
    outcome::OutcomeLegRef,
    prediction::{
        PredictionBookLevel, PredictionBookLevelChange, PredictionCategory, PredictionEventCard,
        PredictionEventRef, PredictionEventView, PredictionEventsPage, PredictionLeg,
        PredictionMarket, PredictionMarketGroup, PredictionMarketRef, PredictionNativeLeg,
        PredictionOptionCard, PredictionOrderBook, PredictionOrderBookUpdate, PredictionPricePoint,
        PredictionSeries,
    },
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};
use tokio_util::sync::CancellationToken;

use super::{
    heartbeat,
    types::{
        ClobBatchHistory, ClobBatchHistoryRequest, ClobBookSnapshot, ClobHistoryPoint, GammaEvent,
        GammaEventsKeyset, GammaIdList, GammaMarket, GammaSearchResponse, PolymarketBookLevel,
        PolymarketMarketEvent, PolymarketMarketFrame, PolymarketMarketSubscribe,
        PolymarketMarketUnsubscribe,
    },
};
use crate::{
    adapter::{
        AdapterLimits, ExecutionContext, MarketParticipantAdapter, PreparedRequest, ProtocolEvent,
        ProtocolRequest, ProtocolResponse, SubscriptionHandle, bind_execution_identity,
        prediction::{
            binary, build_group, exactly_one, group_legs, model_error, parse_iso8601_millis,
            probability,
        },
    },
    catalog::registry::asset::AssetTerms,
    connection::{
        Connection, ConnectionKey, ConnectionState, ConnectionSubscription, ReconnectPolicy,
        SubscriptionTopic, physical::ConnectionProtocol,
    },
    error::MarketParticipantError,
    market_participants::model::{
        PredictionEventQuery, PredictionEventsQuery, PredictionOrderBookQuery,
        PredictionSeriesQuery, PredictionSeriesSubscriptionQuery,
    },
};

/// The Polygon chain id every Polymarket settlement asset lives on.
const POLYGON_CHAIN: &str = "137";

/// The pUSD settlement asset of both CTF and Protocol V2.
const PUSD_CONTRACT: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";

/// The eight fixed categories the selector offers.
///
/// The first matching tag wins, in this order. The key is what a filter stores;
/// the label is what the venue publishes; the id and slug are Gamma's own.
const CATEGORIES: [(&str, &str, &str, &str); 8] = [
    ("politics", "Politics", "2", "politics"),
    ("sports", "Sports", "1", "sports"),
    ("crypto", "Crypto", "21", "crypto"),
    ("finance", "Finance", "120", "finance"),
    ("economy", "Economy", "100328", "economy"),
    ("tech", "Tech", "1401", "tech"),
    ("culture", "Culture", "596", "culture"),
    ("world", "World", "101970", "world"),
];

/// The most options one card draws.
const CARD_OPTIONS: usize = 6;

/// The most markets one CLOB batch history request accepts.
const CLOB_BATCH_LIMIT: usize = 20;

/// The largest page Gamma's event directories accept.
const GAMMA_PAGE_LIMIT: u16 = 100;

/// The path the CLOB market channel answers on.
const MARKET_CHANNEL_PATH: &str = "/ws/market";

/// The Polymarket adapter.
#[derive(Clone)]
pub struct PolymarketAdapter {
    venue: VenueId,
}

impl PolymarketAdapter {
    /// Builds the adapter.
    pub fn new() -> Self {
        Self {
            venue: VenueId::Polymarket,
        }
    }

    /// Reads one page of events from Gamma.
    async fn events(
        &self,
        context: &ExecutionContext,
        query: &PredictionEventsQuery,
    ) -> Result<PredictionEventsPage, MarketParticipantError> {
        let (events, next_cursor, total) = if let Some(search) = query.search.as_deref() {
            self.search(context, query, search).await?
        } else {
            self.keyset(context, query).await?
        };
        let events = events
            .into_iter()
            .filter(|event| {
                if let Some(parent) = &event.parent_event_id {
                    log::debug!("skipped Polymarket child event {} of {parent}", event.id);
                    return false;
                }
                if event.title.trim().is_empty() {
                    log::debug!("skipped Polymarket event {} without a title", event.id);
                    return false;
                }
                true
            })
            .filter_map(|event| match decode_card(&event) {
                Ok(card) => Some(card),
                Err(error) => {
                    log::debug!("skipped a Polymarket event: {error}");
                    None
                }
            })
            .collect();
        Ok(PredictionEventsPage {
            events,
            categories: categories(),
            next_cursor,
            total,
        })
    }

    async fn keyset(
        &self,
        context: &ExecutionContext,
        query: &PredictionEventsQuery,
    ) -> Result<(Vec<GammaEvent>, Option<SmolStr>, Option<u32>), MarketParticipantError> {
        let answer = self.keyset_page(context, query, None).await?;
        let Some(locale) = query.locale.as_deref() else {
            return Ok((answer.events, answer.next_cursor, None));
        };
        // Gamma localizes list content per request. Keep the canonical answer as the
        // semantic source and use the localized page only for display fields.
        let localized = self.keyset_page(context, query, Some(locale)).await?;
        Ok((
            merge_localized_events(answer.events, localized.events),
            answer.next_cursor,
            None,
        ))
    }

    async fn keyset_page(
        &self,
        context: &ExecutionContext,
        query: &PredictionEventsQuery,
        locale: Option<&str>,
    ) -> Result<GammaEventsKeyset, MarketParticipantError> {
        let mut url = endpoint(context.http_metadata(), "events/keyset")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("closed", "false");
            pairs.append_pair("order", "volume24hr");
            pairs.append_pair("ascending", "false");
            let limit = query.page_size.min(GAMMA_PAGE_LIMIT);
            pairs.append_pair("limit", &limit.to_string());
            if let Some(category) = query.category.as_deref()
                && let Some((_, _, id, _)) = CATEGORIES.iter().find(|(key, ..)| *key == category)
            {
                pairs.append_pair("tag_id", id);
            }
            if let Some(cursor) = query.cursor.as_deref() {
                pairs.append_pair("after_cursor", cursor);
            }
            if let Some(locale) = locale {
                pairs.append_pair("locale", locale);
            }
        }
        get_json(context, &url).await
    }

    async fn search(
        &self,
        context: &ExecutionContext,
        query: &PredictionEventsQuery,
        search: &str,
    ) -> Result<(Vec<GammaEvent>, Option<SmolStr>, Option<u32>), MarketParticipantError> {
        let page = query
            .cursor
            .as_deref()
            .and_then(|cursor| cursor.parse::<u32>().ok())
            .unwrap_or(1);
        let mut url = endpoint(context.http_metadata(), "public-search")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", search);
            pairs.append_pair("events_status", "active");
            let limit = query.page_size.min(GAMMA_PAGE_LIMIT);
            pairs.append_pair("limit_per_type", &limit.to_string());
            pairs.append_pair("page", &page.to_string());
            if let Some(category) = query.category.as_deref()
                && let Some((_, _, _, slug)) = CATEGORIES.iter().find(|(key, ..)| *key == category)
            {
                pairs.append_pair("events_tag", slug);
            }
        }
        let answer: GammaSearchResponse = get_json(context, &url).await?;
        let has_more = answer.pagination.as_ref().is_some_and(|page| page.has_more);
        let total = answer.pagination.and_then(|page| page.total_results);
        let next = has_more.then(|| SmolStr::new(format_smolstr!("{}", page.saturating_add(1))));
        let events = self
            .localized_events(
                context,
                answer.events.unwrap_or_default(),
                query.locale.as_deref(),
            )
            .await?;
        Ok((events, next, total))
    }

    /// Reads the localized copy of every event a search returned.
    ///
    /// Gamma's public search does not localize its event titles, so each result is
    /// re-read by event id when the caller asked for a locale. The canonical search
    /// result remains the source of market structure; the localized copy only
    /// supplies display text.
    async fn localized_events(
        &self,
        context: &ExecutionContext,
        events: Vec<GammaEvent>,
        locale: Option<&str>,
    ) -> Result<Vec<GammaEvent>, MarketParticipantError> {
        let Some(locale) = locale else {
            return Ok(events);
        };
        let localized = join_all(
            events
                .iter()
                .map(|event| self.event_by_id(context, event.id.as_str(), Some(locale))),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
        Ok(merge_localized_events(events, localized))
    }

    /// Reads one event from Gamma, including the child events that carry its
    /// additional markets.
    async fn event(
        &self,
        context: &ExecutionContext,
        query: &PredictionEventQuery,
    ) -> Result<PredictionEventView, MarketParticipantError> {
        let id = query.event.native_event.as_str();
        let mut event = self.event_with_children(context, id, None).await?;
        if let Some(locale) = query.locale.as_deref() {
            let localized = self.event_with_children(context, id, Some(locale)).await?;
            merge_localized_event(&mut event, localized);
        }
        decode_view(&event).map(|(view, _)| view)
    }

    /// Reads one event and appends the markets of every child event.
    async fn event_with_children(
        &self,
        context: &ExecutionContext,
        id: &str,
        locale: Option<&str>,
    ) -> Result<GammaEvent, MarketParticipantError> {
        let mut event = self.event_by_id(context, id, locale).await?;
        let children = self.event_children(context, id, locale).await?;
        merge_child_events(&mut event, children);
        Ok(event)
    }

    /// Reads the child events Gamma publishes under one parent event.
    async fn event_children(
        &self,
        context: &ExecutionContext,
        id: &str,
        locale: Option<&str>,
    ) -> Result<Vec<GammaEvent>, MarketParticipantError> {
        // Only the keyset directory accepts `parent_event_id`; the plain event
        // directory would ignore it and return unrelated events.
        let mut children = Vec::new();
        let mut cursor: Option<SmolStr> = None;
        loop {
            let mut url = endpoint(context.http_metadata(), "events/keyset")?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("parent_event_id", id);
                pairs.append_pair("include_children", "true");
                pairs.append_pair("limit", "100");
                if let Some(cursor) = cursor.as_deref() {
                    pairs.append_pair("after_cursor", cursor);
                }
                if let Some(locale) = locale {
                    pairs.append_pair("locale", locale);
                }
            }
            let page: GammaEventsKeyset = get_json(context, &url).await?;
            children.extend(page.events);
            let Some(next) = page.next_cursor else {
                break;
            };
            if cursor.as_ref() == Some(&next) {
                break;
            }
            cursor = Some(next);
        }
        Ok(children)
    }

    async fn event_by_id(
        &self,
        context: &ExecutionContext,
        id: &str,
        locale: Option<&str>,
    ) -> Result<GammaEvent, MarketParticipantError> {
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(MarketParticipantError::ProtocolError {
                operation: "fetch a Polymarket event".into(),
                reason: SmolStr::new_static("the event id is not a Gamma path segment"),
                raw_data: None,
            });
        }
        let mut url = endpoint(context.http_metadata(), &format_smolstr!("events/{id}"))?;
        if let Some(locale) = locale {
            url.query_pairs_mut().append_pair("locale", locale);
        }
        get_json(context, &url).await
    }

    /// Reads probability series from CLOB.
    async fn series(
        &self,
        context: &ExecutionContext,
        query: &PredictionSeriesQuery,
    ) -> Result<Vec<PredictionSeries>, MarketParticipantError> {
        let range = query.range;
        self.history(
            context,
            &query.legs,
            range.span_millis(),
            range.fidelity_minutes(),
        )
        .await
    }

    async fn history(
        &self,
        context: &ExecutionContext,
        legs: &[PredictionLeg],
        span: Option<i64>,
        fidelity_minutes: u16,
    ) -> Result<Vec<PredictionSeries>, MarketParticipantError> {
        let mut series: Vec<PredictionSeries> = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.leg.clone(),
                points: Vec::new(),
            })
            .collect();
        let ids: Vec<SmolStr> = legs
            .iter()
            .filter_map(|leg| native_asset_id(leg).map(SmolStr::new))
            .collect();
        let url = endpoint(context.http_origin(), "batch-prices-history")?;
        let window = history_window(span, fidelity_minutes)?;
        for chunk in ids.chunks(CLOB_BATCH_LIMIT) {
            let body = ClobBatchHistoryRequest {
                markets: chunk,
                interval: window.interval,
                fidelity: window.fidelity,
                start_ts: window.start_ts,
                end_ts: window.end_ts,
            };
            let answer: ClobBatchHistory = post_json(context, &url, &body).await?;
            for (index, leg) in legs.iter().enumerate() {
                let Some(id) = native_asset_id(leg) else {
                    continue;
                };
                let Some(points) = answer.history.get(id) else {
                    continue;
                };
                let converted = series[index].points.clone();
                series[index].points = points
                    .iter()
                    .filter_map(history_point)
                    // A preset window can reach further back than the caller
                    // asked for, so the answer is trimmed to the request.
                    .filter(|point| window.contains(point.timestamp))
                    .fold(converted, |mut all, point| {
                        if all
                            .last()
                            .is_none_or(|last| last.timestamp != point.timestamp)
                        {
                            all.push(point);
                        }
                        all
                    });
            }
        }
        Ok(series)
    }

    /// Opens or reuses the market-channel connection of this route.
    ///
    /// The configured address is the stream host: the market channel lives under
    /// `/ws/market`, so a host configured without a path is given that one. A path
    /// an operator did configure is kept as it stands, because it is then the
    /// operator's own route to the channel.
    async fn connection(
        &self,
        context: &ExecutionContext,
    ) -> Result<Arc<Connection>, MarketParticipantError> {
        let mut endpoint = context.websocket_origin().clone();
        if endpoint.path() == "/" {
            endpoint.set_path(MARKET_CHANNEL_PATH);
        }
        endpoint.set_query(None);
        let protocol = connection_protocol();
        let key = ConnectionKey::for_route(
            endpoint,
            context.proxy.clone(),
            None,
            context.route(),
            context.network_route_generation(),
            &protocol,
        );
        let key = bind_execution_identity(key, context)?;
        let pool = Arc::clone(&context.connection_pool);
        pool.connect_or_create(
            key,
            ConnectionState::stateless(),
            protocol,
            pool.cancellation(),
            ReconnectPolicy::default(),
        )
        .await
    }

    /// Opens or joins the market-channel topic of one prediction event's legs.
    ///
    /// The topic is the subscription payload itself, so every subscription that
    /// states the same assets acquires the same lease: a probability series and an
    /// order book of one market therefore read one topic on one connection.
    async fn market_channel(
        &self,
        context: &ExecutionContext,
        legs: &[PredictionLeg],
    ) -> Result<MarketChannelSubscription, MarketParticipantError> {
        let (subscribe_payload, unsubscribe_payload) = market_channel_payloads(legs)?;
        let connection = self.connection(context).await?;
        // Subscribe to the connection's frames before the subscription command is
        // queued: the market channel answers a subscription with a book snapshot,
        // and a receiver created afterwards could miss it.
        let events = Box::pin(connection.event_stream());
        let topic = SubscriptionTopic::from(subscribe_payload.clone());
        let lease = connection.subscribe(topic, subscribe_payload, unsubscribe_payload)?;
        Ok(MarketChannelSubscription {
            events,
            cancellation: CancellationToken::new(),
            lease,
        })
    }

    /// Subscribes to the live midpoints of one prediction event's legs.
    async fn subscribe_prediction_series(
        &self,
        context: &ExecutionContext,
        query: &PredictionSeriesSubscriptionQuery,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let legs: Vec<(SmolStr, OutcomeLegRef)> = query
            .legs
            .iter()
            .filter_map(|leg| match &leg.native {
                PredictionNativeLeg::Polymarket { asset_id, .. } => {
                    Some((SmolStr::new(asset_id.as_str()), leg.leg.clone()))
                }
                PredictionNativeLeg::HyperliquidOutcome { .. } => None,
            })
            .collect();
        if legs.is_empty() {
            return Err(MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: CapabilityId::PredictionSeries,
            });
        }
        let MarketChannelSubscription {
            events,
            cancellation,
            lease,
        } = self.market_channel(context, &query.legs).await?;
        let state = (events, cancellation.clone(), legs);
        let stream = stream::unfold(state, |(mut events, cancellation, legs)| async move {
            loop {
                let next = tokio::select! {
                    () = cancellation.cancelled() => return None,
                    next = events.next() => next,
                };
                match next {
                    Some(Ok(payload)) => match decode_market_frame(&payload, &legs) {
                        Ok(Some(series)) => {
                            return Some((
                                Ok(ProtocolEvent::PredictionSeries(series)),
                                (events, cancellation, legs),
                            ));
                        }
                        Ok(None) => continue,
                        Err(error) => return Some((Err(error), (events, cancellation, legs))),
                    },
                    Some(Err(error)) => return Some((Err(error), (events, cancellation, legs))),
                    None => return None,
                }
            }
        });
        Ok(SubscriptionHandle::with_connection_subscription(
            Box::pin(stream),
            cancellation,
            lease,
        ))
    }

    /// Reads one prediction leg's CLOB book snapshot.
    async fn prediction_order_book(
        &self,
        context: &ExecutionContext,
        query: &PredictionOrderBookQuery,
    ) -> Result<PredictionOrderBook, MarketParticipantError> {
        let asset_id = prediction_order_book_asset(query)?;
        let mut url = endpoint(context.http_origin(), "book")?;
        url.query_pairs_mut().append_pair("token_id", &asset_id);
        let snapshot: ClobBookSnapshot = get_json(context, &url).await?;
        decode_book_snapshot(snapshot, &query.leg)
    }

    /// Subscribes to the live book of one prediction leg.
    ///
    /// The subscription joins the whole market's topic, exactly like the
    /// probability series of the same legs, and its frames are filtered to the
    /// requested asset before they are decoded.
    async fn subscribe_prediction_order_book(
        &self,
        context: &ExecutionContext,
        query: &PredictionOrderBookQuery,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let asset_id = prediction_order_book_asset(query)?;
        let MarketChannelSubscription {
            events,
            cancellation,
            lease,
        } = self.market_channel(context, &query.legs).await?;
        let state = (events, cancellation.clone(), asset_id);
        let stream = stream::unfold(state, |(mut events, cancellation, asset_id)| async move {
            loop {
                let next = tokio::select! {
                    () = cancellation.cancelled() => return None,
                    next = events.next() => next,
                };
                match next {
                    Some(Ok(payload)) => match decode_prediction_book_frame(&payload, &asset_id) {
                        Ok(Some(updates)) => {
                            return Some((
                                Ok(ProtocolEvent::PredictionOrderBookUpdate(updates)),
                                (events, cancellation, asset_id),
                            ));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((Err(error), (events, cancellation, asset_id)));
                        }
                    },
                    Some(Err(error)) => {
                        return Some((Err(error), (events, cancellation, asset_id)));
                    }
                    None => return None,
                }
            }
        });
        Ok(SubscriptionHandle::with_connection_subscription(
            Box::pin(stream),
            cancellation,
            lease,
        ))
    }
}

/// One lease on the market-channel topic of one prediction event's legs.
struct MarketChannelSubscription {
    events: ConnectionFrames,
    cancellation: CancellationToken,
    lease: ConnectionSubscription,
}

/// The market-channel frames one connection publishes.
type ConnectionFrames =
    Pin<Box<dyn Stream<Item = Result<Bytes, MarketParticipantError>> + Send + 'static>>;

/// Builds the market-channel payloads of one prediction subscription.
///
/// The assets are the Polymarket legs in the order the query states them. A
/// probability series and an order book of the same market therefore encode the
/// same bytes, which is what makes their subscriptions share one topic lease on
/// the connection.
fn market_channel_payloads(
    legs: &[PredictionLeg],
) -> Result<(Bytes, Bytes), MarketParticipantError> {
    let asset_ids: Vec<SmolStr> = legs
        .iter()
        .filter_map(|leg| match &leg.native {
            PredictionNativeLeg::Polymarket { asset_id, .. } => {
                Some(SmolStr::new(asset_id.as_str()))
            }
            PredictionNativeLeg::HyperliquidOutcome { .. } => None,
        })
        .collect();
    let subscribe = PolymarketMarketSubscribe {
        assets_ids: &asset_ids,
        kind: "market",
        custom_feature_enabled: true,
    };
    let unsubscribe = PolymarketMarketUnsubscribe {
        assets_ids: &asset_ids,
        operation: "unsubscribe",
    };
    let subscribe_payload = serde_json::to_vec(&subscribe)
        .map(Bytes::from)
        .map_err(|error| {
            MarketParticipantError::Configuration(format_smolstr!(
                "encode a Polymarket market subscription: {error}"
            ))
        })?;
    let unsubscribe_payload =
        serde_json::to_vec(&unsubscribe)
            .map(Bytes::from)
            .map_err(|error| {
                MarketParticipantError::Configuration(format_smolstr!(
                    "encode a Polymarket market unsubscription: {error}"
                ))
            })?;
    Ok((subscribe_payload, unsubscribe_payload))
}

impl Default for PolymarketAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// The protocol every market-channel connection dials with.
fn connection_protocol() -> ConnectionProtocol {
    ConnectionProtocol::new(polymarket_response_id)
        .with_keepalive(heartbeat::KEEPALIVE)
        .with_liveness_timeout(heartbeat::LIVENESS_TIMEOUT)
}

/// The market channel carries no request identifiers.
fn polymarket_response_id(_: &[u8]) -> Option<u64> {
    None
}

/// Reads the probability-moving events of one market-channel frame.
fn decode_market_frame(
    payload: &[u8],
    legs: &[(SmolStr, OutcomeLegRef)],
) -> Result<Option<Vec<PredictionSeries>>, MarketParticipantError> {
    let frame: PolymarketMarketFrame = match serde_json::from_slice(payload) {
        Ok(frame) => frame,
        // The channel also carries frames this build does not read; a frame that
        // is not one of its typed events is not a failure of the subscription.
        Err(_) => return Ok(None),
    };
    let events = match frame {
        PolymarketMarketFrame::One(event) => vec![event],
        PolymarketMarketFrame::Many(events) => events,
    };
    let mut points: BTreeMap<OutcomeLegRef, PredictionPricePoint> = BTreeMap::new();
    for event in events {
        match event {
            PolymarketMarketEvent::Book {
                asset_id,
                bids,
                asks,
                timestamp,
                ..
            } => {
                let Some(probability) = midpoint_from_levels(&bids, &asks) else {
                    continue;
                };
                insert_point(
                    &mut points,
                    legs,
                    &asset_id,
                    probability,
                    frame_timestamp(timestamp)?,
                );
            }
            PolymarketMarketEvent::PriceChange {
                price_changes,
                timestamp,
            } => {
                let timestamp = frame_timestamp(timestamp)?;
                for change in price_changes {
                    let Some(probability) = midpoint_from_strings(
                        change.best_bid.as_deref(),
                        change.best_ask.as_deref(),
                    ) else {
                        continue;
                    };
                    insert_point(&mut points, legs, &change.asset_id, probability, timestamp);
                }
            }
            PolymarketMarketEvent::BestBidAsk {
                asset_id,
                best_bid,
                best_ask,
                timestamp,
            } => {
                let Some(probability) =
                    midpoint_from_strings(best_bid.as_deref(), best_ask.as_deref())
                else {
                    continue;
                };
                insert_point(
                    &mut points,
                    legs,
                    &asset_id,
                    probability,
                    frame_timestamp(timestamp)?,
                );
            }
            // A tick-size or last-trade event moves no midpoint, so the series
            // reads neither; the order book decodes both.
            PolymarketMarketEvent::TickSizeChange { .. }
            | PolymarketMarketEvent::LastTradePrice { .. }
            | PolymarketMarketEvent::Unknown => {}
        }
    }
    if points.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        points
            .into_iter()
            .map(|(leg, point)| PredictionSeries {
                leg,
                points: vec![point],
            })
            .collect(),
    ))
}

fn insert_point(
    points: &mut BTreeMap<OutcomeLegRef, PredictionPricePoint>,
    legs: &[(SmolStr, OutcomeLegRef)],
    asset_id: &str,
    probability: Fraction,
    timestamp: UnixMillis,
) {
    let Some((_, leg)) = legs.iter().find(|(id, _)| id.as_str() == asset_id) else {
        return;
    };
    points.insert(
        leg.clone(),
        PredictionPricePoint {
            timestamp,
            probability,
        },
    );
}

fn midpoint_from_levels(
    bids: &[PolymarketBookLevel],
    asks: &[PolymarketBookLevel],
) -> Option<Fraction> {
    let bid = bids
        .iter()
        .filter(|level| positive_size(&level.size))
        .filter_map(|level| level.price.parse::<Fraction>().ok())
        .max()?;
    let ask = asks
        .iter()
        .filter(|level| positive_size(&level.size))
        .filter_map(|level| level.price.parse::<Fraction>().ok())
        .min()?;
    midpoint(bid, ask)
}

fn positive_size(size: &str) -> bool {
    size.parse::<Quantity>()
        .is_ok_and(|quantity| !quantity.is_zero())
}

fn midpoint_from_strings(bid: Option<&str>, ask: Option<&str>) -> Option<Fraction> {
    let bid = bid?.parse::<Fraction>().ok()?;
    let ask = ask?.parse::<Fraction>().ok()?;
    midpoint(bid, ask)
}

fn midpoint(bid: Fraction, ask: Fraction) -> Option<Fraction> {
    let sum = bid.ratio().inner().checked_add(ask.ratio().inner())?;
    Fraction::new(Ratio::from_inner(sum / 2)).ok()
}

/// Reads the instant a market-channel frame states.
///
/// The channel publishes milliseconds. A frame without one is stamped with the
/// arrival time. A stated instant is raised to the arrival time when it is older:
/// the chart appends a live point only when it is not older than the history it
/// was folded into, so a venue clock running behind the CLOB history endpoint
/// would otherwise silently freeze every line.
fn frame_timestamp(value: Option<UnixMillis>) -> Result<UnixMillis, MarketParticipantError> {
    let now = now_millis()?;
    Ok(value.map_or(now, |timestamp| timestamp.max(now)))
}

/// Reads the instant a market-channel book frame states.
///
/// A book update keeps the venue's instant exactly as stated. The reader folds
/// live updates into a REST snapshot by comparing instants, so raising a stated
/// one to the arrival time — which the series decoder does on purpose, to keep a
/// live point from freezing behind history — would make every frame buffered while
/// the snapshot was in flight look newer than the snapshot, including the full book
/// dump that opened the subscription. A frame without an instant is stamped with
/// its arrival time.
fn book_frame_timestamp(value: Option<UnixMillis>) -> Result<UnixMillis, MarketParticipantError> {
    match value {
        Some(timestamp) => Ok(timestamp),
        None => now_millis(),
    }
}

/// Returns the CLOB asset id of the leg one prediction order-book query is for.
///
/// The leg has to be one of the market's legs, and it has to be a Polymarket leg:
/// this adapter publishes the CLOB book, so another native outcome leg is a
/// capability this route does not serve.
fn prediction_order_book_asset(
    query: &PredictionOrderBookQuery,
) -> Result<SmolStr, MarketParticipantError> {
    query.validate().map_err(model_error)?;
    let native = query
        .legs
        .iter()
        .find(|leg| leg.leg == query.leg)
        .map(|leg| &leg.native);
    match native {
        Some(PredictionNativeLeg::Polymarket { asset_id, .. }) => {
            Ok(SmolStr::new(asset_id.as_str()))
        }
        Some(PredictionNativeLeg::HyperliquidOutcome { .. }) | None => {
            Err(MarketParticipantError::UnsupportedCapability {
                route: query.event.route.clone(),
                capability: CapabilityId::PredictionOrderBook,
            })
        }
    }
}

/// Decodes one CLOB book snapshot into a complete prediction order book.
fn decode_book_snapshot(
    snapshot: ClobBookSnapshot,
    leg: &OutcomeLegRef,
) -> Result<PredictionOrderBook, MarketParticipantError> {
    let (bids, asks) = best_first_levels(&snapshot.bids, &snapshot.asks)?;
    Ok(PredictionOrderBook {
        leg: leg.clone(),
        tick_size: snapshot.tick_size.parse::<Tick>().map_err(model_error)?,
        bids,
        asks,
        last_trade_price: optional_fraction(snapshot.last_trade_price.as_deref())?,
        timestamp: snapshot.timestamp.parse::<UnixMillis>().map_err(|_| {
            protocol_error(
                "decode a Polymarket book timestamp",
                format_smolstr!(
                    "`{}` is not a millisecond instant",
                    snapshot.timestamp.as_str()
                ),
            )
        })?,
    })
}

/// Normalizes both sides of a venue book to best-first order.
///
/// The channel and the REST snapshot both publish bids ascending and asks
/// descending; a level of size zero is not part of the book.
fn best_first_levels(
    bids: &[PolymarketBookLevel],
    asks: &[PolymarketBookLevel],
) -> Result<(Vec<PredictionBookLevel>, Vec<PredictionBookLevel>), MarketParticipantError> {
    let mut bids = book_levels(bids)?;
    let mut asks = book_levels(asks)?;
    bids.sort_by_key(|level| std::cmp::Reverse(level.price));
    asks.sort_by_key(|level| level.price);
    Ok((bids, asks))
}

fn book_levels(
    levels: &[PolymarketBookLevel],
) -> Result<Vec<PredictionBookLevel>, MarketParticipantError> {
    let mut decoded = Vec::with_capacity(levels.len());
    for level in levels {
        let price = level.price.parse::<Fraction>().map_err(model_error)?;
        let size = level.size.parse::<Quantity>().map_err(model_error)?;
        if !size.is_zero() {
            decoded.push(PredictionBookLevel { price, size });
        }
    }
    Ok(decoded)
}

/// Parses a decimal the venue may omit or publish as an empty string.
fn optional_fraction(value: Option<&str>) -> Result<Option<Fraction>, MarketParticipantError> {
    match value {
        Some(value) if !value.is_empty() => {
            value.parse::<Fraction>().map(Some).map_err(model_error)
        }
        _ => Ok(None),
    }
}

/// Parses the tick size the venue may omit or publish as an empty string.
fn optional_tick(value: Option<&str>) -> Result<Option<Tick>, MarketParticipantError> {
    match value {
        Some(value) if !value.is_empty() => value.parse::<Tick>().map(Some).map_err(model_error),
        _ => Ok(None),
    }
}

/// Reads the book updates of one market-channel frame for one asset.
///
/// Frames are single objects or batches, and only the updates of the requested
/// asset are published; a batch keeps its frame order. The frame this build does
/// not read — a global lifecycle event, another venue event — is not an error.
fn decode_prediction_book_frame(
    payload: &[u8],
    asset_id: &str,
) -> Result<Option<Vec<PredictionOrderBookUpdate>>, MarketParticipantError> {
    let frame: PolymarketMarketFrame = match serde_json::from_slice(payload) {
        Ok(frame) => frame,
        Err(_) => return Ok(None),
    };
    let events = match frame {
        PolymarketMarketFrame::One(event) => vec![event],
        PolymarketMarketFrame::Many(events) => events,
    };
    let mut updates = Vec::new();
    for event in events {
        match event {
            PolymarketMarketEvent::Book {
                asset_id: event_asset,
                bids,
                asks,
                tick_size,
                last_trade_price,
                timestamp,
            } => {
                if event_asset.as_str() != asset_id {
                    continue;
                }
                let timestamp = book_frame_timestamp(timestamp)?;
                let (bids, asks) = best_first_levels(&bids, &asks)?;
                updates.push(PredictionOrderBookUpdate::Replace {
                    bids,
                    asks,
                    timestamp,
                });
                // A live book states its own precision and last trade, so a
                // subscription that joined a leased topic can seed both before the
                // REST snapshot is read.
                if let Some(tick_size) = optional_tick(tick_size.as_deref())? {
                    updates.push(PredictionOrderBookUpdate::TickSize {
                        tick_size,
                        timestamp,
                    });
                }
                if let Some(price) = optional_fraction(last_trade_price.as_deref())? {
                    updates.push(PredictionOrderBookUpdate::LastTrade { price, timestamp });
                }
            }
            PolymarketMarketEvent::PriceChange {
                price_changes,
                timestamp,
            } => {
                let timestamp = book_frame_timestamp(timestamp)?;
                let mut bids = Vec::new();
                let mut asks = Vec::new();
                for change in price_changes {
                    if change.asset_id.as_str() != asset_id {
                        continue;
                    }
                    // The size is the new absolute aggregate size of the level, so a
                    // zero is published as the removal it is rather than dropped.
                    let level = PredictionBookLevelChange {
                        price: change.price.parse::<Fraction>().map_err(model_error)?,
                        size: change.size.parse::<Quantity>().map_err(model_error)?,
                    };
                    match change.side.as_str() {
                        "BUY" => bids.push(level),
                        "SELL" => asks.push(level),
                        _ => {}
                    }
                }
                if !bids.is_empty() || !asks.is_empty() {
                    updates.push(PredictionOrderBookUpdate::Levels {
                        bids,
                        asks,
                        timestamp,
                    });
                }
            }
            PolymarketMarketEvent::TickSizeChange {
                asset_id: event_asset,
                new_tick_size,
                timestamp,
            } => {
                if event_asset.as_str() != asset_id {
                    continue;
                }
                updates.push(PredictionOrderBookUpdate::TickSize {
                    tick_size: new_tick_size.parse::<Tick>().map_err(model_error)?,
                    timestamp: book_frame_timestamp(timestamp)?,
                });
            }
            PolymarketMarketEvent::LastTradePrice {
                asset_id: event_asset,
                price,
                timestamp,
            } => {
                if event_asset.as_str() != asset_id {
                    continue;
                }
                updates.push(PredictionOrderBookUpdate::LastTrade {
                    price: price.parse::<Fraction>().map_err(model_error)?,
                    timestamp: book_frame_timestamp(timestamp)?,
                });
            }
            PolymarketMarketEvent::BestBidAsk { .. } | PolymarketMarketEvent::Unknown => {}
        }
    }
    if updates.is_empty() {
        return Ok(None);
    }
    Ok(Some(updates))
}

impl MarketParticipantAdapter for PolymarketAdapter {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    fn limits(&self, _route: &VenueRoute) -> AdapterLimits {
        AdapterLimits {
            order_book_depth_limit: 0,
            timeframes: Vec::new(),
            adjustments: Vec::new(),
        }
    }

    fn execute(
        &self,
        context: ExecutionContext,
        prepared: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move {
            let request = prepared.request().clone();
            match &request {
                ProtocolRequest::FetchPredictionEvents(query) => Ok(
                    ProtocolResponse::PredictionEvents(adapter.events(&context, query).await?),
                ),
                ProtocolRequest::FetchPredictionEvent(query) => Ok(
                    ProtocolResponse::PredictionEvent(adapter.event(&context, query).await?),
                ),
                ProtocolRequest::FetchPredictionSeries(query) => Ok(
                    ProtocolResponse::PredictionSeries(adapter.series(&context, query).await?),
                ),
                ProtocolRequest::FetchPredictionOrderBook(query) => {
                    Ok(ProtocolResponse::PredictionOrderBook(
                        adapter.prediction_order_book(&context, query).await?,
                    ))
                }
                _ => Err(MarketParticipantError::UnsupportedCapability {
                    route: context.route(),
                    capability: context.capability(),
                }),
            }
        })
    }

    fn subscribe(
        &self,
        context: ExecutionContext,
        prepared: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move {
            let request = prepared.request().clone();
            match &request {
                ProtocolRequest::SubscribePredictionSeries(query) => {
                    adapter.subscribe_prediction_series(&context, query).await
                }
                ProtocolRequest::SubscribePredictionOrderBook(query) => {
                    adapter
                        .subscribe_prediction_order_book(&context, query)
                        .await
                }
                _ => Err(MarketParticipantError::UnsupportedCapability {
                    route: context.route(),
                    capability: context.capability(),
                }),
            }
        })
    }
}

fn categories() -> Vec<PredictionCategory> {
    CATEGORIES
        .iter()
        .map(|(key, label, _, _)| PredictionCategory::new(*key, *label))
        .collect()
}

fn merge_localized_events(events: Vec<GammaEvent>, localized: Vec<GammaEvent>) -> Vec<GammaEvent> {
    let mut localized: BTreeMap<SmolStr, GammaEvent> = localized
        .into_iter()
        .map(|event| (event.id.clone(), event))
        .collect();
    events
        .into_iter()
        .map(|mut event| {
            if let Some(localized_event) = localized.remove(&event.id) {
                merge_localized_event(&mut event, localized_event);
            }
            event
        })
        .collect()
}

fn merge_localized_event(event: &mut GammaEvent, localized: GammaEvent) {
    if !localized.title.is_empty() {
        event.title = localized.title;
    }
    let mut localized: BTreeMap<SmolStr, GammaMarket> = localized
        .markets
        .into_iter()
        .map(|market| (market.id.clone(), market))
        .collect();
    for market in &mut event.markets {
        if let Some(localized_market) = localized.remove(&market.id) {
            market.apply_localized(localized_market);
        }
    }
}

/// Appends the markets of every child event that the parent does not already
/// publish.
fn merge_child_events(event: &mut GammaEvent, children: Vec<GammaEvent>) {
    let mut seen: BTreeSet<SmolStr> = event
        .markets
        .iter()
        .map(|market| market.id.clone())
        .collect();
    for child in children {
        for market in child.markets {
            if seen.insert(market.id.clone()) {
                event.markets.push(market);
            }
        }
    }
}

fn category_of(event: &GammaEvent) -> PredictionCategory {
    for (key, label, id, _) in CATEGORIES {
        if event.tags.iter().any(|tag| tag.id == id) {
            return PredictionCategory::new(key, label);
        }
    }
    PredictionCategory::new("other", "Other")
}

fn decode_card(event: &GammaEvent) -> Result<PredictionEventCard, MarketParticipantError> {
    // A card lists what a reader can act on now; the event detail keeps closed
    // markets so a finished event's history stays chartable.
    let open = GammaEvent {
        markets: event
            .markets
            .iter()
            .filter(|market| !market.closed && market.active)
            .cloned()
            .collect(),
        ..event.clone()
    };
    let (view, options) = decode_view(&open)?;
    let headline: Vec<&PredictionMarket> = view
        .markets
        .iter()
        .filter(|market| market.headline)
        .collect();
    let mut options: Vec<PredictionOptionCard> = if headline.is_empty() {
        view.markets
            .iter()
            .filter_map(|market| best_option(&options, market))
            .collect()
    } else if headline.len() == 1 && headline[0].legs.len() > 1 {
        options
            .iter()
            .filter(|option| option.market == headline[0].market)
            .cloned()
            .collect()
    } else {
        headline
            .iter()
            .filter_map(|market| best_option(&options, market))
            .collect()
    };
    options.sort_by(|left, right| {
        // An option the venue published no price for sorts after every option
        // it did price, so a missing quote cannot push a priced option out of
        // the card's bounded option list.
        right
            .probability
            .is_some()
            .cmp(&left.probability.is_some())
            .then_with(|| right.probability.cmp(&left.probability))
    });
    let total_options = options.len();
    options.truncate(CARD_OPTIONS);
    let remaining_options = total_options.saturating_sub(CARD_OPTIONS);
    let represented_markets = if headline.is_empty() {
        options.len()
    } else {
        headline.len().min(options.len())
    };
    let market_count = view.markets.len();
    let remaining_markets = market_count.saturating_sub(represented_markets);
    Ok(PredictionEventCard {
        event: view.event,
        title: view.title,
        category: view.category,
        ends_at: view.ends_at,
        volume: quantity(event.volume),
        liquidity: quantity(event.liquidity),
        options,
        remaining_options: u16::try_from(remaining_options).unwrap_or(u16::MAX),
        market_count: u16::try_from(market_count).unwrap_or(u16::MAX),
        remaining_markets: u16::try_from(remaining_markets).unwrap_or(u16::MAX),
    })
}

/// Picks the option a card shows for one market: the side marked as the
/// market's first outcome, otherwise its first side.
fn best_option(
    options: &[PredictionOptionCard],
    market: &PredictionMarket,
) -> Option<PredictionOptionCard> {
    let mut fallback = None;
    let mut selected = None;
    for option in options
        .iter()
        .filter(|option| option.market == market.market)
    {
        if market.default_leg.as_ref() == Some(&option.leg) {
            selected = Some(option);
            break;
        }
        fallback.get_or_insert(option);
    }
    let mut option = selected.or(fallback)?.clone();
    option.label = market.label.clone();
    Some(option)
}

fn decode_view(
    event: &GammaEvent,
) -> Result<(PredictionEventView, Vec<PredictionOptionCard>), MarketParticipantError> {
    let event_ref = PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the prediction scope is canonical"),
        ),
        native_event: IdentifierValue::new(event.id.as_str()).map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "decode a Polymarket event id".into(),
                reason: format_smolstr!("{error}"),
                raw_data: None,
            }
        })?,
    };
    let settlement = pusd_asset()?;
    let ends_at = event.end_date.as_deref().and_then(parse_iso8601_millis);
    let mut markets = Vec::new();
    let mut options = Vec::new();
    for market in &event.markets {
        let protocol = match protocol_of(market) {
            Ok(protocol) => protocol,
            Err(error) => {
                // One stale or future market must not hide the rest of an event.
                log::debug!("skipped a Polymarket market: {error}");
                continue;
            }
        };
        let Some((group, assets, prices)) = market_group(market, &settlement, ends_at, protocol)?
        else {
            continue;
        };
        let market_ref = PredictionMarketRef {
            event: event_ref.clone(),
            native_market: IdentifierValue::new(market.id.as_str()).map_err(|error| {
                MarketParticipantError::ProtocolError {
                    operation: "decode a Polymarket market id".into(),
                    reason: format_smolstr!("{error}"),
                    raw_data: None,
                }
            })?,
        };
        let question = if market.question.is_empty() {
            market.id.clone()
        } else {
            market.question.clone()
        };
        let label = market
            .group_item_title
            .as_ref()
            .filter(|label| !label.is_empty())
            .cloned()
            .unwrap_or_else(|| question.clone());
        let group_meta = market_group_meta(market);
        let headline = group_meta.key == "moneyline";
        let display_outcomes = market_display_outcomes(market);
        let mut default_leg = None;
        let mut legs = Vec::new();
        for (leg, side_label) in group_legs(&group) {
            // The group sorts members and sides by their canonical ids, which need
            // not be the order Gamma published. Resolve the asset by the side's own
            // position in the outcome list, never by zipping sorted members to the
            // wire arrays.
            let Some(index) = display_outcomes
                .iter()
                .position(|outcome| outcome == &side_label)
            else {
                continue;
            };
            let Some(asset_id) = assets.get(index) else {
                continue;
            };
            if index == 0 {
                default_leg = Some(leg.clone());
            }
            let probability = prices.get(index).copied().flatten();
            let listing = ListingContinuityKey {
                route: event_ref.route.clone(),
                terms: ListingContinuityTerms::Polymarket {
                    condition_id: market.condition_id.clone().try_into().map_err(|error| {
                        MarketParticipantError::ProtocolError {
                            operation: "decode a Polymarket condition id".into(),
                            reason: format_smolstr!("{error}"),
                            raw_data: None,
                        }
                    })?,
                    asset_id: asset_id.clone(),
                    protocol,
                },
            }
            .listing_key()
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
            legs.push(PredictionLeg {
                leg: leg.clone(),
                label: side_label.clone(),
                probability,
                listing,
                native: PredictionNativeLeg::Polymarket {
                    protocol,
                    asset_id: asset_id.clone(),
                },
            });
            options.push(PredictionOptionCard {
                market: market_ref.clone(),
                leg,
                label: side_label,
                probability,
            });
        }
        markets.push(PredictionMarket {
            market: market_ref,
            question,
            label,
            group: group_meta,
            headline,
            open: !market.closed && market.active,
            default_leg,
            legs,
        });
    }
    Ok((
        PredictionEventView {
            event: event_ref,
            title: if event.title.is_empty() {
                event.id.clone()
            } else {
                event.title.clone()
            },
            category: category_of(event),
            ends_at,
            markets,
        },
        options,
    ))
}

fn market_group(
    market: &GammaMarket,
    settlement: &market_participants_model::asset::AssetId,
    ends_at: Option<UnixMillis>,
    protocol: PolymarketProtocolVersion,
) -> Result<
    Option<(
        market_participants_model::outcome::OutcomeGroup,
        Vec<PolymarketAssetId>,
        Vec<Option<Fraction>>,
    )>,
    MarketParticipantError,
> {
    let canonical_outcomes = parse_string_array(
        market
            .canonical_outcomes
            .as_ref()
            .or(market.outcomes.as_ref()),
    );
    let outcomes = market_display_outcomes(market);
    let prices: Vec<Option<Fraction>> = parse_string_array(market.outcome_prices.as_ref())
        .iter()
        .map(|value| probability(value))
        .collect();
    let ids = match protocol {
        PolymarketProtocolVersion::V2 => market
            .position_ids
            .as_ref()
            .map(GammaIdList::values)
            .unwrap_or_default(),
        PolymarketProtocolVersion::V1 => market
            .clob_token_ids
            .as_ref()
            .map(GammaIdList::values)
            .unwrap_or_default(),
    };
    if outcomes.is_empty() || ids.is_empty() || outcomes.len() != ids.len() {
        log::debug!(
            "skipped a Polymarket market with {} outcomes and {} asset ids",
            outcomes.len(),
            ids.len()
        );
        return Ok(None);
    }
    let condition = market.condition_id.clone();
    if condition.is_empty() {
        return Ok(None);
    }
    let question = if market.question.is_empty() {
        market.id.clone()
    } else {
        market.question.clone()
    };
    // The localized copy may say `是` / `否`; only the canonical outcomes decide
    // the market's shape. The display outcomes are used for the option labels.
    let binary_market = is_binary_outcome_set(&canonical_outcomes);
    let member_label = market
        .group_item_title
        .as_ref()
        .filter(|label| !label.is_empty())
        .cloned()
        .unwrap_or_else(|| question.clone());
    let (topology, members) = if binary_market {
        let sides = if outcomes.len() == 2 {
            outcomes
        } else {
            canonical_outcomes
        };
        (binary(), vec![(member_label, sides)])
    } else {
        (
            exactly_one(),
            outcomes
                .iter()
                .map(|outcome| (outcome.clone(), vec![outcome.clone()]))
                .collect(),
        )
    };
    let group = build_group(question, ends_at, settlement.clone(), topology, members)?;
    let mut assets = Vec::with_capacity(ids.len());
    for id in &ids {
        assets.push(id.parse::<PolymarketAssetId>().map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "decode a Polymarket asset id".into(),
                reason: format_smolstr!("{error}"),
                raw_data: None,
            }
        })?);
    }
    let mut aligned_prices = Vec::with_capacity(assets.len());
    for index in 0..assets.len() {
        aligned_prices.push(prices.get(index).copied().flatten());
    }
    Ok(Some((group, assets, aligned_prices)))
}

/// The market group Gamma publishes for a sports market, or the single generic
/// group of a non-sports event.
fn market_group_meta(market: &GammaMarket) -> PredictionMarketGroup {
    let Some(kind) = market
        .sports_market_type
        .as_deref()
        .filter(|kind| !kind.is_empty())
    else {
        return PredictionMarketGroup::new("markets", "Markets");
    };
    PredictionMarketGroup::new(kind, market_type_label(kind))
}

/// Turns a Gamma sports market type such as `first_half_spreads` into a label a
/// reader can scan.
fn market_type_label(kind: &str) -> SmolStr {
    let (prefix, rest) = if let Some(rest) = kind.strip_prefix("first_half_") {
        (SmolStr::new_static("1st Half"), rest)
    } else if let Some(rest) = kind.strip_prefix("second_half_") {
        (SmolStr::new_static("2nd Half"), rest)
    } else if let Some(rest) = kind.strip_prefix("q1_") {
        (SmolStr::new_static("Q1"), rest)
    } else if let Some(rest) = kind.strip_prefix("q2_") {
        (SmolStr::new_static("Q2"), rest)
    } else if let Some(rest) = kind.strip_prefix("q3_") {
        (SmolStr::new_static("Q3"), rest)
    } else if let Some(rest) = kind.strip_prefix("q4_") {
        (SmolStr::new_static("Q4"), rest)
    } else if kind == "first_half" {
        return SmolStr::new_static("1st Half");
    } else if kind == "second_half" {
        return SmolStr::new_static("2nd Half");
    } else {
        (SmolStr::new_static(""), kind)
    };
    let rest = humanize_market_type(rest);
    if prefix.is_empty() {
        rest
    } else if rest.is_empty() {
        prefix
    } else {
        format_smolstr!("{prefix} {rest}")
    }
}

fn humanize_market_type(kind: &str) -> SmolStr {
    let mut label = SmolStr::new_static("");
    for word in kind.split('_').filter(|word| !word.is_empty()) {
        let word = match word {
            "ou" => SmolStr::new_static("O/U"),
            "q1" => SmolStr::new_static("Q1"),
            "q2" => SmolStr::new_static("Q2"),
            "q3" => SmolStr::new_static("Q3"),
            "q4" => SmolStr::new_static("Q4"),
            _ => {
                let mut chars = word.chars();
                let Some(first) = chars.next() else {
                    continue;
                };
                format_smolstr!("{}{}", first.to_ascii_uppercase(), chars.as_str())
            }
        };
        label = if label.is_empty() {
            word
        } else {
            format_smolstr!("{label} {word}")
        };
    }
    label
}

fn protocol_of(market: &GammaMarket) -> Result<PolymarketProtocolVersion, MarketParticipantError> {
    match market.version.as_deref() {
        // Gamma leaves `version` out of the legacy CTF markets. Those markets
        // are the original v1 protocol, so absence is the v1 answer rather than
        // a malformed market. A version the venue does publish must be one this
        // build knows how to address.
        None | Some("v1") => Ok(PolymarketProtocolVersion::V1),
        Some("v2") => Ok(PolymarketProtocolVersion::V2),
        Some(version) => Err(protocol_error(
            "decode a Polymarket market version",
            format_smolstr!("unsupported market version `{version}`"),
        )),
    }
}

fn is_binary_outcome_set(outcomes: &[SmolStr]) -> bool {
    outcomes.len() == 2
        && outcomes[0].eq_ignore_ascii_case("yes")
        && outcomes[1].eq_ignore_ascii_case("no")
}

fn market_display_outcomes(market: &GammaMarket) -> Vec<SmolStr> {
    let mut outcomes = parse_string_array(market.outcomes.as_ref());
    if outcomes.is_empty() {
        outcomes = parse_string_array(market.canonical_outcomes.as_ref());
    }
    outcomes
}

fn parse_string_array(value: Option<&GammaIdList>) -> Vec<SmolStr> {
    value.map(GammaIdList::values).unwrap_or_default()
}

fn native_asset_id(leg: &PredictionLeg) -> Option<&str> {
    match &leg.native {
        PredictionNativeLeg::Polymarket { asset_id, .. } => Some(asset_id.as_str()),
        PredictionNativeLeg::HyperliquidOutcome { .. } => None,
    }
}

fn protocol_error(operation: &'static str, reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: reason.into(),
        raw_data: None,
    }
}

fn history_point(point: &ClobHistoryPoint) -> Option<PredictionPricePoint> {
    if point.t < 0 {
        return None;
    }
    let timestamp = point.t.checked_mul(1_000)?;
    let probability = probability_f64(Some(point.p))?;
    Some(PredictionPricePoint {
        timestamp,
        probability,
    })
}

fn probability_f64(value: Option<f64>) -> Option<Fraction> {
    let value = value?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return None;
    }
    let inner = (value * 1_000_000_000_000_000_000.0).round();
    if !inner.is_finite() || inner < 0.0 || inner > u128::MAX as f64 {
        return None;
    }
    Fraction::new(Ratio::from_inner(inner as u128)).ok()
}

fn quantity(value: Option<f64>) -> Option<Quantity> {
    let value = value?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    format!("{value:.8}").parse().ok()
}

fn pusd_asset() -> Result<market_participants_model::asset::AssetId, MarketParticipantError> {
    let chain =
        ChainId::new(POLYGON_CHAIN).map_err(|error| MarketParticipantError::ProtocolError {
            operation: "build the Polymarket settlement asset".into(),
            reason: format_smolstr!("{error}"),
            raw_data: None,
        })?;
    let address = ContractAddress::new(PUSD_CONTRACT).map_err(|error| {
        MarketParticipantError::ProtocolError {
            operation: "build the Polymarket settlement asset".into(),
            reason: format_smolstr!("{error}"),
            raw_data: None,
        }
    })?;
    let terms = CryptoTokenTerms {
        origin: CryptoTokenOrigin::Chain {
            chain_id: chain,
            token: TokenReference::Contract { address },
        },
    };
    terms
        .validate()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: "build the Polymarket settlement asset".into(),
            reason: format_smolstr!("{error}"),
            raw_data: None,
        })?;
    Ok(AssetTerms::CryptoToken(terms).asset_id())
}

fn endpoint(base: &url::Url, path: &str) -> Result<url::Url, MarketParticipantError> {
    base.join(path)
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))
}

async fn get_json<T>(
    context: &ExecutionContext,
    url: &url::Url,
) -> Result<T, MarketParticipantError>
where
    T: serde::de::DeserializeOwned,
{
    let response = context
        .http_client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| transport_error(context, url, error))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| MarketParticipantError::DecodeFailed {
            operation: "read a Polymarket response".into(),
            raw_data: Bytes::from(error.to_string()),
        })?;
    if !status.is_success() {
        return Err(crate::adapter::error::http_response_error(
            crate::adapter::error::FailureTarget::Route(&context.route()),
            url.as_str(),
            status,
            body,
        ));
    }
    serde_json::from_slice(&body).map_err(|_| MarketParticipantError::DecodeFailed {
        operation: "decode a Polymarket response".into(),
        raw_data: body,
    })
}

async fn post_json<T, B>(
    context: &ExecutionContext,
    url: &url::Url,
    body: &B,
) -> Result<T, MarketParticipantError>
where
    T: serde::de::DeserializeOwned,
    B: serde::Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(body)
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let response = context
        .http_client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(bytes)
        .send()
        .await
        .map_err(|error| transport_error(context, url, error))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| MarketParticipantError::DecodeFailed {
            operation: "read a Polymarket response".into(),
            raw_data: Bytes::from(error.to_string()),
        })?;
    if !status.is_success() {
        return Err(crate::adapter::error::http_response_error(
            crate::adapter::error::FailureTarget::Route(&context.route()),
            url.as_str(),
            status,
            body,
        ));
    }
    serde_json::from_slice(&body).map_err(|_| MarketParticipantError::DecodeFailed {
        operation: "decode a Polymarket response".into(),
        raw_data: body,
    })
}

fn transport_error(
    context: &ExecutionContext,
    url: &url::Url,
    error: reqwest::Error,
) -> MarketParticipantError {
    MarketParticipantError::ConnectionFailed {
        route: context.route(),
        endpoint: format_smolstr!("{url}"),
        source: network::error::NetworkError::Io {
            message: format_smolstr!("{error}"),
        },
    }
}

/// One CLOB history request's window.
///
/// The endpoint states a window two ways with different ceilings: an explicit
/// `start_ts`/`end_ts` pair is exact but refused past fifteen days, and an
/// `interval` preset reaches a month but silently collapses to one month at a
/// fine `fidelity`. This picks the shape the span can actually be asked with,
/// and remembers the instant the caller asked from so a preset answer wider than
/// the request can be trimmed.
struct HistoryWindow {
    interval: Option<&'static str>,
    fidelity: u16,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
    /// The oldest instant the caller asked for, in milliseconds. The whole-life
    /// range asks from the epoch, so every answer passes the trim.
    since: UnixMillis,
}

impl HistoryWindow {
    fn contains(&self, timestamp: UnixMillis) -> bool {
        timestamp >= self.since
    }
}

/// The longest span the endpoint accepts as an explicit timestamp pair.
const CLOB_EXPLICIT_WINDOW_MILLIS: i64 = 15 * 86_400_000;

/// The longest span the month preset reaches.
const CLOB_PRESET_WINDOW_MILLIS: i64 = 30 * 86_400_000;

/// The shortest span a request states: a non-positive span is one minute.
const CLOB_MINIMUM_SPAN_MILLIS: i64 = 60_000;

/// The bucket width the venue serves the whole history at, in minutes.
///
/// Below twelve hours the `max` preset silently answers one month instead of the
/// whole history, so a span past the explicit limit has to be sampled at least
/// this coarsely to actually reach back that far.
const CLOB_MAX_PRESET_FIDELITY: u16 = 720;

fn history_window(
    span: Option<i64>,
    fidelity_minutes: u16,
) -> Result<HistoryWindow, MarketParticipantError> {
    let end_ts = now_millis()? / 1_000;
    let Some(span) = span else {
        // The whole life of the market: the preset states the reach and there is
        // no earlier instant to trim to.
        return Ok(HistoryWindow {
            interval: Some("max"),
            fidelity: fidelity_minutes.max(CLOB_MAX_PRESET_FIDELITY),
            start_ts: None,
            end_ts: None,
            since: 0,
        });
    };
    // A span of zero (or less) would ask the venue for an empty pair, so it is
    // clamped to one minute. The pair itself is stated in whole seconds.
    let span = span.max(CLOB_MINIMUM_SPAN_MILLIS);
    let start_ts = end_ts.saturating_sub(span / 1_000);
    let since = start_ts.saturating_mul(1_000);
    if span <= CLOB_EXPLICIT_WINDOW_MILLIS {
        return Ok(HistoryWindow {
            interval: None,
            fidelity: fidelity_minutes.max(1),
            start_ts: Some(start_ts),
            end_ts: Some(end_ts),
            since,
        });
    }
    // Past the explicit limit the span is stated by a preset. `1m` is the
    // venue's one-month window, not one minute: the sample spacing is
    // `fidelity`, which is a separate field. A span past a month has to be asked
    // with `max`, which only reaches the whole life at a twelve-hour bucket.
    let interval = if span <= CLOB_PRESET_WINDOW_MILLIS {
        "1m"
    } else {
        "max"
    };
    Ok(HistoryWindow {
        interval: Some(interval),
        fidelity: if interval == "max" {
            fidelity_minutes.max(CLOB_MAX_PRESET_FIDELITY)
        } else {
            fidelity_minutes.max(1)
        },
        start_ts: None,
        end_ts: None,
        since,
    })
}

fn now_millis() -> Result<UnixMillis, MarketParticipantError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| {
            MarketParticipantError::Configuration("the clock is before the epoch".into())
        })
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        error::ModelError,
        listing::venue::{HyperliquidAssetId, HyperliquidOutcomeId, PolymarketProtocolVersion},
        outcome::OutcomeLegRef,
        prediction::{
            PredictionEventRef, PredictionEventView, PredictionLeg, PredictionNativeLeg,
            PredictionOrderBookUpdate,
        },
        routing::CapabilityId,
    };
    use smol_str::SmolStr;

    use super::{
        CATEGORIES, category_of, decode_book_snapshot, decode_card, decode_market_frame,
        decode_prediction_book_frame, decode_view, history_window, is_binary_outcome_set,
        market_channel_payloads, market_type_label, merge_child_events, merge_localized_event,
        now_millis, parse_string_array, prediction_order_book_asset,
    };
    use crate::{
        adapter::polymarket::types::{
            ClobBookSnapshot, GammaEvent, GammaId, GammaIdList, GammaMarket, GammaTag,
        },
        error::MarketParticipantError,
        market_participants::model::{PredictionOrderBookQuery, PredictionSeriesSubscriptionQuery},
    };

    #[test]
    fn the_fixed_category_table_is_the_agreed_eight() {
        assert_eq!(
            CATEGORIES.map(|(key, ..)| key),
            [
                "politics", "sports", "crypto", "finance", "economy", "tech", "culture", "world"
            ]
        );
    }

    #[test]
    fn the_first_matching_tag_wins() {
        let event = GammaEvent {
            id: "1".into(),
            title: "An event".into(),
            end_date: None,
            volume: None,
            liquidity: None,
            parent_event_id: None,
            tags: vec![GammaTag { id: "1401".into() }, GammaTag { id: "2".into() }],
            markets: Vec::new(),
        };
        assert_eq!(category_of(&event).key, "politics");
    }

    #[test]
    fn an_event_without_a_known_tag_is_other() {
        let event = GammaEvent {
            id: "1".into(),
            title: "An event".into(),
            end_date: None,
            volume: None,
            liquidity: None,
            parent_event_id: None,
            tags: Vec::new(),
            markets: Vec::new(),
        };
        assert_eq!(category_of(&event).key, "other");
    }

    #[test]
    fn a_history_window_is_asked_the_way_the_endpoint_accepts_it() {
        // Every range inside fifteen days is an exact timestamp pair at the
        // fidelity its range asks for.
        for (span, fidelity) in [
            (3_600_000_i64, 1_u16),
            (21_600_000, 5),
            (86_400_000, 10),
            (604_800_000, 60),
        ] {
            let short = history_window(Some(span), fidelity).expect("the clock is readable");
            assert!(short.interval.is_none());
            let start = short.start_ts.expect("the pair has a start");
            let end = short.end_ts.expect("the pair has an end");
            assert_eq!(end - start, span / 1_000);
            assert_eq!(short.fidelity, fidelity);
            assert_eq!(short.since, start * 1_000);
        }

        // The endpoint refuses an explicit span past fifteen days, so the
        // boundary itself is the last explicit pair.
        let boundary = history_window(Some(15 * 86_400_000), 60).expect("the clock is readable");
        assert!(boundary.interval.is_none());
        assert!(boundary.start_ts.is_some() && boundary.end_ts.is_some());

        // Past that, the span is stated by the month preset.
        let month = history_window(Some(30 * 86_400_000), 60).expect("the clock is readable");
        assert_eq!(month.interval, Some("1m"));
        assert!(month.start_ts.is_none() && month.end_ts.is_none());
        assert_eq!(month.fidelity, 60);

        // A span longer than a month only reaches back at a twelve-hour bucket.
        let beyond = history_window(Some(30 * 86_400_000 + 1), 60).expect("the clock is readable");
        assert_eq!(beyond.interval, Some("max"));
        assert_eq!(beyond.fidelity, 720);

        // The whole life of the market asks with no explicit bounds and trims
        // nothing.
        let all = history_window(None, 720).expect("the clock is readable");
        assert_eq!(all.interval, Some("max"));
        assert_eq!(all.fidelity, 720);
        assert!(all.start_ts.is_none() && all.end_ts.is_none());
        assert_eq!(all.since, 0);
        assert!(all.contains(0), "the whole-life range trims nothing");
    }

    #[test]
    fn a_preset_answer_is_trimmed_to_the_span_that_was_asked_for() {
        let window = history_window(Some(30 * 86_400_000), 60).expect("the clock is readable");
        let now = now_millis().expect("the clock is readable");
        let requested_from = now - 30 * 86_400_000;

        assert!(window.contains(requested_from + 2_000));
        // The request states its bounds in whole seconds, so the trim edge sits
        // within the second the caller asked from.
        assert!(!window.contains(requested_from - 2_000));
    }

    #[test]
    fn a_non_positive_span_is_asked_as_one_minute() {
        let window = history_window(Some(0), 1).expect("the clock is readable");
        let start = window.start_ts.expect("the clamped pair has a start");
        let end = window.end_ts.expect("the clamped pair has an end");

        assert_eq!(end - start, 60);
        assert!(window.contains(start * 1_000));
        assert!(!window.contains(start * 1_000 - 1));
    }

    #[test]
    fn a_json_array_string_is_read_as_its_values() {
        assert_eq!(
            parse_string_array(Some(GammaIdList::String("[\"Yes\", \"No\"]".into())).as_ref()),
            ["Yes", "No"]
        );
        assert!(
            parse_string_array(Some(GammaIdList::String("not json".into())).as_ref()).is_empty()
        );
    }

    #[test]
    fn only_the_canonical_yes_no_set_is_binary() {
        let yes_no =
            parse_string_array(Some(GammaIdList::String("[\"Yes\", \"No\"]".into())).as_ref());
        let localized =
            parse_string_array(Some(GammaIdList::String("[\"是\", \"否\"]".into())).as_ref());
        let teams = parse_string_array(
            Some(GammaIdList::String("[\"Bears\", \"Seahawks\"]".into())).as_ref(),
        );

        assert!(is_binary_outcome_set(&yes_no));
        assert!(!is_binary_outcome_set(&localized));
        assert!(!is_binary_outcome_set(&teams));
    }

    #[test]
    fn a_localized_binary_market_keeps_its_shape_and_uses_localized_labels() {
        let mut event = binary_event(binary_market("v1", &["123", "456"]));
        event.title = "An event".into();
        let mut localized = event.clone();
        localized.title = "一个事件".into();
        localized.markets[0].question = "会发生吗？".into();
        localized.markets[0].group_item_title = Some("十二月三十一日".into());
        localized.markets[0].outcomes = Some("[\"是\", \"否\"]".into());

        merge_localized_event(&mut event, localized);
        let (view, options) = decode_view(&event).expect("the localized fixture decodes");

        assert_eq!(view.title, "一个事件");
        assert_eq!(
            event.markets[0]
                .canonical_outcomes
                .as_ref()
                .map(GammaIdList::values),
            Some(vec!["Yes".into(), "No".into()])
        );
        assert_eq!(
            event.markets[0].outcomes.as_ref().map(GammaIdList::values),
            Some(vec!["是".into(), "否".into()])
        );
        assert_eq!(view.markets[0].label, "十二月三十一日");
        assert_eq!(view.markets[0].question, "会发生吗？");
        assert_eq!(view.markets[0].legs.len(), 2);
        assert_eq!(options.len(), 2, "both localized sides stay addressable");
        let yes = options
            .iter()
            .find(|option| option.label == "是")
            .expect("the localized Yes side is present");
        assert_eq!(
            yes.probability,
            Some("0.4".parse().expect("the fixture probability is valid"))
        );
        let no = options
            .iter()
            .find(|option| option.label == "否")
            .expect("the localized No side is present");
        assert_eq!(
            no.probability,
            Some("0.6".parse().expect("the fixture probability is valid"))
        );
    }

    fn binary_market(version: &str, ids: &[&str]) -> GammaMarket {
        GammaMarket {
            id: "10".into(),
            question: "Will it happen?".into(),
            condition_id: format!("0x{}", "a".repeat(64)).into(),
            outcomes: Some("[\"Yes\", \"No\"]".into()),
            canonical_outcomes: None,
            outcome_prices: Some("[\"0.4\", \"0.6\"]".into()),
            clob_token_ids: (version == "v1").then(|| {
                GammaIdList::String(
                    serde_json::to_string(ids)
                        .expect("the fixture ids should encode")
                        .into(),
                )
            }),
            position_ids: (version == "v2")
                .then(|| GammaIdList::List(ids.iter().map(|id| (*id).into()).collect())),
            group_item_title: None,
            sports_market_type: None,
            closed: false,
            active: true,
            version: Some(version.into()),
        }
    }

    fn binary_event(market: GammaMarket) -> GammaEvent {
        event_with_markets(vec![market])
    }

    fn event_with_markets(markets: Vec<GammaMarket>) -> GammaEvent {
        GammaEvent {
            id: "1".into(),
            title: "An event".into(),
            end_date: None,
            volume: None,
            liquidity: None,
            parent_event_id: None,
            tags: Vec::new(),
            markets,
        }
    }

    #[test]
    fn a_localized_participant_market_keeps_both_members() {
        let mut market = binary_market("v1", &["123", "456"]);
        market.question = "Bears vs. Seahawks".into();
        market.group_item_title = None;
        market.outcomes = Some("[\"Bears\", \"Seahawks\"]".into());
        let mut event = binary_event(market);
        let mut localized = event.clone();
        localized.markets[0].question = "熊对海鹰".into();
        localized.markets[0].outcomes = Some("[\"熊\", \"海鹰\"]".into());

        merge_localized_event(&mut event, localized);
        let (_, options) = decode_view(&event).expect("the localized fixture decodes");

        assert_eq!(options.len(), 2, "a two-participant market is not binary");
        let bear = options
            .iter()
            .find(|option| option.label == "熊")
            .expect("the bear option is present");
        let seahawk = options
            .iter()
            .find(|option| option.label == "海鹰")
            .expect("the seahawk option is present");
        assert_eq!(
            bear.probability,
            Some("0.4".parse().expect("the fixture probability is valid"))
        );
        assert_eq!(
            seahawk.probability,
            Some("0.6".parse().expect("the fixture probability is valid"))
        );
    }

    #[test]
    fn a_book_snapshot_becomes_the_midpoint_of_its_best_levels() {
        let event = binary_event(binary_market("v1", &["123", "456"]));
        let (view, _) = decode_view(&event).expect("the fixture decodes");
        let legs: Vec<(SmolStr, _)> = view.markets[0]
            .legs
            .iter()
            .map(|leg| (SmolStr::new(asset_id(leg)), leg.leg.clone()))
            .collect();
        let frame = br#"{
            "event_type": "book",
            "asset_id": "123",
            "bids": [{"price": "0.4", "size": "10"}],
            "asks": [{"price": "0.6", "size": "10"}],
            "timestamp": "1000"
        }"#;

        let series = decode_market_frame(frame, &legs)
            .expect("the book frame decodes")
            .expect("the book frame carries a probability");

        assert_eq!(series.len(), 1);
        assert_eq!(series[0].leg, legs[0].1);
        assert!(series[0].points[0].timestamp > 0);
        assert_eq!(
            series[0].points[0].probability,
            "0.5".parse().expect("the midpoint is a valid fraction")
        );
    }

    #[test]
    fn a_price_change_accepts_a_numeric_timestamp_and_uses_its_best_levels() {
        let event = binary_event(binary_market("v1", &["123", "456"]));
        let (view, _) = decode_view(&event).expect("the fixture decodes");
        let legs: Vec<(SmolStr, _)> = view.markets[0]
            .legs
            .iter()
            .map(|leg| (SmolStr::new(asset_id(leg)), leg.leg.clone()))
            .collect();
        let frame = br#"{
            "event_type": "price_change",
            "price_changes": [{
                "asset_id": "123",
                "best_bid": "0.2",
                "best_ask": "0.4"
            }],
            "timestamp": 2000
        }"#;

        let series = decode_market_frame(frame, &legs)
            .expect("the price-change frame decodes")
            .expect("the price-change frame carries a probability");

        assert!(series[0].points[0].timestamp > 0);
        assert_eq!(
            series[0].points[0].probability,
            "0.3".parse().expect("the midpoint is a valid fraction")
        );
    }

    #[test]
    fn an_unrelated_market_event_is_ignored() {
        let event = binary_event(binary_market("v1", &["123", "456"]));
        let (view, _) = decode_view(&event).expect("the fixture decodes");
        let legs: Vec<(SmolStr, _)> = view.markets[0]
            .legs
            .iter()
            .map(|leg| (SmolStr::new(asset_id(leg)), leg.leg.clone()))
            .collect();

        let decoded = decode_market_frame(
            br#"{"event_type":"last_trade_price","asset_id":"123","price":"0.7"}"#,
            &legs,
        )
        .expect("an unrelated frame is not an error");

        assert!(decoded.is_none());
    }

    fn leg<'a>(view: &'a PredictionEventView, label: &str) -> &'a PredictionLeg {
        view.markets[0]
            .legs
            .iter()
            .find(|leg| leg.label == label)
            .unwrap_or_else(|| panic!("the {label} leg should exist"))
    }

    fn asset_id(leg: &PredictionLeg) -> &str {
        match &leg.native {
            PredictionNativeLeg::Polymarket { asset_id, .. } => asset_id.as_str(),
            PredictionNativeLeg::HyperliquidOutcome { .. } => {
                panic!("a Polymarket fixture should decode a Polymarket leg")
            }
        }
    }

    #[test]
    fn a_v1_market_reads_each_outcome_side_from_its_clob_token() {
        let event = binary_event(binary_market("v1", &["123", "456"]));
        let (view, options) = decode_view(&event).expect("the fixture decodes");

        assert_eq!(view.markets[0].legs.len(), 2);
        let yes = leg(&view, "Yes");
        let no = leg(&view, "No");
        assert_eq!(asset_id(yes), "123");
        assert_eq!(asset_id(no), "456");
        assert!(matches!(
            yes.native,
            PredictionNativeLeg::Polymarket {
                protocol: PolymarketProtocolVersion::V1,
                ..
            }
        ));
        assert_eq!(
            options
                .iter()
                .find(|option| option.label == "Yes")
                .and_then(|option| option.probability),
            Some("0.4".parse().expect("the fixture probability is valid"))
        );
    }

    #[test]
    fn a_v2_market_reads_each_outcome_side_from_its_position_id() {
        let mut market = binary_market("v2", &["987", "654"]);
        market.position_ids = Some(GammaIdList::String("[\"987\", \"654\"]".into()));
        let (view, _) = decode_view(&binary_event(market)).expect("the fixture decodes");

        let yes = leg(&view, "Yes");
        assert_eq!(asset_id(yes), "987");
        assert!(matches!(
            yes.native,
            PredictionNativeLeg::Polymarket {
                protocol: PolymarketProtocolVersion::V2,
                ..
            }
        ));
    }

    #[test]
    fn a_position_id_array_is_read_as_the_asset_id() {
        let mut market = binary_market("v2", &["987", "654"]);
        market.position_ids = Some(GammaIdList::List(vec!["987".into(), "654".into()]));
        let (view, _) = decode_view(&binary_event(market)).expect("the fixture decodes");

        assert_eq!(asset_id(leg(&view, "Yes")), "987");
    }

    #[test]
    fn a_market_without_a_version_is_a_v1_market() {
        let mut market = binary_market("v1", &["123", "456"]);
        market.version = None;
        let (view, _) = decode_view(&binary_event(market)).expect("the market is legacy v1");

        assert_eq!(view.markets.len(), 1);
        assert!(matches!(
            view.markets[0].legs[0].native,
            PredictionNativeLeg::Polymarket {
                protocol: PolymarketProtocolVersion::V1,
                ..
            }
        ));
    }

    #[test]
    fn an_unsupported_market_version_is_skipped() {
        let mut market = binary_market("v1", &["123", "456"]);
        market.version = Some("v3".into());
        let (view, _) = decode_view(&binary_event(market)).expect("the event still decodes");

        assert!(view.markets.is_empty());
    }

    #[test]
    fn a_card_skips_closed_or_inactive_markets_but_detail_keeps_them() {
        let mut closed = binary_market("v1", &["123", "456"]);
        closed.closed = true;
        let card = decode_card(&binary_event(closed.clone())).expect("the event decodes");
        assert!(card.options.is_empty());
        assert_eq!(card.market_count, 0);
        let (view, _) = decode_view(&binary_event(closed)).expect("the detail decodes");
        assert_eq!(view.markets.len(), 1);
        assert!(!view.markets[0].open);

        let mut inactive = binary_market("v1", &["123", "456"]);
        inactive.active = false;
        let card = decode_card(&binary_event(inactive.clone())).expect("the event decodes");
        assert!(card.options.is_empty());
        assert_eq!(card.market_count, 0);
        let (view, _) = decode_view(&binary_event(inactive)).expect("the detail decodes");
        assert_eq!(view.markets.len(), 1);
        assert!(!view.markets[0].open);
    }

    #[test]
    fn a_sports_market_keeps_its_group_and_is_a_headline_market() {
        let mut market = binary_market("v1", &["123", "456"]);
        market.question = "Bears vs. Seahawks".into();
        market.outcomes = Some("[\"Bears\", \"Seahawks\"]".into());
        market.outcome_prices = Some("[\"0.565\", \"0.435\"]".into());
        market.sports_market_type = Some("moneyline".into());

        let (view, _) = decode_view(&binary_event(market)).expect("the fixture decodes");

        assert_eq!(view.markets[0].group.key, "moneyline");
        assert_eq!(view.markets[0].group.label, "Moneyline");
        assert!(view.markets[0].headline);
        assert!(view.markets[0].open);
        assert_eq!(view.markets[0].legs.len(), 2);
    }

    #[test]
    fn a_sports_market_type_is_rendered_as_a_readable_group_label() {
        assert_eq!(market_type_label("moneyline"), "Moneyline");
        assert_eq!(market_type_label("first_half_spreads"), "1st Half Spreads");
        assert_eq!(
            market_type_label("q1_both_teams_to_score_points"),
            "Q1 Both Teams To Score Points"
        );
    }

    #[test]
    fn child_event_markets_are_appended_once() {
        let mut parent = event_with_markets(vec![binary_market("v1", &["1", "2"])]);
        let mut child_market = binary_market("v1", &["3", "4"]);
        child_market.id = "11".into();
        let mut child = event_with_markets(vec![child_market]);
        child.parent_event_id = Some(GammaId::String("1".into()));

        merge_child_events(&mut parent, vec![child.clone()]);
        merge_child_events(&mut parent, vec![child]);

        assert_eq!(parent.markets.len(), 2);
    }

    #[test]
    fn a_card_shows_the_headline_market_and_counts_the_rest() {
        let mut moneyline = binary_market("v1", &["1", "2"]);
        moneyline.id = "m1".into();
        moneyline.question = "Bears vs. Seahawks".into();
        moneyline.outcomes = Some("[\"Bears\", \"Seahawks\"]".into());
        moneyline.outcome_prices = Some("[\"0.565\", \"0.435\"]".into());
        moneyline.sports_market_type = Some("moneyline".into());

        let mut spread = binary_market("v1", &["3", "4"]);
        spread.id = "m2".into();
        spread.question = "Spread: Bears (-1.5)".into();
        spread.outcomes = Some("[\"Bears\", \"Seahawks\"]".into());
        spread.outcome_prices = Some("[\"0.525\", \"0.475\"]".into());
        spread.sports_market_type = Some("spreads".into());

        let card = decode_card(&event_with_markets(vec![moneyline, spread]))
            .expect("the sports event decodes");

        assert_eq!(card.options.len(), 2);
        assert_eq!(card.market_count, 2);
        assert_eq!(card.remaining_markets, 1);
        assert_eq!(card.remaining_options, 0);
        assert_eq!(card.options[0].label, "Bears");
        assert_eq!(card.options[1].label, "Seahawks");
    }

    #[test]
    fn a_card_without_a_headline_shows_one_option_per_market() {
        let mut first = binary_market("v1", &["1", "2"]);
        first.id = "a".into();
        first.question = "Will A win?".into();
        first.group_item_title = Some("Candidate A".into());
        let mut second = binary_market("v1", &["3", "4"]);
        second.id = "b".into();
        second.question = "Will B win?".into();
        second.group_item_title = Some("Candidate B".into());

        let card = decode_card(&event_with_markets(vec![first, second]))
            .expect("the candidate event decodes");

        assert_eq!(card.options.len(), 2);
        assert_eq!(card.market_count, 2);
        assert_eq!(card.remaining_markets, 0);
        assert_eq!(card.remaining_options, 0);
        assert_eq!(card.options[0].label, "Candidate A");
        assert_eq!(card.options[1].label, "Candidate B");
    }

    #[test]
    fn a_card_never_draws_more_than_six_options() {
        let markets: Vec<_> = (0..8)
            .map(|index| {
                let mut market = binary_market("v1", &["1", "2"]);
                market.id = format!("m{index}").into();
                market.question = format!("Will candidate {index} win?").into();
                market.group_item_title = Some(format!("Candidate {index}").into());
                market
            })
            .collect();
        let card = decode_card(&event_with_markets(markets)).expect("the event decodes");

        assert_eq!(card.options.len(), 6);
        assert_eq!(card.remaining_options, 2);
        assert_eq!(card.remaining_markets, 2);
    }

    #[test]
    fn a_localized_candidate_card_keeps_the_yes_side() {
        let mut event = binary_event(binary_market("v1", &["123", "456"]));
        event.markets[0].question = "Will candidate A win?".into();
        event.markets[0].group_item_title = Some("Candidate A".into());
        let mut localized = event.clone();
        localized.markets[0].outcomes = Some("[\"是\", \"否\"]".into());
        localized.markets[0].group_item_title = Some("候选人 A".into());

        merge_localized_event(&mut event, localized);
        let card = decode_card(&event).expect("the localized candidate event decodes");

        assert_eq!(card.options.len(), 1);
        assert_eq!(card.options[0].label, "候选人 A");
        assert_eq!(
            card.options[0].probability,
            Some("0.4".parse().expect("the fixture probability is valid"))
        );
    }

    #[test]
    fn an_empty_event_has_no_options_and_no_markets() {
        let event = event_with_markets(Vec::new());
        let card = decode_card(&event).expect("an event with no markets still decodes");
        assert!(card.options.is_empty());
        assert_eq!(card.market_count, 0);
        assert_eq!(card.remaining_options, 0);
        assert_eq!(card.remaining_markets, 0);
    }

    /// The event, legs and first leg of one fixture prediction market.
    fn prediction_market() -> (PredictionEventRef, Vec<PredictionLeg>, OutcomeLegRef) {
        let event = binary_event(binary_market("v1", &["123", "456"]));
        let (view, _) = decode_view(&event).expect("the fixture decodes");
        let legs = view.markets[0].legs.clone();
        let requested = legs[0].leg.clone();
        (view.event, legs, requested)
    }

    /// A book snapshot of the documented CLOB shape.
    fn book_snapshot() -> ClobBookSnapshot {
        serde_json::from_str(
            r#"{
                "market": "0xd174",
                "asset_id": "123",
                "timestamp": "1793896716580",
                "hash": "0xabc",
                "bids": [
                    {"price": "0.001", "size": "691209.17"},
                    {"price": "0.008", "size": "0"},
                    {"price": "0.004", "size": "12"}
                ],
                "asks": [
                    {"price": "0.999", "size": "4020342.53"},
                    {"price": "0.9", "size": "20"},
                    {"price": "0.95", "size": "0"}
                ],
                "min_order_size": "5",
                "tick_size": "0.001",
                "neg_risk": false,
                "last_trade_price": "0.008"
            }"#,
        )
        .expect("the documented snapshot decodes")
    }

    #[test]
    fn a_book_snapshot_is_decoded_best_first_with_its_string_numbers() {
        let (_, _, leg) = prediction_market();
        let book =
            decode_book_snapshot(book_snapshot(), &leg).expect("the snapshot decodes into a book");

        assert_eq!(book.leg, leg);
        assert_eq!(book.tick_size.to_string(), "0.001");
        assert_eq!(book.timestamp, 1_793_896_716_580);
        assert_eq!(
            book.last_trade_price.map(|price| price.to_string()),
            Some("0.008".to_owned())
        );
        assert_eq!(
            book.bids
                .iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            ["0.004", "0.001"],
            "bids are published best first, and a zero-size level is not part of the book"
        );
        assert_eq!(
            book.asks
                .iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            ["0.9", "0.999"],
            "asks are published best first, and a zero-size level is not part of the book"
        );
        assert_eq!(book.asks[0].size.to_string(), "20");
    }

    #[test]
    fn a_book_snapshot_without_a_last_trade_reads_no_price() {
        let snapshot: ClobBookSnapshot = serde_json::from_str(
            r#"{"timestamp":"1793896716580","bids":[],"asks":[],"tick_size":"0.01"}"#,
        )
        .expect("a snapshot without a last trade decodes");
        let (_, _, leg) = prediction_market();
        let book = decode_book_snapshot(snapshot, &leg).expect("the snapshot decodes into a book");

        assert_eq!(book.last_trade_price, None);
        assert!(book.bids.is_empty() && book.asks.is_empty());
        assert_eq!(book.tick_size.to_string(), "0.01");
    }

    #[test]
    fn a_book_frame_replaces_the_book_and_seeds_its_tick_and_last_trade() {
        let frame: &[u8] = br#"[
            {"event_type":"new_market","asset_id":"123","timestamp":"1"},
            {"event_type":"book","market":"0xd174","asset_id":"123","timestamp":"1793896716580","hash":"0xabc",
             "bids":[{"price":"0.001","size":"691209.17"},{"price":"0.008","size":"5"},{"price":"0.004","size":"0"}],
             "asks":[{"price":"0.999","size":"4020342.53"},{"price":"0.9","size":"7"}],
             "tick_size":"0.001","last_trade_price":"0.008"},
            {"event_type":"book","asset_id":"456","bids":[],"asks":[],"timestamp":"1793896716580"}
        ]"#;
        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the frame decodes")
            .expect("the frame carries updates");

        assert_eq!(updates.len(), 3, "replace, then the tick, then the trade");
        let PredictionOrderBookUpdate::Replace {
            bids,
            asks,
            timestamp,
        } = &updates[0]
        else {
            panic!("the first update of a book frame is the replacement");
        };
        assert_eq!(
            *timestamp, 1_793_896_716_580,
            "a book update keeps the instant the venue stated"
        );
        assert_eq!(
            bids.iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            ["0.008", "0.001"]
        );
        assert_eq!(
            asks.iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            ["0.9", "0.999"]
        );
        let PredictionOrderBookUpdate::TickSize { tick_size, .. } = &updates[1] else {
            panic!("the book's own tick follows the replacement");
        };
        assert_eq!(tick_size.to_string(), "0.001");
        let PredictionOrderBookUpdate::LastTrade { price, .. } = &updates[2] else {
            panic!("the book's last trade follows its tick");
        };
        assert_eq!(price.to_string(), "0.008");
    }

    #[test]
    fn a_book_frame_without_a_tick_or_trade_publishes_the_replacement_alone() {
        let frame: &[u8] =
            br#"{"event_type":"book","asset_id":"123","bids":[],"asks":[],"timestamp":"1"}"#;
        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the frame decodes")
            .expect("the frame carries a replacement");

        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            PredictionOrderBookUpdate::Replace { .. }
        ));
    }

    #[test]
    fn a_price_change_frame_maps_sides_to_new_absolute_sizes() {
        let frame: &[u8] = br#"{
            "event_type":"price_change","market":"0xd174","timestamp":"1793896724835",
            "price_changes":[
                {"asset_id":"4329","price":"0.29","size":"8275","side":"BUY","hash":"0xa","best_bid":"0.991","best_ask":"0.992"},
                {"asset_id":"123","price":"0.71","size":"8275","side":"SELL","hash":"0xb","best_bid":"0.008","best_ask":"0.009"},
                {"asset_id":"123","price":"0.008","size":"0","side":"BUY","hash":"0xc","best_bid":"0.009","best_ask":"0.01"}
            ]
        }"#;
        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the frame decodes")
            .expect("the frame carries one change batch");

        assert_eq!(updates.len(), 1);
        let PredictionOrderBookUpdate::Levels { bids, asks, .. } = &updates[0] else {
            panic!("a price change is published as level changes");
        };
        assert_eq!(bids.len(), 1, "the frame's BUY level is a bid");
        assert_eq!(bids[0].price.to_string(), "0.008");
        assert_eq!(
            bids[0].size.to_string(),
            "0",
            "a zero size is the removal of the level, not a level to drop"
        );
        assert_eq!(asks.len(), 1, "the frame's SELL level is an ask");
        assert_eq!(asks[0].price.to_string(), "0.71");
        assert_eq!(asks[0].size.to_string(), "8275");
    }

    #[test]
    fn a_tick_size_change_and_a_last_trade_frame_are_decoded() {
        let tick: &[u8] = br#"{
            "event_type":"tick_size_change","asset_id":"123","market":"0xd174",
            "old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1793896724835"
        }"#;
        let updates = decode_prediction_book_frame(tick, "123")
            .expect("the frame decodes")
            .expect("the frame carries a tick change");
        let PredictionOrderBookUpdate::TickSize { tick_size, .. } = &updates[0] else {
            panic!("a tick-size change is published as a tick-size update");
        };
        assert_eq!(tick_size.to_string(), "0.001");
        assert_eq!(updates.len(), 1);

        let trade: &[u8] = br#"{
            "event_type":"last_trade_price","asset_id":"123","market":"0xd174",
            "price":"0.008","size":"100","side":"BUY","timestamp":"1793896724835"
        }"#;
        let updates = decode_prediction_book_frame(trade, "123")
            .expect("the frame decodes")
            .expect("the frame carries a trade");
        let PredictionOrderBookUpdate::LastTrade { price, .. } = &updates[0] else {
            panic!("a last trade is published as a last-trade update");
        };
        assert_eq!(price.to_string(), "0.008");
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn frames_of_other_assets_and_unknown_events_are_ignored() {
        let frames: [&[u8]; 6] = [
            br#"{"event_type":"last_trade_price","asset_id":"456","price":"0.7","timestamp":"1"}"#,
            br#"{"event_type":"book","asset_id":"456","bids":[],"asks":[],"timestamp":"1"}"#,
            br#"{"event_type":"tick_size_change","asset_id":"456","new_tick_size":"0.01","timestamp":"1"}"#,
            br#"{"event_type":"price_change","price_changes":[{"asset_id":"456","price":"0.5","size":"1","side":"BUY"}],"timestamp":"1"}"#,
            br#"{"event_type":"new_market","asset_id":"123","timestamp":"1"}"#,
            br#"{"unexpected":"shape"}"#,
        ];

        for frame in frames {
            assert!(
                decode_prediction_book_frame(frame, "123")
                    .expect("an unread or unrelated frame is not an error")
                    .is_none(),
                "a frame this build does not read for this asset publishes no update"
            );
        }
    }

    #[test]
    fn a_frame_without_a_timestamp_is_stamped_with_its_arrival() {
        let before = now_millis().expect("the clock is readable");
        let frame: &[u8] = br#"{"event_type":"last_trade_price","asset_id":"123","price":"0.5"}"#;
        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the frame decodes")
            .expect("the frame carries a trade");

        let PredictionOrderBookUpdate::LastTrade { timestamp, .. } = &updates[0] else {
            panic!("a last trade is published as a last-trade update");
        };
        assert!(*timestamp >= before);
    }

    /// A book update keeps the venue's instant exactly.
    ///
    /// A reader folds live updates into the REST snapshot it fetched by comparing
    /// instants, so a frame buffered while the snapshot was in flight must not look
    /// newer than the snapshot it belongs with. The series decoder raises a stated
    /// instant on purpose; the book decoder must not.
    #[test]
    fn a_book_frame_keeps_the_instant_the_venue_stated() {
        let read_at = now_millis().expect("the clock is readable");
        let frames: [&[u8]; 4] = [
            br#"{"event_type":"book","asset_id":"123","bids":[],"asks":[],"timestamp":"100"}"#,
            br#"{"event_type":"price_change","price_changes":[{"asset_id":"123","price":"0.4","size":"1","side":"BUY"}],"timestamp":"100"}"#,
            br#"{"event_type":"tick_size_change","asset_id":"123","new_tick_size":"0.001","timestamp":"100"}"#,
            br#"{"event_type":"last_trade_price","asset_id":"123","price":"0.5","timestamp":"100"}"#,
        ];

        for frame in frames {
            let updates = decode_prediction_book_frame(frame, "123")
                .expect("the frame decodes")
                .expect("the frame carries an update");
            for update in &updates {
                let timestamp = match update {
                    PredictionOrderBookUpdate::Replace { timestamp, .. }
                    | PredictionOrderBookUpdate::Levels { timestamp, .. }
                    | PredictionOrderBookUpdate::TickSize { timestamp, .. }
                    | PredictionOrderBookUpdate::LastTrade { timestamp, .. } => *timestamp,
                };
                assert_eq!(
                    timestamp, 100,
                    "a stated book instant is published as it was stated: {update:?}"
                );
                assert!(
                    timestamp < read_at,
                    "the stated instant is not raised to the arrival time"
                );
            }
        }
    }

    #[test]
    fn a_batch_keeps_its_frame_order() {
        let frame: &[u8] = br#"[
            {"event_type":"last_trade_price","asset_id":"123","price":"0.5","timestamp":"1"},
            {"event_type":"tick_size_change","asset_id":"123","new_tick_size":"0.01","timestamp":"2"},
            {"event_type":"price_change","price_changes":[{"asset_id":"123","price":"0.4","size":"1","side":"BUY"}],"timestamp":"3"}
        ]"#;
        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the frame decodes")
            .expect("the batch carries updates");

        assert_eq!(updates.len(), 3);
        assert!(matches!(
            updates[0],
            PredictionOrderBookUpdate::LastTrade { .. }
        ));
        assert!(matches!(
            updates[1],
            PredictionOrderBookUpdate::TickSize { .. }
        ));
        assert!(matches!(
            updates[2],
            PredictionOrderBookUpdate::Levels { .. }
        ));
    }

    #[test]
    fn one_price_change_frame_moves_both_the_series_and_the_book() {
        let (_, legs, _) = prediction_market();
        let mapping: Vec<(SmolStr, OutcomeLegRef)> = legs
            .iter()
            .map(|leg| (SmolStr::new(asset_id(leg)), leg.leg.clone()))
            .collect();
        let frame: &[u8] = br#"{
            "event_type":"price_change","timestamp":"2000",
            "price_changes":[{"asset_id":"123","price":"0.4","size":"15","side":"BUY","best_bid":"0.2","best_ask":"0.4"}]
        }"#;

        let series = decode_market_frame(frame, &mapping)
            .expect("the series still reads the frame")
            .expect("the frame carries a midpoint");
        assert_eq!(
            series[0].points[0].probability,
            "0.3".parse().expect("the midpoint is a fraction")
        );

        let updates = decode_prediction_book_frame(frame, "123")
            .expect("the book reads the frame")
            .expect("the frame carries a level");
        let PredictionOrderBookUpdate::Levels { bids, .. } = &updates[0] else {
            panic!("the change is published as a level change");
        };
        assert_eq!(bids[0].price.to_string(), "0.4");
        assert_eq!(bids[0].size.to_string(), "15");
    }

    #[test]
    fn a_valid_query_names_the_asset_of_the_leg_it_is_for() {
        let (event, legs, _) = prediction_market();
        for leg in &legs {
            let query = PredictionOrderBookQuery {
                event: event.clone(),
                legs: legs.clone(),
                leg: leg.leg.clone(),
            };
            assert_eq!(
                prediction_order_book_asset(&query).expect("the query is valid"),
                asset_id(leg)
            );
        }
    }

    #[test]
    fn a_query_without_legs_is_refused() {
        let (event, _, requested) = prediction_market();
        let query = PredictionOrderBookQuery {
            event,
            legs: Vec::new(),
            leg: requested,
        };
        let error =
            prediction_order_book_asset(&query).expect_err("a query without legs is invalid");

        assert!(matches!(
            error,
            MarketParticipantError::MarketModel(boxed)
                if matches!(*boxed, ModelError::EmptyCollection { field: "legs" })
        ));
    }

    #[test]
    fn a_query_whose_leg_is_not_one_of_the_markets_legs_is_refused() {
        let (event, legs, _) = prediction_market();
        let requested = legs[1].leg.clone();
        let query = PredictionOrderBookQuery {
            event,
            legs: vec![legs[0].clone()],
            leg: requested,
        };
        let error = prediction_order_book_asset(&query)
            .expect_err("a leg outside the market's legs has no book");

        assert!(matches!(
            error,
            MarketParticipantError::MarketModel(boxed)
                if matches!(*boxed, ModelError::InvalidField { field: "leg", .. })
        ));
    }

    #[test]
    fn a_query_for_a_leg_of_another_native_kind_is_refused() {
        let (event, legs, _) = prediction_market();
        let mut foreign = legs[0].clone();
        foreign.native = PredictionNativeLeg::HyperliquidOutcome {
            outcome: HyperliquidOutcomeId(7),
            side: foreign.leg.side,
            encoding: 0,
            coin: SmolStr::new_static("outcome"),
            asset_id: HyperliquidAssetId(1),
        };
        let query = PredictionOrderBookQuery {
            event: event.clone(),
            legs: vec![foreign.clone()],
            leg: foreign.leg,
        };
        let error =
            prediction_order_book_asset(&query).expect_err("the CLOB publishes no such book");

        assert_eq!(
            error,
            MarketParticipantError::UnsupportedCapability {
                route: event.route,
                capability: CapabilityId::PredictionOrderBook,
            }
        );
    }

    #[test]
    fn the_series_and_the_book_of_one_market_subscribe_with_the_same_bytes() {
        let (event, legs, requested) = prediction_market();
        let series = PredictionSeriesSubscriptionQuery {
            event: event.clone(),
            legs: legs.clone(),
        };
        let book = PredictionOrderBookQuery {
            event,
            legs,
            leg: requested,
        };

        let (series_subscribe, series_unsubscribe) =
            market_channel_payloads(&series.legs).expect("the series subscription encodes");
        let (book_subscribe, book_unsubscribe) =
            market_channel_payloads(&book.legs).expect("the book subscription encodes");

        assert_eq!(
            (&series_subscribe, &series_unsubscribe),
            (&book_subscribe, &book_unsubscribe),
            "one market's series and book share the same topic"
        );
        assert_eq!(
            std::str::from_utf8(&book_subscribe).expect("the payload is UTF-8"),
            r#"{"assets_ids":["123","456"],"type":"market","custom_feature_enabled":true}"#
        );
        assert_eq!(
            std::str::from_utf8(&book_unsubscribe).expect("the payload is UTF-8"),
            r#"{"assets_ids":["123","456"],"operation":"unsubscribe"}"#
        );
    }
}
