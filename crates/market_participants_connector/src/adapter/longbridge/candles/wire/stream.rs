//! What this build sends over the quote socket for candles, and what it makes of the
//! frames that come back.
//!
//! Three requests leave here. A **history fetch** (cmd 27) pages the venue's candles in one
//! of the four shapes §4.5 states, with both anchors written as the market's own wall time.
//! An **opening screen** (cmd 19) reads the candles a subscription starts from. A
//! **subscription** (cmd 6) registers the symbol, and its unsubscription frame (cmd 7) is
//! what closes it. A fourth, the **trading days** of one market (cmd 9), is read lazily by
//! the stream and is where a half day comes from.
//!
//! What comes back is not candles: the venue pushes quotes and trades, and the aggregation
//! is what turns them into candles. The stream below is the one reader of both — the screen
//! first, then the pushes — and it is also what recognises the answer to its own subscribe
//! frame, which the connection layer does not treat as a pending request and so never
//! resolves.

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

/// The operation a candle or a push field that cannot be decoded is reported as.
const DECODE_CANDLE: &str = "decode Longbridge candle";

/// How long one trading-day table (cmd 9) may take.
const TRADING_DAYS_TIMEOUT: Duration = Duration::from_secs(10);

/// The days before today the venue's trading-day table is asked about (§10.8).
const TRADING_DAYS_BEFORE: i64 = 5;
/// The days after today it is asked about.
const TRADING_DAYS_AFTER: i64 = 30;

/// Formats one instant as the market's own `("YYYYMMDD", "HHMM")` (§4.5).
///
/// The venue's anchor fields are wall time with no zone attached, so the conversion has
/// to happen here: an instant formatted in UTC would anchor a US session's page at the
/// wrong candle for every hour the two clocks disagree, which is most of the trading day
/// and all of it across a daylight-saving change.
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

/// Fetches one market's historical candles (cmd 27).
///
/// Every one of §4.5's four shapes is an offset query: the enumeration's own
/// `QUERY_BY_DATE` is deliberately not used, because a date query carries no `count` and
/// so could not be bounded by the request's `limit`. Both anchors are the market's local
/// wall time, and the answer is sorted ascending whatever order the venue sent it in.
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
        // The "both anchors" shape pages backwards from `end`, so the venue answers with
        // candles the caller excluded; the drop happens here rather than at the venue,
        // which has no way to express "backwards from here, but not before there".
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

/// Builds the history request of one fetch query (§4.5).
///
/// The four shapes are the caller's own: `start` alone pages forward from it, `end` alone
/// pages backward from it, neither pages backward from the venue's newest candle with no
/// anchor at all, and both page backward from `end` and are filtered by the caller. Every
/// one states `count = limit` and asks for every session.
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
        // A date query carries no `count`, which is why this build never sends one.
        date_request: None,
        trade_session: codec::TRADE_SESSION_ALL,
    }
}

/// Subscribes to one market's candles (cmd 6, with the opening screen of cmd 19).
///
/// The opening screen is awaited before the subscription frame is registered. That order
/// is §10.8's and it costs one round trip: `subscribe_with_id` sends the frame as soon as
/// the lease exists, so a screen read afterwards would race the pushes it is meant to
/// precede, and the frames that arrived while it was being read would spill out of the
/// connection's broadcast channel.
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

/// The topic one subscription's own frames are shared under.
///
/// The connection shares one subscribe frame between every subscriber of a topic, and the
/// venue's answer to that frame echoes the id of the frame that carried it, so the topic
/// states every term the frame states. The adjustment is one of them even though the wire
/// does not carry it: two readers of the same symbol and interval under different
/// conventions are two different frames, and a topic that merged them would leave the
/// second reader waiting for an answer that names the first reader's id — a refused
/// subscription that never ends its stream with the venue's own words (§12.4).
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

/// Reads one subscription's opening screen (cmd 19).
///
/// The count is the venue's own 1,000 and the session field asks for every session, which
/// is what the official client's subscription path sends. The adjustment convention is the
/// request's own — the one deliberate difference from that path, whose screen is always
/// unadjusted — because a screen of unadjusted prices under an adjusted subscription would
/// be two price bases in one chart (§10.8).
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

/// Reads the trading days of one market (cmd 9), which is where a half day comes from.
///
/// The command is not behind the session's market-data gate: the calendar of a market is
/// not its market data, and a reader that knows which days are half days can say why an
/// afternoon is short without being allowed to read a single candle. It **is** paced by
/// the session's own quota for the command, because the venue publishes one: the measured
/// profile states `{command: 9, limit: 10, burst: 5}` alongside the twenty-three other
/// commands that share that pace (§14.1 item 7). A reader per subscription means one read
/// per subscription, and a hundred of them in one instant is a refusal — which this path
/// treats as an ordinary day (§4.6), so an unpaced read is a silently mis-gridded
/// afternoon rather than a visible failure. The token is therefore taken without the gate
/// ([`LongbridgeSession::wait_for_quota`]), which is the one thing this command needs
/// that `request` would also apply the gate for.
///
/// The answer is decoded here rather than by `request` for one reason: a refusal is a
/// calendar this build does not have, and the caller's fail-open path is what a refusal
/// has to reach. `request` would answer the same refusal as an error, so the two are the
/// same answer to the caller; what differs is that this path reads the command's own
/// status and body rather than the generic decoder's.
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

/// The window one trading-day read asks about, as its two `YYYYMMDD` bounds (§10.8).
///
/// The venue's own client asks about a window around today rather than about one date,
/// because a half day is a property of the calendar: the answer covers the days a reader
/// could page back into and the days the next session opens on.
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

/// Formats one market-local date as the venue's own `YYYYMMDD` (§4.5).
pub(super) fn day_text(date: jiff::civil::Date) -> SmolStr {
    format_smolstr!("{:04}{:02}{:02}", date.year(), date.month(), date.day())
}

/// The zone the venue opens its candles in (§4.8).
///
/// The database ships with `jiff`, so this resolves on a machine that has none of its
/// own; UTC is the type's fallback and is not this venue's market, which is why every
/// assertion about a boundary is made against `America/New_York` in the tests.
fn zone_of() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(MARKET_ZONE).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// The venue's period for one request's interval.
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

/// Reads one venue candle as the model's own.
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

/// Reads one wire timestamp as milliseconds.
///
/// The venue's candle, quote and trade timestamps are Unix **seconds**, which is what the
/// official SDK's own `OffsetDateTime::from_unix_timestamp` says they are. A seconds value
/// read as milliseconds would place every candle in 1970.
pub(crate) fn millis_of(seconds: i64) -> Result<UnixMillis, MarketParticipantError> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(DECODE_CANDLE),
            reason: format_smolstr!("a timestamp of {seconds} seconds overflows milliseconds"),
            raw_data: None,
        })
}

/// Everything one subscription's stream is built from.
///
/// The values are one declaration rather than eight arguments, because they are read
/// together in exactly one place and a caller that named them positionally would be one
/// transposition away from a stream of another symbol's candles.
struct CandlePlan {
    connection: Arc<Connection>,
    /// The session this subscription's connection was read as.
    ///
    /// The trading-day read (cmd 9) is a command of the venue's own rate-limit table, so
    /// the stream takes its token from the session the profile was read into rather than
    /// sending the frame unpaced.
    session: Arc<crate::adapter::longbridge::auth::LongbridgeSession>,
    listing: VenueListingKey,
    symbol: SmolStr,
    /// The request id the subscribe frame carried, which its answer echoes.
    request_id: u32,
    timeframe: TimeFrame,
    screen: Vec<NativeListing<NativeCandlestick>>,
    cancellation: CancellationToken,
}

/// One subscription's stream, from its opening screen to its last frame.
///
/// The screen is answered first and in full, because it is the venue's own history and a
/// reader that saw a push before it would be looking at a candle with no past. Its last
/// candle is what the aggregator is built from, so the first push after it updates a
/// candle the venue published rather than opening a second one beside it.
struct CandleStream {
    /// The opening screen's candles, which are answered before any push is read.
    screen: VecDeque<NativeListing<NativeCandlestick>>,
    /// The candles one push produced that the reader has not seen yet.
    pending: VecDeque<ProtocolEvent>,
    /// The connection's frames.
    source: futures::stream::BoxStream<'static, Result<Bytes, MarketParticipantError>>,
    cancellation: CancellationToken,
    /// The session the half-day table is read through, and the quota it is paced by.
    session: Arc<crate::adapter::longbridge::auth::LongbridgeSession>,
    listing: VenueListingKey,
    symbol: SmolStr,
    request_id: u32,
    /// The half-day table and the zone the pushes are bucketed in.
    pushes: PushState,
    /// The candle the opening screen left off at, built when the screen is adopted.
    aggregator: Option<CandleAggregator>,
}

impl CandleStream {
    /// Opens the stream of one subscription.
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
        // The candle the pushes belong to is the screen's last one, and it is adopted
        // here rather than as the screen is drained: a push already waiting behind the
        // screen must merge into the candle the venue published, and draining the screen
        // happens one frame at a time.
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

    /// Reads frames until the stream has one event to answer with.
    ///
    /// `None` ends the stream: either its cancellation was requested or the connection
    /// stopped publishing. A frame this subscription cannot use produces no event and the
    /// loop reads the next one, which is what "one connection publishes every
    /// subscription's frames" means for a reader.
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
                // A connection that stopped publishing ends its streams with why.
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            };
            if let Err(error) = self.dispatch(&payload).await {
                return Some(Err(error));
            }
        }
    }

    /// Reads one frame, appending the events it produced.
    async fn dispatch(&mut self, payload: &[u8]) -> Result<(), MarketParticipantError> {
        match decode(payload)? {
            // The answer to this subscription's own subscribe frame. It is not a pending
            // request of the connection — `subscribe_with_id` only sends the frame — so
            // the stream is what recognises it, and a refusal is what ends the stream
            // with the venue's own words.
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
            // Every other answer is another request's, and this subscription is not its
            // reader.
            Frame::RequestAnswer { .. } | Frame::Other => Ok(()),
            Frame::Quote(body) => self.quote(&body).await,
            Frame::Trades(body) => self.trades(&body).await,
        }
    }

    /// Merges one quote push.
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

    /// Merges one trade push, which is a batch of prints.
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

    /// Refreshes the half-day table when it is due, then merges one push's prints.
    ///
    /// The table is read against the push's own market day rather than the machine's
    /// clock, so a reader in another zone asks the venue about the session it is actually
    /// subscribed to.
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
            // A push that arrived before the opening screen left a candle behind has no
            // candle to merge into. The screen always arrives first, so this is the venue
            // answering out of order and the frame is dropped rather than guessed at.
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
                // A late push belongs to a candle this subscription is no longer building.
                CandleOutcome::Stale => {}
            }
        }
        Ok(())
    }

    /// Reads the venue's trading days and installs the half days they state.
    ///
    /// A read that failed installs what it found — nothing — because the deadline it
    /// records is the other half of what a read produces: the venue is asked once a day
    /// whether or not it answered, and a calendar that could not be read this minute is
    /// not worth a request on every push for the rest of the day.
    async fn refresh_half_days(&mut self, timestamp: UnixMillis) {
        let Some(today) = self.pushes.market_day(timestamp) else {
            return;
        };
        let (beg, end) = trading_day_window(today);
        match trading_days(&self.session, &day_text(beg), &day_text(end)).await {
            Ok(days) => self
                .pushes
                .install_calendar(&days.half_trade_day, Instant::now()),
            // Fail open: the ordinary table is what an unreadable calendar means, and a
            // reader sees candles of an ordinary day rather than no candles at all.
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

/// One subscription's stream of events.
fn candle_stream(
    state: CandleStream,
) -> impl Stream<Item = Result<ProtocolEvent, MarketParticipantError>> + Send {
    futures::stream::unfold(state, |mut state| async move {
        state.next_event().await.map(|event| (event, state))
    })
}
