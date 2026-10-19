//! Shared fixture infrastructure of this crate's tests.
//!
//! Three fixtures are shared. The venue's `info` endpoint is a real HTTP socket
//! that answers what the adapter's own encoder asked for, so several tests need
//! it and it is served from one place instead of being stubbed. The other is the
//! market a test loads: the definition table is crate-private, so a fixture that
//! wants a venue's statically declared scopes loaded reaches it through the
//! helpers below. The third is the root itself: every test that drives a load
//! builds the same root over the snapshot it wants and the transport it owns,
//! and a GPUI test installs one over its fixture catalogue through
//! [`install_root`].

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
#[cfg(any(test, feature = "test-support"))]
use app_config_provider::notification::ConfigNotification;
use market_participants_model::identifier::VenueId;
#[cfg(test)]
use network::transport::http::HttpClientClass;
#[cfg(any(test, feature = "test-support"))]
use network::transport::hub::{TransportHub, TransportPolicy};
#[cfg(any(test, feature = "test-support"))]
use parking_lot::{Mutex, RwLock};
#[cfg(test)]
use smol_str::SmolStr;
#[cfg(any(test, feature = "test-support"))]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[cfg(any(test, feature = "test-support"))]
use url::Url;
#[cfg(any(test, feature = "test-support"))]
use widget_tokio_runtime::runtime::Runtime;

#[cfg(any(test, feature = "test-support"))]
use crate::runtime::MarketParticipantNetworkSnapshot;
use crate::venue::definition::{VenueDefinition, definition_of};
#[cfg(any(test, feature = "test-support"))]
use crate::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
};

/// The definitions of the venues named, as a fixture loads them.
///
/// [`crate::connector::MarketParticipantsConnector::load_fixture`] takes the definition
/// table's entries, and the table itself is crate-private: this is how a fixture
/// outside this crate names the venues whose statically declared scopes it wants
/// loaded. The table answers every venue the identity carries, so a name the
/// identity does not know is the only one a fixture can ask for and not get — and
/// that is a mistake in the fixture rather than a case to answer with an empty
/// scope set.
pub fn definitions(venues: &[VenueId]) -> Vec<VenueDefinition> {
    venues
        .iter()
        .map(|venue| definition_of(*venue).clone())
        .collect()
}

/// Installs a root over one catalogue as the GPUI global.
///
/// A GPUI test installs this once and then drives the widget against the route,
/// the listing keys and the catalogue the fixture hands back. The application
/// runtime is the installed one when a test has one, so a fetch driven through
/// the root crosses the same boundary the application uses.
#[cfg(any(test, feature = "test-support"))]
pub fn install_root(cx: &mut gpui_kit::App, catalog: Arc<Catalog>) -> MarketParticipantsConnector {
    let runtime = widget_tokio_runtime::global::try_handle(cx).unwrap_or_else(Runtime::for_test);
    let root = root_over_with(catalog, runtime);
    root.attach_events(cx);
    cx.set_global(root.clone());
    root
}

#[cfg(any(test, feature = "test-support"))]
fn root_over_with(catalog: Arc<Catalog>, runtime: Runtime) -> MarketParticipantsConnector {
    let notification =
        ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::default()));
    MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            notification,
        )
        .with_runtime(runtime),
        catalog,
    )
    .expect("the fixture root builds from typed runtime inputs")
}

/// The settings snapshot one fixture installation belongs to.
///
/// A root is built over the settings its fixture addresses were installed into, so this is the
/// one place a test turns an installation into the shape a root takes. The snapshot owns the
/// configuration, which is what carries the addresses: a test holds the root and nothing else.
pub fn fixture_config(installation: &FixtureInstallation) -> Config {
    let mut config = Config::default();
    config.venues = installation.venues().clone();
    config
}

/// One fixture `info` server: the origin it serves on, the log of the request types
/// it has been asked, and the task that owns its socket.
#[cfg(test)]
pub(crate) type InfoFixture = (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>);

/// The transport policy a fixture server needs: the servers speak plain HTTP on a
/// loopback socket, and every other transport decision is the production one.
#[cfg(test)]
pub(crate) fn plaintext_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }
}

/// A fixture root over the snapshot `notification` carries, with the catalog wiring
/// attached.
///
/// This is the one place a test root is built: what varies between the fixtures of
/// this crate is the configuration the snapshot carries and the transport policy the
/// test owns, and both are the caller's. The handle returned is the directory the
/// root writes and reads through, so a fixture drives a real load through
/// the real gateway, registry, commit and request path.
#[cfg(test)]
pub(crate) fn connector_over(
    notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
    policy: TransportPolicy,
) -> (MarketParticipantsConnector, Arc<Catalog>) {
    let catalog = Arc::new(Catalog::new());
    let connector = MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(Arc::new(TransportHub::new(policy)), notification)
            .with_runtime(Runtime::for_test()),
        Arc::clone(&catalog),
    )
    .expect("the market participants root builds from the fixture configuration");
    (connector, catalog)
}

/// Serves one discovery conversation of a venue's `info` endpoint.
///
/// Every answer is selected by the request that arrived: the `perpDexs` answers
/// are handed out in order (the last one repeats), and a `meta` request is
/// answered with the metadata of the DEX it names. The fixture is therefore the
/// wire, not a stub of the adapter's own functions.
///
/// `core_meta` is the core perpetual route's metadata and `dex_meta` the metadata of one
/// deployed DEX each. A `metaAndAssetCtxs` request is answered with that metadata and an
/// empty context list, which is all it takes for a route's catalog to be derivable — and
/// therefore what lets a test drive a read that decodes against several routes' catalogs.
/// `spotMetaAndAssetCtxs` is answered from the spot metadata in the same way.
///
/// The second element of the answer is the log of the request types the fixture served, in
/// arrival order: a test about how many requests a caller makes — whether a remote answer
/// is bounded, for instance — counts them there instead of inferring the count from the
/// answer the caller got back.
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

/// The same server, with each deployment's `meta` answered from a switch.
///
/// One reading of a venue is one `perpDexs` answer plus the `meta` of every deployment
/// it named, and how many requests one reading makes is the adapter's business. A
/// fixture that advanced a body per request would answer one reading with two different
/// universes, or two readings with the same one, depending on the order they arrive in.
/// The switch is the test's instead: it holds the index of the body every `meta` answer
/// uses until the test moves it, which is what "the venue answers its next reading now"
/// means for a test that pins how a reader behaves across a refresh. A caller with one
/// body per deployment wants [`info_fixture_server`].
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

/// Answers one deployment's `meta` from the reading the switch names.
///
/// The index is the test's, not a per-request counter: every `meta` answer of one
/// reading uses the same body however many requests that reading makes and in whatever
/// order they arrive. An index past the last body repeats it.
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

/// Reads one HTTP request off one connection and answers the JSON body it carried.
///
/// The body is what the venue's own encoder wrote, so a fixture that counts requests sees
/// the requests themselves rather than the bytes that carried them — a client is free to
/// reuse one connection for several of them.
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

/// One answer the [`refusing_fixture_server`] hands out, in the order it hands them out.
#[derive(Clone)]
#[cfg(test)]
pub(crate) struct FixtureAnswer {
    status: u16,
    headers: SmolStr,
    body: SmolStr,
}

#[cfg(test)]
impl FixtureAnswer {
    /// One JSON answer the venue serves.
    pub(crate) fn ok(body: &str) -> Self {
        Self {
            status: 200,
            headers: SmolStr::default(),
            body: SmolStr::new(body),
        }
    }

    /// A refusal with a status and whatever the venue put in the body.
    pub(crate) fn refused(status: u16, headers: &str, body: &str) -> Self {
        Self {
            status,
            headers: SmolStr::new(headers),
            body: SmolStr::new(body),
        }
    }
}

/// Serves one conversation from a queue of answers this test states.
///
/// A venue that refuses a request answers with a status and a body and no envelope,
/// which is the shape the adapter's own HTTP failure mapping reads — and the shape a
/// test that pins what a client does about a refusal needs to state itself. The
/// answers are handed out one per request, the last one repeating, so a test can say
/// "refused, then served" as well as "refused however often it is asked".
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

/// One DEX metadata answer whose universe names `coins` and settles in the first
/// spot token.
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

/// The spot token table a DEX's collateral index resolves against.
#[cfg(test)]
pub(crate) fn spot_meta_answer(tokens: &[&str]) -> String {
    let tokens: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(index, name)| format!(r#"{{"name":"{name}","szDecimals":8,"index":{index}}}"#))
        .collect();
    format!(r#"{{"tokens":[{}],"universe":[]}}"#, tokens.join(","))
}

/// One catalog route of the fixture venues, as the fixture names it.
///
/// A route is keyed by the request its own venue's protocol addresses: the
/// `exchangeInfo` of one Binance REST base, or the `instruments` table of one OKX
/// instrument type. What a route serves is that venue's own answer, so a test that
/// changes one route's answer changes what one venue publishes and nothing else.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum CatalogRoute {
    /// Binance spot: `GET /api/v3/exchangeInfo`.
    BinanceSpot,
    /// Binance USD-M perpetuals: `GET /fapi/v1/exchangeInfo`.
    BinancePerp,
    /// OKX spot: `GET /api/v5/public/instruments?instType=SPOT`.
    OkxSpot,
    /// OKX swaps: `GET /api/v5/public/instruments?instType=SWAP`.
    OkxSwap,
}

/// The Binance USD-M perpetual `exchangeInfo` the fixture serves by default.
#[cfg(test)]
const BINANCE_PERPETUAL_ANSWER: &str = r#"{"timezone":"UTC","serverTime":1700000000000,"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT","contractType":"PERPETUAL","onboardDate":1569398400000,"requiredMarginPercent":"2.0000","maintMarginPercent":"1.0000","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;

/// The OKX spot `instruments` answer the fixture serves by default.
#[cfg(test)]
const OKX_SPOT_ANSWER: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live","listTime":"1500000000000"}]}"#;

/// The OKX swap `instruments` answer the fixture serves by default.
#[cfg(test)]
const OKX_SWAP_ANSWER: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}]}"#;

/// One Binance spot `exchangeInfo` answer listing one tradable pair per symbol.
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

/// The catalogue fixture: a socket that answers the catalog protocol of the built-in
/// venues, with one switch per catalog route.
///
/// The fixture is the wire, not a stub of the adapter's own functions: every answer is
/// the JSON the venue's protocol defines, so a request travels through the real
/// gateway, registry and adapter dispatch. A test states what one route answers — the
/// venue's table, a refusal, or a response held back — and reads which routes were
/// asked for, which is how a refresh and two concurrent venues are driven without a
/// venue behind them.
#[cfg(test)]
pub(crate) struct CatalogFixture {
    origin: String,
    state: Arc<Mutex<FixtureState>>,
    stall: Arc<Stall>,
    task: tokio::task::JoinHandle<()>,
}

/// What one [`CatalogFixture`] serves, route by route.
#[cfg(test)]
struct FixtureState {
    /// The answer of every route, as the venue's own protocol states it.
    bodies: AHashMap<CatalogRoute, SmolStr>,
    /// The routes that refuse every request from here on.
    refused: AHashSet<CatalogRoute>,
    /// The route whose requests are held until the fixture is released.
    stalled: Option<CatalogRoute>,
    /// The routes asked for so far, in arrival order.
    served: Vec<CatalogRoute>,
}

/// The stall control of one [`CatalogFixture`]: a held route reports that it was
/// asked and then waits for a latched release, so a release that happens before the
/// request arrives is never lost.
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
    /// Starts a fixture serving the catalog protocol of every fixture venue.
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

    /// The origin every catalog endpoint of this fixture is pointed at.
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    /// Serves `body` for one route from here on.
    pub(crate) fn serve(&self, route: CatalogRoute, body: &str) {
        self.state.lock().bodies.insert(route, SmolStr::new(body));
    }

    /// Refuses every request for one route from here on.
    pub(crate) fn refuse(&self, route: CatalogRoute) {
        self.state.lock().refused.insert(route);
    }

    /// Holds every request for one route until [`Self::release`].
    pub(crate) fn stall(&self, route: CatalogRoute) {
        self.state.lock().stalled = Some(route);
    }

    /// Waits until the stalled route has been asked for something.
    pub(crate) async fn await_stall(&self) {
        let permit = self
            .stall
            .started
            .acquire()
            .await
            .expect("the catalog fixture's stall is never closed");
        permit.forget();
    }

    /// Answers every held request.
    pub(crate) fn release(&self) {
        self.stall.released.store(true, Ordering::Release);
    }

    /// The routes asked for so far, in arrival order.
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

/// The answer every fixture route serves before a test states another one.
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

/// Serves one connection of catalog requests.
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

/// Reads one HTTP request off one connection and answers the path and query it
/// addressed.
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

/// The fixture route one request addresses, when the fixture serves it.
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

/// Whether one request's query asked OKX for one instrument type.
#[cfg(test)]
fn asks_instrument_type(query: &str, instrument_type: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair.strip_prefix("instType=") == Some(instrument_type))
}

/// A Hyperliquid socket that answers every `post` with one fixed info payload.
///
/// A Hyperliquid request path is a socket request, so driving one end to end needs
/// the venue's own wire on a real socket. The fixture answers the info frame the
/// venue sends — the request's own id echoed with the payload under `response` — and
/// interprets nothing else: the connection, the request id and the decode are all
/// the adapter's own. What it received is kept, so a test can assert what went on
/// the wire, and an `event` it was given is pushed down the socket as soon as a
/// subscription is asked for, which is what lets a test drive a stream's decoding.
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
    /// Serves the fixture on a fresh port, answering each subscription it receives with
    /// the next of `events`.
    ///
    /// One event per subscription is enough for a test that reads one stream's opening
    /// message; a test that wants a *later* message on the same venue subscription (the
    /// stream is shared by topic, so the second reader's subscribe never reaches the
    /// wire while the first still holds it) gives the socket a second event, which the
    /// next subscription — a re-acquisition of the released topic — receives.
    ///
    /// A test that has to decide *when* a frame arrives — because what it pins is how a
    /// stream behaves after the venue was re-read — pushes it with [`Self::push`]
    /// instead and leaves `events` empty.
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
                                            // A frame the test decides to push, sent as
                                            // soon as it asks for it.
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

    /// Returns the URL a context dials this fixture on.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Sends one frame down the socket the next time it is asked for one.
    ///
    /// The frame is delivered to the connection this fixture serves, when the caller
    /// says so rather than at subscription time: a test that pins how a stream behaves
    /// after the venue was re-read opens the stream first and pushes afterwards.
    pub(crate) fn push(&self, frame: String) {
        self.pushes
            .send(frame)
            .expect("the fixture socket is serving");
    }

    /// Returns every frame the fixture received so far.
    pub(crate) fn frames(&self) -> Vec<String> {
        self.frames
            .lock()
            .expect("the fixture frame log is not poisoned")
            .clone()
    }

    /// Returns the frames the wire settled on: it waits until the fixture received
    /// nothing new for a settle window, and never longer than a deadline.
    ///
    /// A test about how many frames went on the wire cannot simply sleep: a slow
    /// machine would make a correct implementation look like it never sent the frame,
    /// and an immediate read would make a forked topic look like it never sent a second
    /// one. Waiting for the log to stop growing is bounded either way.
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

/// The address symbol directories are read from.
///
/// The default is the market participant's own site, which the caller states: which host a
/// directory lives on is the adapter's own fact, and a build that answers it here
/// would be a second place to keep in step with it. A test replaces the address
/// through [`set_symbol_directory_root`] for as long as it holds the guard.
#[cfg(any(test, feature = "test-support"))]
pub fn symbol_directory_root(published: &str) -> Url {
    SYMBOL_DIRECTORY_ROOT
        .read()
        .clone()
        .unwrap_or_else(|| Url::parse(published).expect("the published directory address is a URL"))
}

/// Points symbol directory reads at `root` until the returned guard is dropped.
///
/// The guard restores the address that was in place before it, so a fixture cannot
/// leave a later test reading a socket that is already gone.
#[cfg(any(test, feature = "test-support"))]
pub fn set_symbol_directory_root(root: Url) -> SymbolDirectoryRootGuard {
    let mut installed = SYMBOL_DIRECTORY_ROOT.write();
    let guard = SymbolDirectoryRootGuard {
        previous: installed.take(),
    };
    *installed = Some(root);
    guard
}

/// The address a fixture installed for as long as it holds this guard.
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

/// The address one fixture installed, absent while the published one is read.
#[cfg(any(test, feature = "test-support"))]
static SYMBOL_DIRECTORY_ROOT: RwLock<Option<Url>> = RwLock::new(None);

/// The design's own `nasdaqlisted.txt` sample, verbatim.
///
/// Its generation timestamp carries eight fields, where `otherlisted.txt` states
/// the same line with seven: that difference is what the parser's ordering rule is
/// about, so both samples are kept exactly as the market participant publishes them.
///
/// A crate-level integration test drives the real adapter's directory fetch, which is
/// why this and the fixture below are reachable from a target outside this crate. The
/// address a download dials is one process-wide value, so such a target is the second
/// binary that can hold it; [`SymbolDirectoryFixture::install`] serializes on it either
/// way.
#[cfg(any(test, feature = "test-support"))]
pub const NASDAQ_LISTED_SAMPLE: &str = concat!(
    "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares\r\n",
    "AAAP|Pacer Barings CLO Market Flex ETF|G|N|N|100|Y|N\r\n",
    "AACG|ATA Creativity Global - American Depositary Shares, each representing two common shares|S|N|D|100|N|N\r\n",
    "File Creation Time: 1009202621:31|||||||\r\n",
);

/// The design's own `otherlisted.txt` sample, verbatim.
#[cfg(any(test, feature = "test-support"))]
pub const OTHER_LISTED_SAMPLE: &str = concat!(
    "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol\r\n",
    "A|Agilent Technologies, Inc. Common Stock|N|A|N|100|N|A\r\n",
    "AA|Alcoa Corporation Common Stock |N|AA|N|100|N|AA\r\n",
    "AAA|Alternative Access First Priority CLO Bond ETF|P|AAA|Y|100|N|AAA\r\n",
    "File Creation Time: 1009202621:31||||||\r\n",
);

/// What one symbol directory request is answered with.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub enum DirectoryAnswer {
    /// A directory file, served with a `200`.
    File(&'static str),
    /// A refusal status with no directory in it.
    Status(u16),
    /// A body longer than the bound the adapter reads.
    Oversize,
    /// A request that is accepted and never answered.
    Silent,
}

/// One loopback symbol directory, installed as the address a download dials.
///
/// The directory host is not the venue's origin, so the venue fixture address does
/// not reach it: this is the one place a test points symbol directory reads at a
/// socket it owns. A request for a file the fixture was not given is answered with
/// silence, so a test that forgot a file sees a deadline rather than another file's
/// body.
///
/// The address is one process-wide value, so only one fixture exists at a time: a
/// second [`Self::install`] waits until the first fixture is dropped. Without that
/// the tests of one process would point the address at each other's sockets, and
/// the one that read last would pass or fail by timing.
#[cfg(any(test, feature = "test-support"))]
pub struct SymbolDirectoryFixture {
    /// Held for the whole life of the fixture, so one fixture installs at a time.
    exclusive: Option<tokio::sync::MutexGuard<'static, ()>>,
    /// Restores the address that was read before this fixture was installed.
    root: Option<SymbolDirectoryRootGuard>,
    /// The head of every request the fixture answered, as `(lowercase name, value)`.
    ///
    /// A test reads it to state what a download did *not* send: the directory's host is
    /// not the venue's, so the headers the venue routes and authenticates by have no
    /// business on it.
    requests: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(any(test, feature = "test-support"))]
impl SymbolDirectoryFixture {
    /// Serves the answers a test states, one per published file name.
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
                            // One byte past the bound is enough to state the body, and
                            // the socket is held open after it: a reader that has no
                            // bound of its own therefore never sees the body end, and
                            // the bound is the only thing that can end the read.
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
                            // The socket stays open and is never answered: the request
                            // hangs until a deadline ends it.
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

    /// The head of every request the fixture answered.
    ///
    /// A test that states what a download sent reads this rather than the request that
    /// produced it: the download is the adapter's own, and the answer is what reached
    /// the directory's host.
    pub fn request_headers(&self) -> Vec<Vec<(String, String)>> {
        self.requests.lock().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for SymbolDirectoryFixture {
    fn drop(&mut self) {
        self.task.abort();
        // The address is restored once the socket it named is gone, so a later test
        // cannot read a fixture that has already stopped. The lock is released after
        // this body, so the next fixture sees the restored address.
        drop(self.root.take());
        drop(self.exclusive.take());
    }
}

/// The lock that keeps one symbol directory fixture installed at a time.
#[cfg(any(test, feature = "test-support"))]
static SYMBOL_DIRECTORY_FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Takes the exclusivity a symbol directory fixture has for its whole life.
///
/// The address is one value for the whole process, so a test that reads or replaces
/// it — rather than serving a directory through it — has to be the only one doing
/// so. [`SymbolDirectoryFixture::install`] takes this lock itself; this is the same
/// exclusivity for a test that is about the seam instead of about a download.
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn exclusive_symbol_directory() -> tokio::sync::MutexGuard<'static, ()> {
    SYMBOL_DIRECTORY_FIXTURES.lock().await
}

/// The bound the oversized-directory fixture streams past, in bytes.
///
/// It is the adapter's own bound stated a second time on purpose: a fixture that
/// imported it would keep streaming the right amount if the adapter's bound moved,
/// and the test would then pin nothing.
#[cfg(any(test, feature = "test-support"))]
const SYMBOL_DIRECTORY_FIXTURE_BOUND: usize = 16 * 1024 * 1024;

/// One request head as the directory fixture reads it: the path it addressed and the
/// headers it presented.
#[cfg(any(test, feature = "test-support"))]
struct DirectoryRequestHead {
    /// The request target, when the line stated one.
    path: Option<String>,
    /// Every header, as `(lowercase name, value)`.
    headers: Vec<(String, String)>,
}

/// Reads one request head off one connection, answering its target and its headers.
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

/// One execution whose HTTP client dials loopback over plaintext.
///
/// A symbol directory download uses the execution's client and its proxy line, and
/// never the venue's own origin, so this is all a directory fixture needs of a
/// venue. The origins are names the reserved `.invalid` domain answers for, which
/// resolve nowhere: a read that dialled the venue instead of the directory would
/// fail loudly rather than leave the machine.
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
