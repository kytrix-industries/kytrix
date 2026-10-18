//! The framing and the venue's own vocabularies, as the socket carries them.
//!
//! A frame is a one-byte header, a command code and a variable-length protobuf body, and
//! this file is the only place those offsets are written down. What belongs beside the
//! framing is the venue's own numbering: the command codes, and the three enumerations the
//! candle wire reads and writes. The interval numbers are **not** consecutive, so the table
//! here is an explicit mapping in both directions rather than arithmetic on an interval's
//! name.

use std::io::Read;

use bytes::{BufMut as _, Bytes, BytesMut};
use flate2::read::GzDecoder;
use prost::Message as _;
use smol_str::{SmolStr, format_smolstr};

use crate::{error::MarketParticipantError, market::timeframe::TimeFrame};

/// The packet types the Longbridge socket framing carries.
pub(crate) const PACKAGE_TYPE_REQUEST: u8 = 1;
pub(crate) const PACKAGE_TYPE_RESPONSE: u8 = 2;
pub(crate) const PACKAGE_TYPE_PUSH: u8 = 3;

/// The bits of the packet header that are not the packet type.
pub(crate) const HEADER_TYPE_MASK: u8 = 0b0000_1111;
pub(crate) const HEADER_VERIFY: u8 = 0b0001_0000;
pub(crate) const HEADER_GZIP: u8 = 0b0010_0000;

/// The bytes one frame carries before its body, per packet type.
pub(crate) const REQUEST_FIELDS: usize = 11;
pub(crate) const RESPONSE_FIELDS: usize = 10;
pub(crate) const PUSH_FIELDS: usize = 5;

/// The signature a verified frame carries after its body: an 8-byte nonce and a
/// 16-byte signature.
///
/// This build neither sends nor depends on a signed frame, but a decoder that
/// ignored the header bit would read a signed body as longer than it is, so the
/// bytes are measured even though they are never checked.
pub(crate) const SIGNATURE_BYTES: usize = 24;

/// The largest body one frame may carry.
///
/// The three-byte length field of a frame can state one byte less than this, so
/// today the bound is the whole field range and the check below can never refuse a
/// length the protocol can express. It is stated as a bound of its own because the
/// alternative — trusting the field's width — puts the allocation this guard exists
/// for one field change away. Where the bound does bite today is
/// [`decode_body`]: a compressed body may inflate to far more than the bytes that
/// arrived, and that is the allocation a frame's own length does not bound.
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The largest request body the three-byte length field of a frame can state.
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 0xFF_FFFF;

/// The operation every frame decode failure is reported as.
pub(crate) const DECODE_OPERATION: &str = "decode Longbridge frame";

/// The operation every request encode failure is reported as.
pub(crate) const ENCODE_OPERATION: &str = "encode Longbridge request";

/// The control-plane command that authenticates a session (design §4.4).
pub(crate) const COMMAND_CODE_AUTH: u8 = 2;
/// The command that answers the account's quote permissions and rate limits (§4.4).
pub(crate) const QUERY_USER_QUOTE_PROFILE: u8 = 4;
/// The command that opens a real-time subscription (§4.6).
pub(crate) const SUBSCRIBE: u8 = 6;
/// The command that closes a real-time subscription (§4.6).
pub(crate) const UNSUBSCRIBE: u8 = 7;
/// The command that answers the trading days of one market, half days included (§4.6).
pub(crate) const GET_TRADING_DAYS: u8 = 9;
/// The command that answers the real-time quote of one or more securities (§4.6).
pub(crate) const QUERY_SECURITY_QUOTE: u8 = 11;
/// The command that answers a subscription's opening candles (§4.6).
pub(crate) const GET_SECURITY_CANDLESTICKS: u8 = 19;
/// The command that pages one market's historical candles (§4.5).
pub(crate) const GET_SECURITY_HISTORY_CANDLESTICKS: u8 = 27;
/// The push that carries the venue's real-time quote (§4.6).
pub(crate) const PUSH_REALTIME_QUOTE: u8 = 101;
/// The push that carries the venue's real-time trades (§4.6).
pub(crate) const PUSH_REALTIME_TRADES: u8 = 104;

/// The subscription type that asks for quotes (§4.6).
pub(crate) const SUB_TYPE_QUOTE: i32 = 1;
/// The subscription type that asks for trades (§4.6).
pub(crate) const SUB_TYPE_TRADE: i32 = 4;

/// The trade-session field that asks the venue for every session (§4.5, §4.6).
///
/// The venue's own `SecurityCandlestickRequest.trade_session` is an `int32` rather than
/// the enumeration, and 100 is its documented "all sessions": the opening screen and the
/// history both ask for it, because a reader that subscribed to a market wants the
/// pre-market and post-market candles the venue has.
pub(crate) const TRADE_SESSION_ALL: i32 = 100;

/// The count the subscription's opening screen asks for (§4.6).
pub(crate) const FIRST_SCREEN_COUNT: i32 = 1_000;

/// The query type that pages candles by an offset (§4.5).
pub(crate) const QUERY_BY_OFFSET: i32 = 1;

/// The direction that pages away from an anchor and towards the venue's history (§4.5).
pub(crate) const DIRECTION_BACKWARD: i32 = 0;
/// The direction that pages away from an anchor and towards the newest candle (§4.5).
pub(crate) const DIRECTION_FORWARD: i32 = 1;

/// One of the venue's candlestick intervals, as its wire numbers them (§4.7).
///
/// The numbers are the venue's `Period` values and are deliberately not consecutive: the
/// discriminants here are those values, so a period cannot be encoded without being one
/// the venue published. The list is closed over the intervals this build encodes, and
/// [`LONGBRIDGE_TIMEFRAMES`](super::adapter) is the same list read as intervals — the
/// test beside this type is what holds the two together.
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
    /// Every period this build encodes, in the venue's own ascending order.
    ///
    /// The list is the table's own order, and it is what ties the two vocabularies
    /// together: [`super::adapter::LONGBRIDGE_TIMEFRAMES`] is the same eighteen intervals
    /// read as the model's own, and a test asserts the two are one list rather than two
    /// that happen to agree. What the venue publishes and this build does not encode is
    /// therefore what is absent from both — and there is nothing absent today: the
    /// venue's eighteen periods are all named here.
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

    /// Reads one interval as the venue's period.
    ///
    /// The answer for an interval the venue publishes no period for is `None`, and the
    /// intervals it does not publish are named one by one rather than caught by a
    /// wildcard: a variant added to the model is a compile error here until someone
    /// decides whether this venue publishes it, which is what keeps
    /// [`super::adapter::LONGBRIDGE_TIMEFRAMES`] from silently drifting out of step with
    /// what this function can encode.
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
            // The intervals this venue publishes no period for, all seven of them:
            // `Second1` is finer than the venue's finest period, and the two- and
            // three-day and six-, eight- and twelve-hour intervals are not in its table
            // at all.
            TimeFrame::Second1
            | TimeFrame::Hour6
            | TimeFrame::Hour8
            | TimeFrame::Hour12
            | TimeFrame::Day2
            | TimeFrame::Day3
            | TimeFrame::Month3 => return None,
        })
    }

    /// The number the wire carries.
    pub(crate) const fn number(self) -> i32 {
        self as i32
    }
}

/// One price-adjustment convention, as the venue's wire numbers it (§4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongbridgeAdjust {
    /// Prices exactly as they were published at the time.
    NoAdjust = 0,
    /// Prices restated on the latest basis.
    ForwardAdjust = 1,
}

impl LongbridgeAdjust {
    /// Reads one adjustment convention as the venue's number.
    pub(crate) const fn of(adjustment: crate::market::timeframe::Adjustment) -> Self {
        match adjustment {
            crate::market::timeframe::Adjustment::None => Self::NoAdjust,
            crate::market::timeframe::Adjustment::Forward => Self::ForwardAdjust,
        }
    }

    /// The number the wire carries.
    pub(crate) const fn number(self) -> i32 {
        self as i32
    }
}

/// The session one print happened in, as the venue's wire numbers it (§4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongbridgeSession {
    Normal = 0,
    Pre = 1,
    Post = 2,
    Overnight = 3,
}

impl LongbridgeSession {
    /// Reads the venue's number as a session.
    ///
    /// A number outside the four the venue defines is not a session this build knows,
    /// and the answer is `None` rather than a default: a print filed under an unknown
    /// session is one whose bucket this build would be guessing at.
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

/// One decoded inbound packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    /// An answer to one request this build sent.
    Response {
        command_code: u8,
        request_id: u32,
        status: u8,
        body: Bytes,
    },
    /// One message of a subscription, sent without a request.
    Push { command_code: u8, body: Bytes },
}

/// The refusal one failed answer states, when its body states one.
///
/// A response that failed carries the venue's own `Error` as its body: the code
/// is the venue's number and the message is its own wording, so the refusal
/// reaches a reader as the venue wrote it. A refusal is the venue's answer
/// about the request itself — this build holds no table saying which of the
/// venue's codes are transient, and one invented here would promise a retry the
/// code does not support — so nothing decoded here is offered as retryable.
///
/// A failed response whose body is not the venue's `Error` is a frame this build
/// cannot read, and is reported as the decode failure it is rather than as a
/// refusal the venue never stated. An empty body decodes into the default
/// `Error` — prost answers a zero-length message with every field at its
/// default — and that default is not a refusal either: it would reach a reader
/// as "the market participant rejected the request (0): ", a sentence about nothing the
/// venue said. A body that states neither a code nor a message therefore fails
/// the decode, exactly as a body that is not a protobuf message at all does.
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

/// Whether one command reads a market's own data.
///
/// The five commands the venue answers market data on are the ones a session
/// without the account's permission refuses before a frame is sent (§4.4, §10.7).
/// Which commands those are is a fact of the venue's numbering, so the set is
/// stated where the numbers are; the profile request itself is deliberately not in
/// it, because that command is where the permission comes from.
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

/// Encodes one request packet with the caller's already allocated request id.
///
/// Every multibyte integer of the protocol is big-endian, which is what the
/// `BufMut` writers below are: the header states the packet type alone, because
/// this build sends neither a signed nor a compressed request.
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
    // The body length is a three-byte field, which `BufMut` has no writer for.
    frame.put_u8((body.len() >> 16) as u8);
    frame.put_u8((body.len() >> 8) as u8);
    frame.put_u8(body.len() as u8);
    frame.put_slice(body);
    Ok(frame.freeze())
}

/// Decodes one inbound frame, decompressing a gzip body.
///
/// This is a decoder of untrusted network input, so every boundary the protocol
/// states is judged here rather than assumed: a frame that is too short for its own
/// type has no length field to read, a body length must describe exactly the bytes
/// that arrived, a length above the bound is refused before anything is read, and a
/// verified frame must carry exactly its signature after the body.
pub(crate) fn decode(data: &[u8]) -> Result<Packet, MarketParticipantError> {
    let Some(&header) = data.first() else {
        return Err(decode_failed(data));
    };
    let package_type = header & HEADER_TYPE_MASK;
    // A request is what this build sends and never receives, but its prefix is
    // measured like the other two: no frame is indexed by a length field it did not
    // carry, whatever type it claims.
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
    // The bound is judged before the body is read, so a declared length cannot make
    // this build allocate.
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
        // A request frame is the venue speaking a direction this protocol does not
        // use, so it is not a packet anything here can read.
        _ => Err(decode_failed(data)),
    }
}

/// Reads the request id a response frame answers.
///
/// A push frame carries its body length where a response carries its id, so the
/// packet type bits are what decide: reading the same offset without them would
/// answer a length as an id, and a frame that is neither is not this venue's at all.
pub(crate) fn extract_response_id(data: &[u8]) -> Option<u64> {
    if data.len() < 6 || data[0] & HEADER_TYPE_MASK != PACKAGE_TYPE_RESPONSE {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([
        data[2], data[3], data[4], data[5],
    ])))
}

/// Reads the three-byte big-endian body length of one frame.
fn body_length(field: &[u8]) -> usize {
    (usize::from(field[0]) << 16) | (usize::from(field[1]) << 8) | usize::from(field[2])
}

/// Reads the body of one frame, decompressing it when the header says it is gzip.
///
/// The inflated size is bounded by the same limit the frame is: a length that may
/// not make this build allocate must not become one after decompression, or the
/// bound on the compressed bytes would be no bound at all.
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

/// Builds one decode failure over the bytes it is about.
///
/// The frame travels with the failure: a frame nothing can read is diagnosed from
/// what arrived, and this is the last place that still has it.
fn decode_failed(raw_data: &[u8]) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new_static(DECODE_OPERATION),
        raw_data: Bytes::copy_from_slice(raw_data),
    }
}
