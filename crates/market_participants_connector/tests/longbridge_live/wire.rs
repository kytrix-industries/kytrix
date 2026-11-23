use bytes::{BufMut as _, Bytes, BytesMut};
use flate2::read::GzDecoder;
use prost::Message as _;

pub const PACKAGE_TYPE_REQUEST: u8 = 1;
pub const PACKAGE_TYPE_RESPONSE: u8 = 2;
pub const PACKAGE_TYPE_PUSH: u8 = 3;
pub const HEADER_TYPE_MASK: u8 = 0b0000_1111;
pub const HEADER_VERIFY: u8 = 0b0001_0000;
pub const HEADER_GZIP: u8 = 0b0010_0000;
pub const REQUEST_FIELDS: usize = 11;
pub const RESPONSE_FIELDS: usize = 10;
pub const PUSH_FIELDS: usize = 5;

pub const COMMAND_CODE_AUTH: u8 = 2;
pub const QUERY_USER_QUOTE_PROFILE: u8 = 4;
pub const SUBSCRIBE: u8 = 6;
pub const UNSUBSCRIBE: u8 = 7;
pub const GET_TRADING_DAYS: u8 = 9;
pub const QUERY_SECURITY_QUOTE: u8 = 11;
pub const GET_SECURITY_CANDLESTICKS: u8 = 19;
pub const GET_SECURITY_HISTORY_CANDLESTICKS: u8 = 27;
pub const PUSH_REALTIME_QUOTE: u8 = 101;
pub const PUSH_REALTIME_TRADES: u8 = 104;

pub const SUB_TYPE_QUOTE: i32 = 1;
pub const SUB_TYPE_TRADE: i32 = 4;
pub const QUERY_BY_OFFSET: i32 = 1;
pub const DIRECTION_BACKWARD: i32 = 0;
pub const DIRECTION_FORWARD: i32 = 1;

pub const SUBSCRIPTION_REFUSAL: &str = "the venue refused the subscription";
pub const LOGIN_REFUSAL: &str = "the venue refused the login";

#[derive(Clone)]
pub struct Inbound {
    pub frame: Bytes,
    pub header: u8,
    pub command_code: u8,
    pub request_id: Option<u32>,
    pub status: Option<u8>,
    pub declared_body_len: usize,
    pub body: Bytes,
}

#[derive(Default)]
pub struct FrameStats {
    pub responses: usize,
    pub pushes: usize,
    pub gzip: usize,
    pub verify: usize,
    pub length_mismatches: Vec<String>,
    pub unknown_types: Vec<u8>,
}

#[derive(Debug)]
pub enum Failure {
    Transport(String),
    Refused {
        command: u8,
        code: u64,
        message: String,
    },
    Malformed(String),
    Fact(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(reason) => write!(formatter, "the socket failed: {reason}"),
            Self::Refused {
                command,
                code,
                message,
            } => write!(
                formatter,
                "the venue refused command {command} with code {code}: {message}"
            ),
            Self::Malformed(reason) => write!(formatter, "unreadable frame: {reason}"),
            Self::Fact(reason) => write!(formatter, "the venue contradicts the design: {reason}"),
        }
    }
}

impl std::error::Error for Failure {}

pub fn encode_request(
    command_code: u8,
    request_id: u32,
    timeout_millis: u16,
    body: &[u8],
) -> Bytes {
    let mut frame = BytesMut::with_capacity(REQUEST_FIELDS + body.len());
    frame.put_u8(PACKAGE_TYPE_REQUEST);
    frame.put_u8(command_code);
    frame.put_u32(request_id);
    frame.put_u16(timeout_millis);
    frame.put_u8((body.len() >> 16) as u8);
    frame.put_u8((body.len() >> 8) as u8);
    frame.put_u8(body.len() as u8);
    frame.put_slice(body);
    frame.freeze()
}

fn body_length(field: &[u8]) -> usize {
    (usize::from(field[0]) << 16) | (usize::from(field[1]) << 8) | usize::from(field[2])
}

pub fn decode_inbound(frame: Bytes, stats: &mut FrameStats) -> Result<Inbound, Failure> {
    let header = *frame
        .first()
        .ok_or_else(|| Failure::Malformed("an empty frame".to_owned()))?;
    let package_type = header & HEADER_TYPE_MASK;
    let (fields, declared) = match package_type {
        PACKAGE_TYPE_RESPONSE if frame.len() >= RESPONSE_FIELDS => {
            (RESPONSE_FIELDS, body_length(&frame[7..10]))
        }
        PACKAGE_TYPE_PUSH if frame.len() >= PUSH_FIELDS => (PUSH_FIELDS, body_length(&frame[2..5])),
        PACKAGE_TYPE_REQUEST if frame.len() >= REQUEST_FIELDS => {
            (REQUEST_FIELDS, body_length(&frame[8..11]))
        }
        _ => {
            stats.unknown_types.push(package_type);
            return Err(Failure::Malformed(format!(
                "a frame of {} bytes with packet type {package_type}",
                frame.len()
            )));
        }
    };
    let signature = if header & HEADER_VERIFY != 0 { 24 } else { 0 };
    let expected = fields + declared + signature;
    if frame.len() != expected {
        stats.length_mismatches.push(format!(
            "packet type {package_type}: {} bytes arrived, {expected} stated",
            frame.len()
        ));
    }
    if header & HEADER_GZIP != 0 {
        stats.gzip += 1;
    }
    if header & HEADER_VERIFY != 0 {
        stats.verify += 1;
    }
    let end = (fields + declared).min(frame.len());
    let raw_body = &frame[fields.min(frame.len())..end];
    let body = if header & HEADER_GZIP != 0 {
        let mut inflated = Vec::with_capacity(raw_body.len() * 4);
        let mut decoder = GzDecoder::new(raw_body);
        std::io::Read::read_to_end(&mut decoder, &mut inflated)
            .map_err(|error| Failure::Malformed(format!("a gzip body: {error}")))?;
        Bytes::from(inflated)
    } else {
        Bytes::copy_from_slice(raw_body)
    };
    if package_type == PACKAGE_TYPE_PUSH {
        stats.pushes += 1;
    } else {
        stats.responses += 1;
    }
    Ok(Inbound {
        header,
        command_code: frame[1],
        request_id: (package_type == PACKAGE_TYPE_RESPONSE)
            .then(|| u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]])),
        status: (package_type == PACKAGE_TYPE_RESPONSE).then(|| frame[6]),
        declared_body_len: declared,
        body,
        frame,
    })
}

pub fn extract_response_id(data: &[u8]) -> Option<u64> {
    if data.len() < 6 || data[0] & HEADER_TYPE_MASK != PACKAGE_TYPE_RESPONSE {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([
        data[2], data[3], data[4], data[5],
    ])))
}

pub fn refusal(body: &Bytes) -> (u64, String) {
    match longbridge_proto::Error::decode(body.as_ref()) {
        Ok(error) => (error.code, error.msg),
        Err(_) => (0, format!("<{} unreadable bytes>", body.len())),
    }
}

pub fn measure_round_trip() -> Vec<String> {
    let mut lines = Vec::with_capacity(4);
    let cases: [(u8, u32, &[u8]); 4] = [
        (
            COMMAND_CODE_AUTH,
            super::bounds::AUTH_REQUEST_ID,
            b"auth-body",
        ),
        (QUERY_USER_QUOTE_PROFILE, 1, b"profile-body"),
        (SUBSCRIBE, 42, b"subscribe-body"),
        (GET_SECURITY_HISTORY_CANDLESTICKS, u32::MAX, b""),
    ];
    for (command, id, body) in cases {
        let frame = encode_request(command, id, super::bounds::REQUEST_TIMEOUT_MILLIS, body);
        let mut stats = FrameStats::default();
        let decoded = decode_inbound(frame.clone(), &mut stats)
            .expect("a frame this build encoded must decode");
        assert_eq!(decoded.header & HEADER_TYPE_MASK, PACKAGE_TYPE_REQUEST);
        assert_eq!(decoded.command_code, command);
        assert_eq!(
            decoded.request_id, None,
            "a request frame states no request id at a response's offset: the two shapes are \
             read by their packet type"
        );
        assert_eq!(
            u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]),
            id,
            "a request's id sits at offset 2"
        );
        assert_eq!(
            u16::from_be_bytes([frame[6], frame[7]]),
            super::bounds::REQUEST_TIMEOUT_MILLIS,
            "a request's timeout sits at offset 6"
        );
        assert_eq!(decoded.body.as_ref(), body);
        assert_eq!(frame.len(), REQUEST_FIELDS + body.len());
        assert_eq!(decoded.declared_body_len, body.len());
        assert_eq!(
            extract_response_id(&frame),
            None,
            "a request frame is not a response, so it states no request id"
        );
        lines.push(format!(
            "  round-trip ok: command={command} request_id={id} body_len={} frame_len={}",
            body.len(),
            frame.len()
        ));
    }
    let response = encode_request(
        COMMAND_CODE_AUTH,
        7,
        super::bounds::REQUEST_TIMEOUT_MILLIS,
        b"",
    );
    let mut response = response.to_vec();
    response[0] = PACKAGE_TYPE_RESPONSE;
    assert_eq!(extract_response_id(&response), Some(7));
    let mut push = vec![0_u8; PUSH_FIELDS];
    push[0] = PACKAGE_TYPE_PUSH;
    push[1] = PUSH_REALTIME_QUOTE;
    push[2] = 0x00;
    push[3] = 0x00;
    push[4] = 0x07;
    assert_eq!(
        extract_response_id(&push),
        None,
        "a push frame's third byte is a length, not an id: only the type bits decide"
    );
    assert_eq!(extract_response_id(&[]), None);
    lines.push("  id extractor ok: response=Some(7), push=None, empty=None".to_owned());
    lines
}
