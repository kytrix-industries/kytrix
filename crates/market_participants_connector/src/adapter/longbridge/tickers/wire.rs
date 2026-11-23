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

const DECODE_QUOTE: &str = "decode Longbridge quote";

const DECODE_QUOTE_PUSH: &str = "decode the Longbridge quote push";

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

pub(crate) async fn subscribe_ticker(
    context: &ExecutionContext,
    session: &Arc<LongbridgeSession>,
    listing: &ResolvedListingRef,
) -> Result<SubscriptionHandle, MarketParticipantError> {
    let symbol = market_symbol(listing)?;
    session.require_market_data(&context.route(), codec::SUBSCRIBE)?;
    let baseline = read_quote(context, session, &symbol).await?;

    session.wait_for_quota(codec::SUBSCRIBE).await;
    let connection = session.connection();
    let request_id = connection.allocate_request_id();
    let wire_request_id = wire_id(request_id)?;
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

struct TickerWire {
    source: futures::stream::BoxStream<'static, Result<Bytes, MarketParticipantError>>,
    cancellation: CancellationToken,
    listing: VenueListingKey,
    symbol: SmolStr,
    request_id: u32,
    pending_baseline: Option<NativeTicker>,
    baseline: NativeTicker,
}

impl TickerWire {
    async fn next_event(&mut self) -> Option<Result<ProtocolEvent, MarketParticipantError>> {
        loop {
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

    fn event(&self, ticker: NativeTicker) -> ProtocolEvent {
        ProtocolEvent::Ticker(NativeListing::new(self.listing.clone(), ticker))
    }

    fn dispatch(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
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
                Ok(None)
            }
            Frame::RequestAnswer { .. } | Frame::Other | Frame::Trades(_) => Ok(None),
            Frame::Quote(body) => self.quote(&body),
        }
    }

    fn quote(&mut self, body: &Bytes) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
        let quote =
            PushQuote::decode(body.as_ref()).map_err(|_| decode_failed(DECODE_QUOTE_PUSH, body))?;
        if quote.symbol != self.symbol.as_str() {
            return Ok(None);
        }
        self.merge(&quote)?;
        Ok(Some(self.event(self.baseline.clone())))
    }

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

fn ticker_stream(
    state: TickerWire,
) -> impl Stream<Item = Result<ProtocolEvent, MarketParticipantError>> + Send {
    futures::stream::unfold(state, |mut state| async move {
        state.next_event().await.map(|event| (event, state))
    })
}

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

fn optional_price(value: &str) -> Result<Option<Price>, MarketParticipantError> {
    if value.is_empty() {
        return Ok(None);
    }
    let price = price_of(DECODE_QUOTE, value)?;
    Ok((price != Price::ZERO).then_some(price))
}

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

fn quote_protocol_error(reason: SmolStr) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static(DECODE_QUOTE),
        reason,
        raw_data: None,
    }
}
