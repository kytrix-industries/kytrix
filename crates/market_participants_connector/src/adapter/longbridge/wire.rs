//! The shape of one inbound frame, and the readings both wires take of one.
//!
//! A wire frame carries a command and either a request id or a body length, so which one a
//! frame is decides how it is read; the first half of this file states that, and nothing
//! about what the wire sends. The candle wire and the ticker wire are two readers of the
//! same inbound frames — one connection publishes every subscription's frames to every
//! subscription — so what they read a frame *as* belongs to neither of them.
//!
//! The second half is the readings of the venue's own field encodings that both wires take:
//! a decimal field is a price and an integer field is a quantity, and both are the venue's
//! spelling of a number rather than a candle's or a quote's. Each takes the operation its
//! caller reports a failure under, because the two wires are two operations on the same
//! bytes and a shared function cannot name one of them.

use bytes::Bytes;
use market_participants_model::decimal::{Price, Quantity};
use smol_str::{SmolStr, format_smolstr};

use crate::{adapter::longbridge::codec, error::MarketParticipantError};

/// One decoded inbound frame, as far as the wires read it.
pub(crate) enum Frame {
    /// The answer to a request this build sent, named by the command it answers.
    ///
    /// The frame states its command because this build sends more than one command
    /// whose answer it reads: a subscribe frame's answer is what a stream recognises
    /// itself — the connection layer never treats it as a pending request — while a
    /// trading-day frame's answer is resolved by the request that sent it, and the
    /// reader of each is told which one arrived rather than inferring it from the
    /// shape of the body.
    RequestAnswer {
        command_code: u8,
        request_id: u32,
        status: u8,
        body: Bytes,
    },
    /// The venue's real-time quote.
    Quote(Bytes),
    /// The venue's real-time trades.
    Trades(Bytes),
    /// A frame no subscription of this build reads.
    Other,
}

/// Decodes one inbound frame into what the wires read of it.
pub(crate) fn decode(payload: &[u8]) -> Result<Frame, MarketParticipantError> {
    Ok(match codec::decode(payload)? {
        codec::Packet::Response {
            command_code,
            request_id,
            status,
            body,
        } => Frame::RequestAnswer {
            command_code,
            request_id,
            status,
            body,
        },
        codec::Packet::Push {
            command_code: codec::PUSH_REALTIME_QUOTE,
            body,
        } => Frame::Quote(body),
        codec::Packet::Push {
            command_code: codec::PUSH_REALTIME_TRADES,
            body,
        } => Frame::Trades(body),
        codec::Packet::Push { .. } => Frame::Other,
    })
}

/// Reads the refusal one failed answer states, when its body states one.
pub(crate) fn refusal(
    status: u8,
    body: &Bytes,
) -> Result<Option<MarketParticipantError>, MarketParticipantError> {
    codec::refusal(status, body)
}

/// Builds one decode failure over the body it is about.
pub(crate) fn decode_failed(operation: &'static str, raw: &Bytes) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new(operation),
        raw_data: raw.clone(),
    }
}

/// Reads one wire decimal as a price.
pub(crate) fn price_of(
    operation: &'static str,
    value: &str,
) -> Result<Price, MarketParticipantError> {
    value
        .parse()
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(operation),
            reason: format_smolstr!("`{value}` is not a decimal"),
            raw_data: None,
        })
}

/// Reads one wire volume as a quantity.
pub(crate) fn quantity_of(
    operation: &'static str,
    volume: i64,
) -> Result<Quantity, MarketParticipantError> {
    u64::try_from(volume)
        .map(Quantity::from)
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(operation),
            reason: format_smolstr!("a volume of {volume} is not a quantity"),
            raw_data: None,
        })
}

/// Reads one connection request id as the protocol's 32-bit field.
pub(crate) fn wire_id(request_id: u64) -> Result<u32, MarketParticipantError> {
    u32::try_from(request_id).map_err(|_| MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static("encode Longbridge request"),
        reason: format_smolstr!("the request id {request_id} does not fit the wire field"),
        raw_data: None,
    })
}
