use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use futures::SinkExt as _;
use longbridge_proto::{
    control::AuthResponse,
    quote::{
        Candlestick, MarketTradeDayResponse, RateLimit, SecurityCandlestickResponse,
        SecurityQuoteResponse, UserQuoteLevelDetail, UserQuoteProfileResponse,
        user_quote_level_detail::{MarketPackageDetail, PackageDetail},
    },
};
use prost::Message as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

use super::{
    answers::{
        AuthAnswer, CalendarAnswer, CandleAnswer, FixtureCandle, PROFILE_REFUSAL_CODE,
        PROFILE_REFUSAL_MESSAGE, ProfileAnswer, QuoteAnswer, ReceivedFrame,
        SUBSCRIPTION_REFUSAL_CODE, SUBSCRIPTION_REFUSAL_MESSAGE, TokenAnswer,
    },
    venue::{
        COMMAND_CODE_AUTH, GET_SECURITY_CANDLESTICKS, GET_SECURITY_HISTORY_CANDLESTICKS,
        GET_TRADING_DAYS, PACKAGE_TYPE_RESPONSE, QUERY_SECURITY_QUOTE, QUERY_USER_QUOTE_PROFILE,
        REQUEST_FIELDS, RESPONSE_FIELDS, SUBSCRIBE, UNSUBSCRIBE,
    },
};

pub(crate) async fn serve_token(
    listener: TcpListener,
    answer: TokenAnswer,
    requests: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    cancellation: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancellation.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((mut socket, _)) = accepted else {
            break;
        };
        let answer = answer.clone();
        let requests = Arc::clone(&requests);
        tokio::spawn(async move {
            let Some((_, headers)) = read_request_head(&mut socket).await else {
                return;
            };
            requests
                .lock()
                .expect("the fixture request log is not poisoned")
                .push(headers);
            let body = token_envelope(&answer);
            let reason = if answer.status == 200 {
                "OK"
            } else {
                "Refused"
            };
            let response = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                answer.status,
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}

fn token_envelope(answer: &TokenAnswer) -> String {
    let data = if answer.payload {
        serde_json::json!({
            "otp": answer.otp,
            "limit": answer.limit,
            "online": answer.online,
        })
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "code": answer.code,
        "message": answer.message,
        "data": data,
    })
    .to_string()
}

#[derive(Clone)]
pub(crate) struct QuoteAnswers {
    auth: AuthAnswer,
    profile: ProfileAnswer,
    candles: Arc<Mutex<CandleAnswer>>,
    calendar: Arc<Mutex<CalendarAnswer>>,
    quotes: Arc<Mutex<QuoteAnswer>>,
    pushes: Arc<Mutex<VecDeque<Bytes>>>,
}

impl QuoteAnswers {
    pub(crate) const fn new(
        auth: AuthAnswer,
        profile: ProfileAnswer,
        candles: Arc<Mutex<CandleAnswer>>,
        calendar: Arc<Mutex<CalendarAnswer>>,
        quotes: Arc<Mutex<QuoteAnswer>>,
        pushes: Arc<Mutex<VecDeque<Bytes>>>,
    ) -> Self {
        Self {
            auth,
            profile,
            candles,
            calendar,
            quotes,
            pushes,
        }
    }
}

pub(crate) async fn serve_quote_socket(
    listener: TcpListener,
    answers: QuoteAnswers,
    upgrades: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    frames: Arc<Mutex<Vec<ReceivedFrame>>>,
    cancellation: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancellation.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            break;
        };
        let answers = answers.clone();
        let upgrades = Arc::clone(&upgrades);
        let frames = Arc::clone(&frames);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(
                move |mut request: hyper::Request<hyper::body::Incoming>| {
                    let answers = answers.clone();
                    let upgrades = Arc::clone(&upgrades);
                    let frames = Arc::clone(&frames);
                    let cancellation = cancellation.clone();
                    async move {
                        upgrades
                            .lock()
                            .expect("the fixture upgrade log is not poisoned")
                            .push(header_pairs(request.headers()));
                        let (response, upgrade) = yawc::WebSocket::upgrade(&mut request)?;
                        tokio::spawn(async move {
                            let Ok(mut socket) = upgrade.await else {
                                return;
                            };
                            let mut subscribed = false;
                            loop {
                                let frame = tokio::select! {
                                    () = cancellation.cancelled() => return,
                                    frame = socket.next_frame() => frame,
                                };
                                let Ok(frame) = frame else { return };
                                if !matches!(
                                    frame.opcode(),
                                    yawc::OpCode::Text | yawc::OpCode::Binary
                                ) {
                                    continue;
                                }
                                let payload = frame.payload().clone();
                                let Some(received) = request_frame(&payload) else {
                                    continue;
                                };
                                let received_command = received.command;
                                let answer = answer_of(&received, &answers);
                                frames
                                    .lock()
                                    .expect("the fixture frame log is not poisoned")
                                    .push(received);
                                if socket.send(yawc::Frame::binary(answer)).await.is_err() {
                                    return;
                                }
                                if received_command == SUBSCRIBE {
                                    subscribed = true;
                                }
                                if subscribed {
                                    loop {
                                        let pushed = answers
                                            .pushes
                                            .lock()
                                            .expect("the fixture push queue is not poisoned")
                                            .pop_front();
                                        let Some(pushed) = pushed else { break };
                                        if socket.send(yawc::Frame::binary(pushed)).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        });
                        Ok::<_, yawc::WebSocketError>(response)
                    }
                },
            );
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

fn candle_response(candles: &[FixtureCandle]) -> SecurityCandlestickResponse {
    SecurityCandlestickResponse {
        symbol: "AAPL.US".to_owned(),
        candlesticks: candles
            .iter()
            .map(|candle| Candlestick {
                close: format!("{}.00", candle.close),
                open: format!("{}.00", candle.open),
                low: format!("{}.00", candle.low),
                high: format!("{}.00", candle.high),
                volume: candle.volume,
                turnover: String::new(),
                timestamp: candle.timestamp,
                trade_session: 0,
            })
            .collect(),
    }
}

fn answer_of(request: &ReceivedFrame, answers: &QuoteAnswers) -> Bytes {
    let auth = &answers.auth;
    let profile = &answers.profile;
    let candles = answers
        .candles
        .lock()
        .expect("the fixture answer is not poisoned")
        .clone();
    let calendar = answers
        .calendar
        .lock()
        .expect("the fixture calendar is not poisoned")
        .clone();
    let quotes = answers
        .quotes
        .lock()
        .expect("the fixture quote is not poisoned")
        .clone();
    match request.command {
        COMMAND_CODE_AUTH => response_frame(
            COMMAND_CODE_AUTH,
            request.request_id,
            auth.status,
            &AuthResponse {
                session_id: "fixture-session".to_owned(),
                expires: 0,
                limit: 0,
                online: 0,
            }
            .encode_to_vec(),
        ),
        GET_SECURITY_CANDLESTICKS | GET_SECURITY_HISTORY_CANDLESTICKS if candles.status != 0 => {
            response_frame(
                request.command,
                request.request_id,
                candles.status,
                &longbridge_proto::Error {
                    code: SUBSCRIPTION_REFUSAL_CODE,
                    msg: SUBSCRIPTION_REFUSAL_MESSAGE.to_owned(),
                }
                .encode_to_vec(),
            )
        }
        GET_SECURITY_CANDLESTICKS | GET_SECURITY_HISTORY_CANDLESTICKS => response_frame(
            request.command,
            request.request_id,
            0,
            &candle_response(&candles.candles).encode_to_vec(),
        ),
        QUERY_SECURITY_QUOTE if quotes.status != 0 => response_frame(
            QUERY_SECURITY_QUOTE,
            request.request_id,
            quotes.status,
            &longbridge_proto::Error {
                code: SUBSCRIPTION_REFUSAL_CODE,
                msg: SUBSCRIPTION_REFUSAL_MESSAGE.to_owned(),
            }
            .encode_to_vec(),
        ),
        QUERY_SECURITY_QUOTE => response_frame(
            QUERY_SECURITY_QUOTE,
            request.request_id,
            0,
            &SecurityQuoteResponse {
                secu_quote: quotes.quotes,
            }
            .encode_to_vec(),
        ),
        SUBSCRIBE => {
            let body = if candles.subscribe_status == 0 {
                Vec::new()
            } else {
                longbridge_proto::Error {
                    code: SUBSCRIPTION_REFUSAL_CODE,
                    msg: SUBSCRIPTION_REFUSAL_MESSAGE.to_owned(),
                }
                .encode_to_vec()
            };
            response_frame(
                SUBSCRIBE,
                request.request_id,
                candles.subscribe_status,
                &body,
            )
        }
        UNSUBSCRIBE => response_frame(UNSUBSCRIBE, request.request_id, 0, &[]),
        GET_TRADING_DAYS if calendar.status != 0 => response_frame(
            GET_TRADING_DAYS,
            request.request_id,
            calendar.status,
            &longbridge_proto::Error {
                code: SUBSCRIPTION_REFUSAL_CODE,
                msg: SUBSCRIPTION_REFUSAL_MESSAGE.to_owned(),
            }
            .encode_to_vec(),
        ),
        GET_TRADING_DAYS => response_frame(
            GET_TRADING_DAYS,
            request.request_id,
            0,
            &MarketTradeDayResponse {
                trade_day: Vec::new(),
                half_trade_day: calendar.half_days,
            }
            .encode_to_vec(),
        ),
        QUERY_USER_QUOTE_PROFILE if profile.status != 0 => response_frame(
            QUERY_USER_QUOTE_PROFILE,
            request.request_id,
            profile.status,
            &longbridge_proto::Error {
                code: PROFILE_REFUSAL_CODE,
                msg: PROFILE_REFUSAL_MESSAGE.to_owned(),
            }
            .encode_to_vec(),
        ),
        QUERY_USER_QUOTE_PROFILE => {
            let answered = UserQuoteProfileResponse {
                member_id: 1,
                quote_level: "fixture".to_owned(),
                subscribe_limit: 10,
                history_candlestick_limit: 100,
                rate_limit: profile
                    .rate_limit
                    .iter()
                    .map(|(command, limit, burst)| RateLimit {
                        command: i32::from(*command),
                        limit: *limit,
                        burst: *burst,
                    })
                    .collect(),
                quote_level_detail: Some(UserQuoteLevelDetail {
                    by_package_key: std::collections::HashMap::new(),
                    by_market_code: std::collections::HashMap::from([(
                        "US".to_owned(),
                        MarketPackageDetail {
                            packages: (0..profile.us_packages)
                                .map(|index| PackageDetail {
                                    key: format!("fixture-{index}"),
                                    name: "fixture package".to_owned(),
                                    description: String::new(),
                                    start: 0,
                                    end: 0,
                                })
                                .collect(),
                            warning_msg: String::new(),
                        },
                    )]),
                }),
            };
            response_frame(
                QUERY_USER_QUOTE_PROFILE,
                request.request_id,
                profile.status,
                &answered.encode_to_vec(),
            )
        }
        command => response_frame(
            command,
            request.request_id,
            1,
            &longbridge_proto::Error {
                code: 1,
                msg: format!("the fixture serves no answer for command {command}"),
            }
            .encode_to_vec(),
        ),
    }
}

fn response_frame(command: u8, request_id: u32, status: u8, body: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(RESPONSE_FIELDS + body.len());
    frame.push(PACKAGE_TYPE_RESPONSE);
    frame.push(command);
    frame.extend_from_slice(&request_id.to_be_bytes());
    frame.push(status);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    Bytes::from(frame)
}

fn request_frame(payload: &[u8]) -> Option<ReceivedFrame> {
    if payload.len() < REQUEST_FIELDS {
        return None;
    }
    let body_start = REQUEST_FIELDS;
    let body_len =
        (usize::from(payload[8]) << 16) | (usize::from(payload[9]) << 8) | usize::from(payload[10]);
    if payload.len() != body_start + body_len {
        return None;
    }
    Some(ReceivedFrame {
        command: payload[1],
        request_id: u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]),
        body: Bytes::copy_from_slice(&payload[body_start..]),
    })
}

async fn read_request_head(
    socket: &mut tokio::net::TcpStream,
) -> Option<(String, Vec<(String, String)>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some((request_line, headers))
}

fn header_pairs(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}
