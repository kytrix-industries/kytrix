use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    routing::CapabilityId,
};

use crate::{
    adapter::longbridge::{NASDAQ_LISTED_FILE, OTHER_LISTED_FILE},
    catalog::error::CatalogError,
    error::MarketParticipantError,
    market::{
        error::{MarketError, MarketNetworkError},
        timeframe::{Adjustment, TimeFrame},
    },
    routing::{CORE_PERP_SCOPE, HYPERLIQUID_VENUE},
    test_support::{
        CatalogFixture, DirectoryAnswer, NASDAQ_LISTED_SAMPLE, OTHER_LISTED_SAMPLE,
        SymbolDirectoryFixture, dex_meta, info_fixture_server, spot_meta_answer,
    },
    venue::{
        definition::venues_supporting,
        test_support::{fixture_connector, load},
    },
};

fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

fn route(venue: &VenueId, scope: &str) -> VenueRoute {
    VenueRoute::new(
        *venue,
        VenueScopeId::new(scope).expect("the built-in scope is canonical"),
    )
}

async fn hyperliquid_fixture() -> (String, tokio::task::JoinHandle<()>) {
    let (origin, _requests, server) = info_fixture_server(
        vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
        spot_meta_answer(&["USD", "USDC", "BTC"]),
        dex_meta(&["BTC"]),
        vec![("xyz".to_owned(), dex_meta(&["xyz:ABC"]))],
    )
    .await;
    (origin, server)
}

#[tokio::test]
async fn every_loaded_route_answers_its_bounds() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);

    for venue in [venue("binance"), venue("okx"), venue(HYPERLIQUID_VENUE)] {
        load(&connector, &venue)
            .await
            .unwrap_or_else(|error| panic!("{venue} loads: {error:?}"));
    }

    let market = connector.market();
    let venues = connector.venues();
    let deployed = route(&venue(HYPERLIQUID_VENUE), "perp-dex/xyz");
    assert!(
        venues.scope(&deployed).is_some_and(|entry| entry
            .capabilities
            .iter()
            .any(|support| support.capability == CapabilityId::Candlesticks)),
        "the scope the discovery named is part of the registry this test reads"
    );

    let mut checked = 0_usize;
    let scopes = venues.scopes();
    let entries = venues.entries();
    for (index, route) in scopes.iter().enumerate() {
        for capability in &entries[index].1.capabilities {
            market
                .limits(route, capability.capability)
                .unwrap_or_else(|error| {
                    panic!(
                        "{route} declares {} but its bounds were refused: {error:?}",
                        capability.capability
                    )
                });
            checked += 1;
        }
    }
    assert!(
        checked >= venues.scopes().len(),
        "every registered route declares at least one capability"
    );

    connector.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn every_candlestick_route_publishes_the_neutral_convention() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    let _directory = SymbolDirectoryFixture::install(vec![
        (
            NASDAQ_LISTED_FILE,
            DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
        ),
        (
            OTHER_LISTED_FILE,
            DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
        ),
    ])
    .await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);

    let expected = venues_supporting(CapabilityId::Candlesticks);
    assert!(
        !expected.is_empty(),
        "this build serves candlesticks for some venue, or the invariant is not exercised"
    );
    for venue in &expected {
        load(&connector, venue)
            .await
            .unwrap_or_else(|error| panic!("{venue} loads: {error:?}"));
    }

    let market = connector.market();
    let venues = connector.venues();
    let scopes = venues.scopes();
    let entries = venues.entries();
    let mut served: Vec<VenueId> = scopes.iter().map(|route| route.venue).collect();
    served.sort_unstable();
    served.dedup();
    let mut expected_venues = expected.clone();
    expected_venues.sort_unstable();
    assert_eq!(
        served, expected_venues,
        "the venues whose routes are checked are the venues the table says serve candlesticks"
    );

    let mut checked = 0_usize;
    for (index, route) in scopes.iter().enumerate() {
        let serves_candlesticks = entries[index]
            .1
            .capabilities
            .iter()
            .any(|support| support.capability == CapabilityId::Candlesticks);
        assert!(
            serves_candlesticks,
            "{route} belongs to a venue the table says serves candlesticks, so the bounds \
             this check reads are candlestick bounds"
        );
        let adjustments = market
            .supported_adjustments(route)
            .unwrap_or_else(|error| panic!("{route} answers its adjustments: {error:?}"));
        assert!(
            adjustments.contains(&Adjustment::None),
            "{route} serves candlesticks but does not publish the neutral convention: \
             {adjustments:?}"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        scopes.len(),
        "every route of a loaded venue is checked, so no route serves candlesticks unguarded"
    );

    connector.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn a_discovered_scope_answers_the_venues_own_bounds() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);
    let hyperliquid = venue(HYPERLIQUID_VENUE);
    let deployed = route(&hyperliquid, "perp-dex/xyz");

    load(&connector, &hyperliquid)
        .await
        .expect("the venue discovers its deployed scope and loads every catalogue");

    let timeframes = connector
        .market()
        .supported_timeframes(&deployed)
        .expect("a discovered scope answers its bounds instead of refusing the capability");

    assert_eq!(
        timeframes.len(),
        14,
        "the venue's own candle set is the answer for a discovered scope: {timeframes:?}"
    );
    assert!(timeframes.contains(&TimeFrame::Hour8));
    assert!(timeframes.contains(&TimeFrame::Day3));
    assert!(!timeframes.contains(&TimeFrame::Hour6));
    assert!(!timeframes.contains(&TimeFrame::Day2));
    assert!(!timeframes.contains(&TimeFrame::Second1));

    connector.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn an_unloaded_venue_is_not_a_runtime_answer() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    let (connector, catalog) = fixture_connector(catalogs.origin(), &info);
    let market = connector.market();
    let spot = route(&venue("binance"), "spot");

    assert!(
        connector.venues().scopes().is_empty(),
        "no load committed anything in this root, not even the venue's declared routes"
    );
    assert!(
        catalog.cached_listings(&spot).is_none(),
        "and the catalogue holds no snapshot for the declared route"
    );

    assert_eq!(
        market.supported_timeframes(&spot),
        Err(MarketError::Network(MarketNetworkError::from(
            MarketParticipantError::VenueNotLoaded {
                venue: venue("binance"),
                scope: spot.scope.clone(),
            }
        ))),
        "the bounds of a market nothing loaded are refused as not loaded, not as an \
         unsupported capability"
    );
    assert_eq!(
        market.listings(&spot),
        Err(MarketError::Catalog(Box::new(
            CatalogError::RouteNotRegistered {
                route: spot.clone()
            }
        ))),
        "and the route is not admitted into the catalogue either"
    );

    connector.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn the_two_refusals_are_decided_against_the_loaded_venues_alone() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);
    let market = connector.market();
    let binance = venue("binance");
    let spot = route(&binance, "spot");

    load(&connector, &binance)
        .await
        .expect("the venue loads its two statically declared scopes");
    let served = catalogs.served().len();

    assert_eq!(
        market.limits(&spot, CapabilityId::ForceOrder),
        Err(MarketError::UnsupportedCapability {
            route: spot.clone(),
            capability: CapabilityId::ForceOrder,
        }),
        "a scope that loaded without the capability is the refusal this build makes"
    );
    let absent = route(&venue(HYPERLIQUID_VENUE), "core-perp");
    assert_eq!(
        market.supported_timeframes(&absent),
        Err(MarketError::Network(MarketNetworkError::from(
            MarketParticipantError::VenueNotLoaded {
                venue: absent.venue,
                scope: absent.scope.clone(),
            }
        ))),
        "while a market no load committed is the answer a caller waits for"
    );
    assert_eq!(
        catalogs.served().len(),
        served,
        "neither refusal was asked of the venue: both are answered by the loaded venues"
    );

    connector.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn a_scope_a_shrinking_commit_took_away_answers_not_loaded() {
    let (info, requests, server) = info_fixture_server(
        vec![
            r#"[null,{"name":"xyz"},{"name":"abc"}]"#.to_owned(),
            r#"[null,{"name":"xyz"}]"#.to_owned(),
        ],
        spot_meta_answer(&["USD", "USDC", "BTC"]),
        dex_meta(&["BTC"]),
        vec![
            ("xyz".to_owned(), dex_meta(&["xyz:ABC"])),
            ("abc".to_owned(), dex_meta(&["abc:DEF"])),
        ],
    )
    .await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);
    let market = connector.market();
    let hyperliquid = venue(HYPERLIQUID_VENUE);
    let shrinking = route(&hyperliquid, "perp-dex/abc");

    load(&connector, &hyperliquid)
        .await
        .expect("the first load commits both deployments");
    assert!(
        market.supported_timeframes(&shrinking).is_ok(),
        "the scope the first reading published answers its bounds"
    );

    load(&connector, &hyperliquid)
        .await
        .expect("the refresh commits the one deployment the venue still publishes");
    let served = requests.lock().len();

    assert_eq!(
        market.supported_timeframes(&shrinking),
        Err(MarketError::Network(MarketNetworkError::from(
            MarketParticipantError::VenueNotLoaded {
                venue: hyperliquid,
                scope: shrinking.scope.clone(),
            }
        ))),
        "a scope a shrinking commit took away is a market to wait for, not one this build \
         stopped serving"
    );
    assert_eq!(
        requests.lock().len(),
        served,
        "and that answer cost no request"
    );
    assert!(
        market
            .supported_timeframes(&route(&hyperliquid, CORE_PERP_SCOPE))
            .is_ok(),
        "the scope the venue still publishes keeps answering"
    );

    connector.shutdown().await;
    server.abort();
}
