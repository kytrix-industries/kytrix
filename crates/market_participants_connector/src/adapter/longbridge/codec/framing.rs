use std::io::Read;

use bytes::{BufMut as _, Bytes, BytesMut};
use flate2::read::GzDecoder;
use prost::Message as _;
use smol_str::{SmolStr, format_smolstr};

use crate::{error::MarketParticipantError, market::timeframe::TimeFrame};

pub(crate) const PACKAGE_TYPE_REQUEST: u8 = 1;
pub(crate) const PACKAGE_TYPE_RESPONSE: u8 = 2;
pub(crate) const PACKAGE_TYPE_PUSH: u8 = 3;

pub(crate) const HEADER_TYPE_MASK: u8 = 0b0000_1111;
pub(crate) const HEADER_VERIFY: u8 = 0b0001_0000;
pub(crate) const HEADER_GZIP: u8 = 0b0010_0000;

pub(crate) const REQUEST_FIELDS: usize = 11;
pub(crate) const RESPONSE_FIELDS: usize = 10;
pub(crate) const PUSH_FIELDS: usize = 5;

pub(crate) const SIGNATURE_BYTES: usize = 24;

pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 0xFF_FFFF;

pub(crate) const DECODE_OPERATION: &str = "decode Longbridge frame";

pub(crate) const ENCODE_OPERATION: &str = "encode Longbridge request";

pub(crate) const COMMAND_CODE_AUTH: u8 = 2;
pub(crate) const QUERY_USER_QUOTE_PROFILE: u8 = 4;
pub(crate) const SUBSCRIBE: u8 = 6;
pub(crate) const UNSUBSCRIBE: u8 = 7;
pub(crate) const GET_TRADING_DAYS: u8 = 9;
pub(crate) const QUERY_SECURITY_QUOTE: u8 = 11;
pub(crate) const GET_SECURITY_CANDLESTICKS: u8 = 19;
pub(crate) const GET_SECURITY_HISTORY_CANDLESTICKS: u8 = 27;
pub(crate) const PUSH_REALTIME_QUOTE: u8 = 101;
pub(crate) const PUSH_REALTIME_TRADES: u8 = 104;

pub(crate) const SUB_TYPE_QUOTE: i32 = 1;
pub(crate) const SUB_TYPE_TRADE: i32 = 4;

pub(crate) const TRADE_SESSION_ALL: i32 = 100;

pub(crate) const FIRST_SCREEN_COUNT: i32 = 1_000;

pub(crate) const QUERY_BY_OFFSET: i32 = 1;

pub(crate) const DIRECTION_BACKWARD: i32 = 0;
pub(crate) const DIRECTION_FORWARD: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum LongbridgePeriod {
    Minute1 = 1,
    Minute2 = 2,
    Minute3 = 3,
    Minute5 = 5,
    Minute10 = 10,
    Minute15 = 15,
    Minute20 = 20,
    Minute30 = 30,
    Minute45 = 45,
    Hour1 = 60,
    Hour2 = 120,
    Hour3 = 180,
    Hour4 = 240,
    Day = 1_000,
    Week = 2_000,
    Month = 3_000,
    Quarter = 3_500,
    Year = 4_000,
}

impl LongbridgePeriod {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 18] = [
        Self::Minute1,
        Self::Minute2,
        Self::Minute3,
        Self::Minute5,
        Self::Minute10,
        Self::Minute15,
        Self::Minute20,
        Self::Minute30,
        Self::Minute45,
        Self::Hour1,
        Self::Hour2,
        Self::Hour3,
        Self::Hour4,
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Year,
    ];

    pub(crate) const fn of(timeframe: TimeFrame) -> Option<Self> {
        Some(match timeframe {
            TimeFrame::Minute1 => Self::Minute1,
            TimeFrame::Minute2 => Self::Minute2,
            TimeFrame::Minute3 => Self::Minute3,
            TimeFrame::Minute5 => Self::Minute5,
            TimeFrame::Minute10 => Self::Minute10,
            TimeFrame::Minute15 => Self::Minute15,
            TimeFrame::Minute20 => Self::Minute20,
            TimeFrame::Minute30 => Self::Minute30,
            TimeFrame::Minute45 => Self::Minute45,
            TimeFrame::Hour1 => Self::Hour1,
            TimeFrame::Hour2 => Self::Hour2,
            TimeFrame::Hour3 => Self::Hour3,
            TimeFrame::Hour4 => Self::Hour4,
            TimeFrame::Day1 => Self::Day,
            TimeFrame::Week1 => Self::Week,
            TimeFrame::Month1 => Self::Month,
            TimeFrame::Quarter1 => Self::Quarter,
            TimeFrame::Year1 => Self::Year,
            TimeFrame::Second1
            | TimeFrame::Hour6
            | TimeFrame::Hour8
            | TimeFrame::Hour12
            | TimeFrame::Day2
            | TimeFrame::Day3
            | TimeFrame::Month3 => return None,
        })
    }

    pub(crate) const fn number(self) -> i32 {
        self as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongbridgeAdjust {
    NoAdjust = 0,
    ForwardAdjust = 1,
}

impl LongbridgeAdjust {
    pub(crate) const fn of(adjustment: crate::market::timeframe::Adjustment) -> Self {
        match adjustment {
            crate::market::timeframe::Adjustment::None => Self::NoAdjust,
            crate::market::timeframe::Adjustment::Forward => Self::ForwardAdjust,
        }
    }

    pub(crate) const fn number(self) -> i32 {
        self as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongbridgeSession {
    Normal = 0,
    Pre = 1,
    Post = 2,
    Overnight = 3,
}

impl LongbridgeSession {
    pub(crate) const fn of(number: i32) -> Option<Self> {
        Some(match number {
            0 => Self::Normal,
            1 => Self::Pre,
            2 => Self::Post,
            3 => Self::Overnight,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    Response {
        command_code: u8,
        request_id: u32,
        status: u8,
        body: Bytes,
    },
    Push {
        command_code: u8,
        body: Bytes,
    },
}

pub(crate) fn refusal(
    status: u8,
    body: &Bytes,
) -> Result<Option<MarketParticipantError>, MarketParticipantError> {
    if status == 0 {
        return Ok(None);
    }
    let error =
        longbridge_proto::Error::decode(body.as_ref()).map_err(|_| decode_failed(body.as_ref()))?;
    if error.code == 0 && error.msg.is_empty() {
        return Err(decode_failed(body.as_ref()));
    }
    Ok(Some(MarketParticipantError::Rejected {
        code: format_smolstr!("{}", error.code),
        message: SmolStr::new(error.msg),
        retryable: false,
        raw_response: Some(body.clone()),
    }))
}

pub(crate) const fn is_market_data(command_code: u8) -> bool {
    matches!(
        command_code,
        SUBSCRIBE
            | UNSUBSCRIBE
            | QUERY_SECURITY_QUOTE
            | GET_SECURITY_CANDLESTICKS
            | GET_SECURITY_HISTORY_CANDLESTICKS
    )
}

pub(crate) fn encode_request(
    command_code: u8,
    request_id: u32,
    timeout_millis: u16,
    body: &[u8],
) -> Result<Bytes, MarketParticipantError> {
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static(ENCODE_OPERATION),
            reason: format_smolstr!(
                "a body of {} bytes does not fit the {MAX_REQUEST_BODY_BYTES}-byte length field",
                body.len()
            ),
            raw_data: None,
        });
    }
    let mut frame = BytesMut::with_capacity(REQUEST_FIELDS + body.len());
    frame.put_u8(PACKAGE_TYPE_REQUEST);
    frame.put_u8(command_code);
    frame.put_u32(request_id);
    frame.put_u16(timeout_millis);
    frame.put_u8((body.len() >> 16) as u8);
    frame.put_u8((body.len() >> 8) as u8);
    frame.put_u8(body.len() as u8);
    frame.put_slice(body);
    Ok(frame.freeze())
}

pub(crate) fn decode(data: &[u8]) -> Result<Packet, MarketParticipantError> {
    let Some(&header) = data.first() else {
        return Err(decode_failed(data));
    };
    let package_type = header & HEADER_TYPE_MASK;
    let (fields, body_len) = match package_type {
        PACKAGE_TYPE_RESPONSE if data.len() >= RESPONSE_FIELDS => {
            (RESPONSE_FIELDS, body_length(&data[7..10]))
        }
        PACKAGE_TYPE_PUSH if data.len() >= PUSH_FIELDS => (PUSH_FIELDS, body_length(&data[2..5])),
        PACKAGE_TYPE_REQUEST if data.len() >= REQUEST_FIELDS => {
            (REQUEST_FIELDS, body_length(&data[8..11]))
        }
        _ => return Err(decode_failed(data)),
    };
    if body_len > MAX_BODY_BYTES {
        return Err(decode_failed(data));
    }
    let signature = if header & HEADER_VERIFY != 0 {
        SIGNATURE_BYTES
    } else {
        0
    };
    let body_end = fields + body_len;
    if data.len() != body_end + signature {
        return Err(decode_failed(data));
    }
    let body = decode_body(&data[fields..body_end], header & HEADER_GZIP != 0)?;
    match package_type {
        PACKAGE_TYPE_RESPONSE => Ok(Packet::Response {
            command_code: data[1],
            request_id: u32::from_be_bytes([data[2], data[3], data[4], data[5]]),
            status: data[6],
            body,
        }),
        PACKAGE_TYPE_PUSH => Ok(Packet::Push {
            command_code: data[1],
            body,
        }),
        _ => Err(decode_failed(data)),
    }
}

pub(crate) fn extract_response_id(data: &[u8]) -> Option<u64> {
    if data.len() < 6 || data[0] & HEADER_TYPE_MASK != PACKAGE_TYPE_RESPONSE {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([
        data[2], data[3], data[4], data[5],
    ])))
}

fn body_length(field: &[u8]) -> usize {
    (usize::from(field[0]) << 16) | (usize::from(field[1]) << 8) | usize::from(field[2])
}

fn decode_body(body: &[u8], compressed: bool) -> Result<Bytes, MarketParticipantError> {
    if !compressed {
        return Ok(Bytes::copy_from_slice(body));
    }
    let mut decoder = GzDecoder::new(body).take(MAX_BODY_BYTES as u64 + 1);
    let mut inflated = BytesMut::with_capacity(body.len().min(64 * 1024));
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = decoder.read(&mut chunk).map_err(|_| decode_failed(body))?;
        if read == 0 {
            break;
        }
        inflated.extend_from_slice(&chunk[..read]);
    }
    if inflated.len() > MAX_BODY_BYTES {
        return Err(decode_failed(body));
    }
    Ok(inflated.freeze())
}

fn decode_failed(raw_data: &[u8]) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new_static(DECODE_OPERATION),
        raw_data: Bytes::copy_from_slice(raw_data),
    }
}
