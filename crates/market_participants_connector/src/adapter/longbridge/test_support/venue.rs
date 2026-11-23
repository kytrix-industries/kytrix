use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use app_config::market_participants::origins::OriginPair;
use bytes::Bytes;
use longbridge_proto::quote::{PushQuote, PushTrade};
use smol_str::{SmolStr, format_smolstr};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    answers::{
        AuthAnswer, CalendarAnswer, CandleAnswer, ProfileAnswer, QuoteAnswer, ReceivedFrame,
        TokenAnswer,
    },
    serve::{QuoteAnswers, serve_quote_socket, serve_token},
};
use crate::{
    adapter::longbridge::{auth::SOCKET_TOKEN_PATH, signature::RequestSigning},
    runtime::LongbridgeCredentialValues,
};
pub(crate) const PACKAGE_TYPE_RESPONSE: u8 = 2;
pub(crate) const PACKAGE_TYPE_PUSH: u8 = 3;
pub(crate) const RESPONSE_FIELDS: usize = 10;
pub(crate) const PUSH_FIELDS: usize = 5;
pub(crate) const REQUEST_FIELDS: usize = 11;
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
pub(crate) const PUSH_REALTIME_DEPTH: u8 = 102;
pub(crate) const SETTLE: Duration = Duration::from_millis(50);
pub(crate) struct LongbridgeFixture {
    http_origin: Url,
    websocket_origin: Url,
    token_requests: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    upgrades: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    frames: Arc<Mutex<Vec<ReceivedFrame>>>,
    candles: Arc<Mutex<CandleAnswer>>,
    calendar: Arc<Mutex<CalendarAnswer>>,
    quotes: Arc<Mutex<QuoteAnswer>>,
    pushes: Arc<Mutex<VecDeque<Bytes>>>,
    cancellation: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl LongbridgeFixture {
    pub async fn install(token: TokenAnswer, auth: AuthAnswer, profile: ProfileAnswer) -> Self {
        Self::install_with(
            token,
            auth,
            profile,
            CandleAnswer::default(),
            QuoteAnswer::default(),
        )
        .await
    }

    pub async fn install_with(
        token: TokenAnswer,
        auth: AuthAnswer,
        profile: ProfileAnswer,
        candles: CandleAnswer,
        quotes: QuoteAnswer,
    ) -> Self {
        let token_requests = Arc::new(Mutex::new(Vec::new()));
        let upgrades = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let pushes = Arc::new(Mutex::new(VecDeque::new()));
        let candles = Arc::new(Mutex::new(candles));
        let calendar = Arc::new(Mutex::new(CalendarAnswer::default()));
        let quotes = Arc::new(Mutex::new(quotes));
        let cancellation = CancellationToken::new();

        let http = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the token fixture should bind");
        let http_address = http.local_addr().expect("the token fixture has an address");
        let socket = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the quote socket fixture should bind");
        let socket_address = socket
            .local_addr()
            .expect("the quote socket has an address");

        let token_task = tokio::spawn(serve_token(
            http,
            token,
            Arc::clone(&token_requests),
            cancellation.clone(),
        ));
        let socket_task = tokio::spawn(serve_quote_socket(
            socket,
            QuoteAnswers::new(
                auth,
                profile,
                Arc::clone(&candles),
                Arc::clone(&calendar),
                Arc::clone(&quotes),
                Arc::clone(&pushes),
            ),
            Arc::clone(&upgrades),
            Arc::clone(&frames),
            cancellation.clone(),
        ));

        Self {
            http_origin: Url::parse(&format!("http://{http_address}/"))
                .expect("the fixture address is a URL"),
            websocket_origin: Url::parse(&format!("ws://{socket_address}/"))
                .expect("the fixture address is a URL"),
            token_requests,
            upgrades,
            frames,
            candles,
            calendar,
            quotes,
            pushes,
            cancellation,
            tasks: vec![token_task, socket_task],
        }
    }

    pub fn serve_candles(&self, answer: CandleAnswer) {
        *self
            .candles
            .lock()
            .expect("the fixture answer is not poisoned") = answer;
    }

    pub fn serve_quotes(&self, answer: QuoteAnswer) {
        *self
            .quotes
            .lock()
            .expect("the fixture quote is not poisoned") = answer;
    }

    pub fn serve_calendar(&self, answer: CalendarAnswer) {
        *self
            .calendar
            .lock()
            .expect("the fixture calendar is not poisoned") = answer;
    }

    pub fn serve_half_days(&self, days: Vec<String>) {
        self.serve_calendar(CalendarAnswer {
            status: 0,
            half_days: days,
        });
    }

    pub fn push(&self, command: u8, body: &impl prost::Message) {
        let bytes = body.encode_to_vec();
        let mut frame = Vec::with_capacity(PUSH_FIELDS + bytes.len());
        frame.push(PACKAGE_TYPE_PUSH);
        frame.push(command);
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes()[1..]);
        frame.extend_from_slice(&bytes);
        self.pushes
            .lock()
            .expect("the fixture push queue is not poisoned")
            .push_back(Bytes::from(frame));
    }

    pub fn push_quote(&self, quote: &PushQuote) {
        self.push(PUSH_REALTIME_QUOTE, quote);
    }

    pub fn push_depth(&self, depth: &longbridge_proto::quote::PushDepth) {
        self.push(PUSH_REALTIME_DEPTH, depth);
    }

    pub fn push_trades(&self, trades: &PushTrade) {
        self.push(PUSH_REALTIME_TRADES, trades);
    }

    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: self.http_origin.clone(),
            websocket: self.websocket_origin.clone(),
        }
    }

    pub fn token_header_blocks(&self) -> Vec<Vec<(String, String)>> {
        self.token_requests
            .lock()
            .expect("the fixture request log is not poisoned")
            .clone()
    }

    pub fn token_signings(&self) -> Vec<(SmolStr, SmolStr)> {
        self.token_header_blocks()
            .into_iter()
            .map(|headers| {
                let read = |name: &str| {
                    headers
                        .iter()
                        .find(|(header, _)| header == name)
                        .map(|(_, value)| value.clone())
                };
                let token = read("authorization").unwrap_or_default();
                let key = read("x-api-key").unwrap_or_default();
                let timestamp: u64 = read("x-timestamp")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default();
                let expected = token_signature(&token, &key, timestamp);
                (
                    SmolStr::new(read("x-api-signature").unwrap_or_default()),
                    expected,
                )
            })
            .collect()
    }

    pub fn upgrade_header_blocks(&self) -> Vec<Vec<(String, String)>> {
        self.upgrades
            .lock()
            .expect("the fixture upgrade log is not poisoned")
            .clone()
    }

    pub fn frames(&self) -> Vec<ReceivedFrame> {
        self.frames
            .lock()
            .expect("the fixture frame log is not poisoned")
            .clone()
    }

    pub fn commands(&self) -> Vec<u8> {
        self.frames()
            .into_iter()
            .map(|frame| frame.command)
            .collect()
    }

    pub fn body_of<M: prost::Message + Default>(&self, command: u8) -> M {
        let frames = self.frames();
        let frame = frames
            .iter()
            .find(|frame| frame.command == command)
            .unwrap_or_else(|| {
                let received: Vec<u8> = frames.iter().map(|frame| frame.command).collect();
                panic!("the fixture received no command {command}, only {received:?}")
            });
        M::decode(frame.body.as_ref()).expect("the frame carries the venue's own message")
    }

    pub async fn await_command(&self, command: u8) -> ReceivedFrame {
        let deadline = tokio::time::Instant::now() + SETTLE * 20;
        loop {
            if let Some(frame) = self
                .frames()
                .into_iter()
                .find(|frame| frame.command == command)
            {
                return frame;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture never received command {command}, only {:?}",
                self.commands()
            );
            tokio::time::sleep(SETTLE / 5).await;
        }
    }

    pub async fn settled_frames(&self) -> Vec<ReceivedFrame> {
        let mut settled = self.frames();
        loop {
            tokio::time::sleep(SETTLE).await;
            let frames = self.frames();
            if frames.len() == settled.len() {
                return frames;
            }
            settled = frames;
        }
    }
}

impl Drop for LongbridgeFixture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn token_signature(token: &str, key: &str, timestamp: u64) -> SmolStr {
    let credentials = LongbridgeCredentialValues {
        app_key: SmolStr::new(key),
        app_secret: SmolStr::new_static(super::APP_SECRET),
        access_token: SmolStr::new(token),
    };
    RequestSigning::new("GET", SOCKET_TOKEN_PATH, "", None)
        .sign(&credentials, timestamp)
        .into_iter()
        .find(|(name, _)| *name == "x-api-signature")
        .map(|(_, value)| value)
        .unwrap_or_else(|| format_smolstr!("the request presented no signature"))
}
