//! The catalog source registry: candidate updates and route bindings.
//!
//! A whole candidate update is accepted or refused as one: a route that provides
//! a catalog must bind exactly one registered source, a route without one must
//! bind none, and a route that is absent from the update is revoked.

use market_participants_model::{
    error::ModelError,
    identifier::NativeSymbol,
    instrument::InstrumentKind,
    listing::{
        native::ListingIdentifier,
        venue::{BinanceProduct, HyperliquidAssetProduct, OkxInstrumentType},
    },
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
};

use crate::catalog::{
    error::CatalogError,
    source::{
        CatalogRouteRegistration, CatalogRouteRequirements, CatalogRouteUpdate,
        CatalogSourceBinding, CatalogSourceId, CatalogSourceRegistry, RouteInputRevision,
        RouteRegistration,
    },
    test_support::{
        self, SPOT_SOURCE, binding, case_insensitive_symbols, listing, non_catalog_registration,
        non_catalog_requirements, perp_route, registration, requirements, spot_route,
    },
};

fn update(
    sources: impl IntoIterator<Item = CatalogSourceBinding>,
    routes: impl IntoIterator<Item = CatalogRouteRegistration>,
) -> CatalogRouteUpdate {
    CatalogRouteUpdate::new(sources, routes)
}

#[test]
fn an_empty_update_revokes_every_route() {
    let mut registry = CatalogSourceRegistry::new();
    registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the first update is accepted");
    assert!(registry.is_registered(&spot_route()));

    registry
        .publish(update([], []), &requirements(&[]))
        .expect("an empty update revokes everything");

    assert!(!registry.is_registered(&spot_route()));
    assert!(registry.route_entry(&spot_route()).is_none());
    assert!(registry.source_of(&spot_route()).is_none());
}

#[test]
fn a_route_absent_from_the_candidate_is_revoked() {
    let mut registry = CatalogSourceRegistry::new();
    registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the first update is accepted");

    registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&perp_route(), SPOT_SOURCE)],
            ),
            &requirements(&[perp_route()]),
        )
        .expect("the second update is accepted");

    assert!(!registry.is_registered(&spot_route()));
    assert!(registry.is_registered(&perp_route()));
}

#[test]
fn a_catalog_route_must_bind_exactly_one_source() {
    let registry = CatalogSourceRegistry::new();
    let error = registry
        .plan(
            update(
                [],
                [CatalogRouteRegistration::without_catalog(
                    spot_route(),
                    case_insensitive_symbols(),
                    vec![CapabilityId::Tickers],
                    RouteInputRevision::describe().build(),
                )],
            ),
            &requirements(&[spot_route()]),
        )
        .expect_err("a catalog route without a source is refused");

    assert!(matches!(error, CatalogError::MissingCatalogSource { .. }));
}

#[test]
fn a_route_without_a_catalog_must_not_bind_a_catalog_source() {
    let registry = CatalogSourceRegistry::new();
    let error = registry
        .plan(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [CatalogRouteRegistration::catalog(
                    spot_route(),
                    CatalogSourceId::new(SPOT_SOURCE).expect("the source id is valid"),
                    case_insensitive_symbols(),
                    vec![CapabilityId::MarketCatalog],
                    RouteInputRevision::describe().build(),
                )],
            ),
            &non_catalog_requirements(&[spot_route()]),
        )
        .expect_err("a route without a catalog must not bind a catalog source");

    assert!(matches!(
        error,
        CatalogError::CatalogSourceOnNonCatalogRoute { .. }
    ));
}

#[test]
fn a_route_without_a_catalog_is_registered_but_not_fetchable() {
    let mut registry = CatalogSourceRegistry::new();
    let route_set = registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [
                    registration(&spot_route(), SPOT_SOURCE),
                    non_catalog_registration(&perp_route()),
                ],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the mixed route set is accepted");

    assert_eq!(route_set.catalog_routes(), &[spot_route()]);
    assert!(route_set.route(&perp_route()).is_some());
    assert!(!registry.is_registered(&perp_route()));
    assert!(registry.registered_route(&perp_route()).is_none());
    assert!(matches!(
        registry.require_registered_route(&perp_route()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

#[test]
fn a_source_id_must_be_unique() {
    let registry = CatalogSourceRegistry::new();
    let error = registry
        .plan(
            update(
                [
                    binding(SPOT_SOURCE, listing(Vec::new())),
                    binding(SPOT_SOURCE, listing(Vec::new())),
                ],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect_err("a source id is registered once");

    assert!(matches!(
        error,
        CatalogError::DuplicateCatalogSourceId { .. }
    ));
}

#[test]
fn a_route_cannot_bind_an_unregistered_source() {
    let registry = CatalogSourceRegistry::new();
    let error = registry
        .plan(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), "binance-margin")],
            ),
            &requirements(&[spot_route()]),
        )
        .expect_err("the source must exist");

    assert!(matches!(
        error,
        CatalogError::UnregisteredCatalogSource { .. }
    ));
}

#[test]
fn a_refused_candidate_leaves_both_maps_untouched() {
    let mut registry = CatalogSourceRegistry::new();
    registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the first update is accepted");

    let error = registry
        .plan(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), "binance-margin")],
            ),
            &requirements(&[spot_route()]),
        )
        .expect_err("the candidate names a source that is not in it");
    assert!(matches!(
        error,
        CatalogError::UnregisteredCatalogSource { .. }
    ));

    assert!(registry.is_registered(&spot_route()));
    assert_eq!(
        registry
            .require_source(&spot_route())
            .expect("the registered source survives")
            .id()
            .as_str(),
        SPOT_SOURCE
    );
}

#[test]
fn a_route_registration_exposes_the_capability_verdict_of_the_bridge() {
    let requirements = CatalogRouteRequirements::new([(spot_route(), CapabilityId::MarketCatalog)]);

    assert!(requirements.requires_catalog_source(&spot_route()));
    assert_eq!(requirements.len(), 1);
    assert!(!requirements.requires_catalog_source(&perp_route()));

    let without_a_catalog = CatalogRouteRequirements::new([(spot_route(), CapabilityId::Tickers)]);
    assert!(without_a_catalog.is_empty());
}

#[test]
fn a_route_registration_keeps_the_rule_set_it_was_accepted_under() {
    let mut registry = CatalogSourceRegistry::new();
    let candidate = registration(&spot_route(), SPOT_SOURCE);
    let set = registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [candidate.clone()],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the update is accepted");
    let registered = set
        .registered_route(&spot_route())
        .expect("the route is registered");

    assert_eq!(
        registered
            .registration()
            .normalization_mode(ListingIdentifierKind::Symbol),
        SymbolNormalization::AsciiCaseInsensitive
    );
    assert_eq!(
        registered.route_revision(),
        candidate.route_revision(),
        "a registered route carries the revision of the declaration it was accepted under"
    );
    assert_eq!(registered.identifier_normalization().len(), 2);
}

#[test]
fn an_unsorted_normalization_rule_set_is_refused() {
    let registry = CatalogSourceRegistry::new();
    let rules = vec![
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::BinanceInstrument,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        ),
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        ),
    ];
    let error = registry
        .plan(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [CatalogRouteRegistration::catalog(
                    spot_route(),
                    CatalogSourceId::new(SPOT_SOURCE).expect("the source id is valid"),
                    rules,
                    vec![CapabilityId::MarketCatalog],
                    RouteInputRevision::describe().build(),
                )],
            ),
            &requirements(&[spot_route()]),
        )
        .expect_err("rules must be sorted and unique");

    assert!(matches!(
        error,
        CatalogError::InconsistentRouteRegistration { .. }
    ));
}

#[test]
fn a_source_is_never_reached_through_an_unregistered_route() {
    let mut registry = CatalogSourceRegistry::new();
    registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the update is accepted");

    assert!(registry.require_source(&spot_route()).is_ok());
    assert!(matches!(
        registry.require_source(&perp_route()),
        Err(CatalogError::RouteNotRegistered { .. })
    ));
}

#[test]
fn a_route_registration_of_two_catalog_routes_keeps_both() {
    let mut registry = CatalogSourceRegistry::new();
    let second = "binance-perp";
    let set = registry
        .publish(
            update(
                [
                    binding(SPOT_SOURCE, listing(Vec::new())),
                    binding(second, listing(Vec::new())),
                ],
                [
                    registration(&spot_route(), SPOT_SOURCE),
                    registration(&perp_route(), second),
                ],
            ),
            &requirements(&[spot_route(), perp_route()]),
        )
        .expect("both routes are accepted");

    assert_eq!(set.catalog_routes().len(), 2);
    assert_eq!(set.sources().len(), 2);
    assert_eq!(
        set.source_of(&perp_route())
            .map(|binding| binding.id().as_str()),
        Some(second)
    );
}

#[test]
fn a_candidate_update_that_moves_only_a_revision_is_accepted() {
    let mut registry = CatalogSourceRegistry::new();
    let before = registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [registration(&spot_route(), SPOT_SOURCE)],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the update is accepted");
    let first_revision = before
        .registered_route(&spot_route())
        .expect("the route is registered")
        .route_revision();

    let after = registry
        .publish(
            update(
                [binding(SPOT_SOURCE, listing(Vec::new()))],
                [CatalogRouteRegistration::catalog(
                    spot_route(),
                    CatalogSourceId::new(SPOT_SOURCE).expect("the source id is valid"),
                    case_insensitive_symbols(),
                    vec![CapabilityId::MarketCatalog],
                    RouteInputRevision::describe().catalog_descriptor(9).build(),
                )],
            ),
            &requirements(&[spot_route()]),
        )
        .expect("the revision update is accepted");

    assert_ne!(
        after
            .registered_route(&spot_route())
            .expect("the route is registered")
            .route_revision(),
        first_revision
    );
}

#[test]
fn a_source_binding_carries_its_id() {
    let binding = test_support::binding(SPOT_SOURCE, listing(Vec::new()));

    assert_eq!(binding.id().as_str(), SPOT_SOURCE);
    assert_eq!(binding.id().to_smol_str(), SPOT_SOURCE);
    assert_eq!(format!("{}", binding.id()), SPOT_SOURCE);
    assert!(format!("{binding:?}").contains(SPOT_SOURCE));
}

/// One route registration built from an explicit rule set.
fn registered(normalization: Vec<IdentifierNormalizationRule>) -> RouteRegistration {
    RouteRegistration {
        normalization,
        capabilities: vec![CapabilityId::MarketCatalog],
        revision: RouteInputRevision::describe().build(),
    }
}

/// A rule at one identifier and product granularity.
fn rule(
    identifier: ListingIdentifierKind,
    product: Option<IdentifierNormalizationProduct>,
    mode: SymbolNormalization,
) -> IdentifierNormalizationRule {
    IdentifierNormalizationRule::new(identifier, product, mode)
}

/// A product-qualified rule is the one a lookup at that product resolves, and a
/// lookup at a product the route never declared falls back to the route-wide
/// rule — the precedence the model documents.
#[test]
fn a_product_rule_wins_and_a_route_wide_rule_is_the_fallback() {
    let registration = registered(vec![
        rule(
            ListingIdentifierKind::BinanceInstrument,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        ),
        rule(
            ListingIdentifierKind::BinanceInstrument,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Spot,
            )),
            SymbolNormalization::Exact,
        ),
    ]);

    assert_eq!(
        registration.normalization_mode_at(
            ListingIdentifierKind::BinanceInstrument,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Spot
            ))
        ),
        Ok(SymbolNormalization::Exact),
        "the product-qualified rule wins"
    );
    assert_eq!(
        registration.normalization_mode_at(
            ListingIdentifierKind::BinanceInstrument,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Perpetual
            ))
        ),
        Ok(SymbolNormalization::AsciiCaseInsensitive),
        "a product with no rule of its own falls back to the route-wide rule"
    );
    assert_eq!(
        registration.normalization_mode_at(ListingIdentifierKind::OkxInstrument, None),
        Ok(SymbolNormalization::Exact),
        "an identifier the route declared no rule for is exact"
    );
}

/// A product that does not belong to the identifier kind is reported instead of
/// being answered with another product's mode.
#[test]
fn a_product_of_another_identifier_family_is_a_normalization_mismatch() {
    let registration = registered(vec![rule(
        ListingIdentifierKind::BinanceInstrument,
        Some(IdentifierNormalizationProduct::Binance(
            BinanceProduct::Spot,
        )),
        SymbolNormalization::AsciiCaseInsensitive,
    )]);

    assert_eq!(
        registration.normalization_mode_at(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Spot
            ))
        ),
        Err(ModelError::IdentifierNormalizationMismatch {
            kind: ListingIdentifierKind::Symbol
        })
    );
}

/// The bare kind-only lookup folds under the product the route declared for that
/// kind, so a product-qualified declaration is not a dead rule; and the typed
/// identifier lookup folds under the product the variant itself carries.
#[test]
fn the_symbol_kind_is_folded_under_the_product_its_route_declared() {
    let registration = registered(vec![
        rule(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Spot,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        ),
        rule(
            ListingIdentifierKind::BinanceInstrument,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Spot,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        ),
    ]);
    let symbol = NativeSymbol::new("btcusdt").expect("the fixture symbol is valid");

    assert_eq!(
        registration.normalization_mode(ListingIdentifierKind::Symbol),
        SymbolNormalization::AsciiCaseInsensitive
    );
    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::Symbol(symbol.clone())),
        SymbolNormalization::AsciiCaseInsensitive
    );
    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::BinanceInstrument {
            symbol,
            product: BinanceProduct::Spot
        }),
        SymbolNormalization::AsciiCaseInsensitive
    );
    assert_eq!(
        registration
            .normalization_mode(ListingIdentifierKind::Symbol)
            .apply("btcusdt")
            .expect("the ASCII folding never fails"),
        "BTCUSDT"
    );
}

/// A Hyperliquid asset is indexed by the product its own variant names: a HIP-3
/// perpetual is exact, and a spot asset the route declared no rule for is exact
/// too rather than another product's fold.
#[test]
fn a_hyperliquid_asset_is_normalized_at_its_own_product() {
    let registration = registered(vec![rule(
        ListingIdentifierKind::HyperliquidAsset,
        Some(IdentifierNormalizationProduct::Hyperliquid(
            HyperliquidAssetProduct::Perpetual,
        )),
        SymbolNormalization::Exact,
    )]);

    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::HyperliquidAsset {
            scope: test_support::perp_route().scope,
            product: HyperliquidAssetProduct::Perpetual,
            asset_id: market_participants_model::listing::venue::HyperliquidAssetId(110_000),
        }),
        SymbolNormalization::Exact
    );
    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::HyperliquidAsset {
            scope: test_support::perp_route().scope,
            product: HyperliquidAssetProduct::Spot,
            asset_id: market_participants_model::listing::venue::HyperliquidAssetId(150),
        }),
        SymbolNormalization::Exact,
        "an undeclared product must never inherit another product's mode"
    );
    assert!(
        registration
            .normalization_mode(ListingIdentifierKind::HyperliquidAsset)
            .apply("xyz:abc")
            .is_ok_and(|value| value == "xyz:abc"),
        "an exact rule keeps the case it was given"
    );
}

/// A route that declared two products for one kind has no single product of its
/// own, so the bare lookup answers the route-wide rule instead of one of them.
#[test]
fn an_ambiguous_kind_answers_the_route_wide_rule() {
    let registration = registered(vec![
        rule(
            ListingIdentifierKind::Symbol,
            None,
            SymbolNormalization::Exact,
        ),
        rule(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Spot,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        ),
        rule(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Perpetual,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        ),
    ]);

    assert_eq!(
        registration.normalization_mode(ListingIdentifierKind::Symbol),
        SymbolNormalization::Exact
    );
}

/// The OKX instruments a route declares are folded under the instrument type its
/// own variant names.
#[test]
fn an_okx_instrument_is_folded_under_its_own_instrument_type() {
    let registration = registered(vec![
        rule(
            ListingIdentifierKind::OkxInstrument,
            Some(IdentifierNormalizationProduct::Okx(OkxInstrumentType::Spot)),
            SymbolNormalization::Exact,
        ),
        rule(
            ListingIdentifierKind::OkxInstrument,
            Some(IdentifierNormalizationProduct::Okx(
                OkxInstrumentType::Perpetual,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        ),
    ]);
    let instrument_id = NativeSymbol::new("btc-usdt-swap").expect("the fixture id is valid");

    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::OkxInstrument {
            instrument_id: instrument_id.clone(),
            instrument_type: OkxInstrumentType::Spot
        }),
        SymbolNormalization::Exact
    );
    assert_eq!(
        registration.identifier_normalization_mode(&ListingIdentifier::OkxInstrument {
            instrument_id,
            instrument_type: OkxInstrumentType::Perpetual
        }),
        SymbolNormalization::AsciiCaseInsensitive
    );
}
