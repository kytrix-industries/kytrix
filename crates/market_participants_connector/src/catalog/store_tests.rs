//! Catalog-level behaviour: pure reads, atomic publication and the resolution
//! surface.

use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use market_participants_model::{
    asset::AssetId,
    decimal::{Quantity, Tick},
    identifier::{
        CtfConditionId, CtfTokenId, HyperliquidQuestionId, IdentifierValue, NativeSymbol,
        PolymarketEventId, PolymarketMarketId, ResolutionSourceId, TradingCalendarId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, OutcomeTerms},
    listing::{
        identifiers::{
            HyperliquidListingIdentifiers, PolymarketListingIdentifiers, VenueListingIdentifiers,
        },
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        native::{ListingIdentifier, OutcomeEventIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, OutcomeTradingRules, PriceLimitRule, PriceRule,
            ProductTradingRules, QuantityRule, SettlementCycle, TradingRules,
        },
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidTokenId,
            PriceBasis,
        },
    },
    market::units::QuantityUnit,
    outcome::{
        OutcomeEvent, OutcomeEventId, OutcomeGroup, OutcomeGroupSeed, OutcomeMember,
        OutcomeRiskPool, OutcomeSide, OutcomeTopology, ResolutionRule, SettlementFractionRule,
    },
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
    settlement::Expiry,
};
use smol_str::SmolStr;
use test_support::{
    FIXTURE_CODES, Fixture, SPOT_SOURCE, case_insensitive_symbols, currency, entry,
    entry_with_aliases, failing, fixture_batch, gated, instrument_normalization, listing,
    listing_key, perp_route, requirements, spot, spot_route, spot_token, token,
};
use widget_tokio_runtime::runtime::Runtime;

use crate::catalog::{
    batch::{CatalogBatch, CatalogEntry, ReferencedTerms},
    display::types::DisplayError,
    error::{CatalogError, CatalogValidationError, SourceError},
    generation::CatalogGenerationId,
    registry::asset::{AssetRegistry, AssetRegistryPending, AssetTerms},
    source::{
        CatalogFetchContext, CatalogRouteRegistration, CatalogRouteRequirements,
        CatalogRouteUpdate, CatalogSource, CatalogSourceId, RouteInputRevision,
    },
    store::Catalog,
    test_support,
};

/// The native event lookups a route publishes, read from the public surface.
fn published_event_lookups(catalog: &Catalog, route: &VenueRoute) -> Vec<PolymarketEventId> {
    let mut natives: Vec<PolymarketEventId> = catalog
        .generation()
        .route_snapshot(route)
        .expect("the route has a published snapshot")
        .native_events()
        .keys()
        .cloned()
        .collect();
    natives.sort();
    natives
}

fn btc() -> Instrument {
    spot_token("binance", "BTC", "USDT")
}

fn eth() -> Instrument {
    spot_token("binance", "ETH", "USDT")
}

fn btc_entry() -> CatalogEntry {
    entry(&spot_route(), btc(), "BTCUSDT")
}

fn eth_entry() -> CatalogEntry {
    entry(&spot_route(), eth(), "ETHUSDT")
}

fn symbols_of(entries: &Arc<[CatalogEntry]>) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            entry
                .listing
                .native_symbol
                .as_ref()
                .map(|symbol| symbol.as_str().to_owned())
                .unwrap_or_default()
        })
        .collect()
}

fn btc_key() -> VenueListingKey {
    test_support::listing_key(&spot_route(), "BTCUSDT")
}

/// The application runtime a load crosses its blocking boundary on.
///
/// One runtime for the whole test binary, exactly as the application has one, and a
/// test that drives a load from its own thread never runs it — which is the
/// situation a GPUI task is in.
fn application() -> Runtime {
    static APPLICATION: OnceLock<Runtime> = OnceLock::new();
    APPLICATION.get_or_init(Runtime::for_test).clone()
}

/// A source that counts its fetches and reports the thread they ran on.
struct Threaded {
    entries: Vec<CatalogEntry>,
    fetches: Arc<std::sync::atomic::AtomicUsize>,
    fetched_on: Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
}

impl CatalogSource for Threaded {
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        let entries = self
            .entries
            .iter()
            .filter(|entry| &entry.listing.key.route == context.venue_route())
            .cloned()
            .collect::<Vec<_>>();
        let batch = fixture_batch(
            CatalogBatch {
                listings: entries,
                ..CatalogBatch::default()
            },
            &context,
        );
        let fetches = Arc::clone(&self.fetches);
        let fetched_on = Arc::clone(&self.fetched_on);
        Box::pin(async move {
            fetches.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            *fetched_on
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(std::thread::current().id());
            Ok(batch)
        })
    }
}

/// reads never fetch. However many readers ask a published route for its
/// snapshot, the source behind it is never reached — the count is the source's
/// own, so a read that quietly detached a fetch would be counted here.
#[tokio::test]
async fn a_read_never_fetches() {
    let (gate, source) = gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    gate.release();
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = Arc::clone(fixture.catalog_arc());
    let after_load = gate.calls();

    let mut readers = Vec::new();
    for _ in 0..4 {
        let catalog = Arc::clone(&catalog);
        readers.push(tokio::spawn(async move { catalog.listings(&spot_route()) }));
    }
    for reader in readers {
        assert!(reader.await.expect("no reader panics").is_ok());
    }
    assert_eq!(
        gate.calls(),
        after_load,
        "a read is answered from the published snapshot, so the source is never asked"
    );
}

/// A route with no published snapshot answers "not loaded" and
/// reaches no source at all.
#[tokio::test]
async fn a_read_of_a_route_with_no_snapshot_fetches_nothing() {
    let (gate, source) = gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    let catalog = fixture.catalog();

    let error = catalog
        .listings(&spot_route())
        .expect_err("a route with nothing published is not loaded");
    assert_eq!(
        error,
        CatalogError::CatalogNotLoaded {
            route: spot_route()
        }
    );
    assert!(matches!(
        fixture.ensured(&spot_route()),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
    assert_eq!(
        gate.calls(),
        0,
        "the refusal is answered from memory: no source is asked and nothing is queued"
    );
}

/// The published snapshot is served as it is: a second read answers with the very
/// pointer the first one read, whatever the route's inputs say.
#[tokio::test]
async fn a_published_snapshot_is_served_as_it_is() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    let first = catalog.listings(&spot_route()).expect("first read");
    let second = catalog.listings(&spot_route()).expect("second read");

    assert_eq!(symbols_of(&first), ["BTCUSDT"]);
    assert!(
        Arc::ptr_eq(&first, &second),
        "a published snapshot is served as it is"
    );
}

/// a load that fails leaves the snapshot the route already had exactly as
/// it was.
#[tokio::test]
async fn a_failed_refresh_preserves_the_previous_snapshot() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    let loaded = catalog.listings(&spot_route()).expect("first read");

    fixture.republish_source(SPOT_SOURCE, failing());
    let error = catalog
        .refresh_for_test(&spot_route())
        .await
        .expect_err("the refresh reports the source failure");

    assert!(matches!(error, CatalogError::SourceFailed { .. }));
    let served = catalog.listings(&spot_route()).expect("kept snapshot");
    assert!(Arc::ptr_eq(&served, &loaded));
}

/// A route with no published snapshot answers "not loaded" on every
/// read entry, and reaches no source at all, however its source would have answered.
#[tokio::test]
async fn a_route_with_no_snapshot_is_not_loaded_on_every_read_entry() {
    let fixture = Fixture::failing();

    let error = fixture
        .catalog()
        .listings(&spot_route())
        .expect_err("a route with nothing published is not loaded");

    assert_eq!(
        error,
        CatalogError::CatalogNotLoaded {
            route: spot_route()
        }
    );
    assert!(fixture.catalog().cached_listings(&spot_route()).is_none());
    assert!(
        matches!(
            fixture.catalog().contains(&btc_key()),
            Err(CatalogError::CatalogNotLoaded { .. })
        ),
        "contains answers the same refusal for a route with nothing published"
    );
    assert!(matches!(
        fixture.catalog().listing(&btc_key()),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
    assert!(matches!(
        fixture.catalog().resolve_listing(&btc_key()),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
    assert!(matches!(
        fixture.catalog().resolve_listing_ref(&btc_key()),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
    assert!(matches!(
        fixture
            .catalog()
            .resolve_native_symbol(&spot_route(), "BTCUSDT"),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
    assert!(matches!(
        fixture.catalog().ensure(&spot_route()),
        Err(CatalogError::CatalogNotLoaded { .. })
    ));
}

#[tokio::test]
async fn an_empty_catalog_is_a_successful_snapshot() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    let listings = catalog
        .listings(&spot_route())
        .expect("an empty catalog is a success");
    assert!(listings.is_empty());
    assert!(!catalog.contains(&btc_key()).expect("contains answers"));
    assert!(catalog.listings_for(btc().id()).is_empty());
    assert!(catalog.is_registered(&spot_route()));

    let before = catalog.generation_id();
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("refresh answers");
    assert!(catalog.generation_id() > before);
}

#[tokio::test]
async fn an_empty_route_can_publish_listings_later() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("empty load");

    fixture.republish_source(SPOT_SOURCE, listing(vec![btc_entry()]));
    let listings = catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("publishes");

    assert_eq!(symbols_of(&listings), ["BTCUSDT"]);
}

#[tokio::test]
async fn an_unregistered_route_is_refused() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");

    let error = fixture
        .catalog()
        .listings(&perp_route())
        .expect_err("an unregistered route is refused");

    assert_eq!(
        error,
        CatalogError::RouteNotRegistered {
            route: perp_route()
        }
    );
}

#[tokio::test]
async fn revoking_a_route_removes_it_atomically() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    assert!(catalog.contains(&btc_key()).expect("contains answers"));

    fixture.revoke_all();

    assert!(catalog.cached_listings(&spot_route()).is_none());
    assert!(matches!(
        catalog.listings(&spot_route()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
    assert!(catalog.listings_for(btc().id()).is_empty());
    assert!(catalog.cached_listing(&btc_key()).is_none());
    assert!(catalog.generation().route_snapshots().is_empty());
}

#[tokio::test]
async fn contains_and_native_resolution_use_one_snapshot() {
    let fixture = Fixture::new(vec![entry_with_aliases(
        &spot_route(),
        btc(),
        "BTCUSDT",
        &["XBTUSDT"],
    )]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    assert!(catalog.contains(&btc_key()).expect("contains answers"));
    assert_eq!(
        catalog
            .resolve_native_symbol(&spot_route(), "BTCUSDT")
            .expect("the symbol resolves"),
        btc_key()
    );
    assert_eq!(
        catalog
            .resolve_native_symbol(&spot_route(), "xbtusdt")
            .expect("an alias resolves through the route normalization"),
        btc_key()
    );
    assert_eq!(
        catalog
            .native_symbol(&btc_key())
            .as_ref()
            .map(NativeSymbol::as_str),
        Some("BTCUSDT")
    );
}

#[tokio::test]
async fn one_instrument_can_have_several_listings_across_routes() {
    let shared = btc();
    let other = perp_route();
    let first = entry(&spot_route(), shared.clone(), "BTCUSDT");
    let second = entry(&other, shared.clone(), "BTC-USDT");
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![first.clone(), second.clone()]),
        &[first.clone(), second.clone()],
        FIXTURE_CODES,
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(&other, "binance-perp", &[first.clone(), second.clone()]);
    fixture.load_route(&other).await;
    let catalog = fixture.catalog();

    let listings = catalog.listings_for(shared.id());
    assert_eq!(listings.len(), 2);
    assert!(listings.contains(&first.listing.key));
    assert!(listings.contains(&second.listing.key));
}

#[tokio::test]
async fn two_instruments_of_one_route_keep_their_own_identity() {
    // Two distinct economic instruments of one route. What this test asserts is
    // that each listing keeps its own instrument and its own identity in the
    // published snapshot, whatever name the two instruments carry.
    let primary = btc_entry();
    let secondary = eth_entry();
    let fixture = Fixture::new(vec![primary.clone(), secondary.clone()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("the load answers");

    let resolved = catalog
        .listing(&primary.listing.key)
        .expect("the primary listing resolves");
    let other = catalog
        .listing(&secondary.listing.key)
        .expect("the secondary listing resolves");

    assert_ne!(resolved.listing.key, other.listing.key);
    assert_ne!(resolved.instrument.id(), other.instrument.id());
    assert_eq!(catalog.listings_for(primary.instrument.id()).len(), 1);
    assert_eq!(catalog.listings_for(secondary.instrument.id()).len(), 1);
}

#[tokio::test]
async fn a_listing_resolution_carries_the_generation() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    let resolved = catalog
        .resolve_listing(&btc_key())
        .expect("the listing resolves");

    assert_eq!(resolved.key, btc_key());
    assert_eq!(resolved.generation, catalog.generation_id());
    assert_eq!(resolved.status, ListingStatus::Trading);
    assert_eq!(resolved.instrument.id(), btc().id());

    let reference = catalog
        .resolve_listing_ref(&btc_key())
        .expect("the reference resolves");
    assert_eq!(reference.generation, resolved.generation);
    assert_eq!(reference.key, resolved.key);
}

#[tokio::test]
async fn a_typed_identifier_resolves_without_relying_on_the_symbol() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    let identifier = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("BTCUSDT").expect("the fixture symbol is valid"),
        product: market_participants_model::listing::venue::BinanceProduct::Spot,
    };

    assert_eq!(
        catalog
            .resolve_listing_identifier(&spot_route(), &identifier)
            .expect("the typed identifier resolves"),
        btc_key()
    );
}

#[tokio::test]
async fn a_native_identifier_of_another_scope_is_refused() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    let error = catalog
        .resolve_listing_identifier(
            &spot_route(),
            &ListingIdentifier::HyperliquidAsset {
                scope: perp_route().scope,
                product:
                    market_participants_model::listing::venue::HyperliquidAssetProduct::Perpetual,
                asset_id: market_participants_model::listing::venue::HyperliquidAssetId(7),
            },
        )
        .expect_err("a scoped identifier of another route is refused");

    assert!(matches!(
        error,
        CatalogError::NativeIdentifierRouteMismatch { .. }
    ));
}

#[tokio::test]
async fn an_unknown_native_identifier_is_reported() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("the load answers");

    assert!(matches!(
        catalog.resolve_listing_identifier(
            &spot_route(),
            &ListingIdentifier::PolymarketToken(
                market_participants_model::identifier::CtfTokenId::new("12345")
                    .expect("the token id is valid")
            )
        ),
        Err(CatalogError::NativeIdentifierNotFound { .. })
    ));
    assert!(matches!(
        catalog.resolve_group_identifier(
            &spot_route(),
            &OutcomeGroupIdentifier::PolymarketMarket(
                market_participants_model::identifier::PolymarketMarketId::new("market-1")
                    .expect("the market id is valid")
            )
        ),
        Err(CatalogError::NativeGroupIdentifierNotFound { .. })
    ));
    assert!(matches!(
        catalog.resolve_event_identifier(
            &spot_route(),
            &OutcomeEventIdentifier::PolymarketEvent(
                market_participants_model::identifier::PolymarketEventId::new("event-1")
                    .expect("the event id is valid")
            )
        ),
        Err(CatalogError::NativeEventIdentifierNotFound { .. })
    ));
    assert!(
        catalog
            .outcome_group(
                &market_participants_model::outcome::OutcomeGroupId::from_identity_value(&format!(
                    "v1/{}",
                    "0".repeat(64)
                ))
                .expect("the placeholder group id is well formed")
            )
            .is_none()
    );
}

#[tokio::test]
async fn a_catalog_route_the_registry_does_not_know_is_never_fetched() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");

    assert!(fixture.catalog().cached_listings(&perp_route()).is_none());
    assert!(matches!(
        fixture.catalog().refresh_for_test(&perp_route()).await,
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

#[tokio::test]
async fn ensure_proves_the_route_is_loaded() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");

    let ensured = fixture.ensure().await.expect("ensure loads the route");

    assert_eq!(ensured.route(), &spot_route());
    assert_eq!(ensured.listings().len(), 1);
    assert_eq!(
        ensured
            .resolve_native_symbol(&NativeSymbol::new("BTCUSDT").expect("the symbol is valid"))
            .expect("the symbol resolves"),
        btc_key()
    );
    assert_eq!(
        ensured
            .native_symbol(&btc_key())
            .as_ref()
            .map(NativeSymbol::as_str),
        Some("BTCUSDT")
    );
    assert_eq!(
        ensured
            .resolve_listing(&btc_key())
            .expect("the listing resolves")
            .key,
        btc_key()
    );
}

#[tokio::test]
async fn ensure_reports_an_unregistered_route() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");

    assert!(matches!(
        fixture.catalog().ensure(&perp_route()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

/// The kind of refusal one catalog error carries.
fn refused_kind(error: &CatalogError) -> &CatalogValidationError {
    match error {
        CatalogError::InvalidCatalog { error, .. } => error.as_ref(),
        other => panic!("expected a refused catalog, got {other}"),
    }
}

#[tokio::test]
async fn a_publication_advances_the_generation() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("first read");
    let first = catalog.generation_id();

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("refresh");
    let second = catalog.generation_id();
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("second refresh");
    let third = catalog.generation_id();

    assert!(second > first);
    assert!(third > second);
}

#[tokio::test]
async fn a_cached_generation_is_immutable() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("first read");
    let generation = catalog.generation();
    let id = generation.id();

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("refresh");

    assert_eq!(generation.id(), id);
    assert_eq!(generation.route_snapshots().len(), 1);
    assert!(catalog.generation().id() > id);
}

#[tokio::test]
async fn a_publication_is_one_atomic_replacement() {
    let first = btc_entry();
    let second = eth_entry();
    let fixture = Fixture::new(vec![first.clone()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("first read");
    let before = catalog.generation();

    fixture.republish_source(SPOT_SOURCE, listing(vec![first.clone(), second.clone()]));
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("refresh");
    let after = catalog.generation();

    assert_eq!(before.route_snapshots().len(), 1);
    assert_eq!(after.route_snapshots().len(), 1);
    assert_eq!(
        after.listings_for(btc().id()).map(|keys| keys.len()),
        Some(1)
    );
    assert_eq!(
        after.listings_for(eth().id()).map(|keys| keys.len()),
        Some(1)
    );
    assert!(before.listings_for(eth().id()).is_none());
}

#[tokio::test]
async fn two_catalog_instances_never_produce_the_same_generation() {
    let first = Fixture::new(vec![btc_entry()]);
    let second = Fixture::new(vec![btc_entry()]);
    let third = Fixture::new(vec![btc_entry()]);
    let mut published: Vec<CatalogGenerationId> = Vec::with_capacity(12);

    for _ in 0..4 {
        for fixture in [&first, &second, &third] {
            fixture
                .catalog()
                .refresh_for_test(&spot_route())
                .await
                .expect("every catalog refreshes");
            published.push(fixture.catalog().generation_id());
        }
    }

    let distinct: ahash::AHashSet<CatalogGenerationId> = published.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        published.len(),
        "ids minted by different catalog instances must never collide"
    );
    for window in published.windows(2) {
        assert!(
            window[1] > window[0],
            "the shared clock only ever moves forward"
        );
    }
    // The three catalogs published in lockstep, so their own sequences are the
    // three interleaved lanes of the same strictly increasing series.
    for lane in 0..3 {
        let own: Vec<CatalogGenerationId> =
            published.iter().skip(lane).step_by(3).copied().collect();
        assert_eq!(own.len(), 4);
        for window in own.windows(2) {
            assert!(
                window[1] > window[0],
                "one catalog's own publications strictly increase"
            );
        }
    }
    assert_ne!(
        first.catalog().generation_id(),
        second.catalog().generation_id(),
        "two fixtures publishing the same batch still mint different generations"
    );
}

#[tokio::test]
async fn one_catalog_never_reuses_a_generation_across_its_clones() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = Arc::clone(fixture.catalog_arc());
    let mut published: Vec<CatalogGenerationId> = Vec::with_capacity(6);

    for _ in 0..6 {
        catalog
            .refresh_for_test(&spot_route())
            .await
            .expect("the catalogue refreshes");
        published.push(catalog.generation_id());
    }

    let distinct: ahash::AHashSet<CatalogGenerationId> = published.iter().copied().collect();
    assert_eq!(distinct.len(), published.len());
    for window in published.windows(2) {
        assert!(window[1] > window[0]);
    }
}

#[tokio::test]
async fn every_clone_of_one_catalog_shares_the_same_clock() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let first = Arc::clone(fixture.catalog_arc());
    let second = Arc::clone(fixture.catalog_arc());
    let before = first.generation_id();

    first
        .refresh_for_test(&spot_route())
        .await
        .expect("the first clone refreshes");

    assert!(second.generation_id() > before);
    assert!(second.cached_listings(&spot_route()).is_some());
}

#[tokio::test]
async fn the_route_set_exposes_its_catalog_routes() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");

    let route_set = fixture.catalog().route_set().expect("the route set exists");

    assert_eq!(route_set.catalog_routes(), &[spot_route()]);
    assert_eq!(
        route_set
            .source_of(&spot_route())
            .map(|binding| binding.id().to_smol_str()),
        Some(SPOT_SOURCE.into())
    );
    assert!(route_set.source_of(&perp_route()).is_none());
}

#[tokio::test]
async fn an_unregistered_source_id_is_refused() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let update = CatalogRouteUpdate::new(
        [],
        [CatalogRouteRegistration::catalog(
            spot_route(),
            CatalogSourceId::new("binance-margin").expect("the source id is valid"),
            case_insensitive_symbols(),
            vec![CapabilityId::MarketCatalog],
            RouteInputRevision::describe().build(),
        )],
    );

    let error = fixture
        .catalog()
        .commit_fixture_routes(update, &requirements(&[spot_route()]))
        .expect_err("an unregistered source is refused");

    assert!(matches!(
        error,
        CatalogError::UnregisteredCatalogSource { .. }
    ));
    assert!(fixture.catalog().is_registered(&spot_route()));
}

#[tokio::test]
async fn a_catalog_route_without_a_source_is_refused_as_a_whole() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let before = fixture.catalog().generation_id();
    let update = CatalogRouteUpdate::new(
        [],
        [CatalogRouteRegistration::without_catalog(
            spot_route(),
            case_insensitive_symbols(),
            vec![CapabilityId::Tickers],
            RouteInputRevision::describe().build(),
        )],
    );

    let error = fixture
        .catalog()
        .commit_fixture_routes(update, &requirements(&[spot_route()]))
        .expect_err("a catalog route must bind a source");

    assert!(matches!(error, CatalogError::MissingCatalogSource { .. }));
    let route_set = fixture.catalog().route_set().expect("the set survives");
    assert_eq!(route_set.catalog_routes(), &[spot_route()]);
    assert_eq!(fixture.catalog().generation_id(), before);
}

#[tokio::test]
async fn a_route_without_a_catalog_must_not_bind_a_catalog_source() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let non_catalog = perp_route();
    let update = CatalogRouteUpdate::new(
        [test_support::binding(SPOT_SOURCE, listing(Vec::new()))],
        [CatalogRouteRegistration::catalog(
            non_catalog.clone(),
            CatalogSourceId::new(SPOT_SOURCE).expect("the source id is valid"),
            case_insensitive_symbols(),
            vec![CapabilityId::MarketCatalog],
            RouteInputRevision::describe().build(),
        )],
    );

    let error = fixture
        .catalog()
        .commit_fixture_routes(
            update,
            &test_support::non_catalog_requirements(&[non_catalog]),
        )
        .expect_err("a route without a catalog must not bind a catalog source");

    assert!(matches!(
        error,
        CatalogError::CatalogSourceOnNonCatalogRoute { .. }
    ));
}

#[tokio::test]
async fn a_route_without_a_catalog_never_enters_the_catalog_route_set() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let non_catalog = perp_route();
    let update = CatalogRouteUpdate::new(
        [test_support::binding(
            SPOT_SOURCE,
            listing(vec![btc_entry()]),
        )],
        [
            test_support::registration(&spot_route(), SPOT_SOURCE),
            test_support::non_catalog_registration(&non_catalog),
        ],
    );
    fixture
        .catalog()
        .commit_fixture_routes(
            update,
            &CatalogRouteRequirements::from_catalog_routes([spot_route()]),
        )
        .expect("the mixed route set is accepted");

    let route_set = fixture.catalog().route_set().expect("the route set exists");
    assert_eq!(route_set.catalog_routes(), &[spot_route()]);
    assert!(
        route_set
            .route(&non_catalog)
            .is_some_and(|entry| entry.catalog_source().is_none())
    );
    assert!(matches!(
        fixture.catalog().listings(&non_catalog),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

#[tokio::test]
async fn a_duplicate_source_id_is_refused() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let update = CatalogRouteUpdate::new(
        [
            test_support::binding(SPOT_SOURCE, listing(Vec::new())),
            test_support::binding(SPOT_SOURCE, listing(Vec::new())),
        ],
        [test_support::registration(&spot_route(), SPOT_SOURCE)],
    );

    let error = fixture
        .catalog()
        .commit_fixture_routes(update, &requirements(&[spot_route()]))
        .expect_err("a source id must be unique");

    assert!(matches!(
        error,
        CatalogError::DuplicateCatalogSourceId { .. }
    ));
}

#[test]
fn an_invalid_source_id_is_refused() {
    assert!(CatalogSourceId::new("Binance Spot").is_err());
    assert!(CatalogSourceId::new("").is_err());
    assert!(CatalogSourceId::new("binance-spot").is_ok());
}

/// Both dimensions a declaration is keyed by take part in the revision, so two
/// registrations of one route differ exactly when one of them differs.
#[test]
fn a_route_input_revision_covers_every_declared_input() {
    let bare = RouteInputRevision::of_declaration(&spot_route(), None, &[]);
    let declared = RouteInputRevision::of_declaration(
        &spot_route(),
        Some(&CatalogSourceId::new(SPOT_SOURCE).expect("the fixture source id is valid")),
        &case_insensitive_symbols(),
    );

    assert_ne!(
        bare, declared,
        "the source and the rules a route binds take part in its revision"
    );
    assert_eq!(
        bare.catalog_descriptor(),
        RouteInputRevision::of_declaration(&spot_route(), None, &[]).catalog_descriptor(),
        "the same declaration describes the same revision"
    );
    assert_ne!(
        bare.normalization_rules(),
        declared.normalization_rules(),
        "the declared rules are a dimension of their own"
    );
    assert_ne!(
        declared,
        RouteInputRevision::of_declaration(
            &perp_route(),
            Some(&CatalogSourceId::new(SPOT_SOURCE).expect("the fixture source id is valid")),
            &case_insensitive_symbols(),
        ),
        "two routes never share a revision"
    );
}

/// P1-D1: a route republished with other declared inputs refuses the batch a load
/// staged under the declaration it replaced.
///
/// The revision a registration carries is derived from the inputs the route is
/// declared with — its catalog source and its identifier normalization rules — so
/// re-registering the route under another rule moves its revision, and a fetch
/// that started under the old declaration must not be written under the new one.
#[tokio::test]
async fn a_route_republished_with_other_declared_inputs_discards_its_staged_batch() {
    let (gate, source) = gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    let loader = {
        let catalog: Arc<Catalog> = Arc::clone(fixture.catalog_arc());
        tokio::spawn(async move { catalog.refresh_for_test(&spot_route()).await })
    };
    gate.started().await;
    tokio::task::yield_now().await;
    // The route's declared inputs move while its fetch is in flight, so the answer
    // it staged belongs to a declaration the route no longer carries.
    fixture.republish_declared_inputs(instrument_normalization(SymbolNormalization::Exact));
    gate.release();

    let error = loader
        .await
        .expect("the loader should not panic")
        .expect_err("a batch of another declaration must be discarded");

    assert!(
        matches!(
            error,
            CatalogError::RouteRevisionChanged { ref route, .. } if route == &spot_route()
        ),
        "{error:?}"
    );
    assert!(
        fixture.catalog().cached_listings(&spot_route()).is_none(),
        "the refused batch publishes nothing"
    );
}

#[tokio::test]
async fn a_normalized_symbol_collision_is_refused() {
    let shared = btc();
    let lower = entry(&spot_route(), shared.clone(), "btcusdt");
    let upper = entry(&spot_route(), shared.clone(), "BTCUSDT");
    let fixture = Fixture::new(vec![upper.clone()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.republish_source(SPOT_SOURCE, listing(vec![lower, upper]));

    let error = fixture
        .catalog()
        .refresh_for_test(&spot_route())
        .await
        .expect_err("two listings cannot share a normalized symbol");

    assert!(matches!(
        refused_kind(&error),
        CatalogValidationError::DuplicateNativeIdentifier { .. }
    ));
}

#[tokio::test]
async fn a_listing_that_declares_another_route_is_refused() {
    let foreign = entry(&perp_route(), btc(), "BTCUSDT");
    let fixture = Fixture::new(Vec::new());
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.republish_source(SPOT_SOURCE, test_support::raw(vec![foreign]));

    let error = fixture
        .catalog()
        .refresh_for_test(&spot_route())
        .await
        .expect_err("a listing of another route is refused");

    assert!(matches!(
        refused_kind(&error),
        CatalogValidationError::ListingRouteMismatch { .. }
    ));
}

#[tokio::test]
async fn a_listing_that_declares_another_instrument_is_refused() {
    let mut broken = btc_entry();
    broken.listing.instrument_id = eth().id().clone();
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.republish_source(SPOT_SOURCE, test_support::raw(vec![broken]));

    let error = fixture
        .catalog()
        .refresh_for_test(&spot_route())
        .await
        .expect_err("the declared identity must match the terms");

    assert!(matches!(
        refused_kind(&error),
        CatalogValidationError::InstrumentTermsMismatch { .. }
    ));
}

#[tokio::test]
async fn a_snapshot_of_another_key_is_refused() {
    let mut broken = btc_entry();
    broken.snapshot.key = test_support::listing_key(&spot_route(), "ETHUSDT");
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.republish_source(SPOT_SOURCE, test_support::raw(vec![broken]));

    let error = fixture
        .catalog()
        .refresh_for_test(&spot_route())
        .await
        .expect_err("a snapshot must describe its own listing");

    assert!(matches!(
        refused_kind(&error),
        CatalogValidationError::SnapshotKeyMismatch { .. }
    ));
}

#[tokio::test]
async fn an_invalid_catalog_does_not_replace_the_previous_snapshot() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    let loaded = catalog.listings(&spot_route()).expect("first read");
    let mut broken = btc_entry();
    broken.snapshot.key = test_support::listing_key(&spot_route(), "OTHER");
    fixture.republish_source(SPOT_SOURCE, test_support::raw(vec![broken]));

    assert!(catalog.refresh_for_test(&spot_route()).await.is_err());
    let served = catalog.listings(&spot_route()).expect("kept snapshot");
    assert!(Arc::ptr_eq(&served, &loaded));
}

/// A published snapshot names its symbols, and a route re-registered under the
/// same declaration keeps serving it.
#[tokio::test]
async fn a_published_snapshot_still_names_its_symbols() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    fixture.republish_source(SPOT_SOURCE, listing(vec![btc_entry()]));

    assert_eq!(
        catalog
            .native_symbol(&btc_key())
            .as_ref()
            .map(NativeSymbol::as_str),
        Some("BTCUSDT")
    );
    assert_eq!(
        catalog
            .resolve_native_symbol(&spot_route(), "BTCUSDT")
            .expect("the published snapshot still resolves"),
        btc_key()
    );
}

#[tokio::test]
async fn a_listing_of_another_route_is_refused_by_the_ensured_route() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(&perp_route(), "binance-perp", &[]);
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let other = VenueListingKey::new(perp_route(), test_support::listing_id("a1"));

    assert!(ensured.resolve_listing(&other).is_err());
    assert!(ensured.native_symbol(&other).is_none());
}

/// A load is not answered by the published snapshot: it asks the source again and
/// publishes what it answered, while a read of the same route asks nothing.
#[tokio::test]
async fn a_load_asks_the_source_again() {
    let (gate, source) = gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    gate.release();
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let published = fixture.generation();
    let after_first_load = gate.calls();

    let served = fixture
        .catalog()
        .listings(&spot_route())
        .expect("the published route answers");
    assert_eq!(
        gate.calls(),
        after_first_load,
        "the read is answered from the snapshot, so it asks the source nothing"
    );

    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    assert_eq!(
        gate.calls(),
        after_first_load + 1,
        "a load asks the source for the catalog again"
    );
    assert!(
        fixture.generation() > published,
        "and publishes what the source answered as a new generation"
    );
    assert_eq!(symbols_of(&served), ["BTCUSDT"]);
}

/// The connection action runs on a thread that runs no runtime, and its load still
/// fetches.
///
/// The fetch that answers a load reaches the venue through a blocking pool, and
/// asking for that pool without a runtime is a panic rather than an error. Attaching
/// the application runtime to the catalogue is what makes the load work from there,
/// so this drives a cold route from the test thread — which runs no runtime of its
/// own — through the one write entry a load has.
#[test]
fn a_load_from_a_thread_without_a_runtime_still_fetches() {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "the test thread must run no runtime for this to prove anything"
    );
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture.catalog().set_runtime(application());
    let catalog = Arc::clone(fixture.catalog_arc());

    let listings = application()
        .handle()
        .block_on(catalog.refresh_for_test(&spot_route()))
        .expect("a load must answer instead of panicking");

    assert_eq!(symbols_of(&listings), ["BTCUSDT"]);
    let resolved = catalog
        .resolve_listing(&btc_key())
        .expect("the route the load published answers");
    assert_eq!(resolved.key, btc_key());
    assert_eq!(resolved.instrument.id(), btc().id());
}

/// The fetch itself runs on the application runtime, not on the caller's thread.
///
/// Running the load safely is not enough: the venue request has to leave the thread
/// that draws frames, or a slow venue stalls the window. The source reports the
/// thread it ran on, and it must not be this one.
#[test]
fn a_load_runs_on_the_application_runtime_rather_than_the_caller() {
    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fetched_on = Arc::new(std::sync::Mutex::new(None));
    let source = Arc::new(Threaded {
        entries: vec![btc_entry()],
        fetches: Arc::clone(&fetches),
        fetched_on: Arc::clone(&fetched_on),
    });
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    fixture.catalog().set_runtime(application());
    let catalog = Arc::clone(fixture.catalog_arc());

    let listings = application()
        .handle()
        .block_on(catalog.refresh_for_test(&spot_route()))
        .expect("the cold route loads");

    assert_eq!(symbols_of(&listings), ["BTCUSDT"]);
    assert_eq!(
        fetches.load(std::sync::atomic::Ordering::Acquire),
        1,
        "the load asks the source exactly once"
    );
    let fetch_thread = fetched_on
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .expect("the source must have been asked");
    assert_ne!(
        fetch_thread,
        std::thread::current().id(),
        "the fetch must not run on the thread that asked for it"
    );
}

/// a failed refresh keeps the snapshot the route already had, and a
/// second refresh asks the venue again instead of replaying the failure.
#[tokio::test]
async fn a_manual_refresh_does_not_reuse_a_failure() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    let published = catalog
        .cached_listings(&spot_route())
        .expect("the fixture route published its catalog");

    fixture.republish_source(SPOT_SOURCE, failing());
    assert!(catalog.refresh_for_test(&spot_route()).await.is_err());
    assert!(catalog.refresh_for_test(&spot_route()).await.is_err());
    assert!(
        Arc::ptr_eq(
            &catalog
                .cached_listings(&spot_route())
                .expect("a failed refresh keeps the previous snapshot"),
            &published
        ),
        "nothing a failed refresh did replaces what the route already published"
    );
}

/// A route re-registered under the declaration it already carried keeps serving
/// the snapshot it published: the same inputs are the same registration.
#[tokio::test]
async fn a_published_snapshot_is_served_under_the_declaration_it_was_fetched_with() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    fixture.republish_source(SPOT_SOURCE, listing(vec![btc_entry()]));

    let cached = catalog
        .cached_listings(&spot_route())
        .expect("a published snapshot stays published");

    assert_eq!(symbols_of(&cached), ["BTCUSDT"]);
}

/// A route re-registered under another declaration serves nothing: its snapshot
/// was built for the route it used to be, so it is not this route's catalogue.
#[tokio::test]
async fn a_snapshot_of_another_declaration_is_not_served() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();

    fixture.republish_declared_inputs(instrument_normalization(SymbolNormalization::Exact));

    assert!(catalog.cached_listings(&spot_route()).is_none());
    assert_eq!(
        catalog.listings(&spot_route()),
        Err(CatalogError::CatalogNotLoaded {
            route: spot_route()
        }),
        "a route whose declaration moved has no published catalogue until it is loaded again"
    );
}

#[tokio::test]
async fn a_token_and_a_currency_with_one_code_stay_apart() {
    let token_instrument = spot(token("binance", "BTC"), token("binance", "USDT"));
    let currency_instrument = spot(currency("USD"), currency("EUR"));
    let first = entry(&spot_route(), token_instrument.clone(), "BTCUSDT");
    let second = entry(&spot_route(), currency_instrument.clone(), "USDEUR");
    let fixture = Fixture::new(vec![first, second]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("the load answers");

    assert_ne!(token_instrument.id(), currency_instrument.id());
    assert_eq!(catalog.listings_for(token_instrument.id()).len(), 1);
    assert_eq!(catalog.listings_for(currency_instrument.id()).len(), 1);
}

/// A published generation is only replaced by a load: a read that changes nothing
/// leaves the generation id exactly as it was.
#[tokio::test]
async fn the_fixture_catalog_reports_its_own_generation() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let before = fixture.generation();
    fixture
        .catalog()
        .listings(&spot_route())
        .expect("the published route answers");

    assert_eq!(
        fixture.generation(),
        before,
        "a read publishes nothing, so the generation it read stays the published one"
    );
    assert_eq!(fixture.registered().route(), &spot_route());
    assert_eq!(
        fixture.registered().route_revision(),
        test_support::registration(&spot_route(), SPOT_SOURCE).route_revision(),
        "a registered route carries the revision of the declaration it was accepted under"
    );
}

#[tokio::test]
async fn a_listing_identity_index_is_recomputed_on_every_publication() {
    let first = btc_entry();
    let second = eth_entry();
    let fixture = Fixture::new(vec![first.clone()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("first read");
    assert_eq!(catalog.listings_for(eth().id()).len(), 0);

    fixture.republish_source(SPOT_SOURCE, listing(vec![first, second]));
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("refresh answers");

    assert_eq!(catalog.listings_for(btc().id()).len(), 1);
    assert_eq!(catalog.listings_for(eth().id()).len(), 1);
}

#[tokio::test]
async fn a_revoked_route_is_no_longer_resolvable() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.ensure().await.expect("the load answers");

    fixture.revoke_all();

    assert!(
        fixture
            .catalog()
            .resolve_native_symbol(&spot_route(), "BTCUSDT")
            .is_err()
    );
    assert!(fixture.catalog().cached_listing(&btc_key()).is_none());
    assert!(matches!(
        fixture.catalog().resolve_listing(&btc_key()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

/// the scope a shrinking commit drops is not merely unregistered — it is
/// unaskable.
///
/// Two scopes are loaded and the commit that follows holds a strictly smaller
/// set: the perpetual scope and not the spot one. The dropped scope leaves every
/// answer in that commit — it is out of the registered set, and admission,
/// listings, native resolution and listing resolution all refuse it with
/// [`CatalogError::RouteNotRegistered`] instead of answering from the
/// generation it was last published in. The scope the commit kept stays askable,
/// so the refusals above are about the dropped scope rather than about the
/// commit.
#[tokio::test]
async fn a_revoked_scope_is_unaskable_not_just_unregistered() {
    let perp = entry(&perp_route(), eth(), "ETH-USDT");
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![btc_entry(), perp.clone()]),
        &[btc_entry(), perp.clone()],
        FIXTURE_CODES,
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(&perp_route(), "binance-perp", std::slice::from_ref(&perp));
    fixture.load_route(&perp_route()).await;
    let catalog = fixture.catalog();

    assert!(
        catalog.registered_route(&spot_route()).is_ok(),
        "the spot scope is registered before the shrinking commit"
    );
    assert_eq!(
        catalog
            .resolve_native_symbol(&spot_route(), "BTCUSDT")
            .expect("a loaded scope answers its own symbol"),
        btc_key()
    );

    // The candidate the next commit publishes holds the perpetual scope alone,
    // so the spot scope vanishes from the registered set in that publication.
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [test_support::binding("binance-perp", listing(vec![perp]))],
                [test_support::registration(&perp_route(), "binance-perp")],
            ),
            &requirements(std::slice::from_ref(&perp_route())),
        )
        .expect("a candidate that drops one scope is accepted");

    assert!(
        catalog.registered_route(&spot_route()).is_err(),
        "the dropped scope is out of the registered set"
    );
    assert!(
        catalog.source_of(&spot_route()).is_none(),
        "and its source binding is gone with it"
    );
    assert_eq!(
        fixture.ensured(&spot_route()).err(),
        Some(CatalogError::RouteNotRegistered {
            route: spot_route()
        }),
        "admission of the dropped scope is refused"
    );
    assert_eq!(
        catalog.listings(&spot_route()),
        Err(CatalogError::RouteNotRegistered {
            route: spot_route()
        }),
        "the dropped scope serves no catalogue"
    );
    assert_eq!(
        catalog.resolve_native_symbol(&spot_route(), "BTCUSDT"),
        Err(CatalogError::RouteNotRegistered {
            route: spot_route()
        }),
        "and no symbol resolves on it"
    );
    assert_eq!(
        catalog.resolve_listing(&btc_key()),
        Err(CatalogError::RouteNotRegistered {
            route: spot_route()
        }),
        "and no listing of it resolves"
    );
    assert!(catalog.cached_listings(&spot_route()).is_none());
    assert!(catalog.cached_listing(&btc_key()).is_none());

    // The scope the shrinking commit kept is still askable, so what was refused
    // above is the dropped scope rather than the commit.
    assert!(
        catalog.registered_route(&perp_route()).is_ok(),
        "the scope the commit kept stays registered"
    );
    assert!(fixture.ensured(&perp_route()).is_ok(), "and stays admitted");
    assert_eq!(
        catalog
            .resolve_native_symbol(&perp_route(), "ETH-USDT")
            .expect("the surviving scope answers its own symbol"),
        listing_key(&perp_route(), "ETH-USDT")
    );
}

#[tokio::test]
async fn the_assets_of_a_published_generation_are_the_registry_terms() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let generation = fixture.catalog().generation();

    assert!(
        generation
            .assets()
            .crypto_token(&test_support::token_id("binance", "USDT"))
            .is_some(),
        "the venue-issued token the fixture listings trade is published"
    );
    assert!(!generation.assets().is_empty());
}

// ---- registry-only publication ------------------------------------------

/// The published registry that carries exactly the given assets.
///
/// It goes through the same boundary a real batch does, so what a test installs is a
/// registry the catalog itself could have published rather than a hand-built map.
fn registry_of(terms: Vec<AssetTerms>) -> Arc<AssetRegistry> {
    let mut pending = AssetRegistryPending::new();
    let mut referenced = ReferencedTerms::default();
    for terms in terms {
        referenced.assets.insert(terms.asset_id());
        pending.stage(terms);
    }
    Arc::new(crate::catalog::batch::publish_assets(
        &Arc::new(AssetRegistry::new()),
        &pending,
        &referenced,
    ))
}

/// The canonical terms of every asset the published generation's registry carries.
fn registered_terms(catalog: &Catalog) -> Vec<AssetTerms> {
    let generation = catalog.generation();
    generation
        .assets()
        .assets()
        .into_iter()
        .filter_map(|asset| generation.assets().terms(&asset))
        .collect()
}

/// The display code the published catalog gives one asset.
fn published_code(catalog: &Catalog, asset: &AssetId) -> SmolStr {
    let generation = catalog.generation();
    SmolStr::new(
        generation
            .display_catalog()
            .asset_code(asset)
            .unwrap_or_else(|| panic!("the published catalog names no code for {asset:?}"))
            .as_str(),
    )
}

/// The display error inside a refused publication.
fn refused_display(error: &CatalogError) -> &DisplayError {
    match error {
        CatalogError::InvalidCatalog { error, .. } => match error.as_ref() {
            CatalogValidationError::DisplayRejected { error, .. } => error,
            other => panic!("expected a display refusal, got {other:?}"),
        },
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// a registry-only publication is a publication of its own.
///
/// The registries are installed under a *fresh* generation id even though not one
/// route snapshot changed, because the display catalog is rebuilt against them and
/// every reader of the generation has to see the rebuild. A publication the display
/// gate refuses publishes nothing: the generation the catalogue served before is the
/// one it still serves, with the id and the catalog of that generation.
#[tokio::test]
async fn a_registry_only_publication_advances_the_generation_and_a_refused_one_keeps_it() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog
        .listings(&spot_route())
        .expect("the fixture route publishes its listing");
    let btc_token = token("binance", "BTC");
    let before = catalog.generation_id();
    assert_eq!(
        published_code(catalog, &btc_token).as_str(),
        "BTC",
        "the generation before the install names the asset the fixture listing is based on"
    );

    let published = catalog
        .install_registries(registry_of(registered_terms(catalog)))
        .expect("a registry-only publication builds the display catalog of its registries");

    assert!(
        published > before,
        "a registry-only publication hands out a new generation id"
    );
    assert_eq!(
        catalog.generation_id(),
        published,
        "the id the install answers with is the generation it published"
    );

    let without_the_base: Vec<AssetTerms> = registered_terms(catalog)
        .into_iter()
        .filter(|terms| terms.asset_id() != btc_token)
        .collect();
    let error = catalog
        .install_registries(registry_of(without_the_base))
        .expect_err(
            "a listing of a surviving snapshot has no terms in the registry being installed",
        );

    assert!(
        matches!(
            refused_display(&error),
            DisplayError::MissingAssetTerms { .. }
        ),
        "the refusal is the display gate's own error, got {error:?}"
    );
    assert_eq!(
        catalog.generation_id(),
        published,
        "a refused registry-only publication keeps the generation it had"
    );
    assert_eq!(
        published_code(catalog, &btc_token).as_str(),
        "BTC",
        "and the display catalog of that generation is still the published one"
    );
}

// ---- outcome reachability, revocation and route isolation ---------------

fn outcome_expiry() -> Expiry {
    Expiry::new(2_000, Some(1_800), Some(2_100)).expect("the expiry is valid")
}

fn outcome_settlement_asset() -> AssetId {
    token("binance", "USDC")
}

/// One binary outcome group of the fixture venue.
fn outcome_group(question: &str) -> OutcomeGroup {
    let seed = OutcomeGroupSeed {
        question: SmolStr::new(question),
        expiry: outcome_expiry(),
        topology: OutcomeTopology::Binary,
        settlement_asset: outcome_settlement_asset(),
        resolution: ResolutionRule::FixedAt { at: 1_900 },
        fraction_rule: SettlementFractionRule::Binary,
    };
    let seed_id = seed.seed_id();
    let yes = OutcomeSide::new(&seed_id, SmolStr::new_static("yes")).expect("the side is valid");
    let no = OutcomeSide::new(&seed_id, SmolStr::new_static("no")).expect("the side is valid");
    let member = OutcomeMember::new(
        &seed_id,
        SmolStr::new_static("winner"),
        vec![yes.id.clone(), no.id.clone()],
    )
    .expect("the member is valid");
    OutcomeGroup::new(seed, vec![yes, no], vec![member]).expect("the group is valid")
}

/// A member of the group's own seed that the group does *not* carry.
/// One outcome listing that trades `member`/`side` of `group` under `symbol`.
fn outcome_entry_of(
    route: &VenueRoute,
    group: &OutcomeGroup,
    member: &OutcomeMember,
    side: &OutcomeSide,
    symbol: &str,
) -> CatalogEntry {
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id.clone(),
        member.id.clone(),
        side.id.clone(),
    )))
    .expect("the outcome instrument is valid");
    let key = listing_key(route, symbol);
    let listing = VenueListing::new(
        key.clone(),
        instrument.id().clone(),
        Some(NativeSymbol::new(symbol).expect("the symbol is valid")),
        Vec::new(),
        VenueListingIdentifiers::Polymarket(PolymarketListingIdentifiers {
            event_id: None,
            market_id: PolymarketMarketId::new("market-1").expect("the market id is valid"),
            condition_id: CtfConditionId::new("0x".to_owned() + &"1".repeat(64))
                .expect("the condition id is valid"),
            clob_token_id: CtfTokenId::new("12345").expect("the token id is valid"),
            // the protocol label of a fixture echoes the registry label of
            // the leg it trades, so a test that changes the leg keeps the echo.
            outcome_label: side.label.clone(),
        }),
    )
    .expect("the outcome listing is valid");
    let snapshot = ListingSnapshot::new(
        key,
        TradingRules {
            common: CommonTradingRules {
                price: PriceRule {
                    quote_asset: outcome_settlement_asset(),
                    basis: PriceBasis::FractionOfCollateral,
                    tick: Tick::try_from(Quantity::from(1_u8)).expect("the tick is valid"),
                    minimum: None,
                    maximum: None,
                },
                quantity: QuantityRule {
                    unit: QuantityUnit::OutcomeTokens {
                        group: group.id.clone(),
                        member: member.id.clone(),
                        side: side.id.clone(),
                    },
                    step: Quantity::from(1_u8),
                    minimum: None,
                    maximum: None,
                },
                notional: None,
                sessions: TradingCalendarId::continuous_24x7(),
                settlement_cycle: SettlementCycle::Immediate,
                price_limits: PriceLimitRule::None,
            },
            product: ProductTradingRules::Outcome(OutcomeTradingRules {
                order_book: true,
                negative_risk_conversion: true,
            }),
        },
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the outcome snapshot is valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

fn outcome_entry(route: &VenueRoute, group: &OutcomeGroup, symbol: &str) -> CatalogEntry {
    outcome_entry_of(route, group, &group.members[0], &group.sides[0], symbol)
}

/// Every route of a fixture with one registered source and one outcome batch.
fn outcome_fixture(
    groups: Vec<OutcomeGroup>,
    pools: Vec<OutcomeRiskPool>,
    events: Vec<OutcomeEvent>,
    listings: Vec<CatalogEntry>,
) -> Fixture {
    let source = Arc::new(ScriptedOutcome {
        groups,
        pools,
        events,
        listings,
    });
    Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        Arc::clone(&source) as Arc<dyn CatalogSource>,
        &[],
        FIXTURE_CODES,
    )
}

/// A source that answers every fetch with the same outcome batch.
struct ScriptedOutcome {
    groups: Vec<OutcomeGroup>,
    pools: Vec<OutcomeRiskPool>,
    events: Vec<OutcomeEvent>,
    listings: Vec<CatalogEntry>,
}

impl CatalogSource for ScriptedOutcome {
    fn fetch_catalog(
        &self,
        context: crate::catalog::source::CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, crate::catalog::error::SourceError>> {
        let listings = self
            .listings
            .iter()
            .filter(|entry| &entry.listing.key.route == context.venue_route())
            .cloned()
            .collect();
        let batch = crate::catalog::test_support::fixture_batch(
            CatalogBatch {
                listings,
                outcome_groups: self.groups.clone(),
                outcome_risk_pools: self.pools.clone(),
                outcome_events: self.events.clone(),
                ..CatalogBatch::default()
            },
            &context,
        );
        Box::pin(async move { Ok(batch) })
    }
}

/// Probe 11: revoking the only route that references an outcome group removes it.
#[tokio::test]
async fn revoking_the_only_route_removes_its_unreachable_outcome_objects() {
    let group = outcome_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    let instrument = listing.instrument.id().clone();
    let fixture = outcome_fixture(vec![group.clone()], Vec::new(), Vec::new(), vec![listing]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    assert!(fixture.catalog().outcome_group(&group.id).is_some());

    fixture.revoke_all();

    assert!(
        fixture.catalog().listings_for(&instrument).is_empty(),
        "the revoked route's listings leave the global index"
    );
    assert!(
        fixture.catalog().outcome_group(&group.id).is_none(),
        "no surviving listing references the group, so it leaves the current generation"
    );
}

/// Probe 5: a group two routes share survives the revocation of one of them.
#[tokio::test]
async fn a_group_shared_by_two_routes_survives_one_revocation() {
    let group = outcome_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    let fixture = outcome_fixture(
        vec![group.clone()],
        Vec::new(),
        Vec::new(),
        vec![listing.clone()],
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(
        &perp_route(),
        "binance-perp",
        std::slice::from_ref(&listing),
    );
    fixture.load_route(&perp_route()).await;

    fixture.revoke_all();

    assert!(
        fixture.catalog().outcome_group(&group.id).is_none(),
        "no route references the group any more"
    );

    // The same group referenced by two routes: revoking one keeps it.
    let shared = outcome_group("Will it rain twice?");
    let shared_spot = outcome_entry(&spot_route(), &shared, "TWICE-A");
    let perp_listing = outcome_entry(&perp_route(), &shared, "TWICE-B");
    let fixture = outcome_fixture(
        vec![shared.clone()],
        Vec::new(),
        Vec::new(),
        vec![shared_spot.clone()],
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(
        &perp_route(),
        "binance-perp",
        std::slice::from_ref(&perp_listing),
    );
    fixture.load_route(&perp_route()).await;

    fixture
        .catalog()
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [test_support::binding(
                    "binance-perp",
                    test_support::listing(vec![perp_listing.clone()]),
                )],
                [test_support::registration(&perp_route(), "binance-perp")],
            ),
            &requirements(std::slice::from_ref(&perp_route())),
        )
        .expect("revoking the spot route is accepted");

    assert!(
        fixture.catalog().outcome_group(&shared.id).is_some(),
        "the surviving route still references the shared group"
    );
    assert!(
        fixture
            .catalog()
            .generation()
            .route_snapshot(&perp_route())
            .expect("the surviving route keeps its snapshot")
            .listing_by_outcome_group(&shared.id)
            .is_some(),
        "the surviving route keeps its reverse index into the shared group"
    );
}

/// A revoked route with a surviving snapshot is never served from the cache.
#[tokio::test]
async fn a_revoked_route_is_absent_from_the_cache_even_with_a_snapshot() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let catalog = fixture.catalog();
    catalog.listings(&spot_route()).expect("the route loads");
    assert!(
        catalog.generation().route_snapshot(&spot_route()).is_some(),
        "the snapshot is published before the revocation"
    );

    fixture.revoke_all();

    assert!(
        catalog.cached_listings(&spot_route()).is_none(),
        "a route that is no longer registered is absent, whatever the snapshot says"
    );
    assert!(
        catalog.cached_listing(&btc_key()).is_none(),
        "the revoked listing is not offered any more"
    );
    assert!(matches!(
        catalog.contains(&btc_key()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

/// Probe 14: a synchronous lookup is answered from its own route's snapshot.
#[tokio::test]
async fn an_ensured_route_resolves_only_its_own_native_lookups() {
    let spot = outcome_group("Probe 14 spot");
    let perp = outcome_group("Probe 14 perp");
    let spot_lookup = OutcomeGroupIdentifier::PolymarketMarket(
        PolymarketMarketId::new("spot-market-14").expect("the market id is valid"),
    );
    let perp_lookup = OutcomeGroupIdentifier::PolymarketMarket(
        PolymarketMarketId::new("perp-market-14").expect("the market id is valid"),
    );
    let catalog = Arc::new(Catalog::new());
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [
                    test_support::binding(
                        "source-a",
                        Arc::new(GroupLookupSource {
                            listing: outcome_entry(&spot_route(), &spot, "P14-SPOT"),
                            group: spot.clone(),
                            lookups: vec![spot_lookup.clone()],
                        }),
                    ),
                    test_support::binding(
                        "source-b",
                        Arc::new(GroupLookupSource {
                            listing: outcome_entry(&perp_route(), &perp, "P14-PERP"),
                            group: perp.clone(),
                            lookups: vec![perp_lookup.clone()],
                        }),
                    ),
                ],
                [
                    test_support::registration(&spot_route(), "source-a"),
                    test_support::registration(&perp_route(), "source-b"),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("the two-route candidate is accepted");

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("spot loads");
    catalog
        .refresh_for_test(&perp_route())
        .await
        .expect("perp loads");
    let ensured_spot = test_support::ensured(&catalog, &spot_route()).expect("spot loads");
    let ensured_perp = test_support::ensured(&catalog, &perp_route()).expect("perp loads");

    assert_eq!(
        ensured_spot
            .resolve_group_identifier(&spot_lookup)
            .expect("a route resolves its own lookup"),
        spot.id
    );
    assert_eq!(
        ensured_perp
            .resolve_group_identifier(&perp_lookup)
            .expect("a route resolves its own lookup"),
        perp.id
    );
    assert!(
        matches!(
            ensured_spot.resolve_group_identifier(&perp_lookup),
            Err(CatalogError::NativeGroupIdentifierNotFound { .. })
        ),
        "the synchronous resolver of one route must not answer another route's lookup"
    );
    assert!(
        catalog
            .resolve_group_identifier(&spot_route(), &perp_lookup)
            .is_err(),
        "the asynchronous resolver agrees"
    );
}

/// Publishes one shared group on two routes: the spot route trades the group
/// without a native lookup of its own, and the perp route trades it too and stages
/// the given native group lookups for it. Both routes are loaded, so the lookups
/// of the perp route are published before the fixture is returned.
async fn two_route_group_lookup_fixture(
    group: &OutcomeGroup,
    lookups: Vec<OutcomeGroupIdentifier>,
) -> (
    Arc<Catalog>,
    crate::catalog::resolved::EnsuredRoute,
    crate::catalog::resolved::EnsuredRoute,
) {
    let catalog = Arc::new(Catalog::new());
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [
                    test_support::binding(
                        "source-spot",
                        Arc::new(GroupLookupSource {
                            listing: outcome_entry(&spot_route(), group, "N3-SPOT"),
                            group: group.clone(),
                            lookups: Vec::new(),
                        }),
                    ),
                    test_support::binding(
                        "source-perp",
                        Arc::new(GroupLookupSource {
                            listing: outcome_entry(&perp_route(), group, "N3-PERP"),
                            group: group.clone(),
                            lookups,
                        }),
                    ),
                ],
                [
                    test_support::registration(&spot_route(), "source-spot"),
                    test_support::registration(&perp_route(), "source-perp"),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("the two-route candidate is accepted");

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("spot loads");
    catalog
        .refresh_for_test(&perp_route())
        .await
        .expect("perp loads");
    let ensured_spot = test_support::ensured(&catalog, &spot_route()).expect("spot loads");
    let ensured_perp = test_support::ensured(&catalog, &perp_route()).expect("perp loads");
    (catalog, ensured_spot, ensured_perp)
}

/// A native condition another route registered names the shared group on both
/// resolvers, and a condition nobody registered is refused by both with the same
/// value.
#[tokio::test]
async fn the_group_resolvers_agree_about_a_condition_registered_by_another_route() {
    let shared = outcome_group("N3 shared condition group");
    let registered = OutcomeGroupIdentifier::PolymarketCondition(
        CtfConditionId::new("0x".to_owned() + &"2".repeat(64)).expect("the condition id is valid"),
    );
    let (catalog, ensured_spot, _) =
        two_route_group_lookup_fixture(&shared, vec![registered.clone()]).await;

    // The spot route stages no lookup of its own: the condition was registered by
    // the perp route's source, and both resolvers answer the shared group.
    let synchronous = ensured_spot.resolve_group_identifier(&registered);
    let asynchronous = catalog.resolve_group_identifier(&spot_route(), &registered);
    assert_eq!(
        synchronous.as_ref().ok(),
        Some(&shared.id),
        "the synchronous resolver answers a condition another route registered: {synchronous:?}"
    );
    assert_eq!(
        asynchronous.as_ref().ok(),
        Some(&shared.id),
        "the asynchronous resolver answers the same: {asynchronous:?}"
    );

    // A condition nobody registered is refused by both, field by field.
    let unknown = OutcomeGroupIdentifier::PolymarketCondition(
        CtfConditionId::new("0x".to_owned() + &"3".repeat(64)).expect("the condition id is valid"),
    );
    let synchronous = ensured_spot
        .resolve_group_identifier(&unknown)
        .expect_err("an unknown condition is refused");
    let asynchronous = catalog
        .resolve_group_identifier(&spot_route(), &unknown)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeGroupIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(unknown),
    };
    assert_eq!(
        synchronous, expected,
        "the synchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        asynchronous, expected,
        "the asynchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// A native market another route registered names the shared group on both
/// resolvers, and a market nobody registered is refused by both with the same
/// value.
#[tokio::test]
async fn the_group_resolvers_agree_about_a_market_registered_by_another_route() {
    let shared = outcome_group("N3 shared market group");
    let market = PolymarketMarketId::new("n3-market").expect("the market id is valid");
    let registered = OutcomeGroupIdentifier::PolymarketMarket(market);
    let (catalog, ensured_spot, _) =
        two_route_group_lookup_fixture(&shared, vec![registered.clone()]).await;

    let synchronous = ensured_spot.resolve_group_identifier(&registered);
    let asynchronous = catalog.resolve_group_identifier(&spot_route(), &registered);
    assert_eq!(
        synchronous.as_ref().ok(),
        Some(&shared.id),
        "the synchronous resolver answers a market another route registered: {synchronous:?}"
    );
    assert_eq!(
        asynchronous.as_ref().ok(),
        Some(&shared.id),
        "the asynchronous resolver answers the same: {asynchronous:?}"
    );

    let unknown = OutcomeGroupIdentifier::PolymarketMarket(
        PolymarketMarketId::new("n3-unknown-market").expect("the market id is valid"),
    );
    let synchronous = ensured_spot
        .resolve_group_identifier(&unknown)
        .expect_err("an unknown market is refused");
    let asynchronous = catalog
        .resolve_group_identifier(&spot_route(), &unknown)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeGroupIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(unknown),
    };
    assert_eq!(
        synchronous, expected,
        "the synchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        asynchronous, expected,
        "the asynchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// A Hyperliquid question carries its scope, so a question of another scope
/// belongs to another route: both resolvers refuse it with one and the same value,
/// on the route that carries the lookup and on the route that does not. A question
/// of the route's own scope is answered by both, whoever registered it, and one
/// nobody registered stays a not-found on both sides.
#[tokio::test]
async fn the_group_resolvers_agree_about_a_question_of_another_scope() {
    let shared = outcome_group("N3 shared question group");
    let elsewhere = || VenueScopeId::new("scope-elsewhere").expect("the scope is valid");
    let own_scope = OutcomeGroupIdentifier::HyperliquidQuestion {
        scope: spot_route().scope,
        question: HyperliquidQuestionId::new("n3-own-question").expect("the question is valid"),
    };
    let foreign_scope = OutcomeGroupIdentifier::HyperliquidQuestion {
        scope: elsewhere(),
        question: HyperliquidQuestionId::new("n3-foreign-question").expect("the question is valid"),
    };
    let (catalog, ensured_spot, ensured_perp) =
        two_route_group_lookup_fixture(&shared, vec![own_scope.clone(), foreign_scope.clone()])
            .await;

    // Control: a question of the spot route's own scope, registered by the perp
    // route's source, names the shared group on both resolvers.
    let synchronous = ensured_spot.resolve_group_identifier(&own_scope);
    let asynchronous = catalog.resolve_group_identifier(&spot_route(), &own_scope);
    assert_eq!(
        synchronous.as_ref().ok(),
        Some(&shared.id),
        "the synchronous resolver answers a question of its own scope: {synchronous:?}"
    );
    assert_eq!(
        asynchronous.as_ref().ok(),
        Some(&shared.id),
        "the asynchronous resolver answers the same: {asynchronous:?}"
    );

    // A question of another scope is a route mismatch on both sides, whether or
    // not this route carries the lookup.
    let scope_refusal = |route: VenueRoute| CatalogError::NativeIdentifierRouteMismatch {
        route,
        identifier: Box::new(ListingIdentifier::HyperliquidOutcome {
            scope: elsewhere(),
            outcome_id: market_participants_model::listing::venue::HyperliquidOutcomeId(0),
            asset_id: market_participants_model::listing::venue::HyperliquidAssetId(0),
        }),
    };
    let synchronous = ensured_spot
        .resolve_group_identifier(&foreign_scope)
        .expect_err("a question of another scope is not this route's to answer");
    let asynchronous = catalog
        .resolve_group_identifier(&spot_route(), &foreign_scope)
        .expect_err("the asynchronous resolver refuses it too");
    assert_eq!(
        synchronous,
        scope_refusal(spot_route()),
        "the synchronous refusal must name this route"
    );
    assert_eq!(
        asynchronous,
        scope_refusal(spot_route()),
        "the asynchronous refusal must name this route"
    );
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );

    let synchronous = ensured_perp
        .resolve_group_identifier(&foreign_scope)
        .expect_err("a question of another scope is refused on the route that registered it too");
    let asynchronous = catalog
        .resolve_group_identifier(&perp_route(), &foreign_scope)
        .expect_err("the asynchronous resolver refuses it there too");
    assert_eq!(
        synchronous,
        scope_refusal(perp_route()),
        "the synchronous refusal must name the perp route"
    );
    assert_eq!(
        asynchronous,
        scope_refusal(perp_route()),
        "the asynchronous refusal must name the perp route"
    );
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );

    // A question of this route's own scope that nobody registered is a not-found
    // on both sides, never a scope refusal.
    let unregistered = OutcomeGroupIdentifier::HyperliquidQuestion {
        scope: spot_route().scope,
        question: HyperliquidQuestionId::new("n3-unregistered-question")
            .expect("the question is valid"),
    };
    let synchronous = ensured_spot
        .resolve_group_identifier(&unregistered)
        .expect_err("a question nobody registered is not found");
    let asynchronous = catalog
        .resolve_group_identifier(&spot_route(), &unregistered)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeGroupIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(unregistered),
    };
    assert_eq!(
        synchronous, expected,
        "the synchronous refusal must be the unregistered identifier of this route"
    );
    assert_eq!(
        asynchronous, expected,
        "the asynchronous refusal must be the unregistered identifier of this route"
    );
}

/// A source that stages a native group lookup next to the batch it publishes.
struct GroupLookupSource {
    listing: CatalogEntry,
    group: OutcomeGroup,
    lookups: Vec<OutcomeGroupIdentifier>,
}

impl CatalogSource for GroupLookupSource {
    fn fetch_catalog(
        &self,
        context: crate::catalog::source::CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, crate::catalog::error::SourceError>> {
        let _ = context.staging().with(|staging| {
            staging
                .pending_outcomes_mut()
                .register_group(self.group.clone(), &self.lookups)
        });
        let batch = crate::catalog::test_support::fixture_batch(
            CatalogBatch {
                listings: vec![self.listing.clone()],
                outcome_groups: vec![self.group.clone()],
                ..CatalogBatch::default()
            },
            &context,
        );
        Box::pin(async move { Ok(batch) })
    }
}

/// The event path of probe 14: the route proof's resolver and the catalog's
/// answer the same thing for the same `(route, native event)`, in both
/// directions.
#[tokio::test]
async fn an_ensured_route_resolves_only_its_own_native_events() {
    let spot = outcome_group("Fix 1 spot");
    let perp = outcome_group("Fix 1 perp");
    let (spot_event, spot_native) = native_event_of(&spot, "fix1-spot-event");
    let (perp_event, perp_native) = native_event_of(&perp, "fix1-perp-event");
    let catalog = Arc::new(Catalog::new());
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [
                    test_support::binding(
                        "source-a",
                        Arc::new(EventLookupSource {
                            listing: outcome_entry_with_event(
                                &spot_route(),
                                &spot,
                                "F1-SPOT",
                                &spot_native,
                            ),
                            group: spot.clone(),
                            event: spot_event.clone(),
                            native: spot_native.clone(),
                        }),
                    ),
                    test_support::binding(
                        "source-b",
                        Arc::new(EventLookupSource {
                            listing: outcome_entry_with_event(
                                &perp_route(),
                                &perp,
                                "F1-PERP",
                                &perp_native,
                            ),
                            group: perp.clone(),
                            event: perp_event.clone(),
                            native: perp_native.clone(),
                        }),
                    ),
                ],
                [
                    test_support::registration(&spot_route(), "source-a"),
                    test_support::registration(&perp_route(), "source-b"),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("the two-route candidate is accepted");

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("spot loads");
    catalog
        .refresh_for_test(&perp_route())
        .await
        .expect("perp loads");
    let ensured_spot = test_support::ensured(&catalog, &spot_route()).expect("spot loads");
    let ensured_perp = test_support::ensured(&catalog, &perp_route()).expect("perp loads");

    // The positive direction: each route resolves the event its own listing
    // declares, and both resolvers answer with the same canonical event.
    let spot_identifier = OutcomeEventIdentifier::PolymarketEvent(spot_native.clone());
    let perp_identifier = OutcomeEventIdentifier::PolymarketEvent(perp_native.clone());
    let sync_spot = ensured_spot
        .resolve_event_identifier(&spot_identifier)
        .expect("a route resolves the event its own listing declares");
    let async_spot = catalog
        .resolve_event_identifier(&spot_route(), &spot_identifier)
        .expect("the asynchronous resolver answers the same");
    assert_eq!(sync_spot, spot_event.id);
    assert_eq!(
        sync_spot, async_spot,
        "both resolvers answer with the same canonical event"
    );
    assert_eq!(
        ensured_perp
            .resolve_event_identifier(&perp_identifier)
            .expect("the other route resolves its own event too"),
        perp_event.id
    );

    // The negative direction: the event of the other route is refused by both,
    // with the same strongly typed error.
    let sync_error = ensured_spot
        .resolve_event_identifier(&perp_identifier)
        .expect_err("the synchronous resolver of one route must not answer another route's event");
    let async_error = catalog
        .resolve_event_identifier(&spot_route(), &perp_identifier)
        .expect_err("the asynchronous resolver agrees");
    assert_eq!(
        sync_error,
        CatalogError::NativeEventIdentifierNotFound {
            route: spot_route(),
            event: perp_native.clone(),
        },
        "the synchronous resolver reports the foreign event as unknown on its route"
    );
    assert_eq!(
        sync_error, async_error,
        "the two resolvers agree about a foreign route's event"
    );
}

/// One event of the fixture venue, named by one native event identifier.
fn native_event_of(group: &OutcomeGroup, native: &str) -> (OutcomeEvent, PolymarketEventId) {
    let native = PolymarketEventId::new(native).expect("the event id is valid");
    let event = OutcomeEvent::new(
        ResolutionSourceId::new("polymarket").expect("the resolution source is valid"),
        IdentifierValue::new(native.as_str()).expect("the event value is valid"),
        vec![group.id.clone()],
        Vec::new(),
    )
    .expect("the event is valid");
    (event, native)
}

/// The fixture outcome listing of `group`, declaring a native event.
fn outcome_entry_with_event(
    route: &VenueRoute,
    group: &OutcomeGroup,
    symbol: &str,
    event: &PolymarketEventId,
) -> CatalogEntry {
    let mut entry = outcome_entry(route, group, symbol);
    let VenueListingIdentifiers::Polymarket(identifiers) = &mut entry.listing.identifiers else {
        panic!("the fixture outcome listing is a Polymarket listing");
    };
    identifiers.event_id = Some(event.clone());
    entry
}

/// A source that stages one event with its native lookup next to the batch whose
/// listing declares it.
struct EventLookupSource {
    listing: CatalogEntry,
    group: OutcomeGroup,
    event: OutcomeEvent,
    native: PolymarketEventId,
}

impl CatalogSource for EventLookupSource {
    fn fetch_catalog(
        &self,
        context: crate::catalog::source::CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, crate::catalog::error::SourceError>> {
        context.staging().with(|staging| {
            let pending = staging.pending_outcomes_mut();
            pending
                .register_group(self.group.clone(), &[])
                .expect("the fixture group is valid");
            pending.register_event(self.event.clone(), Some(&self.native));
        });
        let batch = crate::catalog::test_support::fixture_batch(
            CatalogBatch {
                listings: vec![self.listing.clone()],
                outcome_groups: vec![self.group.clone()],
                outcome_events: vec![self.event.clone()],
                ..CatalogBatch::default()
            },
            &context,
        );
        Box::pin(async move { Ok(batch) })
    }
}

/// A source of a route that publishes its own event and additionally registers a
/// second native identifier for the event of another route, as an adapter
/// tracking a market of a shared event does.
struct ForeignAliasSource {
    listing: CatalogEntry,
    group: OutcomeGroup,
    event: OutcomeEvent,
    native: PolymarketEventId,
    alias: PolymarketEventId,
    foreign: OutcomeEventId,
}

impl CatalogSource for ForeignAliasSource {
    fn fetch_catalog(
        &self,
        context: crate::catalog::source::CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, crate::catalog::error::SourceError>> {
        assert_eq!(context.venue_route(), &self.listing.listing.key.route);
        let foreign = self.foreign.clone();
        let alias = self.alias.clone();
        context.staging().with(|staging| {
            let pending = staging.pending_outcomes_mut();
            pending
                .register_group(self.group.clone(), &[])
                .expect("the fixture group is valid");
            pending.register_event(self.event.clone(), Some(&self.native));
            pending.event_lookups.push((alias, foreign));
        });
        let batch = crate::catalog::test_support::fixture_batch(
            CatalogBatch {
                listings: vec![self.listing.clone()],
                outcome_groups: vec![self.group.clone()],
                outcome_events: vec![self.event.clone()],
                ..CatalogBatch::default()
            },
            &context,
        );
        Box::pin(async move { Ok(batch) })
    }
}

/// The event lifetime is decided by every *surviving* route, never by the batch
/// that happens to be publishing.
///
/// Route A declares event E, route B publishes without declaring it, and B is
/// then revoked: A's event stays in the generation and all three entries — the
/// published registry, the handle's native lookup and the synchronous proof —
/// still answer with it. Only when A itself goes does E leave, which is the
/// monotonic decrease the event lifetime contract asks for.
#[tokio::test]
async fn a_route_keeps_its_event_while_another_route_publishes_and_is_revoked() {
    let spot = outcome_group("Fix 2 spot");
    let perp = outcome_group("Fix 2 perp");
    let (spot_event, spot_native) = native_event_of(&spot, "fix2-spot-event");
    let (perp_event, perp_native) = native_event_of(&perp, "fix2-perp-event");
    let spot_source = Arc::new(EventLookupSource {
        listing: outcome_entry_with_event(&spot_route(), &spot, "F2-SPOT", &spot_native),
        group: spot.clone(),
        event: spot_event.clone(),
        native: spot_native.clone(),
    });
    let perp_source = Arc::new(EventLookupSource {
        listing: outcome_entry_with_event(&perp_route(), &perp, "F2-PERP", &perp_native),
        group: perp.clone(),
        event: perp_event.clone(),
        native: perp_native.clone(),
    });
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        Arc::clone(&spot_source) as Arc<dyn CatalogSource>,
        &[],
        FIXTURE_CODES,
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture
        .catalog()
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [
                    test_support::binding(
                        SPOT_SOURCE,
                        Arc::clone(&spot_source) as Arc<dyn CatalogSource>,
                    ),
                    test_support::binding(
                        "binance-perp",
                        Arc::clone(&perp_source) as Arc<dyn CatalogSource>,
                    ),
                ],
                [
                    test_support::registration(&spot_route(), SPOT_SOURCE),
                    test_support::registration(&perp_route(), "binance-perp"),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("the perp route is registered next to the spot route");
    let catalog = fixture.catalog();
    let ensured_spot = fixture.ensure().await.expect("the spot route loads");
    assert!(
        catalog.outcome_event(&spot_event.id).is_some(),
        "the spot event is published while only the spot route exists"
    );

    catalog
        .refresh_for_test(&perp_route())
        .await
        .expect("the perp route publishes its own event");
    assert!(
        catalog.outcome_event(&perp_event.id).is_some(),
        "the perp route's own event is published"
    );
    assert!(
        catalog.outcome_event(&spot_event.id).is_some(),
        "publishing another route must not unpublish the spot event"
    );
    assert_eq!(
        fixture.catalog().outcome_event_for(&spot_native),
        Some(spot_event.id.clone()),
        "the spot native lookup survives another route's publication"
    );

    // Revoking the perp route leaves the spot route as the only declarer.
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [test_support::binding(
                    SPOT_SOURCE,
                    Arc::clone(&spot_source) as Arc<dyn CatalogSource>,
                )],
                [test_support::registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("revoking the perp route is accepted");
    assert!(
        catalog.outcome_event(&perp_event.id).is_none(),
        "the revoked route's event leaves the generation"
    );
    assert!(
        catalog.outcome_event(&spot_event.id).is_some(),
        "revoking another route must not unpublish the surviving route's event"
    );
    assert_eq!(
        fixture.catalog().outcome_event_for(&spot_native),
        Some(spot_event.id.clone()),
        "the surviving route keeps its native event lookup"
    );
    assert_eq!(
        ensured_spot
            .resolve_event_identifier(&OutcomeEventIdentifier::PolymarketEvent(
                spot_native.clone()
            ))
            .expect("the surviving route resolves its own event synchronously"),
        spot_event.id,
        "the synchronous proof of the surviving route still answers its own event"
    );
    assert_eq!(
        catalog
            .resolve_event_identifier(
                &spot_route(),
                &OutcomeEventIdentifier::PolymarketEvent(spot_native.clone()),
            )
            .expect("the surviving route resolves its own event asynchronously"),
        spot_event.id,
        "the asynchronous resolver agrees with the synchronous one"
    );
    assert_eq!(
        catalog
            .generation()
            .route_snapshot(&spot_route())
            .expect("the surviving route keeps its snapshot")
            .published_events()
            .len(),
        1,
        "the surviving route still publishes exactly its own event"
    );

    // The last declarer going is what takes the event away.
    fixture.revoke_all();
    assert!(
        catalog.outcome_event(&spot_event.id).is_none(),
        "the event leaves with the last route that declares it"
    );
    assert_eq!(
        fixture.catalog().outcome_event_for(&spot_native),
        None,
        "the native event lookup leaves with the event"
    );
}

/// An event this route publishes, reached through a native lookup another route
/// registered, is answered by both resolvers with the same canonical event.
#[tokio::test]
async fn a_native_lookup_registered_by_another_route_finds_this_routes_event() {
    let spot = outcome_group("Fix 2 alias spot");
    let perp = outcome_group("Fix 2 alias perp");
    let (spot_event, spot_native) = native_event_of(&spot, "fix2-alias-spot-event");
    let (perp_event, perp_native) = native_event_of(&perp, "fix2-alias-perp-event");
    let alias = PolymarketEventId::new("fix2-alias-of-the-spot-event").expect("the alias is valid");
    let spot_source = Arc::new(EventLookupSource {
        listing: outcome_entry_with_event(&spot_route(), &spot, "F2-ALIAS-SPOT", &spot_native),
        group: spot.clone(),
        event: spot_event.clone(),
        native: spot_native.clone(),
    });
    let perp_source = Arc::new(ForeignAliasSource {
        listing: outcome_entry_with_event(&perp_route(), &perp, "F2-ALIAS-PERP", &perp_native),
        group: perp.clone(),
        event: perp_event.clone(),
        native: perp_native.clone(),
        alias: alias.clone(),
        foreign: spot_event.id.clone(),
    });
    let catalog = Arc::new(Catalog::new());
    catalog
        .commit_fixture_routes(
            CatalogRouteUpdate::new(
                [
                    test_support::binding(
                        "source-a",
                        Arc::clone(&spot_source) as Arc<dyn CatalogSource>,
                    ),
                    test_support::binding(
                        "source-b",
                        Arc::clone(&perp_source) as Arc<dyn CatalogSource>,
                    ),
                ],
                [
                    test_support::registration(&spot_route(), "source-a"),
                    test_support::registration(&perp_route(), "source-b"),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("the two-route candidate is accepted");

    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("spot loads");
    catalog
        .refresh_for_test(&perp_route())
        .await
        .expect("perp loads");
    let ensured_spot = test_support::ensured(&catalog, &spot_route()).expect("spot loads");
    test_support::ensured(&catalog, &perp_route()).expect("perp loads");

    assert_eq!(
        published_event_lookups(&catalog, &spot_route()),
        vec![spot_native.clone()],
        "the spot route publishes its own native event lookup"
    );
    assert_eq!(
        catalog.outcome_event_for(&alias),
        Some(spot_event.id.clone()),
        "the alias the perp route registered names the spot event in the registry"
    );

    // The positive direction: the spot route publishes the event the alias names,
    // even though the lookup that names it was registered by the perp route.
    let identifier = OutcomeEventIdentifier::PolymarketEvent(alias.clone());
    let synchronous = ensured_spot
        .resolve_event_identifier(&identifier)
        .expect("the spot route publishes the event the alias names");
    let asynchronous = catalog
        .resolve_event_identifier(&spot_route(), &identifier)
        .expect("the asynchronous resolver answers the same");
    assert_eq!(synchronous, spot_event.id);
    assert_eq!(
        synchronous, asynchronous,
        "both resolvers answer with the same canonical event"
    );

    // The negative direction: a foreign route's own event stays refused, with the
    // same strongly typed error on both sides.
    let foreign = OutcomeEventIdentifier::PolymarketEvent(perp_native.clone());
    let sync_error = ensured_spot
        .resolve_event_identifier(&foreign)
        .expect_err("one route never answers another route's own event");
    let async_error = catalog
        .resolve_event_identifier(&spot_route(), &foreign)
        .expect_err("the asynchronous resolver agrees");
    assert_eq!(
        sync_error,
        CatalogError::NativeEventIdentifierNotFound {
            route: spot_route(),
            event: perp_native.clone(),
        },
        "the synchronous resolver reports the foreign event as unknown on its route"
    );
    assert_eq!(
        sync_error, async_error,
        "the two resolvers agree about a foreign route's event"
    );
}

/// One Binance spot listing of `route` whose typed identity is a Hyperliquid
/// spot asset of the same scope, so a scoped listing identifier names it.
fn scoped_listing(
    route: &VenueRoute,
    instrument: Instrument,
    symbol: &str,
    asset_id: u32,
) -> CatalogEntry {
    let mut entry = entry(route, instrument, symbol);
    entry.listing.identifiers =
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Spot {
            asset_id: HyperliquidAssetId(asset_id),
            base_token_id: HyperliquidTokenId(0),
            quote_token_id: HyperliquidTokenId(1),
        });
    entry
}

/// Publishes one listing on the spot route and one on the perp route, both
/// loaded, so every resolver can be asked about this route's own listing, about
/// a scoped identifier of another scope and about the other route's listing.
async fn two_route_listing_fixture() -> (
    Arc<Catalog>,
    crate::catalog::resolved::EnsuredRoute,
    crate::catalog::resolved::EnsuredRoute,
) {
    let spot = scoped_listing(&spot_route(), btc(), "BTCUSDT", 3);
    let perp = entry(&perp_route(), eth(), "ETH-USDT");
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![spot.clone(), perp.clone()]),
        &[spot.clone(), perp.clone()],
        FIXTURE_CODES,
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    fixture.register_route(&perp_route(), "binance-perp", &[spot, perp]);
    fixture.load_route(&perp_route()).await;
    let catalog = Arc::clone(fixture.catalog_arc());
    let ensured_spot = fixture
        .ensured(&spot_route())
        .expect("the spot route loads");
    let ensured_perp = fixture
        .ensured(&perp_route())
        .expect("the perp route loads");
    (catalog, ensured_spot, ensured_perp)
}

/// The typed listing identifier of the route's own listing is answered by both
/// resolvers with one and the same key, and an identifier nobody registered is
/// refused by both with one and the same value.
#[tokio::test]
async fn the_listing_identifier_resolvers_agree() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    let identifier = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("BTCUSDT").expect("the symbol is valid"),
        product: BinanceProduct::Spot,
    };
    let synchronous = ensured
        .resolve_listing_identifier(&identifier)
        .expect("the synchronous resolver answers the typed identifier");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &identifier)
        .expect("the asynchronous resolver answers it too");
    assert_eq!(synchronous, btc_key());
    assert_eq!(
        synchronous, asynchronous,
        "both resolvers answer with the same listing key"
    );

    let unknown = ListingIdentifier::PolymarketToken(
        CtfTokenId::new("900000001").expect("the token id is valid"),
    );
    let synchronous = ensured
        .resolve_listing_identifier(&unknown)
        .expect_err("an identifier nobody registered is refused");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &unknown)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(unknown),
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// The normalization a route declares for a typed identifier is the rule its
/// lookup is folded under: on a route that declares ASCII case folding per typed
/// identifier and product, every spelling of the identifier it published names that
/// listing on both resolvers.
#[tokio::test]
async fn a_typed_identifier_of_any_case_resolves_on_a_case_insensitive_route() {
    let fixture = Fixture::with_normalization(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![btc_entry()]),
        &[btc_entry()],
        FIXTURE_CODES,
        instrument_normalization(SymbolNormalization::AsciiCaseInsensitive),
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    for spelling in ["BTCUSDT", "btcusdt", "BtcUsdt"] {
        let identifier = ListingIdentifier::BinanceInstrument {
            symbol: NativeSymbol::new(spelling).expect("the fixture symbol is valid"),
            product: BinanceProduct::Spot,
        };
        let synchronous = ensured
            .resolve_listing_identifier(&identifier)
            .expect("the synchronous resolver folds with the route's own rule");
        let asynchronous = catalog
            .resolve_listing_identifier(&spot_route(), &identifier)
            .expect("the asynchronous resolver answers the same spelling");
        assert_eq!(synchronous, btc_key(), "{spelling}");
        assert_eq!(
            synchronous, asynchronous,
            "both resolvers answer {spelling} with the same listing key"
        );
    }
}

/// The other direction of the same rule: a route that declares exact matching for
/// the typed identifier it publishes keeps the case apart, so the spelling it did
/// not publish names no listing, on either resolver.
#[tokio::test]
async fn a_typed_identifier_of_another_case_does_not_resolve_on_an_exact_route() {
    let fixture = Fixture::with_normalization(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![btc_entry()]),
        &[btc_entry()],
        FIXTURE_CODES,
        instrument_normalization(SymbolNormalization::Exact),
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    // Control: the spelling the route published resolves on both sides.
    let published = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("BTCUSDT").expect("the fixture symbol is valid"),
        product: BinanceProduct::Spot,
    };
    assert_eq!(
        ensured
            .resolve_listing_identifier(&published)
            .expect("the synchronous resolver answers the published spelling"),
        btc_key()
    );
    assert_eq!(
        catalog
            .resolve_listing_identifier(&spot_route(), &published)
            .expect("the asynchronous resolver answers it too"),
        btc_key()
    );

    let folded = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("btcusdt").expect("the fixture symbol is valid"),
        product: BinanceProduct::Spot,
    };
    let synchronous = ensured
        .resolve_listing_identifier(&folded)
        .expect_err("an exact route never folds the case of a typed identifier");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &folded)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(folded),
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);

    // The symbol space of the same route matches exactly too, so the declaration is
    // the route's answer for both lookups a listing can be reached by.
    assert_eq!(
        ensured
            .resolve_native_symbol(&NativeSymbol::new("BTCUSDT").expect("the symbol is valid"))
            .expect("the published symbol resolves"),
        btc_key()
    );
    assert_eq!(
        ensured
            .resolve_native_symbol(&NativeSymbol::new("btcusdt").expect("the symbol is valid"))
            .expect_err("an exact route keeps case-different symbols apart"),
        CatalogError::NativeSymbolNotFound {
            route: spot_route(),
            symbol: NativeSymbol::new("btcusdt").expect("the symbol is valid"),
        }
    );
}

/// A rule is declared at typed identifier *and* product granularity, so a route that
/// folds the perpetual instrument identifier does not fold the spot one: the spot
/// spelling the route did not publish is not folded into the one it did.
#[tokio::test]
async fn a_typed_identifier_of_a_product_without_a_rule_is_not_folded() {
    let rules = vec![IdentifierNormalizationRule::new(
        ListingIdentifierKind::BinanceInstrument,
        Some(IdentifierNormalizationProduct::Binance(
            BinanceProduct::Perpetual,
        )),
        SymbolNormalization::AsciiCaseInsensitive,
    )];
    let fixture = Fixture::with_normalization(
        spot_route(),
        SPOT_SOURCE,
        listing(vec![btc_entry()]),
        &[btc_entry()],
        FIXTURE_CODES,
        rules,
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");

    assert_eq!(
        ensured
            .resolve_listing_identifier(&ListingIdentifier::BinanceInstrument {
                symbol: NativeSymbol::new("BTCUSDT").expect("the fixture symbol is valid"),
                product: BinanceProduct::Spot,
            })
            .expect("the published spelling resolves"),
        btc_key()
    );

    let folded = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("btcusdt").expect("the fixture symbol is valid"),
        product: BinanceProduct::Spot,
    };
    assert_eq!(
        ensured
            .resolve_listing_identifier(&folded)
            .expect_err("the spot product folds only under a rule of its own"),
        CatalogError::NativeIdentifierNotFound {
            route: spot_route(),
            identifier: Box::new(folded),
        }
    );
}

/// the route's own scoped identifier is answered by both resolvers, another
/// scope is a route mismatch on both, and the same scope without a registered
/// listing stays a not-found on both.
#[tokio::test]
async fn the_listing_identifier_resolvers_agree_about_a_scoped_identifier() {
    let (catalog, ensured_spot, _) = two_route_listing_fixture().await;

    // Control: the scoped identifier of this route's own listing resolves on both
    // sides and names the same listing key.
    let own = ListingIdentifier::HyperliquidAsset {
        scope: spot_route().scope,
        product: HyperliquidAssetProduct::Spot,
        asset_id: HyperliquidAssetId(3),
    };
    let synchronous = ensured_spot
        .resolve_listing_identifier(&own)
        .expect("the synchronous resolver answers a scoped identifier of its own scope");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &own)
        .expect("the asynchronous resolver answers it too");
    assert_eq!(synchronous, listing_key(&spot_route(), "BTCUSDT"));
    assert_eq!(
        synchronous, asynchronous,
        "both resolvers answer with the same listing key"
    );

    // A scoped identifier of this route's own scope nobody registered is a
    // not-found on both sides, never a scope refusal.
    let unregistered = ListingIdentifier::HyperliquidAsset {
        scope: spot_route().scope,
        product: HyperliquidAssetProduct::Perpetual,
        asset_id: HyperliquidAssetId(9),
    };
    let synchronous = ensured_spot
        .resolve_listing_identifier(&unregistered)
        .expect_err("a scoped identifier nobody registered is not found");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &unregistered)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(unregistered),
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);

    // The cross-scope direction: an identifier of another scope is not this
    // route's to answer, whoever registered it, and both resolvers say so with
    // one and the same strongly typed value.
    let foreign = ListingIdentifier::HyperliquidAsset {
        scope: perp_route().scope,
        product: HyperliquidAssetProduct::Spot,
        asset_id: HyperliquidAssetId(3),
    };
    let synchronous = ensured_spot
        .resolve_listing_identifier(&foreign)
        .expect_err("a scoped identifier of another route is not this route's to answer");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &foreign)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeIdentifierRouteMismatch {
        route: spot_route(),
        identifier: Box::new(foreign),
    };
    assert_eq!(
        synchronous, expected,
        "the synchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        asynchronous, expected,
        "the asynchronous refusal must name this route and this identifier"
    );
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// The typed identifier of a listing another route publishes is never answered
/// by this route's resolvers, even though the catalog publishes both routes: the
/// per-route index is the only one consulted, and both sides refuse alike.
#[tokio::test]
async fn the_listing_identifier_resolvers_agree_about_another_routes_listing() {
    let (catalog, ensured_spot, ensured_perp) = two_route_listing_fixture().await;

    let foreign = ListingIdentifier::BinanceInstrument {
        symbol: NativeSymbol::new("ETH-USDT").expect("the symbol is valid"),
        product: BinanceProduct::Spot,
    };
    let synchronous = ensured_spot
        .resolve_listing_identifier(&foreign)
        .expect_err("the spot route does not answer the perp route's listing");
    let asynchronous = catalog
        .resolve_listing_identifier(&spot_route(), &foreign)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeIdentifierNotFound {
        route: spot_route(),
        identifier: Box::new(foreign.clone()),
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );

    // The route that does publish it answers on both sides.
    let synchronous = ensured_perp
        .resolve_listing_identifier(&foreign)
        .expect("the perp route answers its own listing");
    let asynchronous = catalog
        .resolve_listing_identifier(&perp_route(), &foreign)
        .expect("the asynchronous resolver answers it there too");
    assert_eq!(synchronous, listing_key(&perp_route(), "ETH-USDT"));
    assert_eq!(synchronous, asynchronous);
}

/// The route's own normalization rule is applied by both symbol resolvers, so a
/// spelling the route folds names the same listing on both sides, and an unknown
/// symbol is refused by both with one and the same value.
#[tokio::test]
async fn the_native_symbol_resolvers_agree() {
    let fixture = Fixture::new(vec![entry_with_aliases(
        &spot_route(),
        btc(),
        "BTCUSDT",
        &["XBTUSDT"],
    )]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    for spelling in ["BTCUSDT", "btcusdt", "XBTUSDT", "xbtusdt"] {
        let typed = NativeSymbol::new(spelling).expect("the fixture symbol is valid");
        let synchronous = ensured
            .resolve_native_symbol(&typed)
            .expect("the synchronous resolver normalizes with the route's own rule");
        let asynchronous = catalog
            .resolve_native_symbol(&spot_route(), spelling)
            .expect("the asynchronous resolver answers the same spelling");
        assert_eq!(synchronous, btc_key(), "{spelling}");
        assert_eq!(
            synchronous, asynchronous,
            "both resolvers answer {spelling} with the same listing key"
        );
    }

    let unknown = NativeSymbol::new("n4-unknown-symbol").expect("the symbol is valid");
    let synchronous = ensured
        .resolve_native_symbol(&unknown)
        .expect_err("an unknown symbol is refused");
    let asynchronous = catalog
        .resolve_native_symbol(&spot_route(), "n4-unknown-symbol")
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::NativeSymbolNotFound {
        route: spot_route(),
        symbol: unknown,
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// One listing is resolved into the same value by the route proof, the catalog
/// resolver and the protocol-facing projection, and an absent
/// listing is refused by both resolvers with one and the same value.
#[tokio::test]
async fn the_listing_resolvers_agree() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    let synchronous = ensured
        .resolve_listing(&btc_key())
        .expect("the synchronous resolver answers");
    let asynchronous = catalog
        .resolve_listing(&btc_key())
        .expect("the asynchronous resolver answers");
    let reference = catalog
        .resolve_listing_ref(&btc_key())
        .expect("the projection answers");
    let entry = catalog
        .listing(&btc_key())
        .expect("the published entry answers");
    assert_eq!(synchronous.key, btc_key());
    assert_eq!(
        synchronous, asynchronous,
        "both resolvers answer with the same resolution, generation included"
    );
    assert_eq!(reference.generation, synchronous.generation);
    assert_eq!(reference.key, synchronous.key);
    assert_eq!(entry.listing.key, synchronous.key);
    assert_eq!(synchronous.generation, catalog.generation_id());

    let missing = VenueListingKey::new(spot_route(), test_support::listing_id("fade"));
    let synchronous = ensured
        .resolve_listing(&missing)
        .expect_err("an absent listing is refused");
    let asynchronous = catalog
        .resolve_listing(&missing)
        .expect_err("the asynchronous resolver refuses it too");
    let expected = CatalogError::ListingNotFound {
        listing: Box::new(missing),
    };
    assert_eq!(synchronous, expected);
    assert_eq!(asynchronous, expected);
    assert_eq!(
        format!("{synchronous:?}"),
        format!("{asynchronous:?}"),
        "the two refusals must be the same value, field by field"
    );
}

/// The proof of one route never resolves another route's listing, however
/// published it is, while the route-unscoped readers of the catalog answer for
/// the route the listing itself names.
#[tokio::test]
async fn the_ensured_route_never_resolves_another_routes_listing() {
    let (catalog, ensured_spot, ensured_perp) = two_route_listing_fixture().await;
    let foreign = listing_key(&perp_route(), "ETH-USDT");

    let refused = ensured_spot
        .resolve_listing(&foreign)
        .expect_err("the proof of the spot route refuses the perp route's listing");
    assert_eq!(
        refused,
        CatalogError::ListingRouteMismatch {
            expected: Box::new(spot_route()),
            found: Box::new(foreign.clone()),
        },
        "the refusal names the route the proof is about and the listing it was asked for"
    );
    assert!(
        ensured_spot.native_symbol(&foreign).is_none(),
        "the symbol reader of the spot route refuses it too"
    );
    assert_eq!(
        catalog
            .resolve_listing(&foreign)
            .expect("the catalog answers for the route the listing names")
            .key,
        foreign
    );
    assert_eq!(
        ensured_perp
            .resolve_listing(&foreign)
            .expect("the proof of the perp route answers its own listing")
            .key,
        foreign
    );
    assert_eq!(
        catalog
            .native_symbol(&foreign)
            .as_ref()
            .map(NativeSymbol::as_str),
        Some("ETH-USDT")
    );
}

/// The published-listing readers answer from one and the same snapshot: the
/// proof's listings are the catalog's cached ones, and the entry, symbol and
/// identity readers agree with the resolvers.
#[tokio::test]
async fn the_published_listing_readers_agree() {
    let fixture = Fixture::new(vec![btc_entry()]);
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalog");
    let ensured = fixture.ensure().await.expect("the spot route loads");
    let catalog = fixture.catalog();

    assert!(catalog.contains(&btc_key()).expect("contains answers"));
    let cached = catalog
        .cached_listings(&spot_route())
        .expect("the snapshot is cached");
    assert!(
        Arc::ptr_eq(&ensured.listings(), &cached),
        "the proof hands out the snapshot the catalog caches"
    );
    assert_eq!(ensured.listings().len(), cached.len());
    let entry = catalog
        .cached_listing(&btc_key())
        .expect("the listing is published");
    assert_eq!(
        catalog
            .listing(&btc_key())
            .expect("the loading reader answers")
            .listing
            .key,
        entry.listing.key
    );
    assert_eq!(
        ensured.native_symbol(&btc_key()),
        catalog.native_symbol(&btc_key())
    );
    assert_eq!(
        catalog
            .native_symbol(&btc_key())
            .as_ref()
            .map(NativeSymbol::as_str),
        Some("BTCUSDT")
    );
    assert_eq!(catalog.listings_for(btc().id()).to_vec(), vec![btc_key()]);
}

/// A source no venue commit in this module ever fetches through: the commit stages
/// the listings the test states instead of asking a venue for them.
struct Unreachable;

impl CatalogSource for Unreachable {
    fn fetch_catalog(
        &self,
        _: crate::catalog::source::CatalogFetchContext,
    ) -> BoxFuture<
        'static,
        Result<crate::catalog::batch::CatalogBatch, crate::catalog::error::SourceError>,
    > {
        Box::pin(async {
            Err(crate::catalog::error::SourceError::Other {
                reason: SmolStr::new_static("the atomicity fixture never fetches"),
            })
        })
    }
}

/// One venue commit is one atomic replacement.
///
/// A commit installs the snapshots it staged, revokes the scopes the venue no
/// longer publishes and moves the publication pointer in one critical section, so
/// the registered route set and the directory it belongs to move together: every
/// observation is one whole commit, the generation handed to the commit's
/// visibility callback is the very pointer the catalogue serves afterwards, and a
/// generation a reader already holds never changes.
#[tokio::test]
async fn a_venue_commit_is_one_atomic_replacement() {
    let catalog = Arc::new(Catalog::new());
    let venue = spot_route().venue;
    let requirements = crate::catalog::source::CatalogRouteRequirements::from_catalog_routes([
        spot_route(),
        perp_route(),
    ]);

    // The venue's first commit registers both of its routes and publishes both
    // catalogs, which is the state the replacement below starts from.
    let registrations = vec![
        test_support::registration(&spot_route(), SPOT_SOURCE),
        test_support::registration(&perp_route(), SPOT_SOURCE),
    ];
    let plan = test_support::plan_venue_commit(
        &catalog,
        &venue,
        registrations.clone(),
        &requirements,
        Arc::new(Unreachable),
    )
    .expect("the fixture venue plans");
    let mut pending = Vec::with_capacity(2);
    for (route, listings) in [
        (spot_route(), vec![btc_entry()]),
        (perp_route(), vec![entry(&perp_route(), btc(), "BTCUSDT")]),
    ] {
        let registered = plan
            .registered_route(&route)
            .expect("the planned route is a registered catalog route");
        pending.push(
            catalog
                .stage_fixture_listings(&registered, listings)
                .expect("the fixture listings stage"),
        );
    }
    let published_before = catalog.generation();
    let visible_before = std::cell::Cell::new(0_usize);
    test_support::publish_venue_commit(
        &catalog,
        &venue,
        registrations.clone(),
        &requirements,
        pending,
        Arc::new(Unreachable),
        |_| visible_before.set(visible_before.get() + 1),
    )
    .expect("the first venue commit is accepted");
    let published = catalog.generation();

    assert_eq!(
        visible_before.get(),
        1,
        "one commit hands its generation to visibility exactly once"
    );
    assert!(
        published.id() > published_before.id(),
        "the commit published a strictly newer generation"
    );
    assert!(
        !Arc::ptr_eq(&published_before, &catalog.generation()),
        "the published pointer is a new generation, not the one a reader held"
    );
    assert!(
        published_before.route_snapshots().is_empty(),
        "the generation a reader held before the commit is untouched"
    );
    assert_eq!(
        published.route_snapshots().len(),
        2,
        "one commit installed both of the venue's catalogs at once"
    );

    // The replacement: a new snapshot for the spot scope, and the perpetual scope
    // is gone. Both move in one commit.
    let replacement = test_support::plan_venue_commit(
        &catalog,
        &venue,
        registrations,
        &requirements,
        Arc::new(Unreachable),
    )
    .expect("the reduced venue plans");
    let registered = replacement
        .registered_route(&spot_route())
        .expect("the surviving scope is still a registered catalog route");
    let pending = catalog
        .stage_fixture_listings(&registered, vec![btc_entry(), eth_entry()])
        .expect("the replacement listings stage");
    let mut visible = Vec::with_capacity(1);
    let id = test_support::publish_venue_commit(
        &catalog,
        &venue,
        vec![test_support::registration(&spot_route(), SPOT_SOURCE)],
        &requirements,
        vec![pending],
        Arc::new(Unreachable),
        |generation| visible.push(Arc::clone(generation)),
    )
    .expect("the replacement commit is accepted");

    assert_eq!(visible.len(), 1, "one commit is one visibility callback");
    assert_eq!(
        id,
        visible[0].id(),
        "the commit reports the generation it published"
    );
    assert!(
        Arc::ptr_eq(&visible[0], &catalog.generation()),
        "and that generation is the pointer the catalogue serves"
    );
    assert_eq!(
        catalog.generation().route_snapshots().len(),
        1,
        "the vanished scope left the directory in the same publication"
    );
    assert!(
        !catalog.is_registered(&perp_route()),
        "the route registry dropped the vanished scope"
    );
    assert!(
        catalog.cached_listings(&perp_route()).is_none(),
        "and the directory holds no snapshot of it"
    );
    assert_eq!(
        symbols_of(
            &catalog
                .cached_listings(&spot_route())
                .expect("the surviving scope keeps its snapshot")
        ),
        vec!["BTCUSDT".to_owned(), "ETHUSDT".to_owned()],
        "the surviving scope's snapshot is the one this commit installed"
    );
    assert!(
        published.route_snapshots().len() == 2,
        "the generation before the replacement still holds what it published"
    );
}

/// A venue commit the catalog refuses changes nothing.
///
/// The refusal here is the candidate check's own: a staged batch whose route the
/// commit does not register is refused before the generation is built, so this
/// test pins "a refusal at that point leaves every view at the previous
/// generation" — the published pointer and identity, the registered route set,
/// the registration of the route the batch belonged to, and the visibility
/// callback. It is *not* evidence for the commit's write order: for that, see
/// [`a_venue_commit_the_display_gate_refuses_leaves_every_view_at_the_previous_generation`],
/// whose refusal happens while the generation is being built, after staging
/// succeeded.
#[tokio::test]
async fn a_refused_venue_commit_leaves_every_view_at_the_previous_generation() {
    let catalog = Arc::new(Catalog::new());
    let venue = spot_route().venue;
    let requirements = crate::catalog::source::CatalogRouteRequirements::from_catalog_routes([
        spot_route(),
        perp_route(),
    ]);
    let registrations = vec![test_support::registration(&spot_route(), SPOT_SOURCE)];

    // The venue's first commit registers the spot route and publishes its catalog.
    let plan = test_support::plan_venue_commit(
        &catalog,
        &venue,
        registrations.clone(),
        &requirements,
        Arc::new(Unreachable),
    )
    .expect("the fixture venue plans");
    let registered = plan
        .registered_route(&spot_route())
        .expect("the planned route is a registered catalog route");
    let pending = catalog
        .stage_fixture_listings(&registered, vec![btc_entry()])
        .expect("the fixture listings stage");
    test_support::publish_venue_commit(
        &catalog,
        &venue,
        registrations.clone(),
        &requirements,
        vec![pending],
        Arc::new(Unreachable),
        |_| {},
    )
    .expect("the first venue commit is accepted");

    let generation_before = catalog.generation();
    let route_set_before = catalog
        .route_set()
        .expect("the commit registered a route set")
        .catalog_routes()
        .to_vec();
    let listings_before = catalog
        .cached_listings(&spot_route())
        .expect("the first commit published the spot catalog");

    // A batch is staged against a route the commit that follows does not register:
    // the pending's route is absent from the candidate, so the commit is refused.
    let plan = test_support::plan_venue_commit(
        &catalog,
        &venue,
        registrations,
        &requirements,
        Arc::new(Unreachable),
    )
    .expect("the venue plans again");
    let registered = plan
        .registered_route(&spot_route())
        .expect("the planned route is a registered catalog route");
    let pending = catalog
        .stage_fixture_listings(&registered, vec![btc_entry(), eth_entry()])
        .expect("the replacement listings stage");
    let mut visible = Vec::with_capacity(1);
    let refused = test_support::publish_venue_commit(
        &catalog,
        &venue,
        Vec::new(),
        &crate::catalog::source::CatalogRouteRequirements::default(),
        vec![pending],
        Arc::new(Unreachable),
        |generation| visible.push(Arc::clone(generation)),
    );

    assert!(
        matches!(refused, Err(CatalogError::RouteNotRegistered { .. })),
        "the commit is refused: {refused:?}"
    );
    assert!(visible.is_empty(), "a refused commit never becomes visible");
    assert!(
        Arc::ptr_eq(&generation_before, &catalog.generation()),
        "the generation a reader held is still the one the catalogue serves"
    );
    assert_eq!(
        catalog.generation_id(),
        generation_before.id(),
        "and its identity did not move"
    );
    assert_eq!(
        catalog
            .route_set()
            .expect("the route set is still registered")
            .catalog_routes()
            .to_vec(),
        route_set_before,
        "the registered route set is the one the previous commit wrote"
    );
    assert!(
        catalog.is_registered(&spot_route()),
        "the route the refused commit staged for is still registered"
    );
    assert!(
        Arc::ptr_eq(
            &listings_before,
            &catalog
                .cached_listings(&spot_route())
                .expect("the previous catalog is still published")
        ),
        "and the directory still serves the snapshot the previous commit published"
    );
}

/// A venue commit the display gate refuses leaves every view at the previous
/// generation.
///
/// Two scopes of one venue stage one batch each, and the two batches stage a
/// different display code for the same canonical asset. Each stage is accepted —
/// a batch is validated against the base plus itself — and the refusal happens
/// while the venue commit folds the second batch into the same generation, which
/// is exactly the failure that has to be found *before* the route registry is
/// written. A commit that wrote its candidate first and built the generation
/// afterwards would leave the route map holding two routes whose snapshots nobody
/// published, and the route-set and registration assertions below would fail.
///
/// The refusal is therefore the one [`a_refused_venue_commit_leaves_every_view_at_the_previous_generation`]
/// cannot produce: a pending that passed staging, and a generation that refuses it.
#[tokio::test]
async fn a_venue_commit_the_display_gate_refuses_leaves_every_view_at_the_previous_generation() {
    let catalog = Arc::new(Catalog::new());
    let venue = spot_route().venue;
    // Two scopes of one venue, both publishing a spot instrument: the two batches
    // differ only in the display code they stage for the one asset they share.
    let first = spot_route();
    let second = test_support::route("binance", "spot-b");
    let asset = test_support::security("cik/320193", "AAPL");
    let requirements = crate::catalog::source::CatalogRouteRequirements::from_catalog_routes([
        first.clone(),
        second.clone(),
    ]);
    let registrations = vec![
        test_support::registration(&first, SPOT_SOURCE),
        test_support::registration(&second, SPOT_SOURCE),
    ];
    let plan = test_support::plan_venue_commit(
        &catalog,
        &venue,
        registrations.clone(),
        &requirements,
        Arc::new(Unreachable),
    )
    .expect("the fixture venue plans");

    let mut pending = Vec::with_capacity(2);
    for (route, code) in [(first.clone(), "AAA"), (second.clone(), "ZZZ")] {
        let registered = plan
            .registered_route(&route)
            .expect("the planned route is a registered catalog route");
        pending.push(staged_batch_with_code(&catalog, &registered, &asset, code));
    }

    let generation_before = catalog.generation();
    let route_set_before = catalog.route_set().map(|set| set.catalog_routes().to_vec());
    let mut visible = Vec::with_capacity(1);
    let refused = test_support::publish_venue_commit(
        &catalog,
        &venue,
        registrations,
        &requirements,
        pending,
        Arc::new(Unreachable),
        |generation| visible.push(Arc::clone(generation)),
    );

    assert!(
        matches!(
            &refused,
            Err(CatalogError::InvalidCatalog { error, .. })
                if matches!(error.as_ref(), CatalogValidationError::DisplayRejected { .. })
        ),
        "the commit is refused by the display gate: {refused:?}"
    );
    assert!(visible.is_empty(), "a refused commit never becomes visible");
    assert!(
        Arc::ptr_eq(&generation_before, &catalog.generation()),
        "the generation a reader held is still the one the catalogue serves"
    );
    assert_eq!(
        catalog.generation_id(),
        generation_before.id(),
        "and its identity did not move"
    );
    assert_eq!(
        catalog.route_set().map(|set| set.catalog_routes().to_vec()),
        route_set_before,
        "the route registry holds exactly what the previous commit wrote: a commit \
         that wrote its candidate before building the generation would hold the two \
         refused scopes here"
    );
    for route in [first, second] {
        assert!(
            !catalog.is_registered(&route),
            "{route} must not be registered by a commit that published nothing"
        );
        assert!(
            catalog.cached_listings(&route).is_none(),
            "and the directory must hold no snapshot of it"
        );
    }
}

/// Stages one batch of `route` whose listing is based on `asset` and whose batch
/// stages `code` for that asset.
///
/// [`Catalog::stage_fixture_listings`] states no codes, so this builds the fetch
/// context and the pending the way a venue commit's own stage does, which is what
/// lets a test stage two codes for one asset across two batches. The asset is a
/// security because its terms carry no code: a code is what a listing verified,
/// so a batch is free to stage one.
fn staged_batch_with_code(
    catalog: &Catalog,
    registered: &crate::catalog::source::RegisteredCatalogRoute,
    asset: &market_participants_model::asset::AssetId,
    code: &str,
) -> crate::catalog::batch::CatalogPending {
    let base = catalog.generation();
    let staging = crate::catalog::registry::staging::CatalogStagingHandle::new(
        crate::catalog::registry::staging::StagedRegistry::new(
            Arc::clone(base.assets()),
            Arc::clone(base.references()),
            Arc::clone(base.outcomes()),
        ),
    );
    let context = CatalogFetchContext::new(registered.clone(), staging);
    let route = registered.route().clone();
    let instrument = spot(asset.clone(), token("binance", "USDT"));
    let listing = entry(&route, instrument, code);
    let batch = fixture_batch(
        CatalogBatch {
            listings: vec![listing],
            asset_codes: vec![crate::catalog::display::publication::StagedAssetCode {
                asset: asset.clone(),
                code:
                    market_participants_model::display::asset::AssetDisplayCode::from_staged_code(
                        code,
                    )
                    .expect("the fixture code is renderable"),
            }],
            ..CatalogBatch::default()
        },
        &context,
    );
    CatalogBatch::pending(&context, batch, &base)
        .expect("a batch is staged against the base plus itself")
}
