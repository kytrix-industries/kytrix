//! Batch validation: every reason a candidate catalog is refused as a whole,
//! and the registry transaction that goes with it.

use std::sync::Arc;

use market_participants_model::{
    asset::AssetId,
    decimal::{Quantity, Tick},
    identifier::{
        CtfConditionId, CtfTokenId, IdentifierValue, NativeSymbol, PolymarketEventId,
        PolymarketMarketId, ResolutionSourceId, TradingCalendarId,
    },
    instrument::{Instrument, InstrumentTerms, OutcomeTerms},
    listing::{
        identifiers::{PolymarketListingIdentifiers, VenueListingIdentifiers},
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        native::OutcomeGroupIdentifier,
        route::VenueRoute,
        rules::{
            CommonTradingRules, PriceLimitRule, PriceRule, ProductTradingRules, QuantityRule,
            SettlementCycle, TradingRules,
        },
        venue::{OkxInstrumentType, PriceBasis},
    },
    market::units::QuantityUnit,
    outcome::{
        OutcomeConversion, OutcomeConversionOutput, OutcomeEvent, OutcomeGroup,
        OutcomeGroupAdjustment, OutcomeGroupId, OutcomeGroupLineageEdge, OutcomeGroupSeed,
        OutcomeLegRef, OutcomeMember, OutcomeRiskPool, OutcomeRiskPoolAdjustment,
        OutcomeRiskPoolId, OutcomeRiskPoolLineageEdge, OutcomeSide, OutcomeTopology,
        ResolutionRule, SettlementFractionRule,
    },
    routing::{CapabilityId, ListingIdentifierKind},
    settlement::{Expiry, InstrumentAdjustment, InstrumentLineageEdge},
};
use smol_str::SmolStr;

use crate::catalog::{
    batch::{CatalogBatch, CatalogEntry, ReferencedTerms},
    error::CatalogValidationError,
    generation::{CatalogGeneration, CatalogGenerationPublisher},
    registry::{
        asset::{AssetRegistry, AssetRegistryPending},
        outcome::{OutcomeRegistry, OutcomeRegistryPending},
        staging::{CatalogStagingHandle, StagedRegistry},
    },
    source::{
        CatalogFetchContext, CatalogRouteRegistration, CatalogRouteUpdate, CatalogSourceId,
        CatalogSourceRegistry, RegisteredCatalogRoute, RouteInputRevision,
    },
    test_support::{
        self, currency, currency_terms, entry, listing_key, perp_route, spot, spot_route,
        spot_token, token_id, token_terms,
    },
};

fn btc() -> Instrument {
    spot_token("binance", "BTC", "USDT")
}

fn eth() -> Instrument {
    spot_token("binance", "ETH", "USDT")
}

/// A second instrument quoted in another asset, for a route that holds two
/// independent listings of one venue.
fn btc_usdc() -> Instrument {
    spot_token("binance", "BTC", "USDC")
}

/// One registered fixture route, without a catalogue behind it.
fn registered(route: &VenueRoute) -> RegisteredCatalogRoute {
    let update = CatalogRouteUpdate::new(
        [test_support::binding(
            "binance-spot",
            test_support::listing(Vec::new()),
        )],
        [CatalogRouteRegistration::catalog(
            route.clone(),
            CatalogSourceId::new("binance-spot").expect("the source id is valid"),
            test_support::case_insensitive_symbols(),
            vec![CapabilityId::MarketCatalog],
            RouteInputRevision::describe().build(),
        )],
    );
    CatalogSourceRegistry::new()
        .plan(
            update,
            &test_support::requirements(std::slice::from_ref(route)),
        )
        .expect("the fixture route set is accepted")
        .route(route)
        .and_then(|entry| entry.as_registered_catalog_route())
        .expect("the fixture route is registered")
}

fn base_generation(route: &RegisteredCatalogRoute) -> CatalogGeneration {
    let _ = route;
    CatalogGeneration::empty(&CatalogGenerationPublisher::new())
}

fn context(route: &VenueRoute) -> (CatalogFetchContext, RegisteredCatalogRoute) {
    let registered = registered(route);
    let context = CatalogFetchContext::new(
        registered.clone(),
        CatalogStagingHandle::new(StagedRegistry::empty()),
    );
    (context, registered)
}

fn accept(
    context: &CatalogFetchContext,
    base: &CatalogGeneration,
    batch: CatalogBatch,
) -> Result<crate::catalog::batch::CatalogPending, CatalogValidationError> {
    // The fixture vocabulary carries the canonical terms of its own assets, so a
    // test batch is the batch a real source would have produced.
    let mut batch = batch;
    crate::catalog::test_support::stage_fixture_terms(&mut batch, context.staging(), base.assets());
    CatalogBatch::pending(context, batch, base)
}

fn btc_entry(route: &VenueRoute) -> CatalogEntry {
    entry(route, btc(), "BTCUSDT")
}

fn batch_of(entries: Vec<CatalogEntry>) -> CatalogBatch {
    CatalogBatch {
        listings: entries,
        ..CatalogBatch::default()
    }
}

// ---- entries -------------------------------------------------------------

#[test]
fn an_empty_batch_is_a_successful_snapshot() {
    let (context, route) = context(&spot_route());
    let pending = accept(&context, &base_generation(&route), CatalogBatch::default())
        .expect("an empty catalog is a success");

    assert!(pending.entries().is_empty());
    assert!(pending.native_keys().is_empty());
    assert!(pending.outcome_groups.is_empty());
}

#[test]
fn a_listing_of_another_route_is_refused() {
    let (context, route) = context(&spot_route());
    let foreign = entry(&perp_route(), btc(), "BTCUSDT");

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![foreign.clone()]),
    )
    .expect_err("a listing of another route is refused");

    assert_eq!(
        error,
        CatalogValidationError::ListingRouteMismatch {
            route: spot_route(),
            listing: Box::new(foreign.listing.key),
        }
    );
}

#[test]
fn a_snapshot_of_another_key_is_refused() {
    let (context, route) = context(&spot_route());
    let mut broken = btc_entry(&spot_route());
    broken.snapshot.key = listing_key(&spot_route(), "ETHUSDT");

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![broken.clone()]),
    )
    .expect_err("a snapshot must describe its own listing");

    assert_eq!(
        error,
        CatalogValidationError::SnapshotKeyMismatch {
            listing: Box::new(broken.listing.key),
            snapshot: Box::new(broken.snapshot.key),
        }
    );
}

#[test]
fn a_listing_whose_terms_produce_another_instrument_is_refused() {
    let (context, route) = context(&spot_route());
    let mut broken = btc_entry(&spot_route());
    let declared = broken.listing.instrument_id.clone();
    broken.instrument = eth();

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![broken.clone()]),
    )
    .expect_err("the declared identity must match the terms");

    assert_eq!(
        error,
        CatalogValidationError::InstrumentTermsMismatch {
            listing: Box::new(broken.listing.key),
            declared,
            computed: eth().id().clone(),
        }
    );
}

#[test]
fn rules_of_another_quote_asset_are_refused() {
    let (context, route) = context(&spot_route());
    let mut broken = btc_entry(&spot_route());
    broken.snapshot.rules.common.price.quote_asset = currency("EUR");

    let error = accept(&context, &base_generation(&route), batch_of(vec![broken]))
        .expect_err("rules of another quote asset are refused");

    assert!(matches!(
        error,
        CatalogValidationError::InvalidTradingRules { .. }
    ));
}

#[test]
fn rules_of_another_product_are_refused() {
    let (context, route) = context(&spot_route());
    let mut broken = btc_entry(&spot_route());
    broken.snapshot.rules.product = ProductTradingRules::Outcome(
        market_participants_model::listing::rules::OutcomeTradingRules {
            order_book: true,
            negative_risk_conversion: false,
        },
    );

    let error = accept(&context, &base_generation(&route), batch_of(vec![broken]))
        .expect_err("rules of another product are refused");

    assert!(matches!(
        error,
        CatalogValidationError::InstrumentKindMismatch { .. }
    ));
}

#[test]
fn a_quantity_unit_of_another_instrument_is_refused() {
    let (context, route) = context(&spot_route());
    let mut broken = btc_entry(&spot_route());
    broken.snapshot.rules.common.quantity.unit = QuantityUnit::AssetUnits {
        asset: AssetId::CryptoToken(token_id("binance", "USDT")),
    };

    let error = accept(&context, &base_generation(&route), batch_of(vec![broken]))
        .expect_err("the traded unit must belong to the listed instrument");

    assert!(matches!(
        error,
        CatalogValidationError::InvalidTradingRules { .. }
    ));
}

#[test]
fn a_duplicate_listing_id_is_refused() {
    let (context, route) = context(&spot_route());
    let first = btc_entry(&spot_route());
    let mut duplicate = first.clone();
    duplicate
        .listing
        .aliases
        .push(NativeSymbol::new("OTHER").expect("the alias is valid"));

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![first, duplicate]),
    )
    .expect_err("one listing id appears once per route");

    assert!(matches!(
        error,
        CatalogValidationError::DuplicateListingId { .. }
    ));
}

#[test]
fn a_normalized_symbol_collision_is_refused() {
    let (context, route) = context(&spot_route());
    let first = entry(&spot_route(), btc(), "BTCUSDT");
    let mut second = entry(&spot_route(), eth(), "BTCUSDT-ALT");
    second.listing.aliases = vec![NativeSymbol::new("btcusdt").expect("the alias is valid")];

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![first, second]),
    )
    .expect_err("two listings cannot share a normalized symbol");

    assert!(matches!(
        error,
        CatalogValidationError::DuplicateNativeIdentifier { .. }
    ));
}

#[test]
fn an_alias_that_normalizes_onto_its_own_symbol_is_not_a_collision() {
    let (context, route) = context(&spot_route());
    let mut entry = btc_entry(&spot_route());
    entry.listing.aliases = vec![NativeSymbol::new("btcusdt").expect("the alias is valid")];

    let pending = accept(&context, &base_generation(&route), batch_of(vec![entry]))
        .expect("one listing may spell its own symbol twice");

    assert_eq!(pending.native_keys().len(), 1);
}

/// The route folds the typed identifier of every listing with its own rule, so two
/// listings whose typed identifiers differ only in case produce one normalized
/// identifier, and a batch that would publish both is refused as a whole.
#[test]
fn a_case_different_typed_identifier_that_normalizes_onto_another_is_refused() {
    let (context, route) = context(&spot_route());
    let upper = entry(&spot_route(), btc(), "BTCUSDT");
    let lower = entry(&spot_route(), btc(), "btcusdt");

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![upper, lower]),
    )
    .expect_err("two typed identifiers the route folds into one cannot both be published");

    assert!(matches!(
        error,
        CatalogValidationError::DuplicateNativeIdentifier { .. }
    ));
}

#[test]
fn two_case_different_symbols_stay_apart_when_the_route_matches_exactly() {
    let route = spot_route();
    let registration = CatalogRouteRegistration::catalog(
        route.clone(),
        CatalogSourceId::new("binance-spot").expect("the source id is valid"),
        Vec::new(),
        vec![CapabilityId::MarketCatalog],
        RouteInputRevision::describe().build(),
    );
    let set = CatalogSourceRegistry::new()
        .plan(
            CatalogRouteUpdate::new(
                [test_support::binding(
                    "binance-spot",
                    test_support::listing(Vec::new()),
                )],
                [registration],
            ),
            &test_support::requirements(std::slice::from_ref(&route)),
        )
        .expect("the exact route set is accepted");
    let registered = set
        .registered_route(&route)
        .expect("the route is registered");
    let context = CatalogFetchContext::new(
        registered.clone(),
        CatalogStagingHandle::new(StagedRegistry::empty()),
    );
    let upper = entry(&route, btc(), "BTCUSDT");
    let lower = entry(&route, eth(), "btcusdt");

    let pending = accept(
        &context,
        &base_generation(&registered),
        batch_of(vec![upper, lower]),
    )
    .expect("an exact route keeps case-different symbols apart");

    assert_eq!(pending.native_keys().len(), 2);
}

#[test]
fn a_listing_may_carry_several_identifier_families() {
    let (context, route) = context(&spot_route());
    let mut entry = btc_entry(&spot_route());
    entry.listing.identifiers = VenueListingIdentifiers::Okx(
        market_participants_model::listing::identifiers::OkxListingIdentifiers {
            instrument_type: OkxInstrumentType::Spot,
            instrument_family: None,
            future_series: None,
            option_series: None,
        },
    );

    let pending = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![entry.clone()]),
    )
    .expect("the declared identifier is indexed like the symbol it normalizes to");

    assert_eq!(pending.native_keys().len(), 1);
    assert_eq!(
        pending
            .listing_identifiers()
            .get(
                &market_participants_model::listing::native::ListingIdentifier::OkxInstrument {
                    instrument_id: NativeSymbol::new("BTCUSDT").expect("the symbol is valid"),
                    instrument_type: OkxInstrumentType::Spot,
                }
            )
            .cloned(),
        Some(entry.listing.key)
    );
}

#[test]
fn the_canonical_order_of_a_snapshot_is_the_listing_key() {
    let (context, route) = context(&spot_route());
    // Three listings of three instruments: the order of a snapshot is the
    // order of its listing keys, whatever economic name the listings carry.
    let first = entry(&spot_route(), btc(), "AAA");
    let second = entry(&spot_route(), eth(), "EEE");
    let third = entry(&spot_route(), btc_usdc(), "BBB");

    let pending = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![second, third, first]),
    )
    .expect("the batch is accepted");

    let keys: Vec<_> = pending
        .entries()
        .iter()
        .map(|entry| entry.listing.key.clone())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        keys, sorted,
        "route snapshot output follows the listing key"
    );
}

#[test]
fn a_batch_keeps_the_declared_trading_rules_of_each_listing() {
    let (context, route) = context(&spot_route());
    let mut primary = btc_entry(&spot_route());
    primary.snapshot.status = ListingStatus::Halted;
    let secondary = entry(&spot_route(), eth(), "ETHUSDT");

    let pending = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![primary.clone(), secondary]),
    )
    .expect("the batch is accepted");

    let stored = pending
        .entries()
        .iter()
        .find(|entry| entry.listing.key == primary.listing.key)
        .expect("the primary listing is stored");
    assert_eq!(stored.snapshot.status, ListingStatus::Halted);
}

// ---- outcome closure -----------------------------------------------------

#[test]
fn a_listing_without_its_outcome_group_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");

    let error = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![listing.clone()]),
    )
    .expect_err("a listing without its group is refused");

    assert_eq!(
        error,
        CatalogValidationError::IncompleteOutcomeSet {
            route: spot_route(),
            listing: Box::new(listing.listing.key),
            kind: "outcome group",
            count: 1,
        }
    );
}

#[test]
fn a_complete_outcome_batch_is_accepted() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("a complete outcome batch is accepted");

    assert_eq!(
        pending.outcome_listings().get(&group.id).cloned(),
        Some(listing.listing.key)
    );
}

#[test]
fn a_group_whose_terms_do_not_match_its_identity_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    let mut broken = group;
    broken.members[0].sides.pop();

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![broken],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a group whose member lost a side is refused");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeGroupIncomplete { .. }
    ));
}

#[test]
fn a_risk_pool_without_its_groups_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let pool = risk_pool(&first, &second);

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![first],
            outcome_risk_pools: vec![pool],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a pool whose member group is absent is refused");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeRiskPoolIncomplete { .. }
    ));
}

#[test]
fn a_group_of_two_risk_pools_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let third = binary_group("Will it hail?");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![first.clone(), second.clone(), third.clone()],
            outcome_risk_pools: vec![risk_pool(&first, &second), risk_pool(&first, &third)],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a canonical group belongs to at most one risk pool");

    assert!(matches!(
        error,
        CatalogValidationError::AmbiguousRiskPoolMembership { .. }
    ));
}

#[test]
fn an_event_without_its_group_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_events: vec![event_of(std::slice::from_ref(&group.id), &[], "event-1")],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an event whose group is absent is refused");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeEventIncomplete { .. }
    ));
}

#[test]
fn an_event_without_its_risk_pool_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let pool = risk_pool(&first, &second);
    let event = event_of(
        &[first.id.clone(), second.id.clone()],
        std::slice::from_ref(&pool.id),
        "event-1",
    );

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![first, second],
            outcome_events: vec![event],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an event whose pool is absent is refused");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeEventIncomplete { .. }
    ));
}

#[test]
fn a_complete_event_and_pool_are_accepted() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let pool = risk_pool(&first, &second);
    let event = event_of(
        &[first.id.clone(), second.id.clone()],
        std::slice::from_ref(&pool.id),
        "event-1",
    );

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![first, second],
            outcome_risk_pools: vec![pool],
            outcome_events: vec![event],
            ..CatalogBatch::default()
        },
    )
    .expect("a complete event is accepted");

    assert_eq!(pending.outcome_events.len(), 1);
}

// ---- lineage -------------------------------------------------------------

#[test]
fn a_risk_pool_lineage_without_its_predecessor_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let reworded = binary_group("Will it rain tomorrow?");
    let pool = risk_pool(&first, &second);
    let successor = risk_pool(&reworded, &second);
    let edge = OutcomeRiskPoolLineageEdge::new(
        successor.id,
        pool.id.clone(),
        OutcomeRiskPoolAdjustment::GroupMembershipChange,
        10,
    )
    .expect("the lineage edge is valid");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![first, second, reworded],
            outcome_risk_pools: vec![pool],
            outcome_risk_pool_lineage: vec![edge],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a predecessor that is not published is refused");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeRiskPoolLineageInvalid { .. }
    ));
}

#[test]
fn a_risk_pool_successor_is_accepted_with_its_lineage() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let old_pool = risk_pool(&first, &second);
    let base = base_generation(&route)
        .with_pending(
            accept(
                &context,
                &base_generation(&route),
                CatalogBatch {
                    outcome_groups: vec![first.clone(), second.clone()],
                    outcome_risk_pools: vec![old_pool.clone()],
                    ..CatalogBatch::default()
                },
            )
            .expect("the first batch is accepted"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");

    let reworded = binary_group("Will it rain tomorrow?");
    let successor = risk_pool(&reworded, &second);
    let pending = CatalogBatch::pending(
        &context,
        CatalogBatch {
            outcome_groups: vec![reworded.clone(), second],
            outcome_risk_pools: vec![successor.clone()],
            outcome_group_lineage: vec![
                OutcomeGroupLineageEdge::new(
                    first.id,
                    reworded.id,
                    OutcomeGroupAdjustment::QuestionChange,
                    10,
                )
                .expect("the group lineage edge is valid"),
            ],
            outcome_risk_pool_lineage: vec![
                OutcomeRiskPoolLineageEdge::new(
                    old_pool.id.clone(),
                    successor.id,
                    OutcomeRiskPoolAdjustment::GroupMembershipChange,
                    10,
                )
                .expect("the pool lineage edge is valid"),
            ],
            ..CatalogBatch::default()
        },
        &base,
    )
    .expect("the successor pool is accepted with its lineage");

    assert_eq!(pending.outcome_risk_pools.len(), 1);
    assert_eq!(pending.outcome_risk_pool_lineage.len(), 1);
    assert!(base.outcome_risk_pools().contains_key(&old_pool.id));
}

#[test]
fn a_group_successor_without_a_pool_successor_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let pool = risk_pool(&first, &second);
    let base = base_generation(&route)
        .with_pending(
            accept(
                &context,
                &base_generation(&route),
                CatalogBatch {
                    outcome_groups: vec![first.clone(), second.clone()],
                    outcome_risk_pools: vec![pool],
                    ..CatalogBatch::default()
                },
            )
            .expect("the first batch is accepted"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");

    let reworded = binary_group("Will it rain tomorrow?");
    let error = CatalogBatch::pending(
        &context,
        CatalogBatch {
            outcome_groups: vec![reworded.clone(), second.clone()],
            outcome_risk_pools: vec![risk_pool(&reworded, &second)],
            outcome_group_lineage: vec![
                OutcomeGroupLineageEdge::new(
                    first.id,
                    reworded.id.clone(),
                    OutcomeGroupAdjustment::QuestionChange,
                    10,
                )
                .expect("the group lineage edge is valid"),
            ],
            ..CatalogBatch::default()
        },
        &base,
    )
    .expect_err("a group successor needs a pool successor");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeRiskPoolLineageInvalid { .. }
    ));
}

#[test]
fn a_duplicate_risk_pool_successor_is_refused() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let third = binary_group("Will it hail?");
    let fourth = binary_group("Will it thunder?");
    let first_pool = risk_pool(&first, &second);
    let second_pool = risk_pool(&third, &fourth);
    let base = base_generation(&route)
        .with_pending(
            accept(
                &context,
                &base_generation(&route),
                CatalogBatch {
                    outcome_groups: vec![
                        first.clone(),
                        second.clone(),
                        third.clone(),
                        fourth.clone(),
                    ],
                    outcome_risk_pools: vec![first_pool.clone(), second_pool.clone()],
                    ..CatalogBatch::default()
                },
            )
            .expect("the first batch is accepted"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");

    let reworded = binary_group("Will it rain tomorrow?");
    let successor = risk_pool(&reworded, &second);
    let error = CatalogBatch::pending(
        &context,
        CatalogBatch {
            outcome_groups: vec![reworded.clone(), second, third.clone(), fourth.clone()],
            outcome_risk_pools: vec![successor.clone(), risk_pool(&third, &fourth)],
            outcome_group_lineage: vec![
                OutcomeGroupLineageEdge::new(
                    first.id,
                    reworded.id,
                    OutcomeGroupAdjustment::QuestionChange,
                    10,
                )
                .expect("the group lineage edge is valid"),
            ],
            outcome_risk_pool_lineage: vec![
                OutcomeRiskPoolLineageEdge::new(
                    first_pool.id,
                    successor.id.clone(),
                    OutcomeRiskPoolAdjustment::GroupMembershipChange,
                    10,
                )
                .expect("the first pool edge is valid"),
                OutcomeRiskPoolLineageEdge::new(
                    second_pool.id,
                    successor.id,
                    OutcomeRiskPoolAdjustment::GroupMembershipChange,
                    10,
                )
                .expect("the second pool edge is valid"),
            ],
            ..CatalogBatch::default()
        },
        &base,
    )
    .expect_err("a successor has exactly one predecessor");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomeRiskPoolLineageInvalid { .. }
    ));
}

#[test]
fn an_instrument_lineage_cycle_is_refused() {
    let (context, route) = context(&spot_route());
    let first = btc().id().clone();
    let second = eth().id().clone();
    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            instrument_lineage: vec![
                InstrumentLineageEdge::new(
                    first.clone(),
                    second.clone(),
                    InstrumentAdjustment::DeliverableAdjustment,
                    10,
                )
                .expect("the first edge is valid"),
                InstrumentLineageEdge::new(
                    second,
                    first,
                    InstrumentAdjustment::DeliverableAdjustment,
                    20,
                )
                .expect("the second edge is valid"),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an instrument lineage cycle is refused");

    assert!(matches!(
        error,
        CatalogValidationError::InstrumentLineageInvalid { .. }
    ));
}

// ---- registry staging ----------------------------------------------------

#[test]
fn an_unreferenced_staged_identity_is_not_published() {
    let (context, route) = context(&spot_route());
    context.staging().with(|staging| {
        let pending: &mut AssetRegistryPending = staging.pending_assets_mut();
        for code in ["BTC", "USDT"] {
            pending
                .register_crypto_token(token_terms("binance", code))
                .expect("the fixture token is valid");
        }
        pending.register_currency(currency_terms("JPY"));
    });

    let pending = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![btc_entry(&spot_route())]),
    )
    .expect("the batch is accepted");
    let referenced = ReferencedTerms::collect(&pending, &base_generation(&route));
    let registry = crate::catalog::batch::publish_assets(
        &Arc::new(AssetRegistry::new()),
        pending.pending_assets(),
        &referenced,
    );

    assert!(
        registry
            .currency(&test_support::currency_id("JPY"))
            .is_none(),
        "an identity no published listing references must not become visible"
    );
    assert!(
        registry
            .crypto_token(&token_id("binance", "USDT"))
            .is_some(),
        "an identity the batch needs is published"
    );
}

#[test]
fn a_staged_group_reaches_the_published_registry() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    context.staging().with(|staging| {
        let pending: &mut OutcomeRegistryPending = staging.pending_outcomes_mut();
        pending
            .register_group(group.clone(), &[])
            .expect("the group is valid");
    });

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");
    let referenced = ReferencedTerms::collect(&pending, &base_generation(&route));
    let registry = pending
        .pending_outcomes()
        .publish(&OutcomeRegistry::new(), &referenced);

    assert!(registry.group(&group.id).is_some());
}

#[test]
fn a_native_lookup_of_a_dropped_group_is_not_published() {
    let group = binary_group("Will it rain?");
    let unrelated = binary_group("Will it snow?");
    let market = PolymarketMarketId::new("market-2").expect("the market id is valid");
    let mut pending = OutcomeRegistryPending::new();
    pending
        .register_group(
            unrelated.clone(),
            &[OutcomeGroupIdentifier::PolymarketMarket(market.clone())],
        )
        .expect("the group is valid");
    let referenced = ReferencedTerms {
        groups: std::iter::once(group.id).collect(),
        ..ReferencedTerms::default()
    };

    let registry = pending.publish(&OutcomeRegistry::new(), &referenced);

    assert!(registry.group(&unrelated.id).is_none());
    assert!(
        registry
            .native_group(&OutcomeGroupIdentifier::PolymarketMarket(market))
            .is_none(),
        "a lookup key of a dropped group must not survive"
    );
}

#[test]
fn the_same_terms_always_produce_the_same_identity() {
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it rain?");

    assert_eq!(first.id, second.id);
    assert_ne!(first.id, binary_group("Will it snow?").id);
}

#[test]
fn an_asset_and_a_token_with_one_code_stay_in_separate_families() {
    let mut pending = AssetRegistryPending::new();
    pending.register_currency(currency_terms("USD"));
    pending
        .register_crypto_token(token_terms("binance", "USD"))
        .expect("the token terms are valid");

    assert_eq!(pending.currencies.len(), 1);
    assert_eq!(pending.crypto_tokens.len(), 1);
    assert_eq!(
        pending.currencies.keys().next(),
        Some(&test_support::currency_id("USD"))
    );
    assert_eq!(
        pending.crypto_tokens.keys().next(),
        Some(&token_id("binance", "USD"))
    );
}

#[test]
fn a_security_keeps_its_issuer_and_share_class() {
    let mut pending = AssetRegistryPending::new();
    let terms = test_support::security_terms("cik/320193", "common");
    let security_id = pending.register_security(terms.clone());

    assert_eq!(
        pending
            .securities
            .get(&security_id)
            .map(|stored| stored.issuer.clone()),
        Some(terms.issuer)
    );
    assert_eq!(
        pending
            .securities
            .get(&security_id)
            .map(|stored| stored.share_class.clone()),
        Some(terms.share_class)
    );
}

#[test]
fn a_currency_registration_is_deterministic_across_registries() {
    let mut first = AssetRegistryPending::new();
    let mut second = AssetRegistryPending::new();

    assert_eq!(
        first.register_currency(currency_terms("USD")),
        second.register_currency(currency_terms("USD"))
    );
}

#[test]
fn a_token_registration_is_deterministic_across_registries() {
    let mut first = AssetRegistryPending::new();
    let mut second = AssetRegistryPending::new();

    assert_eq!(
        first
            .register_crypto_token(token_terms("binance", "BTC"))
            .expect("the token terms are valid"),
        second
            .register_crypto_token(token_terms("binance", "BTC"))
            .expect("the token terms are valid")
    );
}

#[test]
fn two_issuers_never_share_a_token_identity() {
    assert_ne!(
        token_id("binance", "BTC"),
        token_id("hyperliquid", "BTC"),
        "an unprovable sameness keeps separate identities"
    );
}

// ---- fixtures ------------------------------------------------------------

fn expiry() -> Expiry {
    Expiry::new(2_000, Some(1_800), Some(2_100)).expect("the expiry is valid")
}

fn binary_group(question: &str) -> OutcomeGroup {
    let seed = OutcomeGroupSeed {
        question: SmolStr::new(question),
        expiry: expiry(),
        topology: OutcomeTopology::Binary,
        settlement_asset: settlement_asset(),
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

fn risk_pool(first: &OutcomeGroup, second: &OutcomeGroup) -> OutcomeRiskPool {
    let conversion = OutcomeConversion {
        input: OutcomeLegRef {
            group: first.id.clone(),
            member: first.members[0].id.clone(),
            side: first.members[0].sides[0].clone(),
        },
        outputs: vec![OutcomeConversionOutput {
            leg: OutcomeLegRef {
                group: second.id.clone(),
                member: second.members[0].id.clone(),
                side: second.members[0].sides[0].clone(),
            },
            factor: market_participants_model::decimal::PositiveRatio::new(
                market_participants_model::decimal::Ratio::from(1_u8),
            )
            .expect("the factor is valid"),
        }],
    };
    OutcomeRiskPool::new(
        settlement_asset(),
        vec![first.id.clone(), second.id.clone()],
        vec![conversion],
    )
    .expect("the pool is valid")
}

/// The settlement asset of a fixture outcome relation.
///
/// A stablecoin is a venue-issued token, never a fiat currency.
fn settlement_asset() -> AssetId {
    AssetId::CryptoToken(token_id("binance", "USDC"))
}

/// A member of the group's own seed that the group does *not* carry.
fn ghost_member(group: &OutcomeGroup) -> OutcomeMember {
    let yes = OutcomeSide::new(&group.id, SmolStr::new_static("yes")).expect("the side is valid");
    OutcomeMember::new(&group.id, SmolStr::new_static("ghost"), vec![yes.id])
        .expect("the member is valid")
}

/// One outcome listing that trades `member`/`side` of `group`.
fn outcome_entry_of_leg(
    route: &VenueRoute,
    group: &OutcomeGroup,
    member: &OutcomeMember,
    side: &OutcomeSide,
    symbol: &str,
) -> CatalogEntry {
    let mut entry = outcome_entry(route, group, symbol);
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id.clone(),
        member.id.clone(),
        side.id.clone(),
    )))
    .expect("the outcome instrument is valid");
    entry.listing.instrument_id = instrument.id().clone();
    entry.snapshot.rules.common.quantity.unit = QuantityUnit::OutcomeTokens {
        group: group.id.clone(),
        member: member.id.clone(),
        side: side.id.clone(),
    };
    entry.instrument = instrument;
    entry
}

/// The same outcome listing with a declared native event.
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

fn index_terms(namespace: &str, code: &str) -> market_participants_model::asset::IndexTerms {
    market_participants_model::asset::IndexTerms {
        namespace: market_participants_model::identifier::IdentifierNamespace::new(namespace)
            .expect("the fixture namespace is valid"),
        code: IdentifierValue::new(code).expect("the fixture index code is valid"),
    }
}

fn price_reference_terms() -> market_participants_model::reference::PriceReferenceTerms {
    use market_participants_model::{
        identifier::{ReferenceSourceId, ReferenceTermsVersion},
        reference::{
            ReferenceAggregation, ReferenceObservation, ReferenceObservationKind,
            ReferenceSessionConvention, ReferenceSettlementConvention,
        },
    };
    market_participants_model::reference::PriceReferenceTerms {
        underlying: market_participants_model::asset::UnderlyingRef::Asset(settlement_asset()),
        quote_asset: currency("USD"),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new("binance").expect("the fixture source is valid"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the fixture version is valid"),
    }
}

/// A staged event across a second batch is published while a surviving listing
/// still declares it, and dropped by the batch that replaces that listing.
#[test]
fn a_staged_event_leaves_when_its_listing_does() {
    let (context, route) = context(&spot_route());
    let base = base_generation(&route);
    let group = binary_group("Will it rain?");
    let native = PolymarketEventId::new("event-1").expect("the event id is valid");
    let event = event_of(std::slice::from_ref(&group.id), &[], "event-1");
    context.staging().with(|staging| {
        let pending: &mut OutcomeRegistryPending = staging.pending_outcomes_mut();
        pending
            .register_group(group.clone(), &[])
            .expect("the group is valid");
        pending.register_event(event.clone(), None);
    });

    let published = base
        .clone()
        .with_pending(
            accept(
                &context,
                &base,
                CatalogBatch {
                    listings: vec![outcome_entry_with_event(
                        &spot_route(),
                        &group,
                        "RAIN-YES",
                        &native,
                    )],
                    outcome_groups: vec![group.clone()],
                    outcome_events: vec![event.clone()],
                    ..CatalogBatch::default()
                },
            )
            .expect("the batch is accepted"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");
    assert!(
        published.outcome_events().contains_key(&event.id),
        "a listing of the route declares the event, so it is published"
    );

    // The next batch of the same route replaces the listing, so nothing declares
    // the event any more and it leaves the current generation.
    let replacement = published
        .clone()
        .with_pending(
            accept(
                &context,
                &published,
                CatalogBatch {
                    listings: vec![outcome_entry(&spot_route(), &group, "RAIN-YES")],
                    outcome_groups: vec![group.clone()],
                    ..CatalogBatch::default()
                },
            )
            .expect("the replacement batch is accepted"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");

    assert!(
        !replacement.outcome_events().contains_key(&event.id),
        "no surviving listing declares the event any more"
    );
    assert!(
        replacement.outcome_groups().contains_key(&group.id),
        "the group the surviving listing trades stays reachable"
    );
}

fn event_of(groups: &[OutcomeGroupId], pools: &[OutcomeRiskPoolId], native: &str) -> OutcomeEvent {
    OutcomeEvent::new(
        ResolutionSourceId::new("polymarket").expect("the source is valid"),
        IdentifierValue::new(native).expect("the event value is valid"),
        groups.to_vec(),
        pools.to_vec(),
    )
    .expect("the event is valid")
}

fn outcome_entry(route: &VenueRoute, group: &OutcomeGroup, symbol: &str) -> CatalogEntry {
    let member = group.members[0].id.clone();
    let side = group.members[0].sides[0].clone();
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id.clone(),
        member.clone(),
        side.clone(),
    )))
    .expect("the outcome instrument is valid");
    let key = listing_key(route, symbol);
    let listing = VenueListing::new(
        key.clone(),
        instrument.id().clone(),
        Some(NativeSymbol::new(symbol).expect("the fixture symbol is valid")),
        Vec::new(),
        VenueListingIdentifiers::Polymarket(PolymarketListingIdentifiers {
            event_id: None,
            market_id: PolymarketMarketId::new("market-1").expect("the market id is valid"),
            condition_id: CtfConditionId::new("0x".to_owned() + &"1".repeat(64))
                .expect("the condition id is valid"),
            clob_token_id: CtfTokenId::new("12345").expect("the token id is valid"),
            // the protocol label of a fixture echoes the registry label of
            // the leg it trades, so a test that changes the leg keeps the echo.
            outcome_label: group.members[0]
                .sides
                .iter()
                .find_map(|id| group.side(id))
                .expect("the fixture member has a side")
                .label
                .clone(),
        }),
    )
    .expect("the outcome listing is valid");
    let snapshot = ListingSnapshot::new(
        key,
        TradingRules {
            common: CommonTradingRules {
                price: PriceRule {
                    quote_asset: settlement_asset(),
                    basis: PriceBasis::FractionOfCollateral,
                    tick: Tick::try_from(Quantity::from(1_u8)).expect("the tick is valid"),
                    minimum: None,
                    maximum: None,
                },
                quantity: QuantityRule {
                    unit: QuantityUnit::OutcomeTokens {
                        group: group.id.clone(),
                        member,
                        side,
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
            product: ProductTradingRules::Outcome(
                market_participants_model::listing::rules::OutcomeTradingRules {
                    order_book: true,
                    negative_risk_conversion: true,
                },
            ),
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

#[test]
fn an_outcome_listing_of_another_route_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&perp_route(), &group, "RAIN-YES");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an outcome listing of another route is refused");

    assert!(matches!(
        error,
        CatalogValidationError::ListingRouteMismatch { .. }
    ));
}

#[test]
fn a_spot_listing_with_a_plain_currency_pair_is_accepted() {
    let (context, route) = context(&spot_route());
    let pending = accept(
        &context,
        &base_generation(&route),
        batch_of(vec![entry(
            &spot_route(),
            spot(currency("EUR"), currency("USD")),
            "EURUSD",
        )]),
    )
    .expect("a plain currency pair is accepted");

    assert_eq!(pending.entries().len(), 1);
}

#[test]
fn the_context_keeps_the_registered_route() {
    let (context, _) = context(&spot_route());

    assert_eq!(context.venue_route(), &spot_route());
    assert_eq!(context.route().catalog_source().as_str(), "binance-spot");
}

#[test]
fn a_dropped_route_leaves_its_outcome_relations_reachable_for_the_others() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");
    let publisher = CatalogGenerationPublisher::new();
    let published = base_generation(&route)
        .with_pending(pending, &publisher)
        .expect("the display gate publishes the fixture generation");

    assert!(published.outcome_groups().contains_key(&group.id));
    let (reachable_groups, _) = published.reachable_outcomes();
    assert!(reachable_groups.contains(&group.id));
}

#[test]
fn an_asset_identity_is_never_shared_between_two_kinds() {
    let currency_asset = currency("USD");
    let token_asset = AssetId::CryptoToken(token_id("binance", "USD"));

    assert_ne!(currency_asset, token_asset);
    assert!(currency_asset.is_settlement_asset());
    assert!(token_asset.is_settlement_asset());
    assert_ne!(format!("{currency_asset}"), format!("{token_asset}"));
}

#[test]
fn a_listing_id_is_rendered_with_its_own_prefix() {
    let key = listing_key(&spot_route(), "BTCUSDT");

    assert!(key.listing_id.as_str().starts_with("listing-id/v1/"));
    assert_eq!(key.listing_id.as_str().len(), "listing-id/v1/".len() + 64);
}

#[test]
fn a_route_normalization_of_one_kind_does_not_leak_into_another() {
    let rules = vec![
        market_participants_model::routing::IdentifierNormalizationRule::new(
            ListingIdentifierKind::EquityLocalCode,
            None,
            market_participants_model::routing::SymbolNormalization::AsciiCaseInsensitive,
        ),
    ];

    assert_eq!(
        market_participants_model::routing::normalization_mode(
            &rules,
            ListingIdentifierKind::Symbol,
            None
        )
        .expect("the mode resolves"),
        market_participants_model::routing::SymbolNormalization::Exact
    );
}
#[test]
fn a_batch_publishes_its_own_native_group_lookup() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let market = PolymarketMarketId::new("market-1").expect("the market id is valid");
    let lookup = OutcomeGroupIdentifier::PolymarketMarket(market);
    context.staging().with(|staging| {
        let pending: &mut OutcomeRegistryPending = staging.pending_outcomes_mut();
        pending
            .register_group(group.clone(), std::slice::from_ref(&lookup))
            .expect("the group is valid");
    });
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");

    assert_eq!(pending.native_groups().get(&lookup), Some(&group.id));
    let snapshot = pending.into_snapshot();
    assert_eq!(
        snapshot.group_by_native_identifier(&lookup),
        Some(&group.id),
        "the lookup belongs to the route that published it"
    );
}

#[test]
fn a_group_of_another_route_has_no_lookup_on_this_route() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let market = PolymarketMarketId::new("market-1").expect("the market id is valid");
    let lookup = OutcomeGroupIdentifier::PolymarketMarket(market);

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect("a batch without lookups is accepted");

    let snapshot = pending.into_snapshot();
    assert!(
        snapshot.group_by_native_identifier(&lookup).is_none(),
        "a group without a registered native lookup resolves to nothing"
    );
}

#[test]
fn a_batch_publishes_its_own_native_event_lookup() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let native = PolymarketEventId::new("event-1").expect("the event id is valid");
    let event = OutcomeEvent::new(
        ResolutionSourceId::new("polymarket").expect("the source is valid"),
        IdentifierValue::new("event-1").expect("the event value is valid"),
        vec![group.id.clone()],
        Vec::new(),
    )
    .expect("the event is valid");
    context.staging().with(|staging| {
        let pending: &mut OutcomeRegistryPending = staging.pending_outcomes_mut();
        pending
            .register_group(group.clone(), &[])
            .expect("the group is valid");
        pending.register_event(event.clone(), Some(&native));
    });

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            outcome_groups: vec![group],
            outcome_events: vec![event.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");

    let snapshot = pending.into_snapshot();
    assert_eq!(
        snapshot.event_by_native_identifier(&native),
        Some(&event.id),
        "the event lookup belongs to the route that published it"
    );
}
// ---- outcome price terms -----------------------------------------

#[test]
fn an_outcome_listing_that_quotes_another_asset_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let mut listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    listing.snapshot.rules.common.price.quote_asset = currency("EUR");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an outcome listing must quote its group's settlement asset");

    assert_eq!(
        error,
        CatalogValidationError::OutcomePriceQuoteMismatch {
            listing: Box::new(listing.listing.key),
            expected: Box::new(group.settlement_asset),
            found: Box::new(currency("EUR")),
        }
    );
}

#[test]
fn an_outcome_listing_with_an_unsupported_price_basis_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let mut listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    listing.snapshot.rules.common.price.basis = PriceBasis::PerAssetUnit;

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an outcome token is priced as a fraction of its collateral");

    assert_eq!(
        error,
        CatalogValidationError::OutcomePriceBasisUnsupported {
            listing: Box::new(listing.listing.key),
            basis: Box::new(PriceBasis::PerAssetUnit),
        }
    );
}

#[test]
fn an_outcome_listing_that_quotes_its_settlement_asset_is_accepted() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    assert_eq!(
        listing.snapshot.rules.common.price.quote_asset, group.settlement_asset,
        "the fixture is the correct configuration"
    );
    assert_eq!(
        listing.snapshot.rules.common.price.basis,
        PriceBasis::FractionOfCollateral
    );

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect("a correctly quoted outcome listing is accepted");

    assert_eq!(pending.entries().len(), 1);
}

#[test]
fn an_outcome_listing_may_take_its_group_from_the_previous_generation() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let base = base_generation(&route)
        .with_pending(
            accept(
                &context,
                &base_generation(&route),
                CatalogBatch {
                    outcome_groups: vec![group.clone()],
                    ..CatalogBatch::default()
                },
            )
            .expect("the group is published first"),
            &CatalogGenerationPublisher::new(),
        )
        .expect("the display gate publishes the fixture generation");
    let mut listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    listing.snapshot.rules.common.price.quote_asset = currency("EUR");

    let error = accept(
        &context,
        &base,
        CatalogBatch {
            listings: vec![listing],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a group of the previous generation answers the check too");

    assert!(matches!(
        error,
        CatalogValidationError::OutcomePriceQuoteMismatch { .. }
    ));
}

// ---- outcome closure against the group, events and references ------------

/// Probe 2: a batch whose listing trades a member its own group does not carry
/// is refused as a whole. The check is against the group's terms, never against
/// the batch's own listings.
#[test]
fn an_outcome_listing_of_a_leg_outside_its_group_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let ghost = ghost_member(&group);
    let listing = outcome_entry_of_leg(&spot_route(), &group, &ghost, &group.sides[0], "GHOST-YES");

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect_err("the group does not carry the member the listing trades");

    assert_eq!(
        error,
        CatalogValidationError::OutcomeListingLegUnknown {
            listing: Box::new(listing.listing.key),
            group: group.id.clone(),
            member: ghost.id,
            side: group.sides[0].id.clone(),
        }
    );
}

/// A listing that trades a member the group carries but a side that member does
/// not is refused too.
#[test]
fn an_outcome_listing_of_a_side_outside_its_member_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let mut listing = outcome_entry(&spot_route(), &group, "RAIN-YES");
    let side = group.sides[1].id.clone();
    let member = group.members[0].id.clone();
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id.clone(),
        member.clone(),
        side.clone(),
    )))
    .expect("the outcome instrument is valid");
    listing.listing.instrument_id = instrument.id().clone();
    listing.snapshot.rules.common.quantity.unit = QuantityUnit::OutcomeTokens {
        group: group.id.clone(),
        member,
        side,
    };
    listing.instrument = instrument;
    // The fixture keeps its protocol outcome label in step with the leg it now
    // states, so this test exercises the structural leg check rather than the display gate's
    // label echo, which has its own test.
    let VenueListingIdentifiers::Polymarket(identifiers) = &mut listing.listing.identifiers else {
        panic!("the fixture is a Polymarket listing");
    };
    identifiers.outcome_label = group
        .side(&group.sides[1].id)
        .expect("the fixture group carries the side")
        .label
        .clone();

    // The group does carry both sides, so this batch is accepted: the check must
    // not refuse a leg that exists.
    accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("both sides belong to the member");

    // A side of another seed of the same question is not a side of this group.
    let foreign_seed = OutcomeGroupSeed {
        question: SmolStr::new_static("Will it rain elsewhere?"),
        expiry: expiry(),
        topology: OutcomeTopology::Binary,
        settlement_asset: settlement_asset(),
        resolution: ResolutionRule::FixedAt { at: 1_900 },
        fraction_rule: SettlementFractionRule::Binary,
    };
    let foreign = OutcomeSide::new(&foreign_seed.seed_id(), SmolStr::new_static("yes"))
        .expect("the side is valid");
    let listing = outcome_entry_of_leg(
        &spot_route(),
        &group,
        &group.members[0],
        &foreign,
        "RAIN-YES",
    );

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a side of another group is not a side of this one");

    assert_eq!(
        error,
        CatalogValidationError::OutcomeListingLegUnknown {
            listing: Box::new(listing.listing.key),
            group: group.id.clone(),
            member: group.members[0].id.clone(),
            side: foreign.id,
        }
    );
}

/// Probe 3: a listing that declares a native event no batch and no published
/// generation carries is refused with the exact field that failed.
#[test]
fn an_outcome_listing_of_an_unknown_native_event_is_refused() {
    let (context, route) = context(&spot_route());
    let group = binary_group("Will it rain?");
    let declared = PolymarketEventId::new("event-1").expect("the event id is valid");
    let listing = outcome_entry_with_event(&spot_route(), &group, "RAIN-YES", &declared);

    let error = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing.clone()],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a listing must not declare an event its batch does not carry");

    assert_eq!(
        error,
        CatalogValidationError::ListingEventUnknown {
            listing: Box::new(listing.listing.key),
            event: declared,
        }
    );
}

/// Probe 3, positive half: a batch that carries the declared event is accepted
/// and the route publishes the event index
/// `(VenueRoute, PolymarketEventId) -> OutcomeEventId -> [OutcomeGroupId]`.
#[test]
fn a_batch_publishes_its_own_event_group_index() {
    let (context, route) = context(&spot_route());
    let first = binary_group("Will it rain?");
    let second = binary_group("Will it snow?");
    let native = PolymarketEventId::new("event-7").expect("the event id is valid");
    let event = event_of(&[first.id.clone(), second.id.clone()], &[], "event-7");
    context.staging().with(|staging| {
        let pending: &mut OutcomeRegistryPending = staging.pending_outcomes_mut();
        pending
            .register_group(first.clone(), &[])
            .expect("the group is valid");
        pending
            .register_group(second.clone(), &[])
            .expect("the group is valid");
        pending.register_event(event.clone(), None);
    });
    let listing = outcome_entry_with_event(&spot_route(), &first, "RAIN-YES", &native);

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![first, second],
            outcome_events: vec![event.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch carries the event its listing declares");

    let native_event_groups = pending.native_event_groups().clone();
    let snapshot = pending.into_snapshot();
    assert_eq!(
        snapshot.event_by_native_identifier(&native),
        Some(&event.id),
        "the route indexes the event its listing declares"
    );
    assert_eq!(
        snapshot.event_groups(&native),
        Some(event.groups.as_slice()),
        "the event-level index carries the groups of the event"
    );
    assert_eq!(
        native_event_groups.get(&native),
        Some(&event.groups),
        "the validated batch exposes the same event index"
    );
}

/// Probe 15: a staged event the accepted batch never references stays
/// unpublished, and one the batch does reference is published.
#[test]
fn a_staged_event_without_a_referencing_batch_is_not_published() {
    let (empty_context, empty_route) = context(&spot_route());
    let orphan_group = binary_group("Will it rain?");
    let orphan = event_of(std::slice::from_ref(&orphan_group.id), &[], "orphan-event");
    empty_context.staging().with(|staging| {
        staging
            .pending_outcomes_mut()
            .register_event(orphan.clone(), None);
    });

    let pending = accept(
        &empty_context,
        &base_generation(&empty_route),
        CatalogBatch::default(),
    )
    .expect("an empty batch is a successful snapshot");
    let registry = pending.pending_outcomes().publish(
        &OutcomeRegistry::new(),
        &ReferencedTerms::collect(&pending, &base_generation(&empty_route)),
    );

    assert!(
        registry.event(&orphan.id).is_none(),
        "an event no published listing references must not become visible"
    );

    // A batch that carries an event but publishes no listing for it is refused:
    // the batch declares events, only a listing keeps them alive.
    let (referenced_context, referenced_route) = context(&spot_route());
    let group = binary_group("Will it snow?");
    let referenced = event_of(std::slice::from_ref(&group.id), &[], "referenced-event");
    referenced_context.staging().with(|staging| {
        staging
            .pending_outcomes_mut()
            .register_event(referenced.clone(), None);
    });
    let pending = accept(
        &referenced_context,
        &base_generation(&referenced_route),
        CatalogBatch {
            outcome_groups: vec![group.clone()],
            outcome_events: vec![referenced.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");
    let registry = pending.pending_outcomes().publish(
        &OutcomeRegistry::new(),
        &ReferencedTerms::collect(&pending, &base_generation(&referenced_route)),
    );

    assert!(
        registry.event(&referenced.id).is_none(),
        "a batch event no listing declares must not become visible either"
    );

    // The same event named by a listing of the batch is published.
    let (named_context, named_route) = context(&spot_route());
    let native = PolymarketEventId::new("referenced-event").expect("the event id is valid");
    named_context.staging().with(|staging| {
        staging
            .pending_outcomes_mut()
            .register_event(referenced.clone(), None);
    });
    let listing = outcome_entry_with_event(&spot_route(), &group, "SNOW-YES", &native);
    let pending = accept(
        &named_context,
        &base_generation(&named_route),
        CatalogBatch {
            listings: vec![listing],
            outcome_groups: vec![group],
            outcome_events: vec![referenced.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");
    let registry = pending.pending_outcomes().publish(
        &OutcomeRegistry::new(),
        &ReferencedTerms::collect(&pending, &base_generation(&named_route)),
    );

    assert!(
        registry.event(&referenced.id).is_some(),
        "an event a listing of the batch names reaches the new registry"
    );
}

/// A staged reference the batch never tracks stays unpublished, and one the
/// batch does track is published.
#[test]
fn only_the_references_a_batch_tracks_are_published() {
    let (context, route) = context(&spot_route());
    let tracked = index_terms("sp", "500");
    let untracked = index_terms("sp", "600");
    let tracked_id = market_participants_model::asset::IndexId::of_terms(&tracked);
    let untracked_id = market_participants_model::asset::IndexId::of_terms(&untracked);
    let price = price_reference_terms();
    let price_id = market_participants_model::reference::PriceReferenceId::of_terms(&price);
    let untracked_price = market_participants_model::reference::PriceReferenceTerms {
        basis: PriceBasis::FractionOfCollateral,
        ..price.clone()
    };
    let untracked_price_id =
        market_participants_model::reference::PriceReferenceId::of_terms(&untracked_price);
    context.staging().with(|staging| {
        let pending = staging.pending_references_mut();
        pending.register_index(tracked.clone());
        pending.register_index(untracked.clone());
        pending
            .register_price_reference(price.clone())
            .expect("the price reference terms are valid");
        pending
            .register_price_reference(untracked_price.clone())
            .expect("the other price reference terms are valid");
    });

    let pending = accept(
        &context,
        &base_generation(&route),
        CatalogBatch {
            listings: vec![btc_entry(&spot_route())],
            ..CatalogBatch::default()
        },
    )
    .expect("the batch is accepted");

    // The batch tracks exactly two of the four staged references. The listings
    // happen to be spot listings, so the tracked set a derivative would produce is
    // stated explicitly here and the whole publication path is checked with it.
    let mut referenced = ReferencedTerms::collect(&pending, &base_generation(&route));
    referenced.indices.insert(tracked_id.clone());
    referenced.price_references.insert(price_id.clone());
    let registry = pending.pending_references().publish(
        &crate::catalog::registry::reference::ReferenceRegistry::new(),
        &referenced,
    );

    assert!(
        registry.index(&tracked_id).is_some(),
        "a reference the batch tracks is published"
    );
    assert!(
        registry.price_reference(&price_id).is_some(),
        "the price reference the batch tracks is published"
    );
    assert!(
        registry.index(&untracked_id).is_none(),
        "a staged reference the batch never tracks must not become visible"
    );
    assert!(
        registry.price_reference(&untracked_price_id).is_none(),
        "a staged price reference the batch never tracks must not become visible"
    );

    let published_references = pending.pending_references().publish(
        &crate::catalog::registry::reference::ReferenceRegistry::new(),
        &referenced,
    );
    assert!(
        published_references.index(&tracked_id).is_some()
            && published_references.price_reference(&price_id).is_some(),
        "the tracked references reach the generation's registry map"
    );
    assert!(
        published_references.index(&untracked_id).is_none()
            && published_references
                .price_reference(&untracked_price_id)
                .is_none(),
        "the untracked references never reach a generation's registry map"
    );
}

/// The collector reads the index, external reference and price reference a
/// derivative tracks, so a reference only reaches a generation when an
/// instrument of the batch uses it.
#[test]
fn the_reference_collector_reads_a_derivative_underlying() {
    use market_participants_model::{asset::DerivativeUnderlying, reference::PriceReferenceId};

    let index = index_terms("sp", "500");
    let index_id = market_participants_model::asset::IndexId::of_terms(&index);
    let price_id = PriceReferenceId::of_terms(&price_reference_terms());
    let underlying = DerivativeUnderlying::new(
        market_participants_model::asset::UnderlyingRef::Index(index_id.clone()),
        price_id.clone(),
    );
    let mut referenced = ReferencedTerms::default();
    crate::catalog::batch::collect_derivative(&underlying, &mut referenced);

    assert!(
        referenced.indices.contains(&index_id),
        "the index a derivative tracks is referenced"
    );
    assert!(
        referenced.price_references.contains(&price_id),
        "the price reference a derivative is quoted through is referenced"
    );

    let external_terms = market_participants_model::reference::ExternalReferenceTerms {
        issuer: market_participants_model::identifier::IdentifierNamespace::new("hyperliquid")
            .expect("the fixture issuer is valid"),
        reference: IdentifierValue::new("oracle-1").expect("the fixture reference is valid"),
        quote_asset: currency("USD"),
        basis: PriceBasis::PerAssetUnit,
        observation: price_reference_terms().observation,
        aggregation: price_reference_terms().aggregation,
        session: price_reference_terms().session,
        settlement: price_reference_terms().settlement,
        schema_version: price_reference_terms().schema_version,
    };
    let external_id = external_terms
        .reference_id()
        .expect("the external reference terms are valid");
    let underlying = DerivativeUnderlying::new(
        market_participants_model::asset::UnderlyingRef::External(external_id.clone()),
        price_id,
    );
    let mut other = ReferencedTerms::default();
    crate::catalog::batch::collect_derivative(&underlying, &mut other);

    assert!(
        other.external_references.contains(&external_id),
        "the external reference a derivative tracks is referenced"
    );
    assert!(
        other.indices.is_empty(),
        "an external underlying references no index"
    );
}
