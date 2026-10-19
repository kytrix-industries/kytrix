//! The adapter's own path to the venue: the cases that drive production code.
//!
//! Every other case in this suite speaks the wire itself, which is what makes its
//! measurements independent of the code they check — and it is also the gap the final
//! review named: the adapter's signed `fetch_otp`, its pooled handshake with the declared
//! upgrade headers, its `establish` (AUTH then cmd 4), its permission gate and its cmd 27
//! request had never faced the venue, because the suite dialled its own socket and
//! computed its own signature.
//!
//! Case 17 closes that gap for the candle commands and case 20 for the ticker ones, with
//! no fixture seam: the credentials are the real ones, the origins are the published ones
//! (`LongbridgeRegion`'s own pair, not a fixture binding), the listing comes from the live
//! catalog the venue's directory built, and the market data is fetched through
//! `MarketCapability` — the same call the widgets make.

use std::sync::Arc;

use app_config::{
    market_participants::{
        credentials::LongbridgeCredentials,
        origins::LongbridgeRegion,
        settings::{LongbridgeSettings, VenueSettings},
    },
    network::{ProxyAuthorizationConfig, ProxyConfig, ProxyRoute},
    settings::Config,
};
use app_config_provider::notification::ConfigNotification;
use market_participants_connector::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    error::{AuthError, MarketParticipantError},
    market::error::MarketError,
    runtime::MarketParticipantNetworkSnapshot,
};
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use network::transport::hub::{TransportHub, TransportPolicy};
use smol_str::SmolStr;
use tokio::time::timeout;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    bounds::{CASE_TIMEOUT, READ_TIMEOUT, SETUP_TIMEOUT, SUBSCRIPTION_TIMEOUT},
    credentials::Credentials,
    skip_without_credentials,
};

/// The ticker the catalog is asked for.
const TICKER: &str = "AAPL.US";
/// The route the venue publishes its US listings under.
const SCOPE: &str = "us";

/// Case 17 — the adapter's public path, with real credentials and published origins.
///
/// The measurement this case produces is what §14.1 item 10's second half was waiting
/// for: the loopback fixture proves the adapter's OTP request is signed the way §4.9
/// states, and the venue proves that algorithm is accepted — but only this case proves
/// the adapter's **own** signing path is the one the venue accepts, end to end, together
/// with the handshake, the permission gate and the candle command behind it.
///
/// It asserts what the design states: the venue grants this account US market data, the
/// adapter establishes its session and the marketplace answers a candle read with candles
/// about the listing that was asked for. Everything it measures is printed.
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_17_the_adapter_reaches_the_venue_on_its_own_path() {
    let case = "case 17 (adapter)";
    timeout(CASE_TIMEOUT, async {
        let values = skip_without_credentials!(case, Credentials::load());
        let (connector, catalog) = live_connector(case, &values);
        timeout(SETUP_TIMEOUT, connector.load_venue(VenueId::Longbridge))
            .await
            .unwrap_or_else(|_| panic!("{case}: loading the venue timed out"))
            .unwrap_or_else(|error| panic!("{case}: the venue's catalogs did not load: {error}"));

        let route = VenueRoute::new(
            VenueId::Longbridge,
            market_participants_model::identifier::VenueScopeId::new(SCOPE)
                .expect("the US scope is canonical"),
        );
        let listings = catalog
            .listings(&route)
            .unwrap_or_else(|error| panic!("{case}: the venue published no catalog: {error}"));
        println!(
            "{case}: x-dc-region={} origin={} catalog_listings={}",
            values.data_centre(),
            LongbridgeRegion::Global.http_str(),
            listings.len()
        );
        assert!(
            !listings.is_empty(),
            "{case}: the venue's directory must publish US listings"
        );

        let listing = catalog
            .resolve_native_symbol(&route, TICKER)
            .unwrap_or_else(|error| panic!("{case}: {TICKER} is not in the live catalog: {error}"));
        println!(
            "{case}: resolved {TICKER} to listing {} on {route}",
            listing.listing_id.as_str()
        );

        // The candle read is the whole chain at once: `MarketCapability` resolves the
        // listing inside one catalog generation, the gateway asks the adapter for a
        // session, the session is established the first time any market-data command
        // arrives (OTP with the signed request, the upgrade with its two headers, AUTH,
        // then cmd 4 for the profile and its permission), and the command itself is
        // paced by the quota the profile published.
        let candles = timeout(
            READ_TIMEOUT,
            connector
                .market()
                .market(route.clone())
                .candlesticks(
                    listing.clone(),
                    market_participants_connector::market::timeframe::TimeFrame::Day1,
                )
                .limit(10)
                .fetch(),
        )
        .await
        .expect("the first market-data command of a live session must not time out")
        .unwrap_or_else(|error| panic!("{case}: the adapter's candle read failed: {error}"));

        let first = candles.first().expect("a listed security states candles");
        let last = candles.last().expect("a listed security states candles");
        println!(
            "{case}: the adapter fetched {} DAY candle(s) through its own session: \
             first={} ({} ET) last={} ({} ET)",
            candles.len(),
            first.timestamp,
            crate::describe::market_clock(first.timestamp),
            last.timestamp,
            crate::describe::market_clock(last.timestamp)
        );
        for candle in candles.iter().take(3) {
            println!(
                "    o={} h={} l={} c={} v={}",
                candle.open.value,
                candle.high.value,
                candle.low.value,
                candle.close.value,
                candle.traded_quantity.as_ref().map_or_else(
                    || "<none>".to_owned(),
                    |quantity| quantity.value.to_string()
                )
            );
        }
        assert!(
            !candles.is_empty(),
            "{case}: a live session must answer the adapter's candle read with candles"
        );
        assert!(
            candles.iter().all(|candle| candle.listing == listing),
            "{case}: every candle must carry the listing the read was addressed to"
        );
        assert!(
            candles
                .windows(2)
                .all(|pair| pair[0].timestamp < pair[1].timestamp),
            "{case}: the candles must be ascending and distinct"
        );

        // The subscription is the other half of the adapter's candle surface, and the half
        // nothing else in this suite can reach: the pooled socket's own subscribe lease,
        // the opening screen (cmd 19) that precedes the subscribe frame, the cmd 6
        // acknowledgement the stream recognises for itself, and the aggregation of
        // whatever the venue then pushes. The first event is the screen's oldest candle, so
        // the subscription is answered even on a closed market — and which of the two
        // situations this run measured is the venue's **calendar**'s answer rather than the
        // session clock's, the way cases 9 and 10 already read it: 09:30–16:00 ET on a
        // Sunday is a session the venue states and a day it does not trade, so a clock-only
        // gate would report pushes being merged on a market that pushed nothing.
        let mut stream = timeout(
            READ_TIMEOUT,
            connector
                .market()
                .market(route.clone())
                .candlesticks(
                    listing.clone(),
                    market_participants_connector::market::timeframe::TimeFrame::Minute1,
                )
                .subscribe(),
        )
        .await
        .expect("the subscribe call must not time out")
        .unwrap_or_else(|error| panic!("{case}: the adapter's subscription was refused: {error}"));
        let first = timeout(SUBSCRIPTION_TIMEOUT, futures::StreamExt::next(&mut stream))
            .await
            .expect("the subscription must answer with a candle inside its own bound")
            .unwrap_or_else(|| panic!("{case}: the subscription ended without a candle"))
            .unwrap_or_else(|error| panic!("{case}: the subscription failed: {error}"));
        let trading_day =
            crate::cases::push::trading_day(&values, case, crate::grid::now_millis()).await;
        println!(
            "{case}: the subscription answered with t={} ({} ET) o={} c={}; the venue's \
             calendar names {} a trading day, so this run is {}",
            first.timestamp,
            crate::describe::market_clock(first.timestamp),
            first.open.value,
            first.close.value,
            crate::grid::market_date(crate::grid::now_millis()),
            if trading_day {
                "on a trading day, so pushes are being merged as well"
            } else {
                "on a day the venue does not trade, so the answer is the opening screen's own \
                 history and the push clause cannot be judged here (the wire-level case 10 \
                 states the same)"
            }
        );
        assert_eq!(
            first.listing, listing,
            "{case}: a candle from the stream must carry the listing that was subscribed"
        );

        connector.shutdown().await;
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 20 — the adapter's public path to a live ticker (§2.4, §4.2, §4.3).
///
/// Case 17 proved the adapter's own session, gate and candle command against the venue.
/// This case proves the other half of the market-data surface on the same path: the
/// capability declaration that lets a caller ask for a ticker at all, the catalog that
/// resolves the listing, the session and the quota, and the pull (cmd 11) whose answer is
/// the snapshot a watch-list row is first drawn from.
///
/// The subscription is the second half, and the assertion it makes is deliberately about
/// **arrival** rather than about a price: the adapter's stream opens with the snapshot it
/// pulled itself, so its first event is answered whether or not the venue is pushing
/// anything — which is exactly the property that must hold on a closed market, and the
/// reason the first event is not "the venue's first push" (§4.2).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_20_the_adapter_reads_a_live_ticker() {
    let case = "case 20 (adapter ticker)";
    timeout(CASE_TIMEOUT, async {
        let values = skip_without_credentials!(case, Credentials::load());
        let (connector, catalog) = live_connector(case, &values);
        timeout(SETUP_TIMEOUT, connector.load_venue(VenueId::Longbridge))
            .await
            .unwrap_or_else(|_| panic!("{case}: loading the venue timed out"))
            .unwrap_or_else(|error| panic!("{case}: the venue's catalogs did not load: {error}"));

        let route = VenueRoute::new(
            VenueId::Longbridge,
            market_participants_model::identifier::VenueScopeId::new(SCOPE)
                .expect("the US scope is canonical"),
        );
        let listing = catalog
            .resolve_native_symbol(&route, TICKER)
            .unwrap_or_else(|error| panic!("{case}: {TICKER} is not in the live catalog: {error}"));
        println!(
            "{case}: x-dc-region={} resolved {TICKER} to listing {} on {route}",
            values.data_centre(),
            listing.listing_id.as_str()
        );

        let read = timeout(
            READ_TIMEOUT,
            connector
                .market()
                .market(route.clone())
                .ticker(listing.clone())
                .fetch(),
        )
        .await
        .expect("the adapter's ticker read must not time out");
        let ticker = match read {
            Ok(ticker) => ticker,
            // An account without US market data is not a broken case: the suite has no
            // permission to measure, and says so rather than failing the run — the same
            // situation case 18 skips on. The refusal is the adapter's own permission
            // gate, wrapped by the market capability in its network error.
            Err(MarketError::Network(network))
                if matches!(
                    network.error(),
                    MarketParticipantError::AuthenticationFailed {
                        source: AuthError::NoMarketDataPermission,
                        ..
                    }
                ) =>
            {
                println!(
                    "SKIP {case}: the venue's profile grants this account no US market data \
                     (the account's permission is the design's own precondition, §3.6); the \
                     ticker read was refused with: {network}"
                );
                connector.shutdown().await;
                return;
            }
            Err(other) => panic!("{case}: the adapter's ticker read failed: {other}"),
        };

        println!(
            "{case}: last={} {} open={} high={:?} low={:?} quantity={:?} notional={:?} \
             monotonic_id={:?}",
            ticker.close.value,
            ticker.close.quote_asset,
            ticker.open.value,
            ticker.high.as_ref().map(|price| price.value),
            ticker.low.as_ref().map(|price| price.value),
            ticker
                .traded_quantity
                .as_ref()
                .map(|quantity| quantity.value.to_string()),
            ticker
                .traded_notional
                .as_ref()
                .map(|quantity| quantity.value.to_string()),
            ticker.monotonic_id
        );
        assert_eq!(
            ticker.listing, listing,
            "{case}: the snapshot must carry the listing the read was addressed to"
        );
        assert!(
            ticker.close.value >= market_participants_model::decimal::Price::ZERO,
            "{case}: a price is not negative"
        );
        // A quote of a market that is not trading is legitimately all zeroes — a halted
        // listing, a session that has not opened, a venue with no prints today — so the
        // stronger statement is only made where the venue itself states that something
        // traded.
        if ticker.traded_quantity.as_ref().is_some_and(|quantity| {
            quantity.value > market_participants_model::decimal::Quantity::ZERO
        }) {
            assert!(
                ticker.close.value > market_participants_model::decimal::Price::ZERO,
                "{case}: a snapshot that states a traded quantity states a price as well"
            );
        }
        if let (Some(low), Some(high)) = (ticker.low.as_ref(), ticker.high.as_ref()) {
            assert!(
                low.value <= high.value,
                "{case}: a low above its high is not a session"
            );
        }

        // The subscription's first event is the adapter's own baseline, so it arrives
        // without the venue having to push anything.
        let mut stream = timeout(
            READ_TIMEOUT,
            connector
                .market()
                .market(route)
                .ticker(listing.clone())
                .subscribe(),
        )
        .await
        .expect("the subscribe call must not time out")
        .unwrap_or_else(|error| {
            panic!("{case}: the adapter's ticker subscription was refused: {error}")
        });
        let first = timeout(SUBSCRIPTION_TIMEOUT, futures::StreamExt::next(&mut stream))
            .await
            .expect("the subscription must answer with a snapshot inside its own bound")
            .unwrap_or_else(|| panic!("{case}: the subscription ended without a snapshot"))
            .unwrap_or_else(|error| panic!("{case}: the subscription failed: {error}"));
        println!(
            "{case}: the subscription's first event is last={} {} open={} for listing {} — the \
             snapshot the adapter pulled, which is what an unwatched or closed market still \
             answers",
            first.close.value, first.close.quote_asset, first.open.value, first.listing.listing_id
        );
        assert_eq!(
            first.listing, listing,
            "{case}: the stream's first snapshot must be about the listing that was subscribed"
        );

        connector.shutdown().await;
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// One market participant whose Longbridge settings carry the live credentials and the published
/// origins.
/// The proxy is the same environment variable every live case of this suite takes, so the
/// adapter's own requests travel the route the wire-level cases travel. Nothing here
/// points the venue at a fixture: `LongbridgeRegion`'s own pair is what the adapter dials,
/// which is the point of the case.
fn live_connector(case: &str, values: &Credentials) -> (MarketParticipantsConnector, Arc<Catalog>) {
    let mut settings = LongbridgeSettings {
        region: LongbridgeRegion::Global,
        credentials: LongbridgeCredentials {
            app_key: SmolStr::new(values.app_key.as_str()),
            app_secret: SmolStr::new(values.app_secret.as_str()),
            access_token: SmolStr::new(values.access_token.as_str()),
        },
        ..LongbridgeSettings::default()
    };
    assert!(
        settings.credentials.is_complete(),
        "{case}: the credential set this run read is complete"
    );
    assert_eq!(
        settings.addresses().websocket.as_str(),
        LongbridgeRegion::Global.websocket_str(),
        "{case}: the case drives the venue's published origin rather than a fixture"
    );
    settings.proxy = None;

    let mut config = Config::default();
    config
        .venues
        .insert(VenueId::Longbridge, VenueSettings::Longbridge(settings));
    config.proxies = live_proxies();
    config.proxy_default = ProxyRoute::named(PROXY_NAME);
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config));
    let notification = ConfigNotification::new(snapshot);
    let catalog = Arc::new(Catalog::new());
    let connector = MarketParticipantsConnector::new_with_catalog(
        MarketParticipantsConnectorInput::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            notification,
        )
        .with_runtime(Runtime::for_test()),
        Arc::clone(&catalog),
    )
    .expect("the market participants root builds from a typed runtime snapshot");
    (connector, catalog)
}

/// The proxy name this case's network configuration routes through.
const PROXY_NAME: &str = "longbridge-live";

/// The proxies one live run dials through, as the wire-level cases' environment states.
fn live_proxies() -> std::collections::BTreeMap<SmolStr, ProxyConfig> {
    let raw = std::env::var(crate::client::PROXY_ENV)
        .unwrap_or_else(|_| crate::client::DEFAULT_PROXY.to_owned());
    let mut proxies = std::collections::BTreeMap::new();
    let Ok(url) = Url::parse(raw.trim()) else {
        return proxies;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port()) else {
        return proxies;
    };
    let authorization = match (url.username(), url.password()) {
        (username, Some(password)) if !username.is_empty() => {
            ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new(username),
                password: SmolStr::new(password),
            }
        }
        _ => ProxyAuthorizationConfig::None,
    };
    proxies.insert(
        SmolStr::new_static(PROXY_NAME),
        ProxyConfig {
            addr: SmolStr::new(host),
            port,
            authorization,
            resolve_dns_at_remote: url.scheme() == "socks5h",
        },
    );
    proxies
}
