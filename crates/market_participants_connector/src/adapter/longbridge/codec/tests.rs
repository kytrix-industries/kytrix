//! The unit tests of the framing and of the venue's own vocabularies.
//!
//! They are a file of their own rather than a block inside the module because the module
//! boundary is what they belong to, not the file: what they assert is the byte layout and
//! the tables the parent owns, and they reach every private item of it through `super::`.

use bytes::Bytes;
use flate2::{Compression, write::GzEncoder};
use prost::Message as _;

use super::framing::{
    COMMAND_CODE_AUTH, DIRECTION_BACKWARD, DIRECTION_FORWARD, FIRST_SCREEN_COUNT,
    GET_SECURITY_CANDLESTICKS, GET_SECURITY_HISTORY_CANDLESTICKS, GET_TRADING_DAYS, HEADER_GZIP,
    HEADER_VERIFY, LongbridgeAdjust, LongbridgePeriod, LongbridgeSession, MAX_BODY_BYTES,
    PACKAGE_TYPE_PUSH, PACKAGE_TYPE_REQUEST, PACKAGE_TYPE_RESPONSE, PUSH_REALTIME_QUOTE,
    PUSH_REALTIME_TRADES, Packet, QUERY_BY_OFFSET, QUERY_SECURITY_QUOTE, QUERY_USER_QUOTE_PROFILE,
    SUB_TYPE_QUOTE, SUB_TYPE_TRADE, SUBSCRIBE, TRADE_SESSION_ALL, UNSUBSCRIBE, decode,
    encode_request, extract_response_id, is_market_data, refusal,
};
use crate::{
    error::MarketParticipantError,
    market::timeframe::{Adjustment, TimeFrame},
};

/// The refusal one decoded packet states, read the way a request path reads it.
fn rejection(packet: &Packet) -> Result<Option<MarketParticipantError>, MarketParticipantError> {
    match packet {
        Packet::Response { status, body, .. } => refusal(*status, body),
        Packet::Push { .. } => Ok(None),
    }
}

/// One response frame of the protocol, built byte by byte.
fn response_frame(
    header: u8,
    command_code: u8,
    request_id: u32,
    status: u8,
    body: &[u8],
) -> Vec<u8> {
    let mut frame = vec![header, command_code];
    frame.extend_from_slice(&request_id.to_be_bytes());
    frame.push(status);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    frame
}

/// One push frame of the protocol, built byte by byte.
fn push_frame(header: u8, command_code: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![header, command_code];
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    frame
}

/// A frame whose declared body length is not the length of its body.
fn declared_frame(header: u8, command_code: u8, declared: usize, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![header, command_code, 0, 0, 0, 0];
    frame.push(0);
    frame.extend_from_slice(&(declared as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    frame
}

/// The request layout is the protocol's own, byte for byte, and every multibyte
/// integer of it is big-endian.
#[test]
fn an_encoded_request_is_the_protocol_layout() {
    let frame = encode_request(
        GET_SECURITY_HISTORY_CANDLESTICKS,
        0x0102_0304,
        30_000,
        b"body",
    )
    .expect("the fixture request is encodable");

    assert_eq!(
        frame.as_ref(),
        [
            0b0000_0001, // packet type: request, neither signed nor compressed
            27,          // command code
            1,
            2,
            3,
            4, // request id, big-endian
            0x75,
            0x30, // timeout millis, big-endian
            0,
            0,
            4, // body length, big-endian
            b'b',
            b'o',
            b'd',
            b'y',
        ]
    );
}

/// The three-byte length field is what bounds a request body.
#[test]
fn a_request_body_above_the_length_field_is_refused() {
    let too_long = vec![0_u8; 0x100_0000];

    let error = encode_request(SUBSCRIBE, 1, 30_000, &too_long)
        .expect_err("a body above the three-byte length field has no encoding");

    assert!(matches!(
        error,
        MarketParticipantError::ProtocolError { operation, .. } if operation == "encode Longbridge request"
    ));
}

/// The length field's high bytes are written, not left at zero: a body the
/// three-byte field can state only with its first byte is a body whose length
/// the receiver reads from all three of them.
#[test]
fn a_body_length_reaches_all_three_of_its_bytes() {
    let body = vec![0_u8; 0x01_0203];

    let frame = encode_request(GET_SECURITY_HISTORY_CANDLESTICKS, 1, 30_000, &body)
        .expect("the fixture body fits the length field");

    assert_eq!(
        &frame[8..11],
        [0x01, 0x02, 0x03],
        "the body length is big-endian across all three of its bytes"
    );
    assert_eq!(frame.len(), 11 + body.len());
}

/// Both packet types the venue answers with decode into their own shape, and a
/// push is never mistaken for a response.
#[test]
fn responses_and_pushes_decode_into_their_own_packets() {
    let response = decode(&response_frame(PACKAGE_TYPE_RESPONSE, 27, 42, 0, b"answer"))
        .expect("a response frame decodes");
    assert_eq!(
        response,
        Packet::Response {
            command_code: 27,
            request_id: 42,
            status: 0,
            body: Bytes::from_static(b"answer"),
        }
    );

    let push = decode(&push_frame(
        PACKAGE_TYPE_PUSH,
        PUSH_REALTIME_QUOTE,
        b"quote",
    ))
    .expect("a push frame decodes");
    assert_eq!(
        push,
        Packet::Push {
            command_code: 101,
            body: Bytes::from_static(b"quote"),
        }
    );
}

/// A push frame carries its length where a response carries its id, so the
/// packet type bits are what answers the id — a length is never an id.
#[test]
fn a_push_frame_answers_no_request_id() {
    let push = push_frame(PACKAGE_TYPE_PUSH, PUSH_REALTIME_TRADES, &[0_u8; 9]);

    assert_eq!(extract_response_id(&push), None);
    assert_eq!(
        extract_response_id(&response_frame(PACKAGE_TYPE_RESPONSE, 27, 7, 0, b"")),
        Some(7)
    );
    assert_eq!(
        extract_response_id(&response_frame(PACKAGE_TYPE_RESPONSE, 27, 7, 0, b"")[..5]),
        None,
        "a frame shorter than the id it would answer is not an answer"
    );
    assert_eq!(extract_response_id(&[]), None);

    // The connection layer takes the answer as a plain function pointer, so this
    // reader is one: it captures nothing and reads only the bytes it is given.
    let extractor: fn(&[u8]) -> Option<u64> = extract_response_id;
    assert_eq!(
        extractor(&response_frame(PACKAGE_TYPE_RESPONSE, 27, 7, 0, b"")),
        Some(7)
    );
    assert_eq!(extractor(&push), None);
}

/// The packet type is the low nibble of the header, so the bits beside it do
/// not make a response something else: a verified or compressed answer is still
/// an answer to the request whose id it echoes.
#[test]
fn a_response_is_recognised_whatever_the_other_header_bits_say() {
    let verified = response_frame(
        PACKAGE_TYPE_RESPONSE | HEADER_VERIFY,
        19,
        0x0102_0304,
        0,
        b"",
    );
    let compressed = response_frame(PACKAGE_TYPE_RESPONSE | HEADER_GZIP, 19, 5, 0, b"");
    let both = response_frame(
        PACKAGE_TYPE_RESPONSE | HEADER_VERIFY | HEADER_GZIP,
        19,
        9,
        0,
        b"",
    );

    assert_eq!(extract_response_id(&verified), Some(0x0102_0304));
    assert_eq!(extract_response_id(&compressed), Some(5));
    assert_eq!(extract_response_id(&both), Some(9));
    assert_ne!(
        verified[0], PACKAGE_TYPE_RESPONSE,
        "the fixture header is not the bare packet type, so a whole-byte comparison is not this reader"
    );
}

/// A verified frame carries its signature after the body, and the frame is
/// exactly as long as the header says: neither a shorter nor a longer tail is
/// the frame this build understands.
#[test]
fn a_verified_frame_carries_exactly_its_signature_after_the_body() {
    let mut frame = response_frame(PACKAGE_TYPE_RESPONSE | HEADER_VERIFY, 19, 3, 0, b"candles");
    frame.extend_from_slice(&[0xAA; 24]);
    assert_eq!(
        frame.len(),
        10 + b"candles".len() + 24,
        "a verified frame is its fixed fields, its body and its signature"
    );

    let packet = decode(&frame).expect("a signed frame decodes and its signature is skipped");
    assert_eq!(
        packet,
        Packet::Response {
            command_code: 19,
            request_id: 3,
            status: 0,
            body: Bytes::from_static(b"candles"),
        }
    );

    assert!(
        decode(&frame[..frame.len() - 1]).is_err(),
        "a signature one byte short is not a signature"
    );
    let mut longer = frame.clone();
    longer.push(0);
    assert!(
        decode(&longer).is_err(),
        "bytes beyond the signature are not part of this frame"
    );
}

/// A gzip body is inflated before it is read, a body that is not the gzip it
/// claims is refused, and the bytes it inflates to are bounded by the same limit
/// the frame is: a small compressed body must not become an unbounded
/// allocation.
#[test]
fn a_gzipped_body_is_inflated_within_the_bound() {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, b"compressed answer").expect("the fixture writes");
    let compressed = encoder.finish().expect("the fixture compresses");

    let frame = response_frame(PACKAGE_TYPE_RESPONSE | HEADER_GZIP, 27, 1, 0, &compressed);
    let packet = decode(&frame).expect("a gzip frame decodes");
    assert_eq!(
        packet,
        Packet::Response {
            command_code: 27,
            request_id: 1,
            status: 0,
            body: Bytes::from_static(b"compressed answer"),
        }
    );

    let broken = response_frame(PACKAGE_TYPE_RESPONSE | HEADER_GZIP, 27, 1, 0, b"not gzip");
    assert!(matches!(
        decode(&broken),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));

    // A body that inflates past the bound is refused however small the frame
    // that carried it was.
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    let zeros = vec![0_u8; MAX_BODY_BYTES + 1];
    std::io::Write::write_all(&mut encoder, &zeros).expect("the fixture writes");
    let compressed = encoder.finish().expect("the fixture compresses");
    assert!(
        compressed.len() < MAX_BODY_BYTES,
        "the fixture body is a small frame that inflates past the bound"
    );

    let inflated = response_frame(PACKAGE_TYPE_RESPONSE | HEADER_GZIP, 27, 1, 0, &compressed);
    assert!(matches!(
        decode(&inflated),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));
}

/// Every malformed frame the protocol can produce is refused rather than
/// indexed: an empty frame, an unknown packet type, a frame shorter than its own
/// type, and a length that is not the bytes that arrived.
#[test]
fn a_frame_the_protocol_does_not_allow_is_refused() {
    let empty: &[u8] = &[];
    assert!(matches!(
        decode(empty),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));

    assert!(
        decode(&[0b0000_0100, 27, 0, 0, 0]).is_err(),
        "a packet type outside the three the protocol defines is not a frame"
    );
    assert!(
        decode(&response_frame(PACKAGE_TYPE_RESPONSE, 27, 1, 0, b"")[..9]).is_err(),
        "a response shorter than its own fixed fields has no length to read"
    );
    assert!(
        decode(&push_frame(PACKAGE_TYPE_PUSH, 101, b"")[..4]).is_err(),
        "a push shorter than its own fixed fields has no length to read"
    );
    assert!(
        decode(&[PACKAGE_TYPE_REQUEST, 2, 0, 0, 0, 0, 0, 0, 0, 0]).is_err(),
        "a request shorter than its own fixed fields has no length to read"
    );
    assert!(
        decode(&[PACKAGE_TYPE_REQUEST, 2, 0, 0, 0, 1, 0x75, 0x30, 0, 0, 0]).is_err(),
        "a request is a direction this protocol does not answer with"
    );

    let truncated = declared_frame(PACKAGE_TYPE_RESPONSE, 27, 5, b"body");
    assert!(
        decode(&truncated).is_err(),
        "a length that is not the bytes that arrived is not truncated into a frame"
    );
    let padded = declared_frame(PACKAGE_TYPE_RESPONSE, 27, 3, b"body");
    assert!(
        decode(&padded).is_err(),
        "a length shorter than the bytes that arrived is not padded into a frame"
    );

    // The largest length the three-byte field can state, with no bytes behind
    // it: a declared body is never answered with the bytes that are there.
    let largest = declared_frame(PACKAGE_TYPE_RESPONSE, 27, 0xFF_FFFF, b"");
    assert!(
        decode(&largest).is_err(),
        "a frame that declares a body it does not carry is refused before anything is read"
    );
}

/// A failed response states the venue's own error: the code is its number in
/// decimal and the message its own words, and neither is invented here.
#[test]
fn a_failed_response_states_the_venues_own_error() {
    let failure = longbridge_proto::Error {
        code: 3_000_000,
        msg: "no permission".to_owned(),
    };
    let frame = response_frame(PACKAGE_TYPE_RESPONSE, 19, 9, 1, &failure.encode_to_vec());
    let packet = decode(&frame).expect("a failed response frame decodes");

    let Some(MarketParticipantError::Rejected {
        code,
        message,
        retryable,
        raw_response,
    }) = rejection(&packet).expect("the failure body is the venue's error")
    else {
        panic!("a non-zero status states a refusal");
    };
    assert_eq!(code, "3000000");
    assert_eq!(message, "no permission");
    assert!(!retryable, "a refusal is not promised as retryable");
    assert_eq!(
        raw_response.as_deref(),
        Some(failure.encode_to_vec().as_slice())
    );

    let success = decode(&response_frame(PACKAGE_TYPE_RESPONSE, 19, 9, 0, b"candles"))
        .expect("a successful response frame decodes");
    assert!(
        rejection(&success)
            .expect("a success states no failure")
            .is_none()
    );

    let push = decode(&push_frame(
        PACKAGE_TYPE_PUSH,
        PUSH_REALTIME_QUOTE,
        b"quote",
    ))
    .expect("a push frame decodes");
    assert!(rejection(&push).expect("a push is not an answer").is_none());

    let unreadable = decode(&response_frame(
        PACKAGE_TYPE_RESPONSE,
        19,
        9,
        1,
        b"\xff\xff",
    ))
    .expect("the frame itself decodes");
    assert!(matches!(
        rejection(&unreadable),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));

    // A failed answer that states nothing is not a refusal: the default `Error`
    // prost reads out of an empty body would render as a rejection with the code
    // zero and no message at all, which is a sentence about nothing the venue
    // said. It is the same decode failure a body that is not a message is.
    let empty = decode(&response_frame(PACKAGE_TYPE_RESPONSE, 19, 9, 1, b""))
        .expect("the frame itself decodes");
    assert!(matches!(
        rejection(&empty),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));

    // The same for a body that is a well-formed message stating the default: a
    // field this build does not know is skipped, and what is left is the default
    // `Error` — code zero with no message — rather than a refusal.
    let defaulted = decode(&response_frame(
        PACKAGE_TYPE_RESPONSE,
        19,
        9,
        1,
        b"\x18\x01",
    ))
    .expect("the frame itself decodes");
    assert_eq!(
        longbridge_proto::Error::decode(b"\x18\x01".as_slice())
            .expect("the fixture body is a protobuf message"),
        longbridge_proto::Error {
            code: 0,
            msg: String::new(),
        },
        "the fixture body decodes into the default error this decision is about"
    );
    assert!(matches!(
        rejection(&defaulted),
        Err(MarketParticipantError::DecodeFailed { .. })
    ));
}

/// The protobuf surface this build programs against is the pinned one: the
/// three modules and the failure body the protocol names. A release that moved
/// or hid one of them would be a different wire.
#[test]
fn the_pinned_protobuf_surface_is_the_one_this_build_speaks() {
    let profile = longbridge_proto::quote::UserQuoteProfileRequest {
        language: "en".to_owned(),
    };
    let auth = longbridge_proto::control::AuthRequest {
        token: "otp".to_owned(),
        metadata: std::collections::HashMap::new(),
    };
    let subscription = longbridge_proto::trade::Sub {
        ..Default::default()
    };

    assert_eq!(profile.language, "en");
    assert_eq!(auth.token, "otp");
    assert_eq!(subscription, longbridge_proto::trade::Sub::default());
}

/// The command codes are the protocol's own numbers, and this build sends only
/// what it has a caller for.
///
/// The push codes are stated by the tests above rather than here: nothing of this
/// build reads a push yet, and a code with no reader is not one of this module's
/// numbers. The market-data set is the one place several of these numbers are read
/// together, and it is asked here so a command that moved in or out of it fails
/// this test rather than a gate nobody re-read.
#[test]
fn the_command_codes_are_the_venues_own_numbers() {
    assert_eq!(COMMAND_CODE_AUTH, 2);
    assert_eq!(QUERY_USER_QUOTE_PROFILE, 4);
    assert_eq!(SUBSCRIBE, 6);
    assert_eq!(UNSUBSCRIBE, 7);
    assert_eq!(GET_TRADING_DAYS, 9);
    assert_eq!(QUERY_SECURITY_QUOTE, 11);
    assert_eq!(GET_SECURITY_CANDLESTICKS, 19);
    assert_eq!(GET_SECURITY_HISTORY_CANDLESTICKS, 27);
    assert_eq!(PUSH_REALTIME_QUOTE, 101);
    assert_eq!(PUSH_REALTIME_TRADES, 104);

    for command in [
        SUBSCRIBE,
        UNSUBSCRIBE,
        QUERY_SECURITY_QUOTE,
        GET_SECURITY_CANDLESTICKS,
        GET_SECURITY_HISTORY_CANDLESTICKS,
    ] {
        assert!(
            is_market_data(command),
            "command {command} reads one market's own data"
        );
    }
    for command in [
        COMMAND_CODE_AUTH,
        QUERY_USER_QUOTE_PROFILE,
        GET_TRADING_DAYS,
        PUSH_REALTIME_QUOTE,
        PUSH_REALTIME_TRADES,
    ] {
        assert!(
            !is_market_data(command),
            "command {command} is not behind the market-data gate"
        );
    }
}

/// The numbers the candle wire writes into a frame are the design's own, and the
/// period table is the venue's rather than arithmetic on an interval's name.
#[test]
fn the_candle_wires_own_numbers_are_the_designs() {
    assert_eq!(SUB_TYPE_QUOTE, 1, "QUOTE is the subscription's first type");
    assert_eq!(SUB_TYPE_TRADE, 4, "and TRADE its fourth");
    assert_eq!(
        TRADE_SESSION_ALL, 100,
        "100 asks the venue for every session"
    );
    assert_eq!(FIRST_SCREEN_COUNT, 1_000);
    assert_eq!(
        QUERY_BY_OFFSET, 1,
        "the offset query is the one this build uses"
    );
    assert_eq!(DIRECTION_BACKWARD, 0, "BACKWARD pages into the history");
    assert_eq!(
        DIRECTION_FORWARD, 1,
        "and FORWARD towards the newest candle"
    );

    assert_eq!(LongbridgeAdjust::of(Adjustment::None).number(), 0);
    assert_eq!(LongbridgeAdjust::of(Adjustment::Forward).number(), 1);

    for (number, session) in [
        (0, LongbridgeSession::Normal),
        (1, LongbridgeSession::Pre),
        (2, LongbridgeSession::Post),
        (3, LongbridgeSession::Overnight),
    ] {
        assert_eq!(LongbridgeSession::of(number), Some(session));
    }
    assert_eq!(LongbridgeSession::of(4), None);
    assert_eq!(LongbridgeSession::of(-1), None);
}

/// Every period the venue publishes maps to one interval and back, with the venue's
/// own numbers pinned one by one.
///
/// The 18 values are written out here rather than read from a list the production code
/// also holds, because a table that asserted itself would pin nothing: a period whose
/// number moved would move in both halves of the assertion. The `TimeFrame` side is
/// read back through [`LongbridgePeriod::of`], so the two directions are checked
/// against one another, and the intervals the venue publishes no period for are
/// asserted `None` beside it.
#[test]
fn the_period_table_is_the_venues_own_in_both_directions() {
    let table = [
        (1, LongbridgePeriod::Minute1, TimeFrame::Minute1),
        (2, LongbridgePeriod::Minute2, TimeFrame::Minute2),
        (3, LongbridgePeriod::Minute3, TimeFrame::Minute3),
        (5, LongbridgePeriod::Minute5, TimeFrame::Minute5),
        (10, LongbridgePeriod::Minute10, TimeFrame::Minute10),
        (15, LongbridgePeriod::Minute15, TimeFrame::Minute15),
        (20, LongbridgePeriod::Minute20, TimeFrame::Minute20),
        (30, LongbridgePeriod::Minute30, TimeFrame::Minute30),
        (45, LongbridgePeriod::Minute45, TimeFrame::Minute45),
        (60, LongbridgePeriod::Hour1, TimeFrame::Hour1),
        (120, LongbridgePeriod::Hour2, TimeFrame::Hour2),
        (180, LongbridgePeriod::Hour3, TimeFrame::Hour3),
        (240, LongbridgePeriod::Hour4, TimeFrame::Hour4),
        (1_000, LongbridgePeriod::Day, TimeFrame::Day1),
        (2_000, LongbridgePeriod::Week, TimeFrame::Week1),
        (3_000, LongbridgePeriod::Month, TimeFrame::Month1),
        (3_500, LongbridgePeriod::Quarter, TimeFrame::Quarter1),
        (4_000, LongbridgePeriod::Year, TimeFrame::Year1),
    ];

    assert_eq!(
        table.len(),
        LongbridgePeriod::ALL.len(),
        "the table states every period this build encodes"
    );
    for (number, period, timeframe) in table {
        assert!(
            LongbridgePeriod::ALL.contains(&period),
            "{period:?} is one of the periods this build lists"
        );
        assert_eq!(
            period.number(),
            number,
            "{period:?} is the venue's {number}"
        );
        assert_eq!(
            LongbridgePeriod::of(timeframe),
            Some(period),
            "{timeframe:?} encodes as the venue's {number}"
        );
    }
    assert_eq!(
        table.map(|(number, _, _)| number),
        [
            1, 2, 3, 5, 10, 15, 20, 30, 45, 60, 120, 180, 240, 1_000, 2_000, 3_000, 3_500, 4_000
        ],
        "and the numbers are the venue's own, in the venue's own order"
    );
}

/// The intervals this crate has and the venue has no period for are stated rather
/// than defaulted: `Second1` is finer than the venue's finest, and the rest are
/// intervals its table does not carry at all.
#[test]
fn the_intervals_the_venue_publishes_no_period_for_are_named() {
    for timeframe in [
        TimeFrame::Second1,
        TimeFrame::Hour6,
        TimeFrame::Hour8,
        TimeFrame::Hour12,
        TimeFrame::Day2,
        TimeFrame::Day3,
        TimeFrame::Month3,
    ] {
        assert_eq!(
            LongbridgePeriod::of(timeframe),
            None,
            "{timeframe:?} is not one of the venue's periods"
        );
    }
}
