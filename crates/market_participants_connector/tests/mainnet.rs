//! Live mainnet checks for the market participants connector's built-in routes.
//!
//! These tests are ignored by default: they resolve the built-in routes against
//! the real venue origins, so they need working network access. Set
//! `KYTRIX_NETWORK_TEST_PROXY` to a `socks5://` or `socks5h://` URL when the
//! venues are reachable through a local proxy.
//!
//! What can be checked without credentials is checked here against the live
//! venues: that every built-in route registers, that each catalog route's own
//! endpoint publishes a catalog whose typed identifiers agree with the canonical
//! instrument they claim, and that a public read answers under the exact listing
//! it was asked about. Everything that needs a trading credential — orders,
//! balances, positions, wallet monitoring of a private account — has no
//! credential to run under in this suite; those paths are covered by the
//! adapters' fixture tests instead.

use std::{env, sync::Arc, time::Duration};

use app_config::{
    network::{Config as NetworkConfig, ProxyAuthorizationConfig, ProxyConfig, ProxyRoute},
    settings::Config,
};
use app_config_provider::notification::ConfigNotification;
use market_participants_connector::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    runtime::MarketParticipantNetworkSnapshot,
};
use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    market::{book::ListingOrderBook, selection::ListingTicker},
};
use network::transport::hub::{TransportHub, TransportPolicy};
use smol_str::SmolStr;
use tokio::time::timeout;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

const PROXY_ENV: &str = "KYTRIX_NETWORK_TEST_PROXY";
const DEFAULT_PROXY: &str = "socks5h://127.0.0.1:7890";
const PROXY_NAME: &str = "mainnet-test";
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The URL parser exposes userinfo in its percent-encoded form, while the proxy
/// catalog stores the literal credentials.
fn decode_proxy_credential(value: &str) -> SmolStr {
    let decoded = percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .expect("the test proxy URL must use UTF-8 credentials");
    SmolStr::new(decoded)
}

/// The network configuration one live check routes through.
fn mainnet_network() -> NetworkConfig {
    let proxy_url = env::var(PROXY_ENV).unwrap_or_else(|_| DEFAULT_PROXY.to_owned());
    let proxy_url = Url::parse(&proxy_url)
        .unwrap_or_else(|error| panic!("{PROXY_ENV} must be a valid SOCKS5 URL: {error}"));
    assert!(
        matches!(proxy_url.scheme(), "socks5" | "socks5h"),
        "{PROXY_ENV} must use socks5:// or socks5h://"
    );
    let host = proxy_url
        .host_str()
        .expect("the test proxy URL must contain a host");
    let port = proxy_url
        .port()
        .expect("the test proxy URL must contain a port");
    let authorization = match (proxy_url.username(), proxy_url.password()) {
        (username, Some(password)) if !username.is_empty() => {
            ProxyAuthorizationConfig::UsernameAndPassword {
                username: decode_proxy_credential(username),
                password: decode_proxy_credential(password),
            }
        }
        ("", None) => ProxyAuthorizationConfig::None,
        _ => panic!("the test proxy URL must provide both username and password"),
    };

    let mut network = NetworkConfig::default();
    network.proxies.insert(
        PROXY_NAME.into(),
        ProxyConfig {
            addr: SmolStr::new(host),
            port,
            authorization,
            resolve_dns_at_remote: proxy_url.scheme() == "socks5h",
        },
    );
    network.proxy_default = ProxyRoute::named(PROXY_NAME);
    network
}

fn mainnet_input() -> MarketParticipantsConnectorInput {
    let network = mainnet_network();
    let mut config = Config::default();
    config.proxies = network.proxies;
    config.proxy_default = network.proxy_default;
    config.proxy_applications = network.proxy_applications;
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config));
    let notification = ConfigNotification::new(snapshot);
    MarketParticipantsConnectorInput::new(
        Arc::new(TransportHub::new(TransportPolicy::default())),
        notification,
    )
    .with_runtime(Runtime::for_test())
}

/// One loaded connector together with the routes its venues registered.
///
/// Every declared venue is loaded through the load action itself, so the
/// routes this suite reads are the ones the user's own action registers.
async fn attached_connector() -> (MarketParticipantsConnector, Arc<Catalog>, Vec<VenueRoute>) {
    let catalog = Arc::new(Catalog::new());
    let connector =
        MarketParticipantsConnector::new_with_catalog(mainnet_input(), Arc::clone(&catalog))
            .expect("the market participants connector should build from a typed runtime snapshot");
    for venue in connector.supported_venues() {
        timeout(SETUP_TIMEOUT, connector.load_venue(venue))
            .await
            .unwrap_or_else(|_| panic!("loading {venue} should not time out"))
            .unwrap_or_else(|error| panic!("loading {venue} should load its catalogs: {error}"));
    }
    // What the projection must hold is every **statically declared** scope, and it may
    // hold more: a venue that deploys its own partitions registers them when it is
    // loaded — Hyperliquid's builder-deployed `perp-dex/*` — so the loaded set is a
    // superset of the declared one and no fixed total can be right by construction.
    let declared = declared_scopes();
    timeout(SETUP_TIMEOUT, async {
        while !declared
            .iter()
            .all(|route| connector.venues().scope(route).is_some())
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the built-in market routes should be projected");
    let routes: Vec<VenueRoute> = connector.venues().scopes().to_vec();
    (connector, catalog, routes)
}

/// The routes this build declares statically, one per scope of one venue's specification
/// table.
///
/// The list is named once rather than totalled: what the projection must hold is exactly
/// these routes, and whatever a venue discovered on top of them. It is the public
/// `routing` constants that are named — the same names the specification table is built
/// from — so a venue that gains a scope is a scope this suite has to be told about rather
/// than a count that silently drifts.
fn declared_scopes() -> Vec<VenueRoute> {
    use market_participants_connector::routing::{
        CORE_PERP_SCOPE, CORE_SPOT_SCOPE, LONGBRIDGE_US_SCOPE, PERPETUAL_SCOPE, SPOT_SCOPE,
        SWAP_SCOPE,
    };
    let scopes: [(VenueId, &[&str]); 4] = [
        (VenueId::Binance, &[SPOT_SCOPE, PERPETUAL_SCOPE]),
        (VenueId::Okx, &[SPOT_SCOPE, SWAP_SCOPE]),
        (VenueId::Hyperliquid, &[CORE_SPOT_SCOPE, CORE_PERP_SCOPE]),
        (VenueId::Longbridge, &[LONGBRIDGE_US_SCOPE]),
    ];
    let mut routes = Vec::with_capacity(16);
    for (venue, venue_scopes) in scopes {
        for scope in venue_scopes {
            routes.push(VenueRoute::new(
                venue,
                VenueScopeId::new(scope).expect("a built-in scope name is canonical"),
            ));
        }
    }
    routes
}

/// Every built-in route registers, and every endpoint a descriptor names has a
/// built-in origin, so a fresh installation can route without configuration.
#[tokio::test]
#[ignore = "requires live network access to the venue origins"]
async fn built_in_routes_register_with_endpoints_a_fresh_install_can_reach() {
    let (connector, _catalog, routes) = attached_connector().await;

    assert!(
        routes
            .iter()
            .all(|route| connector.venues().scope(route).is_some()),
    );
    let declared = declared_scopes();
    for route in &declared {
        assert!(
            connector.venues().scope(route).is_some(),
            "{route} is a statically declared scope, so the loaded projection must hold it"
        );
    }
    assert!(
        routes.len() >= declared.len(),
        "the loaded projection holds every statically declared scope and the partitions a \
         venue discovered on top of them: {} loaded, {} declared",
        routes.len(),
        declared.len()
    );
    assert!(
        connector.venues().scopes().len() >= declared.len(),
        "every statically declared scope also binds one catalog source: {} loaded",
        connector.venues().scopes().len()
    );
    println!(
        "  the projection holds {} route(s), of which {} are declared statically; the venues \
         present are {:?}",
        routes.len(),
        declared.len(),
        connector.supported_venues()
    );

    connector.shutdown().await;
}

/// Each catalog route's own endpoint answers with a catalog, and every published
/// entry is about that route, declares typed identifiers of the instrument kind
/// it claims and carries the listing key the route resolved it under.
#[tokio::test]
#[ignore = "requires live network access to the venue origins"]
async fn live_catalogs_publish_entries_about_the_route_they_were_fetched_for() {
    let (connector, catalog, routes) = attached_connector().await;

    for route in routes {
        let entries = catalog
            .listings(&route)
            .unwrap_or_else(|error| panic!("the catalog of {route} should publish: {error}"));
        assert!(
            !entries.is_empty(),
            "{route} is a registered catalog route, so its venue must list something"
        );
        for entry in entries.iter() {
            assert_eq!(
                entry.listing.key.route, route,
                "a catalog entry must be about the route it was fetched for"
            );
            assert_eq!(
                entry.listing.identifiers.instrument_kind(),
                entry.instrument.kind(),
                "listing {} declares identifiers of another instrument kind",
                entry.listing.key.listing_id.as_str()
            );
            assert_eq!(
                entry.snapshot.key, entry.listing.key,
                "the published snapshot must describe the listing it was published with"
            );
        }
    }

    connector.shutdown().await;
}

/// A public ticker read and an order-book snapshot come back under the exact
/// listing they were asked about.
///
/// A candlestick read is deliberately absent: its interval type belongs to the
/// crate's private protocol vocabulary, so an integration test cannot name the
/// interval it would ask for. The candlestick path itself is covered by the
/// adapters' own fixture tests, which live inside the crate.
#[tokio::test]
#[ignore = "requires live network access to the venue origins"]
async fn live_public_reads_answer_under_the_listing_they_were_asked_about() {
    let (connector, catalog, routes) = attached_connector().await;
    let route = routes
        .iter()
        .find(|route| route.venue.as_str() == "binance" && route.scope.as_str() == "spot")
        .expect("the built-in Binance spot route should be registered")
        .clone();
    let listing = catalog
        .listings(&route)
        .expect("the Binance spot catalog should publish")
        .first()
        .expect("the Binance spot catalog should list something")
        .listing
        .key
        .clone();

    let ticker: ListingTicker = timeout(
        READ_TIMEOUT,
        connector
            .market()
            .market(route.clone())
            .ticker(listing.clone())
            .fetch(),
    )
    .await
    .expect("fetching a ticker should not time out")
    .expect("the Binance spot venue should answer a ticker request");
    assert_eq!(
        ticker.listing, listing,
        "a ticker must carry the listing it was asked about"
    );

    let order_book: ListingOrderBook = timeout(
        READ_TIMEOUT,
        connector
            .market()
            .market(route)
            .order_book(listing.clone())
            .depth(5)
            .fetch(),
    )
    .await
    .expect("fetching an order book should not time out")
    .expect("the Binance spot venue should answer an order-book request");
    assert_eq!(
        order_book.listing, listing,
        "an order book must carry the listing it was asked about"
    );
    assert!(
        !order_book.bids.is_empty() && !order_book.asks.is_empty(),
        "a live order book of a listed instrument has both sides"
    );

    connector.shutdown().await;
}
