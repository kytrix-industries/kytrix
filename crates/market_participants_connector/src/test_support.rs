use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(test, feature = "test-support"))]
use ahash::AHashMap;
#[cfg(test)]
use ahash::AHashSet;
use app_config::{market_participants::fixture::FixtureInstallation, settings::Config};
#[cfg(test)]
use app_config::{market_participants::origins::OriginPair, network::ResolvedProxy};
#[cfg(test)]
use app_config_provider::notification::ConfigNotification;
use market_participants_model::identifier::VenueId;
#[cfg(test)]
use network::transport::http::HttpClientClass;
#[cfg(test)]
use network::transport::hub::{TransportHub, TransportPolicy};
#[cfg(any(test, feature = "test-support"))]
use parking_lot::{Mutex, RwLock};
#[cfg(test)]
use smol_str::SmolStr;
#[cfg(any(test, feature = "test-support"))]
use tokio::runtime::Handle;
#[cfg(any(test, feature = "test-support"))]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[cfg(any(test, feature = "test-support"))]
use url::Url;

#[cfg(test)]
use crate::runtime::MarketParticipantNetworkSnapshot;
use crate::venue::definition::{VenueDefinition, definition_of};
#[cfg(test)]
use crate::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
};

pub fn definitions(venues: &[VenueId]) -> Vec<VenueDefinition> {
    venues
        .iter()
        .map(|venue| definition_of(*venue).clone())
        .collect()
}

pub fn test_handle() -> Handle {
    static SHARED: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

    SHARED
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a test runtime must build")
        })
        .handle()
        .clone()
}

pub fn fixture_config(installation: &FixtureInstallation) -> Config {
    let mut config = Config::default();
    config.venues = installation.venues().clone();
    config
}

#[cfg(test)]
pub(crate) type InfoFixture = (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>);

#[cfg(test)]
pub(crate) fn plaintext_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) fn connector_over(
    notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
    policy: TransportPolicy,
) -> (MarketParticipantsConnector, Arc<Catalog>) {
    let catalog = Arc::new(Catalog::new());
    let connector = MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(Arc::new(TransportHub::new(policy)), notification)
            .with_runtime(test_handle()),
        Arc::clone(&catalog),
    )
    .expect("the market participants root builds from the fixture configuration");
    (connector, catalog)
}

#[cfg(test)]
pub(crate) async fn info_fixture_server(
    perp_dexs: Vec<String>,
    spot_meta: String,
    core_meta: String,
    dex_meta: Vec<(String, String)>,
) -> InfoFixture {
    info_fixture_server_with_readings(
        perp_dexs,
        spot_meta,
        core_meta,
        dex_meta
            .into_iter()
            .map(|(dex, body)| (dex, vec![body]))
            .collect(),
    )
    .await
    .0
}

#[cfg(test)]
pub(crate) async fn info_fixture_server_with_readings(
    perp_dexs: Vec<String>,
    spot_meta: String,
    core_meta: String,
    dex_meta: Vec<(String, Vec<String>)>,
) -> (InfoFixture, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the fixture server should bind");
    let address = listener
        .local_addr()
        .expect("the fixture server has an address");
    let answers = Arc::new(Mutex::new(perp_dexs));
    let reading = Arc::new(Mutex::new(0_usize));
    let served = Arc::new(Mutex::new(0_usize));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task_reading = Arc::clone(&reading);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let answers = Arc::clone(&answers);
            let reading = Arc::clone(&task_reading);
            let served = Arc::clone(&served);
            let requests = Arc::clone(&task_requests);
            let spot_meta = spot_meta.clone();
            let core_meta = core_meta.clone();
            let dex_meta = dex_meta.clone();
            tokio::spawn(async move {
                while let Some(request) = read_info_request(&mut socket).await {
                    let request_type = request
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    requests.lock().push(request_type.clone());
                    let answer = match request_type.as_str() {
                        "perpDexs" => {
                            let mut served = served.lock();
                            let index = (*served).min(answers.lock().len().saturating_sub(1));
                            *served += 1;
                            answers
                                .lock()
                                .get(index)
                                .cloned()
                                .unwrap_or_else(|| "null".to_owned())
                        }
                        "spotMeta" => spot_meta.clone(),
                        "spotMetaAndAssetCtxs" => format!("[{spot_meta},[]]"),
                        "meta" => match request.get("dex").and_then(|value| value.as_str()) {
                            Some(dex) => next_dex_body(&dex_meta, &reading, dex)
                                .unwrap_or_else(|| "null".to_owned()),
                            None => "null".to_owned(),
                        },
                        "metaAndAssetCtxs" => {
                            let meta = match request.get("dex").and_then(|value| value.as_str()) {
                                Some(dex) => next_dex_body(&dex_meta, &reading, dex),
                                None => Some(core_meta.clone()),
                            };
                            format!("[{},[]]", meta.unwrap_or_else(|| "null".to_owned()))
                        }
                        "outcomeMeta" => r#"{"outcomes":[],"questions":[]}"#.to_owned(),
                        "outcomeTemplates" => "[]".to_owned(),
                        "allMids" => "{}".to_owned(),
                        _ => "null".to_owned(),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        answer.len(),
                        answer
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    ((format!("http://{address}"), requests, task), reading)
}

#[cfg(test)]
fn next_dex_body(
    dex_meta: &[(String, Vec<String>)],
    reading: &Mutex<usize>,
    dex: &str,
) -> Option<String> {
    let (_, bodies) = dex_meta.iter().find(|(name, _)| name == dex)?;
    if bodies.is_empty() {
        return None;
    }
    let index = *reading.lock();
    bodies.get(index.min(bodies.len() - 1)).cloned()
}

#[cfg(test)]
pub(crate) async fn read_info_request(
    socket: &mut tokio::net::TcpStream,
) -> Option<serde_json::Value> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[header_end..header_end + length]).ok()
}

#[derive(Clone)]
#[cfg(test)]
pub(crate) struct FixtureAnswer {
    status: u16,
    headers: SmolStr,
    body: SmolStr,
}

#[cfg(test)]
impl FixtureAnswer {
    pub(crate) fn ok(body: &str) -> Self {
        Self {
            status: 200,
            headers: SmolStr::default(),
            body: SmolStr::new(body),
        }
    }

    pub(crate) fn refused(status: u16, headers: &str, body: &str) -> Self {
        Self {
            status,
            headers: SmolStr::new(headers),
            body: SmolStr::new(body),
        }
    }
}

#[cfg(test)]
pub(crate) async fn refusing_fixture_server(answers: Vec<FixtureAnswer>) -> InfoFixture {
    assert!(!answers.is_empty(), "a fixture needs one answer to serve");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the fixture server should bind");
    let address = listener
        .local_addr()
        .expect("the fixture server has an address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let served = Arc::new(Mutex::new(0_usize));
    let task_requests = Arc::clone(&requests);
    let task_answers = Arc::new(answers);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let requests = Arc::clone(&task_requests);
            let served = Arc::clone(&served);
            let answers = Arc::clone(&task_answers);
            tokio::spawn(async move {
                while let Some(request) = read_info_request(&mut socket).await {
                    requests.lock().push(
                        request
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                    let answer = {
                        let mut served = served.lock();
                        let index = (*served).min(answers.len() - 1);
                        *served += 1;
                        answers[index].clone()
                    };
                    let response = format!(
                        "HTTP/1.1 {} Served\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\n\r\n{}",
                        answer.status,
                        answer.headers,
                        answer.body.len(),
                        answer.body
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (format!("http://{address}"), requests, task)
}

#[cfg(test)]
pub(crate) fn dex_meta(coins: &[&str]) -> String {
    let universe: Vec<String> = coins
        .iter()
        .map(|coin| format!(r#"{{"name":"{coin}","szDecimals":3,"maxLeverage":10}}"#))
        .collect();
    format!(
        r#"{{"universe":[{}],"marginTables":[],"collateralToken":0}}"#,
        universe.join(",")
    )
}

#[cfg(test)]
pub(crate) fn spot_meta_answer(tokens: &[&str]) -> String {
    let tokens: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(index, name)| format!(r#"{{"name":"{name}","szDecimals":8,"index":{index}}}"#))
        .collect();
    format!(r#"{{"tokens":[{}],"universe":[]}}"#, tokens.join(","))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum CatalogRoute {
    BinanceSpot,
    BinancePerp,
    OkxSpot,
    OkxSwap,
}

#[cfg(test)]
const BINANCE_PERPETUAL_ANSWER: &str = r#"{"timezone":"UTC","serverTime":1700000000000,"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT","contractType":"PERPETUAL","onboardDate":1569398400000,"requiredMarginPercent":"2.0000","maintMarginPercent":"1.0000","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;

#[cfg(test)]
const OKX_SPOT_ANSWER: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live","listTime":"1500000000000"}]}"#;

#[cfg(test)]
const OKX_SWAP_ANSWER: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}]}"#;

#[cfg(test)]
pub(crate) fn binance_spot_market_participant_info(symbols: &[&str]) -> String {
    let pairs: Vec<String> = symbols
        .iter()
        .map(|symbol| {
            let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
            format!(
                r#"{{"symbol":"{symbol}","status":"TRADING","baseAsset":"{base}","quoteAsset":"USDT","onboardDate":1500000000000,"filters":[{{"filterType":"PRICE_FILTER","tickSize":"0.01000000"}},{{"filterType":"LOT_SIZE","stepSize":"0.00001000","minQty":"0.00001000","maxQty":"9000.00000000"}},{{"filterType":"NOTIONAL","minNotional":"5.00000000"}}]}}"#
            )
        })
        .collect();
    format!(
        r#"{{"timezone":"UTC","serverTime":1700000000000,"symbols":[{}]}}"#,
        pairs.join(",")
    )
}

#[cfg(test)]
pub(crate) struct CatalogFixture {
    origin: String,
    state: Arc<Mutex<FixtureState>>,
    stall: Arc<Stall>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
struct FixtureState {
    bodies: AHashMap<CatalogRoute, SmolStr>,
    refused: AHashSet<CatalogRoute>,
    stalled: Option<CatalogRoute>,
    served: Vec<CatalogRoute>,
}

#[cfg(test)]
struct Stall {
    started: tokio::sync::Semaphore,
    released: AtomicBool,
}

#[cfg(test)]
impl Stall {
    fn new() -> Self {
        Self {
            started: tokio::sync::Semaphore::new(0),
            released: AtomicBool::new(false),
        }
    }

    fn begin(&self) {
        self.started.add_permits(1);
    }

    async fn wait(&self) {
        while !self.released.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
impl CatalogFixture {
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the catalog fixture should bind");
        let address = listener
            .local_addr()
            .expect("the catalog fixture has an address");
        let state = Arc::new(Mutex::new(FixtureState {
            bodies: default_catalog_bodies(),
            refused: AHashSet::new(),
            stalled: None,
            served: Vec::new(),
        }));
        let stall = Arc::new(Stall::new());
        let task_state = Arc::clone(&state);
        let task_stall = Arc::clone(&stall);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = Arc::clone(&task_state);
                let stall = Arc::clone(&task_stall);
                tokio::spawn(serve_catalog_requests(socket, state, stall));
            }
        });
        Self {
            origin: format!("http://{address}"),
            state,
            stall,
            task,
        }
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) fn serve(&self, route: CatalogRoute, body: &str) {
        self.state.lock().bodies.insert(route, SmolStr::new(body));
    }

    pub(crate) fn refuse(&self, route: CatalogRoute) {
        self.state.lock().refused.insert(route);
    }

    pub(crate) fn stall(&self, route: CatalogRoute) {
        self.state.lock().stalled = Some(route);
    }

    pub(crate) async fn await_stall(&self) {
        let permit = self
            .stall
            .started
            .acquire()
            .await
            .expect("the catalog fixture's stall is never closed");
        permit.forget();
    }

    pub(crate) fn release(&self) {
        self.stall.released.store(true, Ordering::Release);
    }

    pub(crate) fn served(&self) -> Vec<CatalogRoute> {
        self.state.lock().served.clone()
    }
}

#[cfg(test)]
impl Drop for CatalogFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
fn default_catalog_bodies() -> AHashMap<CatalogRoute, SmolStr> {
    AHashMap::from_iter([
        (
            CatalogRoute::BinanceSpot,
            SmolStr::new(binance_spot_market_participant_info(&["BTCUSDT"])),
        ),
        (
            CatalogRoute::BinancePerp,
            SmolStr::new_static(BINANCE_PERPETUAL_ANSWER),
        ),
        (CatalogRoute::OkxSpot, SmolStr::new_static(OKX_SPOT_ANSWER)),
        (CatalogRoute::OkxSwap, SmolStr::new_static(OKX_SWAP_ANSWER)),
    ])
}

#[cfg(test)]
async fn serve_catalog_requests(
    mut socket: tokio::net::TcpStream,
    state: Arc<Mutex<FixtureState>>,
    stall: Arc<Stall>,
) {
    while let Some((path, query)) = read_catalog_request(&mut socket).await {
        let Some(route) = catalog_route_of(&path, &query) else {
            panic!("the catalog fixture serves no answer for {path}?{query}");
        };
        let (answer, held) = {
            let mut state = state.lock();
            state.served.push(route);
            let answer = (!state.refused.contains(&route))
                .then(|| state.bodies.get(&route).cloned().unwrap_or_default());
            (answer, state.stalled == Some(route))
        };
        if held {
            stall.begin();
            stall.wait().await;
        }
        let response = match answer {
            Some(body) => format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            None => {
                let body = "the catalog fixture refuses this route";
                format!(
                    "HTTP/1.1 422 Refused\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            }
        };
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
async fn read_catalog_request(socket: &mut tokio::net::TcpStream) -> Option<(SmolStr, SmolStr)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let request_line = headers.lines().next().unwrap_or_default();
    let mut fields = request_line.split(' ');
    let _method = fields.next().unwrap_or_default();
    let target = fields.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some((SmolStr::new(path), SmolStr::new(query)))
}

#[cfg(test)]
fn catalog_route_of(path: &str, query: &str) -> Option<CatalogRoute> {
    match path {
        "/api/v3/exchangeInfo" => Some(CatalogRoute::BinanceSpot),
        "/fapi/v1/exchangeInfo" => Some(CatalogRoute::BinancePerp),
        "/api/v5/public/instruments" if asks_instrument_type(query, "SPOT") => {
            Some(CatalogRoute::OkxSpot)
        }
        "/api/v5/public/instruments" if asks_instrument_type(query, "SWAP") => {
            Some(CatalogRoute::OkxSwap)
        }
        _ => None,
    }
}

#[cfg(test)]
fn asks_instrument_type(query: &str, instrument_type: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair.strip_prefix("instType=") == Some(instrument_type))
}

#[cfg(test)]
pub(crate) struct InfoSocket {
    url: String,
    frames: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pushes: tokio::sync::broadcast::Sender<String>,
    cancellation: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
impl InfoSocket {
    pub(crate) async fn spawn(payload: String, events: Vec<String>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fixture socket should bind");
        let address = listener
            .local_addr()
            .expect("the fixture socket has an address");
        let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancellation = tokio_util::sync::CancellationToken::new();
        let accepting = cancellation.clone();
        let task_frames = std::sync::Arc::clone(&frames);
        let (pushes, _) = tokio::sync::broadcast::channel(8);
        let task_pushes = pushes.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = accepting.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let payload = payload.clone();
                let events = events.clone();
                let connection_pushes = task_pushes.clone();
                let connection_cancellation = accepting.clone();
                let frames = std::sync::Arc::clone(&task_frames);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |mut request: hyper::Request<hyper::body::Incoming>| {
                            let payload = payload.clone();
                            let events = events.clone();
                            let mut pushed = connection_pushes.clone().subscribe();
                            let frames = std::sync::Arc::clone(&frames);
                            let connection_cancellation = connection_cancellation.clone();
                            async move {
                                let (response, upgrade) = yawc::WebSocket::upgrade(&mut request)?;
                                tokio::spawn(async move {
                                    use futures::SinkExt as _;
                                    let Ok(mut socket) = upgrade.await else {
                                        return;
                                    };
                                    let mut pending = events.into_iter();
                                    loop {
                                        enum Next {
                                            Frame(Result<yawc::Frame, yawc::WebSocketError>),
                                            Push(String),
                                        }
                                        let next = tokio::select! {
                                            () = connection_cancellation.cancelled() => return,
                                            frame = socket.next_frame() => Next::Frame(frame),
                                            pushed = pushed.recv() => match pushed {
                                                Ok(frame) => Next::Push(frame),
                                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                            },
                                        };
                                        let frame = match next {
                                            Next::Frame(frame) => frame,
                                            Next::Push(frame) => {
                                                if socket
                                                    .send(yawc::Frame::text(frame))
                                                    .await
                                                    .is_err()
                                                {
                                                    return;
                                                }
                                                continue;
                                            }
                                        };
                                        let Ok(frame) = frame else { return };
                                        if !matches!(
                                            frame.opcode(),
                                            yawc::OpCode::Text | yawc::OpCode::Binary
                                        ) {
                                            continue;
                                        }
                                        let received =
                                            String::from_utf8_lossy(frame.payload()).into_owned();
                                        frames
                                            .lock()
                                            .expect("the fixture frame log is not poisoned")
                                            .push(received.clone());
                                        if let Ok(request) =
                                            serde_json::from_str::<SocketRequestId>(&received)
                                        {
                                            let answer = format!(
                                                r#"{{"channel":"post","data":{{"id":{id},"response":{{"type":"info","payload":{{"type":"info","data":{payload}}}}}}}}}"#,
                                                id = request.id,
                                            );
                                            if socket.send(yawc::Frame::text(answer)).await.is_err()
                                            {
                                                return;
                                            }
                                            continue;
                                        }
                                        if received.contains(r#""method":"subscribe""#)
                                            && let Some(event) = pending.next()
                                            && socket.send(yawc::Frame::text(event)).await.is_err()
                                        {
                                            return;
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
        });
        Self {
            url: format!("ws://{address}/ws"),
            frames,
            pushes,
            cancellation,
            task,
        }
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn push(&self, frame: String) {
        self.pushes
            .send(frame)
            .expect("the fixture socket is serving");
    }

    pub(crate) fn frames(&self) -> Vec<String> {
        self.frames
            .lock()
            .expect("the fixture frame log is not poisoned")
            .clone()
    }

    pub(crate) async fn settled_frames(&self) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut settled = self.frames();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let frames = self.frames();
            if frames.len() == settled.len() || std::time::Instant::now() >= deadline {
                return frames;
            }
            settled = frames;
        }
    }

    pub(crate) async fn shutdown(self) {
        self.cancellation.cancel();
        let _ = self.task.await;
    }
}

#[cfg(test)]
#[derive(serde::Deserialize)]
struct SocketRequestId {
    id: u64,
}

#[cfg(any(test, feature = "test-support"))]
pub fn symbol_directory_root(published: &str) -> Url {
    SYMBOL_DIRECTORY_ROOT
        .read()
        .clone()
        .unwrap_or_else(|| Url::parse(published).expect("the published directory address is a URL"))
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_symbol_directory_root(root: Url) -> SymbolDirectoryRootGuard {
    let mut installed = SYMBOL_DIRECTORY_ROOT.write();
    let guard = SymbolDirectoryRootGuard {
        previous: installed.take(),
    };
    *installed = Some(root);
    guard
}

#[cfg(any(test, feature = "test-support"))]
pub struct SymbolDirectoryRootGuard {
    previous: Option<Url>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for SymbolDirectoryRootGuard {
    fn drop(&mut self) {
        *SYMBOL_DIRECTORY_ROOT.write() = self.previous.take();
    }
}

#[cfg(any(test, feature = "test-support"))]
static SYMBOL_DIRECTORY_ROOT: RwLock<Option<Url>> = RwLock::new(None);

#[cfg(any(test, feature = "test-support"))]
pub const NASDAQ_LISTED_SAMPLE: &str = concat!(
    "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares\r\n",
    "AAAP|Pacer Barings CLO Market Flex ETF|G|N|N|100|Y|N\r\n",
    "AACG|ATA Creativity Global - American Depositary Shares, each representing two common shares|S|N|D|100|N|N\r\n",
    "File Creation Time: 1009202621:31|||||||\r\n",
);

#[cfg(any(test, feature = "test-support"))]
pub const OTHER_LISTED_SAMPLE: &str = concat!(
    "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol\r\n",
    "A|Agilent Technologies, Inc. Common Stock|N|A|N|100|N|A\r\n",
    "AA|Alcoa Corporation Common Stock |N|AA|N|100|N|AA\r\n",
    "AAA|Alternative Access First Priority CLO Bond ETF|P|AAA|Y|100|N|AAA\r\n",
    "File Creation Time: 1009202621:31||||||\r\n",
);

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub enum DirectoryAnswer {
    File(&'static str),
    Status(u16),
    Oversize,
    Silent,
}

#[cfg(any(test, feature = "test-support"))]
pub struct SymbolDirectoryFixture {
    exclusive: Option<tokio::sync::MutexGuard<'static, ()>>,
    root: Option<SymbolDirectoryRootGuard>,
    requests: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(any(test, feature = "test-support"))]
impl SymbolDirectoryFixture {
    pub async fn install(answers: Vec<(&'static str, DirectoryAnswer)>) -> Self {
        let exclusive = exclusive_symbol_directory().await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the directory fixture should bind");
        let address = listener
            .local_addr()
            .expect("the directory fixture has an address");
        let root = Url::parse(&format!("http://{address}/dynamic/SymDir/"))
            .expect("the fixture address is a URL");
        let answers: AHashMap<&str, DirectoryAnswer> = answers.into_iter().collect();
        let requests: Arc<Mutex<Vec<Vec<(String, String)>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let answers = answers.clone();
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let Some(head) = read_request_head(&mut socket).await else {
                        return;
                    };
                    recorded.lock().push(head.headers.clone());
                    let path = head.path;
                    let answer = path.as_deref().and_then(|path| {
                        answers
                            .iter()
                            .find(|(file, _)| path.ends_with(*file))
                            .map(|(_, answer)| answer.clone())
                    });
                    match answer {
                        Some(DirectoryAnswer::File(body)) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            );
                            if socket.write_all(head.as_bytes()).await.is_err() {
                                return;
                            }
                            let _ = socket.write_all(body.as_bytes()).await;
                        }
                        Some(DirectoryAnswer::Status(status)) => {
                            let body = "the directory is not served here";
                            let head = format!(
                                "HTTP/1.1 {status} Refused\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = socket.write_all(head.as_bytes()).await;
                        }
                        Some(DirectoryAnswer::Oversize) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                                SYMBOL_DIRECTORY_FIXTURE_BOUND + 1
                            );
                            if socket.write_all(head.as_bytes()).await.is_err() {
                                return;
                            }
                            let chunk = vec![b'A'; 1024 * 1024];
                            for _ in 0..=(SYMBOL_DIRECTORY_FIXTURE_BOUND / chunk.len()) {
                                if socket.write_all(&chunk).await.is_err() {
                                    return;
                                }
                            }
                            std::future::pending::<()>().await;
                        }
                        Some(DirectoryAnswer::Silent) | None => {
                            std::future::pending::<()>().await;
                        }
                    }
                });
            }
        });
        Self {
            exclusive: Some(exclusive),
            root: Some(set_symbol_directory_root(root)),
            requests,
            task,
        }
    }

    pub fn request_headers(&self) -> Vec<Vec<(String, String)>> {
        self.requests.lock().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for SymbolDirectoryFixture {
    fn drop(&mut self) {
        self.task.abort();
        drop(self.root.take());
        drop(self.exclusive.take());
    }
}

#[cfg(any(test, feature = "test-support"))]
static SYMBOL_DIRECTORY_FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn exclusive_symbol_directory() -> tokio::sync::MutexGuard<'static, ()> {
    SYMBOL_DIRECTORY_FIXTURES.lock().await
}

#[cfg(any(test, feature = "test-support"))]
const SYMBOL_DIRECTORY_FIXTURE_BOUND: usize = 16 * 1024 * 1024;

#[cfg(any(test, feature = "test-support"))]
struct DirectoryRequestHead {
    path: Option<String>,
    headers: Vec<(String, String)>,
}

#[cfg(any(test, feature = "test-support"))]
async fn read_request_head(socket: &mut tokio::net::TcpStream) -> Option<DirectoryRequestHead> {
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
    let path = lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .nth(1)
        .map(|target| target.to_owned());
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some(DirectoryRequestHead { path, headers })
}

#[cfg(test)]
pub(crate) fn loopback_execution(
    route: market_participants_model::listing::route::VenueRoute,
    capability: market_participants_model::routing::CapabilityId,
) -> crate::adapter::ExecutionContext {
    let transport = Arc::new(TransportHub::new(plaintext_policy()));
    let client = transport
        .http()
        .client(&ResolvedProxy::NoProxy, HttpClientClass::Public)
        .expect("the fixture HTTP client builds");
    let origins = OriginPair {
        http: format!("https://{}.invalid", route.venue.as_str())
            .parse()
            .expect("the fixture origin is a URL"),
        websocket: format!("wss://{}.invalid", route.venue.as_str())
            .parse()
            .expect("the fixture origin is a URL"),
    };
    let identity = crate::protocol::RequestIdentity::new(route, capability);
    crate::adapter::ExecutionContext::new(
        identity,
        origins,
        client,
        Arc::new(crate::connection::ConnectionPool::with_transport(transport)),
    )
}
