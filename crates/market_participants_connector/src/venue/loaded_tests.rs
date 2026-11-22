//! Unit tests of the loaded-venue query face.
//!
//! The view is read the way its consumers read it: an empty table answers nothing
//! at all, and a committed venue answers the same entries through both of the
//! selector's judgements — the ordered scope list and the ordered entry list — as
//! it does through the single-scope lookup.
//!
//! The last two tests drive a real load through the fixtures and then issue a real
//! market request: the commit is the only place a request path can obtain its
//! venue's metadata, so a request that is answered without a fetch and a scope that
//! is refused without a request are both pinned against the wire.

use std::sync::Arc;

use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    routing::CapabilityId,
};

use super::{
    definition::{CapabilitySupport, ScopeEntry},
    loaded::LoadedVenues,
    metadata::VenueMetadata,
    test_support::{fixture_connector, load},
};
use crate::{
    adapter::ProtocolRequest,
    catalog::{
        generation::{CatalogGeneration, CatalogGenerationPublisher},
        source::CatalogSourceId,
    },
    error::MarketParticipantError,
    market_participants::model::{ForceOrderSubscriptionQuery, TickersQuery},
    protocol::RequestIdentity,
    routing::{
        BINANCE_VENUE, CORE_PERP_SCOPE, HYPERLIQUID_VENUE, OKX_VENUE, PERPETUAL_SCOPE, SPOT_SCOPE,
        SWAP_SCOPE,
    },
    test_support::{CatalogFixture, dex_meta, info_fixture_server, spot_meta_answer},
};

/// One built-in venue identity.
fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

/// The route of one scope of one venue.
fn route(venue: &VenueId, scope: &str) -> VenueRoute {
    VenueRoute::new(
        *venue,
        VenueScopeId::new(scope).expect("the built-in scope name is canonical"),
    )
}

/// One scope entry of a fixture commit.
fn entry(scope: &str, provides_catalog: bool, capabilities: &[CapabilityId]) -> Arc<ScopeEntry> {
    Arc::new(ScopeEntry {
        scope: VenueScopeId::new(scope).expect("the fixture scope name is canonical"),
        catalog_source: provides_catalog.then(|| {
            CatalogSourceId::new(format!("{scope}-catalog"))
                .expect("the fixture catalog source id is canonical")
        }),
        normalization: Vec::new(),
        capabilities: capabilities
            .iter()
            .map(|capability| CapabilitySupport {
                capability: *capability,
                needs_auth: false,
            })
            .collect(),
    })
}

/// A table no load has committed to answers nothing: no venue is loaded, no scope
/// is askable, and neither list has an entry — the static venue list is not a
/// runtime answer, and a scope no commit published is not askable even when the
/// venue that would deploy it is one this build speaks.
#[test]
fn an_empty_loaded_venues_answers_nothing() {
    let venues = LoadedVenues::new();

    for name in [BINANCE_VENUE, OKX_VENUE, HYPERLIQUID_VENUE] {
        assert!(
            venues.loaded(&venue(name)).is_none(),
            "{name} is not loaded before any commit"
        );
    }

    assert!(
        venues.scopes().is_empty(),
        "no commit published a scope to address"
    );
    assert!(
        venues.entries().is_empty(),
        "no commit published a scope entry"
    );
    assert_eq!(
        venues.scopes().len(),
        venues.entries().len(),
        "both lists answer the same commits"
    );

    let binance = venue(BINANCE_VENUE);
    let okx = venue(OKX_VENUE);
    let hyperliquid = venue(HYPERLIQUID_VENUE);
    for route in [
        route(&binance, SPOT_SCOPE),
        route(&okx, SWAP_SCOPE),
        route(&hyperliquid, CORE_PERP_SCOPE),
        route(&hyperliquid, "perp-dex/xyz"),
    ] {
        assert!(
            venues.scope(&route).is_none(),
            "{route} is not askable before its venue is loaded"
        );
    }
}

/// A commit is one reading of one venue: the scope list and the entry list it
/// answers agree entry by entry, the single-scope lookup answers the very entry
/// the list holds, and a later commit replaces the slot whole while the venue's
/// revision advances once.
#[test]
fn scopes_and_entries_agree_for_one_venue() {
    let venues = LoadedVenues::new();
    let binance = venue(BINANCE_VENUE);
    let spot = entry(
        SPOT_SCOPE,
        true,
        &[CapabilityId::MarketCatalog, CapabilityId::Candlesticks],
    );
    let perp = entry(PERPETUAL_SCOPE, true, &[CapabilityId::ForceOrder]);

    let generation = Arc::new(CatalogGeneration::empty(&CatalogGenerationPublisher::new()));
    let generation_id = generation.id();
    let revision = {
        let mut table = venues.lock_for_commit();
        table.replace(
            binance,
            vec![Arc::clone(&spot), Arc::clone(&perp)],
            generation_id,
            Arc::new(VenueMetadata::empty()),
        )
    };
    let committed = venues.loaded(&binance).expect("the commit is visible");
    assert_eq!(revision, committed.revision());

    let scopes = venues.scopes();
    let entries = venues.entries();
    assert!(committed.metadata().perp_dexes().is_empty());
    assert_eq!(
        committed.generation(),
        generation_id,
        "the entry names the generation the same commit published"
    );
    assert_eq!(
        entries.len(),
        scopes.len(),
        "both lists answer the same commit"
    );

    for (index, route) in scopes.iter().enumerate() {
        assert_eq!(
            route.venue, binance,
            "every route belongs to the committing venue"
        );
        assert_eq!(
            route.scope, entries[index].0.scope,
            "the two lists are aligned: the entry at an index is the route at it"
        );
        let answered = venues
            .scope(route)
            .unwrap_or_else(|| panic!("{route} is askable after its commit"));
        assert!(
            Arc::ptr_eq(&answered, &entries[index].1),
            "the lookup answers the entry the list holds"
        );
    }
    assert!(
        Arc::ptr_eq(&entries[0].1, &spot),
        "the first scope is the one the commit was handed first"
    );
    assert!(Arc::ptr_eq(&entries[1].1, &perp));

    assert_eq!(committed.revision(), 1, "a first commit is revision one");
    assert_eq!(
        venues
            .loaded(&binance)
            .expect("the committed venue is loaded")
            .revision(),
        1
    );

    let okx = venue(OKX_VENUE);
    assert!(
        venues.loaded(&okx).is_none(),
        "another venue is not loaded by this commit"
    );
    assert!(venues.scope(&route(&okx, SWAP_SCOPE)).is_none());

    let second_generation = Arc::new(CatalogGeneration::empty(&CatalogGenerationPublisher::new()));
    let second_generation_id = second_generation.id();
    let second_revision = {
        let mut table = venues.lock_for_commit();
        table.replace(
            binance,
            vec![Arc::clone(&spot)],
            second_generation_id,
            Arc::new(VenueMetadata::empty()),
        )
    };
    let replaced = venues
        .loaded(&binance)
        .expect("the second commit is visible");
    assert_eq!(second_revision, replaced.revision());
    assert_eq!(
        replaced.revision(),
        2,
        "a second commit of one venue advances its revision once"
    );
    assert_eq!(venues.scopes().len(), 1, "the slot is replaced whole");
    assert!(
        venues.scope(&route(&binance, PERPETUAL_SCOPE)).is_none(),
        "a scope the new commit dropped is no longer askable"
    );
    assert!(venues.scope(&route(&binance, SPOT_SCOPE)).is_some());
}

/// A request path never fetches metadata.
///
/// The load reads the venue's own tables once and commits them with the venue, so
/// the scope set it registers and the metadata a request is resolved against
/// describe one moment of the venue. The fixture counts the requests it served, so
/// what this pins is the wire rather than what the caller got back: the load is one
/// reading, and a market request after it asks the venue nothing at all.
#[tokio::test]
async fn a_request_path_never_fetches_metadata() {
    let (info, requests, server) = info_fixture_server(
        vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
        spot_meta_answer(&["USDC"]),
        dex_meta(&["BTC"]),
        vec![("xyz".to_owned(), dex_meta(&["xyz:ABC"]))],
    )
    .await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);
    let hyperliquid = venue(HYPERLIQUID_VENUE);

    load(&connector, &hyperliquid)
        .await
        .expect("the venue loads from the fixture reading");

    let count = |kind: &str| requests.lock().iter().filter(|seen| *seen == kind).count();
    assert_eq!(
        count("perpDexs"),
        1,
        "one load is one reading of the venue's deployment table"
    );
    assert_eq!(
        count("spotMeta"),
        0,
        "the token table is not asked for on its own: the spot answer carries it"
    );
    assert_eq!(
        count("spotMetaAndAssetCtxs"),
        1,
        "and one reading of its spot scope"
    );
    assert_eq!(
        count("metaAndAssetCtxs"),
        2,
        "and one reading per perpetual scope: the core one and the deployment"
    );
    let after_load = requests.lock().len();

    // The request path is resolved from the commit: the call is answered without a
    // metadata fetch, and the fixture publishes no asset contexts for its coin, so
    // an answer that found the metadata is the empty ticker set rather than the
    // refusal a scope without metadata would draw.
    let tickers = connector
        .market()
        .market(route(&hyperliquid, CORE_PERP_SCOPE))
        .all_tickers()
        .fetch()
        .await
        .expect("a loaded scope answers its tickers without a fetch");
    assert!(
        tickers.is_empty(),
        "the fixture publishes no asset contexts"
    );
    assert_eq!(
        requests.lock().len(),
        after_load,
        "a market request adds no request of its own"
    );

    connector.shutdown().await;
    server.abort();
}

/// A request for an unloaded scope is refused without a request.
///
/// The refusal is [`MarketParticipantError::VenueNotLoaded`]: the scope's venue has not
/// been loaded, which is a different answer from a loaded scope that does not
/// serve the capability. It happens before the request is encoded and before any
/// listing is resolved, so nothing reaches the wire — the fixture would panic on a
/// catalog request it does not know, and it served none.
#[tokio::test]
async fn a_request_for_an_unloaded_scope_is_refused_without_a_request() {
    let (info, _requests, server) = info_fixture_server(
        vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
        spot_meta_answer(&["USDC"]),
        dex_meta(&["BTC"]),
        vec![("xyz".to_owned(), dex_meta(&["xyz:ABC"]))],
    )
    .await;
    let catalogs = CatalogFixture::start().await;
    let (connector, _catalog) = fixture_connector(catalogs.origin(), &info);
    let market = connector.market();
    let binance = venue(BINANCE_VENUE);
    let spot = route(&binance, SPOT_SCOPE);

    assert!(
        connector.venues().scopes().is_empty(),
        "nothing is loaded in this root"
    );
    let identity = RequestIdentity::new(spot.clone(), CapabilityId::AllTickers);
    let refused = market
        .gateway()
        .execute(
            connector.venues(),
            ProtocolRequest::FetchAllTickers(TickersQuery {
                route: spot.clone(),
            }),
            identity,
            market.catalog(),
        )
        .await;
    let error = match refused {
        Ok(_) => panic!("a scope nothing loaded is refused"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        MarketParticipantError::VenueNotLoaded {
            venue: binance,
            scope: spot.scope.clone(),
        },
        "the refusal names the venue and the scope that were never loaded"
    );
    assert!(
        catalogs.served().is_empty(),
        "the refusal happens before the request is encoded: nothing was served"
    );

    // A scope a commit did publish without the capability is a different answer:
    // the two are not folded into one refusal.
    let hyperliquid = venue(HYPERLIQUID_VENUE);
    load(&connector, &hyperliquid)
        .await
        .expect("the venue loads from the fixture reading");
    let core_perp = route(&hyperliquid, CORE_PERP_SCOPE);
    let identity = RequestIdentity::new(core_perp.clone(), CapabilityId::ForceOrder);
    let refused = market
        .gateway()
        .subscribe(
            connector.venues(),
            ProtocolRequest::SubscribeForceOrder(ForceOrderSubscriptionQuery { route: core_perp }),
            identity,
            market.catalog(),
        )
        .await;
    let error = match refused {
        Ok(_) => panic!("the venue publishes no liquidation stream"),
        Err(error) => error,
    };
    assert!(
        matches!(error, MarketParticipantError::UnsupportedCapability { .. }),
        "a loaded scope without the capability is not an unloaded scope: {error:?}"
    );

    connector.shutdown().await;
    server.abort();
}
