//! The Longbridge quote-socket framing.
//!
//! A frame is a one-byte header, a command code and a variable-length protobuf
//! body, and this module is the only place those offsets are written down. It
//! decodes what the venue sends — including the two things a decoder of untrusted
//! input may never assume, that the declared length matches the bytes that arrived
//! and that a signature is where the header claims it is — and it encodes the one
//! request frame this build sends.
//!
//! What belongs beside the framing is the venue's own vocabulary: the command codes,
//! and the three enumerations the candle wire reads and writes — the intervals a
//! candlestick is cut into, the adjustment conventions its prices are stated in, and the
//! session a print happened in. The interval numbers are **not** consecutive, so the
//! table below is an explicit mapping in both directions rather than arithmetic on the
//! interval's name; the mapping and the list this adapter advertises are tied together
//! by a test, so a period the venue publishes cannot be advertised without being
//! encodable.

mod framing;
#[cfg(test)]
mod tests;

pub(super) use framing::{
    COMMAND_CODE_AUTH, DIRECTION_BACKWARD, DIRECTION_FORWARD, FIRST_SCREEN_COUNT,
    GET_SECURITY_CANDLESTICKS, GET_SECURITY_HISTORY_CANDLESTICKS, GET_TRADING_DAYS,
    LongbridgeAdjust, LongbridgePeriod, LongbridgeSession, PUSH_REALTIME_QUOTE,
    PUSH_REALTIME_TRADES, Packet, QUERY_BY_OFFSET, QUERY_SECURITY_QUOTE, QUERY_USER_QUOTE_PROFILE,
    SUB_TYPE_QUOTE, SUB_TYPE_TRADE, SUBSCRIBE, TRADE_SESSION_ALL, UNSUBSCRIBE, decode,
    encode_request, extract_response_id, is_market_data, refusal,
};
