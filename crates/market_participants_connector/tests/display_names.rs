//! The display names the real adapter path publishes, against the name table.
//!
//! Every name in this file is produced by the built-in routes of the four
//! implemented venues from hand-written, protocol-shaped fixture answers served
//! over a real socket to the real adapter code: the fixture below answers each
//! request with the JSON the venue's own protocol defines — Binance's
//! `exchangeInfo`, OKX's `instruments` and Hyperliquid's `info` — carrying the
//! values those protocols carry, such as the OKX `ctVal = 0.01` that no name states.
//! The built-in descriptors are registered against that origin and the catalog of
//! each route is read through the market participants root's own catalogue exactly as the
//! application reads it. No venue is contacted and no recorded venue response is
//! replayed, so what these names prove is the shape of the real
//! `catalog → route → adapter → canonical terms → publication` path and the strings it publishes for the routes registered today — not live venue
//! acceptance, which is the limit this stage draws for itself. They are not a
//! synthetic catalog handed to the naming gate either: the adapter itself parses
//! each answer and builds the listings from it.
//!
//! Longbridge is the one row whose directory is not the venue's own origin: its
//! listings come from the market participant's published symbol directory, so the row below is
//! served by [`SymbolDirectoryFixture`] on the address the adapter's download dials,
//! and its venue origin — which no catalog read touches — is the same fixture as the
//! others.
//!
//! What this file is the outside view of: a canonical identity is a digest of its
//! terms, so the adapter stages the terms it built beside every listing, and the
//! publication that renders a name refuses the whole batch when one of them is
//! missing. The venue-native spellings (`BTCUSDT`, `BTC-USDT`, `BTC-USDT-SWAP`,
//! the Hyperliquid `@10` pair and the HIP-3 `xyz:ABC` coin) stay search aliases:
//! none of them reaches a name.

use std::{sync::Arc, time::Duration};

use app_config::market_participants::fixture::{self, FixtureAddress};
use market_participants_connector::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    runtime::MarketParticipantNetworkSnapshot,
    test_support::{
        DirectoryAnswer, NASDAQ_LISTED_SAMPLE, OTHER_LISTED_SAMPLE, SymbolDirectoryFixture,
        fixture_config,
    },
};
use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    settlement::DerivativePnlModel,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use smol_str::SmolStr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use widget_tokio_runtime::runtime::Runtime;

const SETUP_TIMEOUT: Duration = Duration::from_secs(20);

/// The venue names the fixture routes belong to.
const BINANCE: &str = "binance";
const OKX: &str = "okx";
const HYPERLIQUID: &str = "hyperliquid";
const LONGBRIDGE: &str = "longbridge";

/// The spot symbol Binance publishes for the row of the name table.
const BINANCE_SPOT: &str = r#"{"timezone":"UTC","serverTime":1700000000000,"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","onboardDate":1500000000000,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","stepSize":"0.00001000","minQty":"0.00001000","maxQty":"9000.00000000"},{"filterType":"NOTIONAL","minNotional":"5.00000000"}]}]}"#;

/// The USD-M perpetual Binance publishes for the row of the name table.
const BINANCE_PERPETUAL: &str = r#"{"timezone":"UTC","serverTime":1700000000000,"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT","contractType":"PERPETUAL","onboardDate":1569398400000,"requiredMarginPercent":"2.0000","maintMarginPercent":"1.0000","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;

/// The OKX spot instruments of the row of the name table.
const OKX_SPOT: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live","listTime":"1500000000000"}]}"#;

/// The OKX swaps of the route: the linear contract of the name table and the
/// inverse one whose settlement is converted through its own mark of the asset it
/// tracks.
const OKX_SWAP: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"},{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"0.1","minSz":"0.1","maxLmtSz":"10000","ctVal":"100","ctValCcy":"USD","ctType":"inverse","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}]}"#;

/// The core perpetual DEX: its universe and the spot token it settles in.
const HYPERLIQUID_CORE_META: &str = r#"{"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":1}"#;

/// The builder-deployed DEX: its coins carry the `{dex}:{coin}` form and it
/// settles in a spot token of its own.
const HYPERLIQUID_DEX_META: &str = r#"{"universe":[{"name":"xyz:ABC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":0}"#;

/// A second builder-deployed DEX that lists the core coin spelled the way a
/// deployed scope spells it, and settles in the core collateral.
const HYPERLIQUID_CORE_COIN_DEX_META: &str = r#"{"universe":[{"name":"abc:BTC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":1}"#;

/// The venue's spot token table and tradable pairs: the core pair of the name table is the
/// `@10` pair, which exchanges the BTC token for the USDC token.
const HYPERLIQUID_SPOT_META: &str = r#"{"tokens":[{"name":"USD","szDecimals":8,"index":0},{"name":"USDC","szDecimals":8,"index":1},{"name":"BTC","szDecimals":8,"index":2}],"universe":[{"name":"@10","tokens":[2,1],"index":10}]}"#;

/// One request the fixture answers.
struct FixtureRequest {
    method: SmolStr,
    path: SmolStr,
    query: SmolStr,
    body: SmolStr,
}

impl FixtureRequest {
    /// Returns whether the request addressed one path with one query parameter.
    fn asks(&self, path: &str, parameter: &str) -> bool {
        self.path == path && self.query.split('&').any(|pair| pair == parameter)
    }

    /// Returns the JSON body the request carried, when it carried one.
    fn payload(&self) -> Option<serde_json::Value> {
        (!self.body.is_empty())
            .then(|| serde_json::from_str(self.body.as_str()).ok())
            .flatten()
    }
}

/// The answer one venue request is served with.
///
/// The fixture is the wire, not a stub of the adapter's own functions: every
/// answer is selected by what the adapter's encoder asked for, on the path and
/// with the JSON body that venues' own protocols define.
fn answer_of(request: &FixtureRequest) -> Option<String> {
    match request.path.as_str() {
        "/api/v3/exchangeInfo" => Some(BINANCE_SPOT.to_owned()),
        "/fapi/v1/exchangeInfo" => Some(BINANCE_PERPETUAL.to_owned()),
        "/api/v5/public/instruments" => {
            if request.asks("/api/v5/public/instruments", "instType=SPOT") {
                Some(OKX_SPOT.to_owned())
            } else if request.asks("/api/v5/public/instruments", "instType=SWAP") {
                Some(OKX_SWAP.to_owned())
            } else {
                None
            }
        }
        "/info" => {
            let payload = request.payload()?;
            let kind = payload.get("type")?.as_str()?;
            let dex = payload.get("dex").and_then(|dex| dex.as_str());
            match (kind, dex) {
                ("perpDexs", _) => Some(
                    r#"[null,{"name":"xyz","fullName":"XYZ DEX"},{"name":"abc","fullName":"ABC DEX"}]"#
                        .to_owned(),
                ),
                ("spotMeta", _) => Some(HYPERLIQUID_SPOT_META.to_owned()),
                ("spotMetaAndAssetCtxs", _) => Some(format!(
                    "[{HYPERLIQUID_SPOT_META},[{{\"markPx\":\"60000\"}}]]"
                )),
                ("meta", None) | ("metaAndAssetCtxs", None) => {
                    Some(format!("[{HYPERLIQUID_CORE_META},[{{}}]]"))
                }
                ("meta", Some("xyz")) => Some(HYPERLIQUID_DEX_META.to_owned()),
                ("metaAndAssetCtxs", Some("xyz")) => {
                    Some(format!("[{HYPERLIQUID_DEX_META},[{{}}]]"))
                }
                ("meta", Some("abc")) => Some(HYPERLIQUID_CORE_COIN_DEX_META.to_owned()),
                ("metaAndAssetCtxs", Some("abc")) => {
                    Some(format!("[{HYPERLIQUID_CORE_COIN_DEX_META},[{{}}]]"))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Serves one conversation of every venue endpoint the built-in routes read.
///
/// Every configured endpoint points at this one origin, so a request is answered
/// by the path and body its own venue's protocol defines.
async fn venue_fixture() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the fixture server should bind");
    let address = listener
        .local_addr()
        .expect("the fixture server has an address");
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Some(request) = read_request(&mut socket).await {
                    let Some(answer) = answer_of(&request) else {
                        panic!(
                            "the fixture serves no answer for {} {}?{}",
                            request.method, request.path, request.query
                        );
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
    (format!("http://{address}"), task)
}

/// Reads one HTTP request and the JSON body it carried.
async fn read_request(socket: &mut TcpStream) -> Option<FixtureRequest> {
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
    let method = SmolStr::new(fields.next().unwrap_or_default());
    let target = fields.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some(FixtureRequest {
        method,
        path: SmolStr::new(path),
        query: SmolStr::new(query),
        body: SmolStr::new(String::from_utf8_lossy(
            &buffer[header_end..header_end + length],
        )),
    })
}

/// The market participants root every row of this file is published by.
///
/// Each venue named is configured as a venue: every scope it publishes — and every
/// scope it deploys, which is reached through its own primary scope — resolves at the
/// fixture origin, so the requests travel through the real gateway, registry and
/// adapter dispatch without a live venue behind them. The configuration the root is
/// built over is what carries those addresses, so one root per test dials its own
/// fixture whatever any other test in this binary is doing.
fn connector_with(venues: &[&str], origin: &str) -> (MarketParticipantsConnector, Arc<Catalog>) {
    let origin = url::Url::parse(origin).expect("the fixture origin is a URL");
    let fixture = fixture::venues(venues.iter().map(|venue| {
        (
            VenueId::from_name(venue).expect("the fixture venue is canonical"),
            FixtureAddress::at(&origin),
        )
    }));
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(fixture_config(
        &fixture,
    )));
    let (_, receiver) = watch::channel(snapshot);
    // The fixture speaks plain HTTP on a loopback socket, so the transport is
    // built with the plaintext policy that a test owns; every other transport
    // decision is the production one, and the venue requests below go through the
    // same gateway, registry and adapter dispatch as a live run.
    let policy = TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    };
    let catalog = Arc::new(Catalog::new());
    let connector = MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(Arc::new(TransportHub::new(policy)), receiver)
            .with_runtime(Runtime::for_test()),
        Arc::clone(&catalog),
    )
    .expect("the market participants root builds from the fixture configuration");
    (connector, catalog)
}

/// Loads the venues named through the load action itself.
///
/// Every route of a loaded venue is registered and its catalogs are loaded, so
/// a row below is read through the same publication a pane reads.
async fn connect(connector: &MarketParticipantsConnector, venues: &[VenueId]) {
    for venue in venues {
        timeout(SETUP_TIMEOUT, connector.load_venue(*venue))
            .await
            .unwrap_or_else(|_| panic!("loading {venue} should not time out"))
            .unwrap_or_else(|error| panic!("loading {venue} should load its catalogs: {error}"));
    }
}

/// One route of the built-in set.
fn route(venue: &str, scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::from_name(venue).expect("the fixture venue is canonical"),
        VenueScopeId::new(scope).expect("the fixture scope is canonical"),
    )
}

/// The name one listing must come out as, and what must stay secondary.
struct ExpectedRow {
    /// The venue's own symbol of the listing this row is about.
    native: &'static str,
    /// The name of the instrument the listing implements, which is the name the
    /// listing carries.
    name: &'static str,
    /// The venue-native spellings that may only appear as search aliases.
    secondary: &'static [&'static str],
}

/// Every row of the name table, on the route that publishes it.
///
/// The OKX perpetual is the live contract of the route and its venue states
/// `ctVal = 0.01`, which no name mentions: a contract size is a term of the
/// contract and not a clause of its name.
fn design_table() -> [(VenueRoute, ExpectedRow); 8] {
    [
        (
            route("binance", "spot"),
            ExpectedRow {
                native: "BTCUSDT",
                name: "BTC/USDT",
                secondary: &["BTCUSDT"],
            },
        ),
        (
            route("okx", "spot"),
            ExpectedRow {
                native: "BTC-USDT",
                name: "BTC/USDT",
                secondary: &["BTC-USDT"],
            },
        ),
        (
            route("hyperliquid", "core-spot"),
            ExpectedRow {
                native: "@10",
                name: "BTC/USDC",
                secondary: &["@10"],
            },
        ),
        (
            route("binance", "usd-m-perp"),
            ExpectedRow {
                native: "BTCUSDT",
                name: "BTC/USDT:USDT",
                secondary: &["BTCUSDT"],
            },
        ),
        (
            route("okx", "swap"),
            ExpectedRow {
                native: "BTC-USDT-SWAP",
                name: "BTC/USDT:USDT",
                secondary: &["BTC-USDT-SWAP"],
            },
        ),
        (
            route("hyperliquid", "core-perp"),
            ExpectedRow {
                native: "BTC",
                name: "BTC/USDC:USDC",
                // A coin is the name's own base, so the venue-native value of this
                // row is its numeric asset index rather than a spelling.
                secondary: &[],
            },
        ),
        (
            route("hyperliquid", "perp-dex/xyz"),
            ExpectedRow {
                native: "xyz:ABC",
                name: "ABC/USD:USD",
                secondary: &["xyz:ABC"],
            },
        ),
        (
            route(LONGBRIDGE, "us"),
            ExpectedRow {
                native: "AACG.US",
                name: "AACG/USD",
                // The directory's ticker is the name's own base, so the venue-native
                // value of this row is the market-suffixed symbol rather than a
                // spelling the name could carry.
                secondary: &[],
            },
        ),
    ]
}

/// every built-in route publishes its row of the name table word for word.
///
/// The batch that carries one of these names carries the canonical terms the adapter
/// translated out of the answer the fixture served: a batch that withheld them is
/// refused by the publication gate, which is what its own refusal tests cover.
#[tokio::test]
async fn every_built_in_catalog_route_publishes_the_names_of_the_design_table() {
    let (origin, server) = venue_fixture().await;
    let _directory = SymbolDirectoryFixture::install(vec![
        (
            "nasdaqlisted.txt",
            DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
        ),
        (
            "otherlisted.txt",
            DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
        ),
    ])
    .await;
    let (connector, catalog) = connector_with(&[BINANCE, OKX, HYPERLIQUID, LONGBRIDGE], &origin);
    connect(
        &connector,
        &[
            VenueId::Binance,
            VenueId::Okx,
            hyperliquid_venue(),
            VenueId::Longbridge,
        ],
    )
    .await;

    for (route, expected) in design_table() {
        assert_row(&catalog, &route, &expected).await;
    }

    connector.shutdown().await;
    server.abort();
}

/// The Hyperliquid venue identity.
fn hyperliquid_venue() -> VenueId {
    VenueId::Hyperliquid
}

/// Asserts the published names of one listing and that its native spelling stayed
/// a search alias.
async fn assert_row(catalog: &Catalog, route: &VenueRoute, expected: &ExpectedRow) {
    let listings = catalog
        .listings(route)
        .unwrap_or_else(|error| panic!("the catalog of {route} should publish, got: {error}"));
    let entry = listings
        .iter()
        .find(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == expected.native)
        })
        .unwrap_or_else(|| {
            let published: Vec<&str> = listings
                .iter()
                .filter_map(|entry| entry.listing.native_symbol.as_ref().map(|s| s.as_str()))
                .collect();
            panic!(
                "{route} publishes no listing named {}, only {published:?}",
                expected.native
            )
        });
    let display = catalog.cached_display();
    let resolver = display.resolver();
    let name = resolver
        .listing_instrument(&entry.listing.key)
        .unwrap_or_else(|error| panic!("{} should be displayed, got: {error}", expected.native));
    let aliases = resolver.listing_search_aliases(&entry.listing.key);

    assert_eq!(
        name.as_str(),
        expected.name,
        "the name of {} on {route}",
        expected.native
    );
    for value in expected.secondary {
        let upper = value.to_ascii_uppercase();
        assert!(
            !name.as_str().to_ascii_uppercase().contains(&upper),
            "{value} must not reach the name {}",
            name.as_str()
        );
        assert!(
            aliases.iter().any(|alias| alias.as_str() == upper),
            "{value} is a search alias of {}",
            name.as_str()
        );
    }
}

/// the inverse swap of the OKX route states the asset it settles in, which
/// is the asset it tracks, and nothing about the conversion it performs.
///
/// The venue's own mark of BTC in USD is the rate that settlement converts through,
/// and a conversion is a listing fact: the contract carries it, and its name does
/// not state it.
#[tokio::test]
async fn an_inverse_swap_publishes_its_settlement_conversion_of_the_same_reference() {
    let (origin, server) = venue_fixture().await;
    let (connector, catalog) = connector_with(&[OKX], &origin);
    connect(&connector, &[VenueId::Okx]).await;
    let route = route("okx", "swap");
    let listings = catalog
        .listings(&route)
        .expect("the swap catalog should publish");
    let inverse = listings
        .iter()
        .find(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == "BTC-USD-SWAP")
        })
        .expect("the inverse swap is published");
    let market_participants_model::instrument::InstrumentTerms::Perpetual(terms) =
        inverse.instrument.terms()
    else {
        panic!("the inverse contract is a perpetual");
    };
    assert_eq!(terms.pnl_model(), DerivativePnlModel::Inverse);
    assert!(
        terms.conversion_reference().is_some(),
        "an inverse contract that settles in another asset carries its conversion"
    );
    let display = catalog.cached_display();
    let resolver = display.resolver();
    let name = resolver
        .listing_instrument(&inverse.listing.key)
        .expect("the inverse swap is displayed");

    assert_eq!(
        name.as_str(),
        "BTC/USD:BTC",
        "the name states the settlement asset, the quote and the tracked asset"
    );
    assert!(
        resolver
            .listing_search_aliases(&inverse.listing.key)
            .iter()
            .any(|alias| alias == "BTC-USD-SWAP"),
        "the venue's own instrument id stays a search alias"
    );

    connector.shutdown().await;
    server.abort();
}

/// the same economic terms of two routes are one instrument, and
/// the route is what tells the two listings apart.
///
/// The `abc` DEX below lists the core coin under the `{dex}:{coin}` spelling the
/// venue reserves for builder-deployed assets and settles in the core collateral,
/// so its contract and the core perpetual are the same canonical terms. The two
/// listings share one `InstrumentId` and one `PriceReferenceId` and publish one
/// economic name under two routes; the `xyz` DEX, which tracks another asset quoted
/// in another asset, names a second reference and therefore a second instrument.
///
/// The canonical base of the deployed listing is the coin and not the coin's DEX
/// prefix: a deployed scope is a listing route and never part of an economic
/// identity.
#[tokio::test]
async fn a_deployed_dex_coin_shares_the_economic_instrument_of_the_core_coin() {
    let (origin, server) = venue_fixture().await;
    let (connector, catalog) = connector_with(&[HYPERLIQUID], &origin);
    connect(&connector, &[hyperliquid_venue()]).await;

    let core = route("hyperliquid", "core-perp");
    let deployed = route("hyperliquid", "perp-dex/abc");
    let other = route("hyperliquid", "perp-dex/xyz");
    let core_listing = listing_named(&catalog, &core, "BTC").await;
    let deployed_listing = listing_named(&catalog, &deployed, "abc:BTC").await;
    let other_listing = listing_named(&catalog, &other, "xyz:ABC").await;

    assert_eq!(
        deployed_listing.instrument.id(),
        core_listing.instrument.id(),
        "the same underlying, quote and reference terms are one economic instrument"
    );
    assert_ne!(
        other_listing.instrument.id(),
        core_listing.instrument.id(),
        "another tracked asset is another instrument"
    );

    let display = catalog.cached_display();
    let resolver = display.resolver();
    let core_display = resolver
        .listing_instrument(&core_listing.listing.key)
        .expect("the core contract is displayed");
    let deployed_display = resolver
        .listing_instrument(&deployed_listing.listing.key)
        .expect("the deployed contract is displayed");
    assert_eq!(core_display.as_str(), "BTC/USDC:USDC");
    assert_eq!(
        deployed_display.as_str(),
        "BTC/USDC:USDC",
        "one instrument has one name, whatever route lists it"
    );
    assert!(
        resolver
            .listing_search_aliases(&deployed_listing.listing.key)
            .iter()
            .any(|alias| alias == "ABC:BTC"),
        "the venue's own deployed coin stays a search alias"
    );

    // Both contracts track one reference: the venue's own mark of the coin quoted
    // in the collateral, which is what its terms are.
    let generation = catalog.generation();
    let reference_of = |listing: &market_participants_connector::catalog::batch::CatalogEntry| {
        let market_participants_model::instrument::InstrumentTerms::Perpetual(terms) =
            listing.instrument.terms()
        else {
            panic!("a Hyperliquid contract is a perpetual");
        };
        terms.underlying().price_reference().clone()
    };
    let core_reference = reference_of(&core_listing);
    assert_eq!(
        reference_of(&deployed_listing),
        core_reference,
        "identical reference terms derive one reference identity"
    );
    assert_ne!(
        reference_of(&other_listing),
        core_reference,
        "another tracked asset quoted in another asset is another reference"
    );
    // Every reference a listing names is registered with the terms its identity was
    // derived from, which is what the adapter staged beside the listing: a
    // reference without registered terms refuses the publication.
    assert!(
        generation
            .references()
            .price_reference(&core_reference)
            .is_some()
            && generation
                .references()
                .price_reference(&reference_of(&other_listing))
                .is_some(),
        "the terms of both references reached the registry of the generation"
    );

    // The canonical base asset of a deployed listing is the coin alone: the DEX
    // prefix is the deployed scope's namespace and not part of the asset. That is
    // what makes the deployed `abc:BTC` contract the core `BTC` contract and not a
    // second asset named after the DEX.
    for (listing, code) in [(&deployed_listing, "BTC"), (&other_listing, "ABC")] {
        let market_participants_model::instrument::InstrumentTerms::Perpetual(terms) =
            listing.instrument.terms()
        else {
            panic!("a Hyperliquid contract is a perpetual");
        };
        let market_participants_model::asset::UnderlyingRef::Asset(base) =
            terms.underlying().reference()
        else {
            panic!("a Hyperliquid contract tracks a token");
        };
        let Some(market_participants_connector::catalog::registry::asset::AssetTerms::CryptoToken(
            base_terms,
        )) = generation.assets().terms(base)
        else {
            panic!("the base asset of a deployed contract is a canonical token");
        };
        assert_eq!(
            base_terms.origin.token(),
            &market_participants_model::asset::TokenReference::Native {
                code: market_participants_model::identifier::IdentifierValue::new(code)
                    .expect("the fixture coin is an identifier"),
            },
            "the deployed coin is the canonical asset; its DEX prefix is listing metadata"
        );
    }

    connector.shutdown().await;
    server.abort();
}

/// Reads the catalog of one route and returns the entry of one native symbol.
async fn listing_named(
    catalog: &Catalog,
    route: &VenueRoute,
    native: &str,
) -> market_participants_connector::catalog::batch::CatalogEntry {
    let listings = catalog
        .listings(route)
        .unwrap_or_else(|error| panic!("the catalog of {route} should publish, got: {error}"));
    listings
        .iter()
        .find(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == native)
        })
        .unwrap_or_else(|| panic!("{route} publishes no listing named {native}"))
        .clone()
}

/// the venue's own spelling of a Hyperliquid pair is a search alias
/// of its listing and never part of its name.
#[tokio::test]
async fn the_hyperliquid_asset_index_of_a_listing_stays_a_search_alias() {
    let (origin, server) = venue_fixture().await;
    let (connector, catalog) = connector_with(&[HYPERLIQUID], &origin);
    connect(&connector, &[hyperliquid_venue()]).await;
    let route = route("hyperliquid", "core-spot");
    let listings = catalog
        .listings(&route)
        .expect("the core spot catalog should publish");
    let pair = listings
        .iter()
        .find(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == "@10")
        })
        .expect("the @10 pair is published");
    let display = catalog.cached_display();
    let resolver = display.resolver();
    let name = resolver
        .listing_instrument(&pair.listing.key)
        .expect("the pair is displayed");

    assert_eq!(name.as_str(), "BTC/USDC");
    assert_eq!(
        pair.listing.native_symbol.as_ref().map(|s| s.as_str()),
        Some("@10"),
        "the venue's own spelling of the pair is its native symbol"
    );
    assert!(
        resolver
            .listing_search_aliases(&pair.listing.key)
            .iter()
            .any(|alias| alias == "@10"),
        "and it is a search alias rather than part of the name"
    );

    connector.shutdown().await;
    server.abort();
}
