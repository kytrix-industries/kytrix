//! What this build sends over the quote socket for tickers, and what it makes of the
//! frames that come back.
//!
//! Two requests leave here. A **pull** (cmd 11) asks the venue for the complete
//! real-time quote of one symbol, and its answer is the baseline every later frame is
//! merged into. A **subscription** (cmd 6) registers that symbol for pushes, and its
//! unsubscription frame (cmd 7) is what closes it; the subscription asks for no opening
//! push, because the baseline was already read.
//!
//! The pushes are incremental by the venue's own documentation — "only the fields that
//! changed are filled in" — and prost decodes an unset field into its default rather
//! than into an `Option`, so a frame that states one price and nothing else is
//! indistinguishable from one that states a zero open and a zero turnover. The merge is
//! therefore part of reading the wire rather than a convenience: without it a row would
//! be drawn from half a snapshot, and the change it shows would be measured against a
//! price nobody published.

use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use longbridge_proto::quote::{
    MultiSecurityRequest, PushQuote, SecurityQuote, SecurityQuoteResponse, SubscribeRequest,
    UnsubscribeRequest,
};
use market_participants_model::{
    decimal::{Price, Quantity},
    listing::route::VenueListingKey,
};
use prost::Message as _;
use smol_str::{SmolStr, format_smolstr};
use tokio_util::sync::CancellationToken;

use crate::{
    adapter::{
        ExecutionContext, NativeListing, NativeTicker, ProtocolEvent, ProtocolResponse,
        SubscriptionHandle,
        longbridge::{
            auth::{LongbridgeSession, REQUEST_TIMEOUT_MILLIS},
            codec,
            symbols::market_symbol,
            wire::{Frame, decode, decode_failed, price_of, quantity_of, refusal, wire_id},
        },
    },
    catalog::resolved::ResolvedListingRef,
    connection::SubscriptionTopic,
    error::MarketParticipantError,
};

/// The operation a quote field that cannot be decoded is reported as.
const DECODE_QUOTE: &str = "decode Longbridge quote";

/// The operation a quote push whose body cannot be decoded is reported as.
const DECODE_QUOTE_PUSH: &str = "decode the Longbridge quote push";

/// Fetches the real-time quote of one listing (cmd 11).
///
/// The answer is one whole snapshot: the venue's pull commands state every field of the
/// quote rather than only the ones that moved, which is what makes this the baseline a
/// subscription merges its pushes into.
pub(crate) async fn fetch_ticker(
    context: &ExecutionContext,
    session: &LongbridgeSession,
    listing: &ResolvedListingRef,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let symbol = market_symbol(listing)?;
    let native = read_quote(context, session, &symbol).await?;
    Ok(ProtocolResponse::Ticker(NativeListing::new(
        listing.key.clone(),
        native,
    )))
}

/// Reads one symbol's complete quote (cmd 11).
///
/// The session's own `request` is what sends it, so the permission gate and the
/// command's quota are applied by the one path that applies them to every other
/// market-data command; a caller that took the token itself would be a second reading of
/// the same two rules.
async fn read_quote(
    context: &ExecutionContext,
    session: &LongbridgeSession,
    symbol: &SmolStr,
) -> Result<NativeTicker, MarketParticipantError> {
    let answer: SecurityQuoteResponse = session
        .request(
            &context.route(),
            codec::QUERY_SECURITY_QUOTE,
            &MultiSecurityRequest {
                symbol: vec![symbol.as_str().to_owned()],
            },
        )
        .await?;

    // One request asks about one symbol, so an answer that carries none — or carries more
    // than one — is not a quote of this listing but a different answer than the one the
    // request asked for. Neither is emptiness to draw: a row built from it would show a
    // price that belongs to nothing.
    let quote = match answer.secu_quote.as_slice() {
        [quote] => quote,
        [] => {
            return Err(quote_protocol_error(format_smolstr!(
                "the answer carries no quote for {symbol}"
            )));
        }
        quotes => {
            return Err(quote_protocol_error(format_smolstr!(
                "the answer carries {} quotes for {symbol}, not one",
                quotes.len()
            )));
        }
    };
    if quote.symbol != symbol.as_str() {
        return Err(quote_protocol_error(format_smolstr!(
            "the answer is about {} rather than about {symbol}",
            quote.symbol
        )));
    }
    native_quote(quote)
}

/// Subscribes to one listing's real-time quote (cmd 6, with the baseline of cmd 11).
///
/// The baseline is read before the subscribe frame is registered, and it is read
/// unconditionally: the venue's pushes state only the fields that changed, so a stream
/// that waited for a complete push would wait for a frame the venue never promised, and
/// the subscription's first event would depend on the market happening to be busy. A
/// baseline that cannot be read fails the whole subscription, which is a failure a caller
/// can retry rather than a stream that stays silent.
pub(crate) async fn subscribe_ticker(
    context: &ExecutionContext,
    session: &Arc<LongbridgeSession>,
    listing: &ResolvedListingRef,
) -> Result<SubscriptionHandle, MarketParticipantError> {
    let symbol = market_symbol(listing)?;
    // The subscription's own permission gate: the subscribe frame is answered by the
    // connection's actor rather than by a pending request, so `request` is not the path
    // it takes and the gate has to be applied here.
    session.require_market_data(&context.route(), codec::SUBSCRIBE)?;
    let baseline = read_quote(context, session, &symbol).await?;

    session.wait_for_quota(codec::SUBSCRIBE).await;
    let connection = session.connection();
    let request_id = connection.allocate_request_id();
    let wire_request_id = wire_id(request_id)?;
    // The topic states the class of frames as well as the symbol: a candlestick
    // subscription's topic is `<symbol>:<period>:<adjustment>`, so a bare symbol would be
    // the one spelling that could collide with a candle topic, and the two subscriptions
    // would then share one lease and one reference count.
    let topic = SubscriptionTopic::from(Bytes::copy_from_slice(
        format_smolstr!("quote:{symbol}").as_bytes(),
    ));
    let payload = codec::encode_request(
        codec::SUBSCRIBE,
        wire_request_id,
        REQUEST_TIMEOUT_MILLIS,
        &SubscribeRequest {
            symbol: vec![symbol.as_str().to_owned()],
            sub_type: vec![codec::SUB_TYPE_QUOTE],
            // The baseline was read above, so there is nothing for the venue to push
            // first: a subscription that asked for one would be asking for a frame whose
            // completeness nothing promises.
            is_first_push: false,
        }
        .encode_to_vec(),
    )?;
    let unsubscribe_payload = codec::encode_request(
        codec::UNSUBSCRIBE,
        wire_id(connection.allocate_request_id())?,
        REQUEST_TIMEOUT_MILLIS,
        &UnsubscribeRequest {
            symbol: vec![symbol.as_str().to_owned()],
            sub_type: vec![codec::SUB_TYPE_QUOTE],
            unsub_all: false,
        }
        .encode_to_vec(),
    )?;
    let lease = connection.subscribe_with_id(request_id, topic, payload, unsubscribe_payload)?;

    let cancellation = CancellationToken::new();
    let state = TickerWire {
        source: Box::pin(connection.event_stream()),
        cancellation: cancellation.clone(),
        listing: listing.key.clone(),
        symbol,
        request_id: wire_request_id,
        pending_baseline: Some(baseline.clone()),
        baseline,
    };
    Ok(SubscriptionHandle::with_connection_subscription(
        Box::pin(ticker_stream(state)),
        cancellation,
        lease,
    ))
}

/// One subscription's stream, from its baseline to its last frame.
///
/// The two ticker fields are one snapshot read twice: `pending_baseline` is the snapshot
/// the stream still owes its reader, and `baseline` is the snapshot every later push is
/// merged into. They start as the same value and part company at the first event — after
/// that the reader has seen the baseline, and the merge has begun to move past it.
struct TickerWire {
    /// This subscription's frame source: one connection publishes every subscription's
    /// frames to every subscription, so a frame that is not this one's is skipped.
    source: futures::stream::BoxStream<'static, Result<Bytes, MarketParticipantError>>,
    cancellation: CancellationToken,
    listing: VenueListingKey,
    symbol: SmolStr,
    /// The request id this subscription's own subscribe frame carried, which its answer
    /// echoes.
    request_id: u32,
    /// The snapshot the stream has not handed over yet.
    pending_baseline: Option<NativeTicker>,
    /// The snapshot every push merges into.
    baseline: NativeTicker,
}

impl TickerWire {
    /// Reads frames until the stream has one event to answer with.
    ///
    /// `None` ends the stream: either its cancellation was requested or the connection
    /// stopped publishing. A frame this subscription cannot use produces no event and the
    /// loop reads the next one, which is what "one connection publishes every
    /// subscription's frames" means for a reader.
    async fn next_event(&mut self) -> Option<Result<ProtocolEvent, MarketParticipantError>> {
        loop {
            // The baseline is owed before any frame is read, which is what makes the
            // snapshot the subscription already holds its first event rather than a push
            // the venue may or may not send.
            if let Some(baseline) = self.pending_baseline.take() {
                return Some(Ok(self.event(baseline)));
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
            match self.dispatch(&payload) {
                Ok(Some(event)) => return Some(Ok(event)),
                Ok(None) => {}
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// Wraps one snapshot as the event this subscription publishes.
    fn event(&self, ticker: NativeTicker) -> ProtocolEvent {
        ProtocolEvent::Ticker(NativeListing::new(self.listing.clone(), ticker))
    }

    /// Reads one frame, answering with the event it produced when it produced one.
    fn dispatch(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
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
                Ok(None)
            }
            // Every other answer is another request's, and this subscription is not its
            // reader. Trades and the pushes of commands this build does not read are not
            // this subscription's either: the quote command is the only push it asked
            // for.
            Frame::RequestAnswer { .. } | Frame::Other | Frame::Trades(_) => Ok(None),
            Frame::Quote(body) => self.quote(&body),
        }
    }

    /// Merges one quote push into the baseline.
    fn quote(&mut self, body: &Bytes) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
        let quote =
            PushQuote::decode(body.as_ref()).map_err(|_| decode_failed(DECODE_QUOTE_PUSH, body))?;
        if quote.symbol != self.symbol.as_str() {
            return Ok(None);
        }
        self.merge(&quote)?;
        Ok(Some(self.event(self.baseline.clone())))
    }

    /// Merges one push's fields into the snapshot the last event published.
    ///
    /// A frame states only what changed, and prost decodes what it did not state into the
    /// field's default, so "the venue did not send this" and "the venue sent a zero" are
    /// one value on the wire. The rule is therefore per field: a text field is the
    /// venue's answer only when it is not empty, and the volume — an integer, which has no
    /// empty spelling — is its answer only when it is not zero. Everything else keeps the
    /// value the previous snapshot published, which is the value that was true when the
    /// venue last stated it.
    fn merge(&mut self, quote: &PushQuote) -> Result<(), MarketParticipantError> {
        if !quote.last_done.is_empty() {
            self.baseline.close = price_of(DECODE_QUOTE, &quote.last_done)?;
        }
        if !quote.open.is_empty() {
            self.baseline.open = price_of(DECODE_QUOTE, &quote.open)?;
        }
        if !quote.high.is_empty() {
            self.baseline.high = optional_price(&quote.high)?;
        }
        if !quote.low.is_empty() {
            self.baseline.low = optional_price(&quote.low)?;
        }
        if !quote.turnover.is_empty() {
            self.baseline.traded_notional = optional_quantity(&quote.turnover)?;
        }
        if quote.volume != 0 {
            self.baseline.traded_quantity = Some(quantity_of(DECODE_QUOTE, quote.volume)?);
        }
        self.baseline.monotonic_id = Some(quote.sequence);
        Ok(())
    }
}

/// One subscription's stream of events.
fn ticker_stream(
    state: TickerWire,
) -> impl Stream<Item = Result<ProtocolEvent, MarketParticipantError>> + Send {
    futures::stream::unfold(state, |mut state| async move {
        state.next_event().await.map(|event| (event, state))
    })
}

/// Reads one complete quote as the model's own.
///
/// The snapshot is whole, so a field the venue left empty is a field it states nothing
/// about rather than one it did not change: the two optional prices and the traded
/// notional are the model's way of saying so, while the open and the last price are what
/// a row is measured from and an answer that carries neither is not a quote. The pull
/// also publishes no ordering marker — the venue's pull commands carry no sequence — so
/// the snapshot says so rather than inventing one.
fn native_quote(quote: &SecurityQuote) -> Result<NativeTicker, MarketParticipantError> {
    Ok(NativeTicker {
        monotonic_id: None,
        open: price_of(DECODE_QUOTE, &quote.open)?,
        high: optional_price(&quote.high)?,
        low: optional_price(&quote.low)?,
        close: price_of(DECODE_QUOTE, &quote.last_done)?,
        traded_quantity: Some(quantity_of(DECODE_QUOTE, quote.volume)?),
        traded_notional: optional_quantity(&quote.turnover)?,
    })
}

/// Reads one optional wire price.
///
/// A price of zero is the venue's spelling of a field it has nothing to say about — a
/// high or a low of a session that has not traded — and the model states that as no price
/// rather than as a price of zero, which would read as a real level.
fn optional_price(value: &str) -> Result<Option<Price>, MarketParticipantError> {
    if value.is_empty() {
        return Ok(None);
    }
    let price = price_of(DECODE_QUOTE, value)?;
    Ok((price != Price::ZERO).then_some(price))
}

/// Reads one optional wire amount.
///
/// Only an empty field is an amount the venue did not state: a turnover of `"0"` is the
/// venue stating a real zero, and a row that read it as missing would say the listing
/// publishes no such figure at all. It is parsed as the quantity it is rather than through
/// [`optional_price`], because a price and an amount are two different fixed-point scales
/// and a price reader would refuse a value the venue really sent.
fn optional_quantity(value: &str) -> Result<Option<Quantity>, MarketParticipantError> {
    if value.is_empty() {
        return Ok(None);
    }
    let quantity: Quantity = value
        .parse()
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(DECODE_QUOTE),
            reason: format_smolstr!("`{value}` is not a decimal"),
            raw_data: None,
        })?;
    Ok(Some(quantity))
}

/// One protocol failure of a quote the venue answered.
fn quote_protocol_error(reason: SmolStr) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static(DECODE_QUOTE),
        reason,
        raw_data: None,
    }
}
