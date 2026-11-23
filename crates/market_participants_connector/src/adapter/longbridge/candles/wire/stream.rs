use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use longbridge_proto::quote::{
    Candlestick, MarketTradeDayRequest, MarketTradeDayResponse, PushQuote, PushTrade,
    SecurityCandlestickRequest, SecurityCandlestickResponse, SecurityHistoryCandlestickRequest,
    SubscribeRequest, UnsubscribeRequest, security_history_candlestick_request::OffsetQuery,
};
use market_participants_model::{
    decimal::UnixMillis,
    listing::route::{VenueListingKey, VenueRoute},
};
use prost::Message as _;
use smol_str::{SmolStr, format_smolstr};
use tokio_util::sync::CancellationToken;

use crate::{
    adapter::{
        ExecutionContext, NativeCandlestick, NativeListing, ProtocolEvent, ProtocolResponse,
        SubscriptionHandle,
        longbridge::{
            candles::{
                aggregator::{CandleAggregator, CandleOutcome, CandlePrint},
                push_state::PushState,
                session_table::MARKET_ZONE,
            },
            codec,
            symbols::market_symbol,
            wire::{Frame, decode, decode_failed, price_of, quantity_of, refusal, wire_id},
        },
    },
    catalog::resolved::ResolvedListingRef,
    connection::{Connection, SubscriptionTopic},
    error::MarketParticipantError,
    market::timeframe::{Adjustment, TimeFrame},
    market_participants::model::{CandlestickFetchQuery, CandlestickSubscriptionQuery},
};

const DECODE_CANDLE: &str = "decode Longbridge candle";

const TRADING_DAYS_TIMEOUT: Duration = Duration::from_secs(10);

const TRADING_DAYS_BEFORE: i64 = 5;
const TRADING_DAYS_AFTER: i64 = 30;

pub(crate) fn market_anchor(millis: UnixMillis) -> (SmolStr, SmolStr) {
    let Ok(timestamp) = jiff::Timestamp::from_millisecond(millis) else {
        return (SmolStr::new_static(""), SmolStr::new_static(""));
    };
    let Ok(zone) = jiff::tz::TimeZone::get(MARKET_ZONE) else {
        return (SmolStr::new_static(""), SmolStr::new_static(""));
    };
    let local = jiff::Zoned::new(timestamp, zone).datetime();
    (
        format_smolstr!("{:04}{:02}{:02}", local.year(), local.month(), local.day()),
        format_smolstr!("{:02}{:02}", local.hour(), local.minute()),
    )
}

pub(crate) async fn fetch_candlesticks(
    context: &ExecutionContext,
    session: &crate::adapter::longbridge::auth::LongbridgeSession,
    listing: &ResolvedListingRef,
    query: &CandlestickFetchQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let symbol = market_symbol(listing)?;
    let period = period_of(&context.route(), query.timeframe)?;
    let response: SecurityCandlestickResponse = session
        .request(
            &context.route(),
            codec::GET_SECURITY_HISTORY_CANDLESTICKS,
            &history_request(
                &symbol,
                period,
                query.adjustment,
                (query.start_timestamp, query.end_timestamp, query.limit),
            ),
        )
        .await?;

    let mut candles = Vec::with_capacity(response.candlesticks.len());
    for candle in &response.candlesticks {
        let timestamp = millis_of(candle.timestamp)?;
        if let Some(start) = query.start_timestamp
            && timestamp < start
        {
            continue;
        }
        candles.push(NativeListing::new(
            listing.key.clone(),
            native_candle(candle, timestamp)?,
        ));
    }
    candles.sort_unstable_by_key(|candle| candle.data.timestamp);
    Ok(ProtocolResponse::Candlesticks(candles))
}

fn history_request(
    symbol: &SmolStr,
    period: codec::LongbridgePeriod,
    adjustment: Adjustment,
    queries: (Option<UnixMillis>, Option<UnixMillis>, u16),
) -> SecurityHistoryCandlestickRequest {
    let (start, end, limit) = queries;
    let (direction, anchor) = match (start, end) {
        (Some(start), None) => (codec::DIRECTION_FORWARD, Some(start)),
        (None, Some(end)) => (codec::DIRECTION_BACKWARD, Some(end)),
        (None, None) => (codec::DIRECTION_BACKWARD, None),
        (Some(_), Some(end)) => (codec::DIRECTION_BACKWARD, Some(end)),
    };
    let (date, minute) = anchor.map_or_else(
        || (SmolStr::new_static(""), SmolStr::new_static("")),
        market_anchor,
    );
    SecurityHistoryCandlestickRequest {
        symbol: symbol.as_str().to_owned(),
        period: period.number(),
        adjust_type: codec::LongbridgeAdjust::of(adjustment).number(),
        query_type: codec::QUERY_BY_OFFSET,
        offset_request: Some(OffsetQuery {
            direction,
            date: date.as_str().to_owned(),
            minute: minute.as_str().to_owned(),
            count: i32::from(limit),
        }),
        date_request: None,
        trade_session: codec::TRADE_SESSION_ALL,
    }
}

pub(crate) async fn subscribe_candlesticks(
    context: &ExecutionContext,
    session: &Arc<crate::adapter::longbridge::auth::LongbridgeSession>,
    listing: &ResolvedListingRef,
    query: &CandlestickSubscriptionQuery,
) -> Result<SubscriptionHandle, MarketParticipantError> {
    let symbol = market_symbol(listing)?;
    let period = period_of(&context.route(), query.timeframe)?;
    let screen = first_screen(context, session, listing, &symbol, period, query.adjustment).await?;

    let connection = session.connection();
    let request_id = connection.allocate_request_id();
    let wire_request_id = wire_id(request_id)?;
    let topic = SubscriptionTopic::from(Bytes::copy_from_slice(
        topic_text(&symbol, period, query.adjustment).as_bytes(),
    ));
    let payload = codec::encode_request(
        codec::SUBSCRIBE,
        wire_request_id,
        crate::adapter::longbridge::auth::REQUEST_TIMEOUT_MILLIS,
        &SubscribeRequest {
            symbol: vec![symbol.as_str().to_owned()],
            sub_type: vec![codec::SUB_TYPE_QUOTE, codec::SUB_TYPE_TRADE],
            is_first_push: true,
        }
        .encode_to_vec(),
    )?;
    let unsubscribe_payload = codec::encode_request(
        codec::UNSUBSCRIBE,
        wire_id(connection.allocate_request_id())?,
        crate::adapter::longbridge::auth::REQUEST_TIMEOUT_MILLIS,
        &UnsubscribeRequest {
            symbol: vec![symbol.as_str().to_owned()],
            sub_type: vec![codec::SUB_TYPE_QUOTE, codec::SUB_TYPE_TRADE],
            unsub_all: false,
        }
        .encode_to_vec(),
    )?;
    let lease = connection.subscribe_with_id(request_id, topic, payload, unsubscribe_payload)?;

    let cancellation = CancellationToken::new();
    let state = CandleStream::new(CandlePlan {
        connection: Arc::clone(connection),
        session: Arc::clone(session),
        listing: listing.key.clone(),
        symbol,
        request_id: wire_request_id,
        timeframe: query.timeframe,
        screen,
        cancellation: cancellation.clone(),
    });
    Ok(SubscriptionHandle::with_connection_subscription(
        Box::pin(candle_stream(state)),
        cancellation,
        lease,
    ))
}

pub(super) fn topic_text(
    symbol: &SmolStr,
    period: codec::LongbridgePeriod,
    adjustment: Adjustment,
) -> SmolStr {
    format_smolstr!(
        "{}:{}:{}",
        symbol.as_str(),
        period.number(),
        adjustment.label()
    )
}

async fn first_screen(
    context: &ExecutionContext,
    session: &crate::adapter::longbridge::auth::LongbridgeSession,
    listing: &ResolvedListingRef,
    symbol: &SmolStr,
    period: codec::LongbridgePeriod,
    adjustment: Adjustment,
) -> Result<Vec<NativeListing<NativeCandlestick>>, MarketParticipantError> {
    let response: SecurityCandlestickResponse = session
        .request(
            &context.route(),
            codec::GET_SECURITY_CANDLESTICKS,
            &SecurityCandlestickRequest {
                symbol: symbol.as_str().to_owned(),
                period: period.number(),
                count: codec::FIRST_SCREEN_COUNT,
                adjust_type: codec::LongbridgeAdjust::of(adjustment).number(),
                trade_session: codec::TRADE_SESSION_ALL,
            },
        )
        .await?;

    let mut candles = Vec::with_capacity(response.candlesticks.len());
    for candle in &response.candlesticks {
        candles.push(NativeListing::new(
            listing.key.clone(),
            native_candle(candle, millis_of(candle.timestamp)?)?,
        ));
    }
    candles.sort_unstable_by_key(|candle| candle.data.timestamp);
    Ok(candles)
}

pub(super) async fn trading_days(
    session: &crate::adapter::longbridge::auth::LongbridgeSession,
    beg_day: &SmolStr,
    end_day: &SmolStr,
) -> Result<MarketTradeDayResponse, MarketParticipantError> {
    session.wait_for_quota(codec::GET_TRADING_DAYS).await;
    let connection = session.connection();
    let request_id = connection.allocate_request_id();
    let payload = codec::encode_request(
        codec::GET_TRADING_DAYS,
        wire_id(request_id)?,
        crate::adapter::longbridge::auth::REQUEST_TIMEOUT_MILLIS,
        &MarketTradeDayRequest {
            market: "US".to_owned(),
            beg_day: beg_day.as_str().to_owned(),
            end_day: end_day.as_str().to_owned(),
        }
        .encode_to_vec(),
    )?;
    let answer = tokio::time::timeout(
        TRADING_DAYS_TIMEOUT,
        connection.send_and_wait_with_id(
            request_id,
            payload,
            crate::adapter::longbridge::auth::REQUEST_TIMEOUT,
        ),
    )
    .await
    .map_err(|_| MarketParticipantError::Timeout {
        operation: SmolStr::new_static("read the Longbridge trading days"),
        timeout_ms: TRADING_DAYS_TIMEOUT
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    })??;
    match decode(&answer)? {
        Frame::RequestAnswer {
            command_code: codec::GET_TRADING_DAYS,
            status,
            body,
            ..
        } => {
            if let Some(refusal) = refusal(status, &body)? {
                return Err(refusal);
            }
            MarketTradeDayResponse::decode(body.as_ref()).map_err(|_| {
                MarketParticipantError::DecodeFailed {
                    operation: SmolStr::new_static("decode the Longbridge trading days"),
                    raw_data: body,
                }
            })
        }
        _ => Err(MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static("decode the Longbridge trading days"),
            reason: SmolStr::new_static("the socket answered another command to this request"),
            raw_data: None,
        }),
    }
}

pub(super) fn trading_day_window(
    today: jiff::civil::Date,
) -> (jiff::civil::Date, jiff::civil::Date) {
    let beg = today
        .checked_sub(jiff::Span::new().days(TRADING_DAYS_BEFORE))
        .unwrap_or(today);
    let end = today
        .checked_add(jiff::Span::new().days(TRADING_DAYS_AFTER))
        .unwrap_or(today);
    (beg, end)
}

pub(super) fn day_text(date: jiff::civil::Date) -> SmolStr {
    format_smolstr!("{:04}{:02}{:02}", date.year(), date.month(), date.day())
}

fn zone_of() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(MARKET_ZONE).unwrap_or(jiff::tz::TimeZone::UTC)
}

fn period_of(
    route: &VenueRoute,
    timeframe: TimeFrame,
) -> Result<codec::LongbridgePeriod, MarketParticipantError> {
    codec::LongbridgePeriod::of(timeframe).ok_or_else(|| {
        MarketParticipantError::UnsupportedTimeFrame {
            route: route.clone(),
            timeframe: SmolStr::new(timeframe.label()),
        }
    })
}

fn native_candle(
    candle: &Candlestick,
    timestamp: UnixMillis,
) -> Result<NativeCandlestick, MarketParticipantError> {
    Ok(NativeCandlestick {
        timestamp,
        open: price_of(DECODE_CANDLE, &candle.open)?,
        high: price_of(DECODE_CANDLE, &candle.high)?,
        low: price_of(DECODE_CANDLE, &candle.low)?,
        close: price_of(DECODE_CANDLE, &candle.close)?,
        volume: quantity_of(DECODE_CANDLE, candle.volume)?,
    })
}

pub(crate) fn millis_of(seconds: i64) -> Result<UnixMillis, MarketParticipantError> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(DECODE_CANDLE),
            reason: format_smolstr!("a timestamp of {seconds} seconds overflows milliseconds"),
            raw_data: None,
        })
}

struct CandlePlan {
    connection: Arc<Connection>,
    session: Arc<crate::adapter::longbridge::auth::LongbridgeSession>,
    listing: VenueListingKey,
    symbol: SmolStr,
    request_id: u32,
    timeframe: TimeFrame,
    screen: Vec<NativeListing<NativeCandlestick>>,
    cancellation: CancellationToken,
}

struct CandleStream {
    screen: VecDeque<NativeListing<NativeCandlestick>>,
    pending: VecDeque<ProtocolEvent>,
    source: futures::stream::BoxStream<'static, Result<Bytes, MarketParticipantError>>,
    cancellation: CancellationToken,
    session: Arc<crate::adapter::longbridge::auth::LongbridgeSession>,
    listing: VenueListingKey,
    symbol: SmolStr,
    request_id: u32,
    pushes: PushState,
    aggregator: Option<CandleAggregator>,
}

impl CandleStream {
    fn new(plan: CandlePlan) -> Self {
        let CandlePlan {
            connection,
            session,
            listing,
            symbol,
            request_id,
            timeframe,
            screen,
            cancellation,
        } = plan;
        let aggregator = screen.last().map(|last| {
            CandleAggregator::from_first_screen(timeframe, zone_of(), last.data.clone())
        });
        Self {
            source: Box::pin(connection.event_stream()),
            screen: screen.into(),
            pending: VecDeque::new(),
            cancellation,
            session,
            listing,
            symbol,
            request_id,
            pushes: PushState::new(timeframe),
            aggregator,
        }
    }

    async fn next_event(&mut self) -> Option<Result<ProtocolEvent, MarketParticipantError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if let Some(candle) = self.screen.pop_front() {
                return Some(Ok(ProtocolEvent::Candlestick(candle)));
            }
            let next = self.source.next();
            let payload = tokio::select! {
                () = self.cancellation.cancelled() => return None,
                payload = next => payload,
            };
            let payload = match payload {
                Some(Ok(payload)) => payload,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            };
            if let Err(error) = self.dispatch(&payload).await {
                return Some(Err(error));
            }
        }
    }

    async fn dispatch(&mut self, payload: &[u8]) -> Result<(), MarketParticipantError> {
        match decode(payload)? {
            Frame::RequestAnswer {
                command_code: codec::SUBSCRIBE,
                request_id,
                status,
                body,
            } if request_id == self.request_id => {
                if let Some(refusal) = refusal(status, &body)? {
                    return Err(refusal);
                }
                Ok(())
            }
            Frame::RequestAnswer { .. } | Frame::Other => Ok(()),
            Frame::Quote(body) => self.quote(&body).await,
            Frame::Trades(body) => self.trades(&body).await,
        }
    }

    async fn quote(&mut self, body: &Bytes) -> Result<(), MarketParticipantError> {
        let quote = PushQuote::decode(body.as_ref())
            .map_err(|_| decode_failed("decode the Longbridge quote push", body))?;
        if quote.symbol != self.symbol.as_str() {
            return Ok(());
        }
        let Some(print) = self.pushes.quote_print(&quote)? else {
            return Ok(());
        };
        self.merge(std::slice::from_ref(&print), print.timestamp)
            .await
    }

    async fn trades(&mut self, body: &Bytes) -> Result<(), MarketParticipantError> {
        let trades = PushTrade::decode(body.as_ref())
            .map_err(|_| decode_failed("decode the Longbridge trade push", body))?;
        if trades.symbol != self.symbol.as_str() {
            return Ok(());
        }
        let prints = self.pushes.trade_prints(&trades)?;
        let Some(first) = prints.first() else {
            return Ok(());
        };
        self.merge(&prints, first.timestamp).await
    }

    async fn merge(
        &mut self,
        prints: &[CandlePrint],
        timestamp: UnixMillis,
    ) -> Result<(), MarketParticipantError> {
        if self.pushes.half_days_due(Instant::now()) {
            self.refresh_half_days(timestamp).await;
        }
        let table = self.pushes.session_table(timestamp);
        let Some(aggregator) = self.aggregator.as_mut() else {
            return Ok(());
        };
        for print in prints {
            match aggregator.apply(print, table) {
                CandleOutcome::Updated(candle) => {
                    self.pending
                        .push_back(ProtocolEvent::Candlestick(NativeListing::new(
                            self.listing.clone(),
                            candle,
                        )));
                }
                CandleOutcome::Rolled { confirmed, opened } => {
                    self.pending
                        .push_back(ProtocolEvent::Candlestick(NativeListing::new(
                            self.listing.clone(),
                            confirmed,
                        )));
                    self.pending
                        .push_back(ProtocolEvent::Candlestick(NativeListing::new(
                            self.listing.clone(),
                            opened,
                        )));
                }
                CandleOutcome::Stale => {}
            }
        }
        Ok(())
    }

    async fn refresh_half_days(&mut self, timestamp: UnixMillis) {
        let Some(today) = self.pushes.market_day(timestamp) else {
            return;
        };
        let (beg, end) = trading_day_window(today);
        match trading_days(&self.session, &day_text(beg), &day_text(end)).await {
            Ok(days) => self
                .pushes
                .install_calendar(&days.half_trade_day, Instant::now()),
            Err(error) => {
                log::warn!(
                    "the Longbridge trading days could not be read, so every day is an ordinary \
                     one: {error}"
                );
                self.pushes.install_calendar(&[], Instant::now());
            }
        }
    }
}

fn candle_stream(
    state: CandleStream,
) -> impl Stream<Item = Result<ProtocolEvent, MarketParticipantError>> + Send {
    futures::stream::unfold(state, |mut state| async move {
        state.next_event().await.map(|event| (event, state))
    })
}
