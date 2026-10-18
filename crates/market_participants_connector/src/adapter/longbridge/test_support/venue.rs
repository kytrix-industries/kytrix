//! The loopback venue itself: the two sockets a fixture serves and the frames it speaks.
//!
//! Two sockets stand for the venue: an HTTP one that answers its token endpoint, and a
//! WebSocket one that speaks the venue's own framing. Neither of them interprets a request
//! beyond the protocol: the token endpoint states the one-time password the fixture was
//! given, the socket answers the commands this build sends from the answers the test
//! configured — an AUTH with an authentication response, the profile command with the
//! profile, a candlestick command with the candles the test granted, the real-time quote
//! command with the quotes it granted, and so on — and every
//! other command is refused, so a frame a test did not expect fails the request that sent
//! it rather than being silently accepted.
//!
//! What the fixture keeps is what a test asserts on: the headers of every token request,
//! the headers of every WebSocket upgrade, and every frame it received with the command and
//! request id read off it. What it also carries is the venue's pushes: a test queues the
//! frames the venue would send unprompted, and the socket delivers them in the order the
//! test stated once the connection carries the subscription they belong to — which is the
//! one ordering the venue's own pushes have relative to this build's requests.

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
/// The packet type every frame the venue sends is.
pub(crate) const PACKAGE_TYPE_RESPONSE: u8 = 2;
/// The packet type the venue's own pushes are.
pub(crate) const PACKAGE_TYPE_PUSH: u8 = 3;
/// The bytes a response frame carries before its body.
pub(crate) const RESPONSE_FIELDS: usize = 10;
/// The bytes a push frame carries before its body.
pub(crate) const PUSH_FIELDS: usize = 5;
/// The bytes a request frame carries before its body.
pub(crate) const REQUEST_FIELDS: usize = 11;
/// The command code the venue authenticates a socket with.
pub(crate) const COMMAND_CODE_AUTH: u8 = 2;
/// The command code the venue answers a market-data profile with.
pub(crate) const QUERY_USER_QUOTE_PROFILE: u8 = 4;
/// The command code the venue registers a subscription with.
pub(crate) const SUBSCRIBE: u8 = 6;
/// The command code the venue closes a subscription with.
pub(crate) const UNSUBSCRIBE: u8 = 7;
/// The command code the venue answers a market's trading days with.
pub(crate) const GET_TRADING_DAYS: u8 = 9;
/// The command code the venue answers the real-time quote of one security with.
pub(crate) const QUERY_SECURITY_QUOTE: u8 = 11;
/// The command code the venue answers a subscription's opening screen with.
pub(crate) const GET_SECURITY_CANDLESTICKS: u8 = 19;
/// The command code the venue pages historical candles with.
pub(crate) const GET_SECURITY_HISTORY_CANDLESTICKS: u8 = 27;
/// The command code the venue pushes its real-time quote under.
pub(crate) const PUSH_REALTIME_QUOTE: u8 = 101;
/// The command code the venue pushes its real-time trades under.
pub(crate) const PUSH_REALTIME_TRADES: u8 = 104;
/// The command code the venue pushes its order book under, which this build does not read.
pub(crate) const PUSH_REALTIME_DEPTH: u8 = 102;
/// How long the fixture waits for the wire to settle before it answers a test.
pub(crate) const SETTLE: Duration = Duration::from_millis(50);
/// One venue served on loopback sockets.
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
    /// Serves one venue whose three answers are what is stated.
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

    /// Serves one venue whose candle and quote commands answer what is stated.
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

    /// Replaces the answer the venue's candlestick commands are served with.
    pub fn serve_candles(&self, answer: CandleAnswer) {
        *self
            .candles
            .lock()
            .expect("the fixture answer is not poisoned") = answer;
    }

    /// Replaces the answer the venue's real-time quote command is served with.
    pub fn serve_quotes(&self, answer: QuoteAnswer) {
        *self
            .quotes
            .lock()
            .expect("the fixture quote is not poisoned") = answer;
    }

    /// Replaces the answer the venue's trading-day command is served with.
    pub fn serve_calendar(&self, answer: CalendarAnswer) {
        *self
            .calendar
            .lock()
            .expect("the fixture calendar is not poisoned") = answer;
    }

    /// Replaces the half days the venue's trading-day answer states, as `YYYYMMDD`.
    pub fn serve_half_days(&self, days: Vec<String>) {
        self.serve_calendar(CalendarAnswer {
            status: 0,
            half_days: days,
        });
    }

    /// Queues a frame the venue sends unprompted.
    ///
    /// The queue is drained by the socket in the order the test stated it, once the
    /// connection carries the subscription they belong to, so a test states the venue's
    /// pushes without having to know when this build will be listening for them. The
    /// frames are the venue's own `Push` packets.
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

    /// Queues one quote push of one symbol.
    pub fn push_quote(&self, quote: &PushQuote) {
        self.push(PUSH_REALTIME_QUOTE, quote);
    }

    /// Queues one push of a command the candle wire does not read.
    ///
    /// A real session carries every subscription's frames, so a stream has to recognise
    /// the ones that are not its own and read on; this is how a test states one.
    pub fn push_depth(&self, depth: &longbridge_proto::quote::PushDepth) {
        self.push(PUSH_REALTIME_DEPTH, depth);
    }

    /// Queues one trade push of one symbol.
    pub fn push_trades(&self, trades: &PushTrade) {
        self.push(PUSH_REALTIME_TRADES, trades);
    }

    /// The pair of origins one execution dials the fixture on.
    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: self.http_origin.clone(),
            websocket: self.websocket_origin.clone(),
        }
    }

    /// The headers of every token request the fixture has answered.
    pub fn token_header_blocks(&self) -> Vec<Vec<(String, String)>> {
        self.token_requests
            .lock()
            .expect("the fixture request log is not poisoned")
            .clone()
    }

    /// The signature every token request the fixture answered presented (§4.9).
    ///
    /// Each answer is read back the way the venue reads it: the three signed values are
    /// taken from the headers themselves, the timestamp is parsed out of its own header,
    /// and the signature is recomputed over that instant with the venue's own algorithm.
    /// The pair is `(the signature header the request carried, the signature the
    /// algorithm states for the values it carried)` — a test compares the two, which is
    /// a statement about what the request proved rather than about which headers it had.
    ///
    /// A request that carries no signature at all is a sentence the venue itself would
    /// answer with `403201`: the pair states the header line as it was absent.
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

    /// The headers of every WebSocket upgrade the fixture has accepted.
    pub fn upgrade_header_blocks(&self) -> Vec<Vec<(String, String)>> {
        self.upgrades
            .lock()
            .expect("the fixture upgrade log is not poisoned")
            .clone()
    }

    /// Every frame the fixture has received so far.
    pub fn frames(&self) -> Vec<ReceivedFrame> {
        self.frames
            .lock()
            .expect("the fixture frame log is not poisoned")
            .clone()
    }

    /// The commands of the frames the fixture received, in order.
    pub fn commands(&self) -> Vec<u8> {
        self.frames()
            .into_iter()
            .map(|frame| frame.command)
            .collect()
    }

    /// The body of the first frame the venue received under one command, decoded.
    ///
    /// A test states the command it is about rather than an index, because the number of
    /// frames in front of it depends on how many handshakes the case needed: the
    /// handshake, the profile, the candle command and the trading-day read all precede the
    /// frame a stream test is about.
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

    /// Waits until the venue has received one frame under a command.
    ///
    /// A subscription frame is written by the connection's own actor rather than by the
    /// caller that registered it, so a test about what it carried has to wait for it: the
    /// actor may not have reached the socket yet when the call returns.
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

    /// The frames the wire settled on: it waits until the fixture received nothing
    /// new for a settle window, so a test about how many frames went out cannot fail
    /// on a slow machine that has not sent one yet.
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

/// The signature §4.9 states for one token request's own values.
///
/// The three values are read back out of the headers the request presented rather than
/// taken from the fixture's configuration, so a request that signed something other
/// than what it sent is a mismatch: the recomputation is over the token, the key and
/// the instant that travelled. The secret is the one the fixture's credentials carry,
/// because that is what the venue holds and what a signature has to be computed under.
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
