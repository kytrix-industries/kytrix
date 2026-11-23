use bytes::Bytes;
use market_participants_model::decimal::{Price, Quantity};
use smol_str::{SmolStr, format_smolstr};

use crate::{adapter::longbridge::codec, error::MarketParticipantError};

pub(crate) enum Frame {
    RequestAnswer {
        command_code: u8,
        request_id: u32,
        status: u8,
        body: Bytes,
    },
    Quote(Bytes),
    Trades(Bytes),
    Other,
}

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

pub(crate) fn refusal(
    status: u8,
    body: &Bytes,
) -> Result<Option<MarketParticipantError>, MarketParticipantError> {
    codec::refusal(status, body)
}

pub(crate) fn decode_failed(operation: &'static str, raw: &Bytes) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new(operation),
        raw_data: raw.clone(),
    }
}

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

pub(crate) fn wire_id(request_id: u64) -> Result<u32, MarketParticipantError> {
    u32::try_from(request_id).map_err(|_| MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static("encode Longbridge request"),
        reason: format_smolstr!("the request id {request_id} does not fit the wire field"),
        raw_data: None,
    })
}
