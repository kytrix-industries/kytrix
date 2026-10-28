//! Unit tests of the market capability's own answers about a route.
//!
//! What a market can be asked is answered from the registry — the one view that
//! says what a load has committed. A scope a venue deploys under discovery
//! answers the venue's own bounds exactly as a declared scope does, an unloaded
//! route answers nothing at all (neither its bounds nor admission into the
//! catalogue), and every route a load committed answers the bounds of every
//! capability it declares.
//!
//! The venues behind the requests are this crate's fixtures: the catalogue
//! fixture is a real socket answering the catalogue protocol of the built-in
//! venues, and the info fixture answers Hyperliquid's discovery conversation, so
//! a load travels through the real gateway, registry and adapter dispatch.

use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    routing::CapabilityId,
};

/// The two directory files the venue's catalogue is published from.
use crate::adapter::longbridge::{NASDAQ_LISTED_FILE, OTHER_LISTED_FILE};
use crate::{
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

/// One built-in venue identity.
fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

/// The route of one built-in scope.
fn route(venue: &VenueId, scope: &str) -> VenueRoute {
    VenueRoute::new(
        *venue,
        VenueScopeId::new(scope).expect("the built-in scope is canonical"),
    )
}

/// The discovery conversation one builder-deployed DEX `xyz` is loaded through.
///
/// The venue answers its DEX table with the core DEX and the one deployed scope,
/// and serves that scope's metadata by name: the request the discovery makes and
/// the request the deployed scope's catalogue read makes.
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

/// Every route a load committed answers the bounds of every capability it
/// declares.
///
/// The view this pins is the registry: the bounds are looked up there, so a scope
/// the venue deployed under discovery is answered from the venue's own numbers
/// exactly as a declared route is, and no registered `(route, capability)` pair is
/// left without an answer. The check runs over the whole registry of a root that
/// loaded all three venues, so a route that is registered but unanswerable fails
/// the test rather than passing unnoticed.
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
    // The scope list and the entry list are one reading of one commit, so an
    // entry at an index belongs to the route at that index.
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

/// Every route that serves candlesticks publishes the neutral convention.
///
/// It is what a caller's fallback stands on. A stored adjustment a route no longer
/// publishes goes back to [`Adjustment::None`] — the chart's own default — and that
/// is only the route's answer while every route serving candlesticks publishes the
/// neutral convention: a route publishing `[Forward]` alone would make the fallback
/// a value the route does not serve, and the fallback would have to become "the
/// route's first published value". The check runs over every venue this build says
/// serves candlesticks, so a route that stopped publishing the neutral convention
/// fails here rather than in a chart.
///
/// Which venues those are is read off the definition table rather than listed here:
/// a venue the build starts serving candlesticks for is one this test has to load
/// and check, and a list written out by hand is exactly what would let it escape
/// the guard unnoticed. The loaded set is asserted against that table as well, so a
/// load that silently registered nothing cannot make the loop vacuous.
#[tokio::test]
async fn every_candlestick_route_publishes_the_neutral_convention() {
    let (info, server) = hyperliquid_fixture().await;
    let catalogs = CatalogFixture::start().await;
    // The venue whose directory is published by the market participant rather than by the
    // venue reads it from a fixture of its own, so no load in this test can leave
    // the process.
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
    // The scope list and the entry list are one reading of one commit, so an
    // entry at an index belongs to the route at that index.
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

/// A scope a venue deploys answers the venue's own bounds.
///
/// `perp-dex/xyz` is named only by the venue's discovery, so this is the answer
/// that proves the bounds are read from the route the load committed: an answer
/// read from the compile-time table refuses the capability outright. The interval
/// set is the venue's own fourteen — the same one the adapter answers for its
/// core scopes.
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

/// An unloaded venue is not a runtime answer.
///
/// Nothing is loaded, so `binance/spot` is a route this build *declares* — which
/// is what offers it to the settings interface — and nothing more. Both answers a
/// caller makes about a market it can use are refused, and refused as the two
/// different things they are: the route's bounds answer that the market is not
/// loaded, because that is what a caller waits on, while admission into the catalogue
/// answers that no registration names the route. The static table read as a runtime
/// answer would answer the first and leave a scope no load ever committed looking
/// usable.
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

/// the two answers, and neither of them costs a request.
///
/// A scope that loaded without a capability and a scope no load has committed are
/// different answers about different situations: the first is a scope this build does
/// not serve, the second is a market the user has not loaded — or one a shrinking
/// commit has taken away — which a caller waits for instead of giving up on. Both are
/// decided against the loaded venues, so neither reaches the venue.
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

/// A scope a shrinking commit took away is a market to wait for again.
///
/// The commit is a successful load that finds fewer deployments, so the scope that
/// disappeared is no longer in the loaded venues. Asking for its bounds after that has
/// to answer "not loaded" — the answer a caller waits on — rather than "this build does
/// not serve it", which is what a pane settles by clearing the user's own selection.
/// The answer is decided against the loaded venues, so it costs no request, and the
/// scopes the venue still publishes keep answering.
#[tokio::test]
async fn a_scope_a_shrinking_commit_took_away_answers_not_loaded() {
    let (info, requests, server) = info_fixture_server(
        // One load is one reading: the first names two deployments, the refresh one.
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
