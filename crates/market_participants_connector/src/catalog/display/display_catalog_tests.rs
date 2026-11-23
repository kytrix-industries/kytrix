use market_participants_model::{
    asset::{
        AssetId, ContractAddress, CryptoTokenId, CryptoTokenOrigin, CryptoTokenTerms, CurrencyId,
        CurrencyTerms, DerivativeUnderlying, Iso4217Code, SecurityForm, SecurityIssuerId,
        SecurityTerms, ShareClassId, ShareClassTerms, TokenReference, UnderlyingRef,
    },
    decimal::{Fraction, Price, Quantity, Ratio, Tick, UnixMillis},
    display::{
        asset::AssetDisplayCode,
        values::{DisplayFormatError, InvalidTextReason},
    },
    identifier::{
        ChainId, ContractCode, CtfConditionId, CtfTokenId, EquityLocalCode, IdentifierNamespace,
        IdentifierValue, MarketParticipantSeriesId, Mic, NativeSymbol, OccSymbol,
        PolymarketMarketId, ReferenceSourceId, ReferenceTermsVersion, TokenIssuerId,
    },
    instrument::{
        FutureTerms, Instrument, InstrumentKind, InstrumentTerms, OptionTerms, OutcomeTerms,
        PerpetualTerms, SpotTerms,
    },
    listing::{
        continuity::ListingContinuityKey,
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        identifiers::{
            BinanceListingIdentifiers, EquityListingIdentifiers, HyperliquidListingIdentifiers,
            PolymarketListingIdentifiers, VenueListingIdentifiers,
        },
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, MarginMode, MarginProfile, OptionTradingRules, PriceRule,
            ProductTradingRules, QuantityRule, TradingRules, VenueTradingPolicy,
        },
        venue::{BinanceProduct, HyperliquidAssetId, HyperliquidTokenId, PriceBasis},
    },
    market::units::QuantityUnit,
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeMember, OutcomeMemberId, OutcomeSide, OutcomeSideId,
        OutcomeTopology, ResolutionRule, SettlementFractionRule,
    },
    reference::{
        PriceReferenceId, PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
        ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    settlement::{
        ContractSize, ContractUnit, DeliveryDirection, DeliveryLeg, DeliveryMode,
        DerivativePnlModel, ExerciseStyle, Expiry, InstrumentId, OptionRight, StrikePrice,
    },
};
use smol_str::SmolStr;

use crate::catalog::{
    batch::{CatalogBatch, CatalogEntry},
    display::{publication::StagedAssetCode, types::DisplayError},
    error::{CatalogError, CatalogValidationError},
    generation::{CatalogGeneration, CatalogGenerationPublisher},
    registry::{
        asset::{AssetRegistry, AssetTerms},
        staging::{CatalogStagingHandle, StagedRegistry},
    },
    source::{
        CatalogFetchContext, CatalogRouteRegistration, CatalogRouteUpdate, CatalogSourceId,
        CatalogSourceRegistry, RouteInputRevision,
    },
    test_support::{
        self, VENUE, entry, entry_of, listing_key, perp_route, spot_route, token, token_terms,
    },
};

fn reference_terms(source: &str, base: &str, quote: &str) -> PriceReferenceTerms {
    PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(token(VENUE, base)),
        quote_asset: token(VENUE, quote),
        basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(source).expect("the fixture source is canonical"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the fixture version is valid"),
    }
}

fn price_reference_terms(
    underlying: UnderlyingRef,
    quote_asset: AssetId,
    basis: PriceBasis,
    kind: ReferenceObservationKind,
) -> PriceReferenceTerms {
    PriceReferenceTerms {
        underlying,
        quote_asset,
        basis,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(VENUE).expect("the fixture source is canonical"),
            kind,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the fixture version is valid"),
    }
}

fn context(route: &VenueRoute) -> CatalogFetchContext {
    context_of(registration(route))
}

fn registration(route: &VenueRoute) -> CatalogRouteRegistration {
    CatalogRouteRegistration::catalog(
        route.clone(),
        CatalogSourceId::new("display-gate").expect("the source id is valid"),
        test_support::case_insensitive_symbols(),
        vec![market_participants_model::routing::CapabilityId::MarketCatalog],
        RouteInputRevision::describe().build(),
    )
}

fn context_of(registration: CatalogRouteRegistration) -> CatalogFetchContext {
    let route = registration.route().clone();
    let update = CatalogRouteUpdate::new(
        [test_support::binding(
            "display-gate",
            test_support::listing(Vec::new()),
        )],
        [registration],
    );
    let mut registry = CatalogSourceRegistry::new();
    let candidate = registry
        .plan(
            update,
            &test_support::requirements(std::slice::from_ref(&route)),
        )
        .expect("the fixture route plans");
    registry.commit(candidate);
    let registered = registry
        .require_registered_route(&route)
        .expect("the fixture route is registered");
    CatalogFetchContext::new(
        registered,
        CatalogStagingHandle::new(StagedRegistry::empty()),
    )
}

fn base_generation() -> CatalogGeneration {
    CatalogGeneration::empty(&CatalogGenerationPublisher::new())
}

fn publish_bare_on(
    context: CatalogFetchContext,
    base: &CatalogGeneration,
    batch: CatalogBatch,
) -> Result<CatalogGeneration, CatalogError> {
    let route = context.venue_route().clone();
    let pending =
        CatalogBatch::pending(&context, batch, base).map_err(|error| refused(route, error))?;
    base.clone()
        .with_pending(pending, &CatalogGenerationPublisher::new())
}

fn publish_bare(
    route: &VenueRoute,
    batch: CatalogBatch,
) -> Result<CatalogGeneration, CatalogError> {
    publish_bare_at(context(route), batch)
}

fn publish_bare_at(
    context: CatalogFetchContext,
    batch: CatalogBatch,
) -> Result<CatalogGeneration, CatalogError> {
    let route = context.venue_route().clone();
    let base = base_generation();
    let pending =
        CatalogBatch::pending(&context, batch, &base).map_err(|error| refused(route, error))?;
    base.with_pending(pending, &CatalogGenerationPublisher::new())
}

fn publish(route: &VenueRoute, batch: CatalogBatch) -> Result<CatalogGeneration, CatalogError> {
    let context = context(route);
    let mut batch = batch;
    let published = std::sync::Arc::clone(context.staging().base().assets());
    test_support::stage_fixture_terms(&mut batch, context.staging(), &published);
    let base = base_generation();
    let pending = CatalogBatch::pending(&context, batch, &base)
        .map_err(|error| refused(route.clone(), error))?;
    base.with_pending(pending, &CatalogGenerationPublisher::new())
}

fn publish_pair(
    first: (&VenueRoute, CatalogBatch),
    second: (&VenueRoute, CatalogBatch),
) -> Result<CatalogGeneration, CatalogError> {
    let generation = publish(first.0, first.1)?;
    let context = context(second.0);
    let mut batch = second.1;
    let published = std::sync::Arc::clone(context.staging().base().assets());
    test_support::stage_fixture_terms(&mut batch, context.staging(), &published);
    let pending = CatalogBatch::pending(&context, batch, &generation)
        .map_err(|error| refused(second.0.clone(), error))?;
    generation.with_pending(pending, &CatalogGenerationPublisher::new())
}

fn refused(route: VenueRoute, error: CatalogValidationError) -> CatalogError {
    CatalogError::InvalidCatalog {
        route,
        error: Box::new(error),
    }
}

fn display_error(error: &CatalogError) -> &DisplayError {
    match error {
        CatalogError::InvalidCatalog { error, .. } => match error.as_ref() {
            CatalogValidationError::DisplayRejected { error, .. } => error,
            other => panic!("expected a display refusal, got {other:?}"),
        },
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

fn fixture_security() -> (AssetId, SecurityTerms) {
    let issuer = SecurityIssuerId::new("cik/320193").expect("the fixture issuer is canonical");
    let share_class = ShareClassId::new(&ShareClassTerms {
        issuer: issuer.clone(),
        designation: SmolStr::new_static("AAPL"),
        form: SecurityForm::CommonStock,
        par_value: None,
        adr_ratio: None,
    })
    .expect("the fixture share class is valid");
    let terms = SecurityTerms {
        issuer,
        share_class,
        form: SecurityForm::CommonStock,
    };
    (
        AssetId::Security(market_participants_model::asset::SecurityId::of_terms(
            &terms,
        )),
        terms,
    )
}

fn asset_spot(asset: AssetId) -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(asset, token(VENUE, "USDT")).expect("the fixture pair is valid"),
    ))
    .expect("the fixture instrument is valid")
}

fn perpetual(reference: PriceReferenceId, base: &str, quote: &str) -> Instrument {
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(1_u8), token(VENUE, base))
            .expect("the fixture contract unit is valid"),
    )
    .expect("the fixture contract size is valid");
    Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            market_participants_model::asset::DerivativeUnderlying::new(
                UnderlyingRef::Asset(token(VENUE, base)),
                reference,
            ),
            token(VENUE, quote),
            token(VENUE, quote),
            contract_size,
            DerivativePnlModel::Linear,
            None,
        )
        .expect("the fixture perpetual terms are valid"),
    ))
    .expect("the fixture perpetual instrument is valid")
}

fn share_class_terms() -> ShareClassTerms {
    ShareClassTerms {
        issuer: SecurityIssuerId::new("cik/320193").expect("the fixture issuer is canonical"),
        designation: SmolStr::new_static("AAPL"),
        form: SecurityForm::CommonStock,
        par_value: None,
        adr_ratio: None,
    }
}

fn staged_code(asset: AssetId, code: &str) -> StagedAssetCode {
    StagedAssetCode {
        asset,
        code: AssetDisplayCode::from_staged_code(code).expect("the fixture code is renderable"),
    }
}

fn name_of(generation: &CatalogGeneration, entry: &CatalogEntry) -> SmolStr {
    SmolStr::new(
        generation
            .display_catalog()
            .listing_instrument(&entry.listing.key)
            .unwrap_or_else(|| panic!("the catalog names no listing {:?}", entry.listing.key))
            .as_str(),
    )
}

fn future_code_series(code: &str) -> FutureSeriesIdentifiers {
    FutureSeriesIdentifiers {
        market_participant_series_id: None,
        local_symbol: None,
        contract_code: Some(ContractCode::new(code).expect("the fixture contract code is valid")),
    }
}

fn market_participant_series(value: &str) -> OptionSeriesIdentifiers {
    OptionSeriesIdentifiers {
        occ_symbol: None,
        market_participant_series_id: Some(
            MarketParticipantSeriesId::new(value)
                .expect("the fixture market-participant series is valid"),
        ),
        local_symbol: None,
    }
}

fn physical_delivery(direction: DeliveryDirection) -> DeliveryMode {
    DeliveryMode::physical(vec![
        DeliveryLeg::new(token(VENUE, "BTC"), Quantity::from(1_u8), direction)
            .expect("the fixture delivery leg is valid"),
    ])
    .expect("the fixture physical delivery is valid")
}

fn physical_future_entry(
    route: &VenueRoute,
    tracked: PriceReferenceId,
    direction: DeliveryDirection,
    symbol: &str,
) -> CatalogEntry {
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, "BTC")), tracked);
    let terms = FutureTerms::new(
        underlying,
        token(VENUE, "USD"),
        DerivativePnlModel::Linear,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(1_u8), token(VENUE, "BTC"))
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        expiry_at(1_766_102_400_000),
        None,
        physical_delivery(direction),
        None,
    )
    .expect("the fixture physical future terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Future(terms))
        .expect("the fixture future instrument is valid");
    series_entry(
        route,
        instrument,
        token(VENUE, "BTC"),
        future_code_series(symbol),
    )
}

fn physical_option_entry(
    route: &VenueRoute,
    tracked: PriceReferenceId,
    symbol: &str,
) -> CatalogEntry {
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, "BTC")), tracked);
    let terms = OptionTerms::new(
        underlying,
        token(VENUE, "USD"),
        OptionRight::Call,
        strike_of("60000", token(VENUE, "USD")),
        expiry_at(1_767_072_000_000),
        ExerciseStyle::European,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(1_u8), token(VENUE, "BTC"))
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        physical_delivery(DeliveryDirection::Receive),
        None,
    )
    .expect("the fixture physical option terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Option(terms))
        .expect("the fixture option instrument is valid");
    series_entry(
        route,
        instrument,
        token(VENUE, "BTC"),
        market_participant_series(symbol),
    )
}

fn perpetual_under(
    reference: PriceReferenceId,
    base: &str,
    quote: &str,
    pnl_model: DerivativePnlModel,
) -> Instrument {
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(1_u8), token(VENUE, base))
            .expect("the fixture contract unit is valid"),
    )
    .expect("the fixture contract size is valid");
    Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, base)), reference),
            token(VENUE, quote),
            token(VENUE, quote),
            contract_size,
            pnl_model,
            None,
        )
        .expect("the fixture perpetual terms are valid"),
    ))
    .expect("the fixture perpetual instrument is valid")
}

fn crypto_future_entry_of(
    route: &VenueRoute,
    tracked: PriceReferenceId,
    quantity: u32,
    series: FutureSeriesIdentifiers,
) -> CatalogEntry {
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, "BTC")), tracked);
    let terms = FutureTerms::new(
        underlying,
        token(VENUE, "USD"),
        DerivativePnlModel::Linear,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(quantity), token(VENUE, "BTC"))
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        expiry_at(1_766_102_400_000),
        None,
        DeliveryMode::Cash {
            asset: token(VENUE, "USD"),
            reference: tracked,
            conversion_reference: None,
        },
        None,
    )
    .expect("the fixture future terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Future(terms))
        .expect("the fixture future instrument is valid");
    series_entry(route, instrument, token(VENUE, "BTC"), series)
}

fn contract_token(address: &str) -> AssetTerms {
    AssetTerms::CryptoToken(CryptoTokenTerms {
        origin: CryptoTokenOrigin::Chain {
            chain_id: ChainId::new("1").expect("1 is a chain"),
            token: TokenReference::Contract {
                address: ContractAddress::new(address).expect("the fixture address is valid"),
            },
        },
    })
}

fn future_entry_with_unit(
    route: &VenueRoute,
    tracked: PriceReferenceId,
    unit: AssetId,
    series: FutureSeriesIdentifiers,
) -> CatalogEntry {
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, "BTC")), tracked);
    let terms = FutureTerms::new(
        underlying,
        token(VENUE, "USD"),
        DerivativePnlModel::Linear,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(1_u8), unit)
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        expiry_at(1_766_102_400_000),
        None,
        DeliveryMode::Cash {
            asset: token(VENUE, "USD"),
            reference: tracked,
            conversion_reference: None,
        },
        None,
    )
    .expect("the fixture future terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Future(terms))
        .expect("the fixture future instrument is valid");
    series_entry(route, instrument, token(VENUE, "BTC"), series)
}

#[test]
fn a_physical_future_states_the_physical_settlement() {
    let route = future_route();
    let (tracked, reference) = crypto_reference("USD");
    let entry = physical_future_entry(&route, tracked, DeliveryDirection::Receive, "BTCUSD");
    let generation = publish(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect("a physically delivered future publishes");

    assert_eq!(
        name_of(&generation, &entry).as_str(),
        "BTC/USD:PHYS-20251219",
        "a physical delivery states PHYS where a cash delivery states USD"
    );
}

#[test]
fn a_physical_option_states_the_physical_settlement() {
    let route = option_route();
    let (tracked, reference) = crypto_reference("USD");
    let entry = physical_option_entry(&route, tracked, "BTC-PHYS");
    let generation = publish(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect("a physically delivered option publishes");

    assert_eq!(
        name_of(&generation, &entry).as_str(),
        "BTC/USD:PHYS-20251230T052000Z-60000-CALL",
        "a physical delivery states PHYS and the price quote never replaces it"
    );
}

#[test]
fn attribute_changes_do_not_change_the_published_name() {
    let route = future_route();
    let (tracked, reference) = crypto_reference("USD");
    let small = crypto_future_entry_of(&route, tracked, 1, future_code_series("ES-1"));
    let large = crypto_future_entry_of(&route, tracked, 10, future_code_series("ES-2"));
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![small.clone(), large.clone()],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect("two contract sizes of one contract publish");
    assert_ne!(
        small.instrument.id(),
        large.instrument.id(),
        "the contract size is part of the identity of a contract"
    );
    assert_eq!(
        name_of(&generation, &small).as_str(),
        "BTC/USD:USD-20251219",
        "the contract size is not a clause of the name"
    );
    assert_eq!(
        name_of(&generation, &large),
        name_of(&generation, &small),
        "the two contract sizes publish one name"
    );

    let route = perp_route();
    let terms = reference_terms("binance", "BTC", "BTC");
    let tracked = terms
        .reference_id()
        .expect("the fixture reference is valid");
    let linear = perpetual_entry(
        &route,
        perpetual_under(tracked, "BTC", "BTC", DerivativePnlModel::Linear),
        "BTCBTC-L",
        "BTC",
    );
    let inverse = perpetual_entry(
        &route,
        perpetual_under(tracked, "BTC", "BTC", DerivativePnlModel::Inverse),
        "BTCBTC-I",
        "BTC",
    );
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![linear.clone(), inverse.clone()],
            asset_terms: vec![AssetTerms::CryptoToken(token_terms(VENUE, "BTC"))],
            reference_terms: vec![terms],
            ..CatalogBatch::default()
        },
    )
    .expect("two pnl models of one contract publish");
    assert_ne!(
        linear.instrument.id(),
        inverse.instrument.id(),
        "the pnl model is part of the identity of a contract"
    );
    assert_eq!(
        name_of(&generation, &linear).as_str(),
        "BTC/BTC:BTC",
        "the pnl model is not a clause of the name"
    );
    assert_eq!(
        name_of(&generation, &inverse),
        name_of(&generation, &linear),
        "the two pnl models publish one name"
    );

    let route = future_route();
    let (tracked, reference) = crypto_reference("USD");
    let receive = physical_future_entry(&route, tracked, DeliveryDirection::Receive, "P-1");
    let deliver = physical_future_entry(&route, tracked, DeliveryDirection::Deliver, "P-2");
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![receive.clone(), deliver.clone()],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect("two delivery legs of one contract publish");
    assert_ne!(
        receive.instrument.id(),
        deliver.instrument.id(),
        "the delivery legs are part of the identity of a contract"
    );
    assert_eq!(
        name_of(&generation, &receive).as_str(),
        "BTC/USD:PHYS-20251219",
        "a delivery leg is not a clause of the name"
    );
    assert_eq!(
        name_of(&generation, &deliver),
        name_of(&generation, &receive),
        "the two delivery legs publish one name"
    );
}

#[test]
fn a_named_contract_token_without_a_code_refuses_the_batch() {
    let base = contract_token("0x0000000000000000000000000000000000000001");
    let base_id = base.asset_id();
    let error = publish_bare(
        &spot_route(),
        CatalogBatch {
            listings: vec![entry(&spot_route(), asset_spot(base_id), "WBTCUSDT")],
            asset_terms: vec![base, AssetTerms::CryptoToken(token_terms(VENUE, "USDT"))],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a named contract token without a code is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::AssetDisplayCodeMissing { .. }
        ),
        "the refusal names the missing code, got {error:?}"
    );
}

#[test]
fn a_contract_unit_without_a_code_does_not_refuse_the_batch() {
    let route = future_route();
    let (tracked, reference) = crypto_reference("USD");
    let unit = contract_token("0x0000000000000000000000000000000000000002");
    let unit_id = unit.asset_id();
    let entry = future_entry_with_unit(&route, tracked, unit_id, future_code_series("BTCUSD"));
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_terms: vec![
                unit,
                AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                AssetTerms::CryptoToken(token_terms(VENUE, "USD")),
            ],
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect("a contract unit no name renders needs no code");

    assert_eq!(
        name_of(&generation, &entry).as_str(),
        "BTC/USD:USD-20251219",
        "the contract unit is not a clause of the name"
    );
}

#[test]
fn a_registry_only_asset_without_a_code_is_not_named() {
    let (security, terms) = fixture_security();
    let generation = registry_only(
        vec![AssetTerms::Security {
            terms,
            share_class: share_class_terms(),
        }],
        Vec::new(),
        Vec::new(),
    );
    let catalog = generation.display_catalog();

    assert!(
        generation.assets().terms(&security).is_some(),
        "the registry of the generation really carries the security"
    );
    assert_eq!(
        catalog.asset_code(&security).map(AssetDisplayCode::as_str),
        None,
        "an asset no name renders and no route staged stays without a code"
    );
    assert!(
        matches!(
            generation.display_resolver().asset_code(&security),
            Err(DisplayError::AssetDisplayCodeMissing { asset }) if asset.as_ref() == &security
        ),
        "a reader asking the resolver for a code no publication holds is told so"
    );
}

#[test]
fn a_staged_code_that_contradicts_the_asset_terms_refuses_the_batch() {
    let asset = token(VENUE, "BTC");
    let error = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![entry(
                &spot_route(),
                test_support::spot_token(VENUE, "BTC", "USDT"),
                "BTCUSDT",
            )],
            asset_codes: vec![staged_code(asset.clone(), "WBTC")],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a staged code the terms contradict is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::StagedCodeMismatch { asset: refused } if **refused == asset
        ),
        "the disagreement is reported against the asset, got {error:?}"
    );
}

#[test]
fn the_native_symbol_is_a_search_alias_and_never_a_name() {
    let asset = test_support::entry_with_aliases(
        &spot_route(),
        test_support::spot_token(VENUE, "BTC", "USDT"),
        "BTCUSDT",
        &["BTC-USDT", "btc-usdt"],
    );
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![asset.clone()],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    let catalog = generation.display_catalog();
    assert_eq!(
        name_of(&generation, &asset).as_str(),
        "BTC/USDT",
        "the native symbol is not part of the name"
    );
    assert_eq!(
        catalog
            .listing_search_aliases(&asset.listing.key)
            .expect("the listing published its search aliases")
            .iter()
            .map(SmolStr::as_str)
            .collect::<Vec<_>>(),
        ["BTC-USDT", "BTCUSDT"],
        "the declared spellings are the search aliases, uppercased and deduplicated"
    );
}

#[test]
fn a_currency_and_a_token_of_one_code_both_keep_it() {
    let currency_terms = CurrencyTerms {
        code: Iso4217Code::new("USD").expect("USD is canonical"),
    };
    let currency = AssetId::Currency(CurrencyId::of_terms(&currency_terms));
    let token_asset = token(VENUE, "USD");
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![
                entry(&spot_route(), asset_spot(currency.clone()), "USDBTC"),
                entry(&spot_route(), asset_spot(token_asset.clone()), "USDETH"),
            ],
            asset_terms: vec![
                AssetTerms::Currency(currency_terms),
                AssetTerms::CryptoToken(token_terms(VENUE, "USD")),
                AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                AssetTerms::CryptoToken(token_terms(VENUE, "ETH")),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    let catalog = generation.display_catalog();
    assert_eq!(
        catalog.asset_code(&currency).map(|code| code.as_str()),
        Some("USD"),
        "the currency publishes its ISO code"
    );
    assert_eq!(
        catalog.asset_code(&token_asset).map(|code| code.as_str()),
        Some("USD"),
        "the token publishes its own native code, which is the same text"
    );
}

#[test]
fn two_tokens_of_one_code_keep_it() {
    let mut first_terms = token_terms("alpha", "USD");
    first_terms.origin = CryptoTokenOrigin::VenueIssued {
        issuer: TokenIssuerId::new("alpha").expect("the issuer is canonical"),
        namespace: IdentifierNamespace::new("alpha").expect("the namespace is canonical"),
        token: TokenReference::Native {
            code: IdentifierValue::new("USD").expect("the code is canonical"),
        },
    };
    let mut second_terms = token_terms("beta", "USD");
    second_terms.origin = CryptoTokenOrigin::VenueIssued {
        issuer: TokenIssuerId::new("beta").expect("the issuer is canonical"),
        namespace: IdentifierNamespace::new("beta").expect("the namespace is canonical"),
        token: TokenReference::Native {
            code: IdentifierValue::new("USD").expect("the code is canonical"),
        },
    };
    let first = AssetId::CryptoToken(CryptoTokenId::of_terms(&first_terms));
    let second = AssetId::CryptoToken(CryptoTokenId::of_terms(&second_terms));

    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![
                entry(&spot_route(), asset_spot(first.clone()), "ALPHA"),
                entry(&spot_route(), asset_spot(second.clone()), "BETA"),
            ],
            asset_terms: vec![
                AssetTerms::CryptoToken(first_terms),
                AssetTerms::CryptoToken(second_terms),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    let catalog = generation.display_catalog();
    let first_code = catalog
        .asset_code(&first)
        .expect("the first token is displayed");
    let second_code = catalog
        .asset_code(&second)
        .expect("the second token is displayed");
    assert_eq!(first_code.as_str(), "USD");
    assert_eq!(second_code.as_str(), "USD");
}

#[test]
fn the_published_names_and_details_agree() {
    let instrument = test_support::spot_token(VENUE, "BTC", "USDT");
    let key = listing_key(&spot_route(), "BTCUSDT");
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![entry(&spot_route(), instrument.clone(), "BTCUSDT")],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    let catalog = generation.display_catalog();
    let details = catalog
        .instrument_details(instrument.id())
        .expect("the instrument has details");
    let name = market_participants_model::display::grammar::format_instrument(details)
        .expect("the published details render");
    assert_eq!(
        catalog.listing_instrument(&key),
        Some(&name),
        "the listing is named by the rendering of the published details"
    );
    assert_eq!(name.as_str(), "BTC/USDT");
    assert_eq!(
        catalog.listing_search_aliases(&key),
        Some(["BTCUSDT".into()].as_slice()),
        "the venue's own spelling stays a search alias"
    );
}

#[test]
fn a_security_staged_with_two_codes_refuses_the_batch() {
    let (security, terms) = fixture_security();
    let error = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![entry(&spot_route(), asset_spot(security.clone()), "AAA")],
            asset_codes: vec![
                staged_code(security.clone(), "AAA"),
                staged_code(security, "ZZZ"),
            ],
            asset_terms: vec![AssetTerms::Security {
                terms,
                share_class: share_class_terms(),
            }],
            ..CatalogBatch::default()
        },
    )
    .expect_err("two codes of one security are refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::StagedCodeMismatch { .. }
        ),
        "the disagreement is a staged code mismatch, got {error:?}"
    );
}

#[test]
fn two_routes_that_stage_two_codes_for_one_asset_refuse_the_batch() {
    let (security, _terms) = fixture_security();
    let binance = spot_route();
    let okx = test_support::route("okx", "spot");
    let error = publish_pair(
        (
            &binance,
            CatalogBatch {
                listings: vec![entry(&binance, asset_spot(security.clone()), "AAA")],
                asset_codes: vec![staged_code(security.clone(), "AAA")],
                ..CatalogBatch::default()
            },
        ),
        (
            &okx,
            CatalogBatch {
                listings: vec![entry(&okx, asset_spot(security.clone()), "AAA")],
                asset_codes: vec![staged_code(security, "ZZZ")],
                ..CatalogBatch::default()
            },
        ),
    )
    .expect_err("two routes that stage two codes of one asset are refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::StagedCodeMismatch { .. }
        ),
        "the second route's code is refused against the staged one, got {error:?}"
    );
}

#[test]
fn a_security_listing_states_its_staged_ticker() {
    let (security, terms) = fixture_security();
    let key = listing_key(&spot_route(), "AAA");
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![entry(&spot_route(), asset_spot(security.clone()), "AAA")],
            asset_codes: vec![staged_code(security.clone(), "  aapl  ")],
            asset_terms: vec![AssetTerms::Security {
                terms,
                share_class: share_class_terms(),
            }],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    let catalog = generation.display_catalog();
    assert_eq!(
        catalog.asset_code(&security).map(|code| code.as_str()),
        Some("AAPL"),
        "the ticker is normalized through the display pipeline"
    );
    assert_eq!(
        catalog.listing_instrument(&key).map(|name| name.as_str()),
        Some("AAPL/USDT"),
        "the name states the economic code and never a MIC or a local code"
    );
}

#[test]
fn two_listings_of_one_instrument_share_its_name() {
    let instrument = test_support::spot_token(VENUE, "BTC", "USDT");
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![
                entry(&spot_route(), instrument.clone(), "BTCUSDT"),
                entry(&spot_route(), instrument, "BTCUSDT-ALT"),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("two listings of one instrument are publishable");

    let catalog = generation.display_catalog();
    for symbol in ["BTCUSDT", "BTCUSDT-ALT"] {
        assert_eq!(
            catalog
                .listing_instrument(&listing_key(&spot_route(), symbol))
                .map(|name| name.as_str()),
            Some("BTC/USDT"),
            "`{symbol}` is named after its instrument"
        );
    }
}

#[test]
fn a_settlement_reference_of_the_wrong_direction_is_refused() {
    let route = future_route();
    let wrong = price_reference_terms(
        UnderlyingRef::Asset(token(VENUE, "BTC")),
        token(VENUE, "EUR"),
        PriceBasis::PerAssetUnit,
        ReferenceObservationKind::Fixing,
    );
    let wrong_id = wrong
        .reference_id()
        .expect("the fixture reference is valid");
    let entry = crypto_future_entry_of(&route, wrong_id, 1, future_code_series("BTCUSD"));

    let error = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![entry],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![wrong],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a settlement reference of another direction is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::ReferenceDirectionMismatch { .. }
        ),
        "the direction is reported, got {error:?}"
    );
}

#[test]
fn a_conversion_reference_of_the_wrong_direction_is_refused() {
    let route = future_route();
    let tracked = reference_terms("binance", "BTC", "USD");
    let tracked_id = tracked
        .reference_id()
        .expect("the fixture reference is valid");
    let conversion = price_reference_terms(
        UnderlyingRef::Asset(token(VENUE, "BTC")),
        token(VENUE, "USD"),
        PriceBasis::PerAssetUnit,
        ReferenceObservationKind::Index,
    );
    let conversion_id = conversion
        .reference_id()
        .expect("the fixture reference is valid");
    let wrong = price_reference_terms(
        UnderlyingRef::Asset(token(VENUE, "BTC")),
        token(VENUE, "EUR"),
        PriceBasis::PerAssetUnit,
        ReferenceObservationKind::Fixing,
    );
    let wrong_id = wrong
        .reference_id()
        .expect("the fixture reference is valid");
    let instrument = inverse_future(tracked_id, conversion_id, conversion);
    let payload = future_payload(&instrument, wrong_id);
    let instrument: Instrument =
        serde_json::from_str(&payload).expect("the decoded payload is a valid dated contract");

    let error = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![series_entry(
                &route,
                instrument,
                token(VENUE, "BTC"),
                future_code_series("BTCUSD"),
            )],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![tracked, wrong],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a conversion of the wrong direction is refused");

    assert_eq!(
        display_error(&error),
        &DisplayError::ReferenceDirectionMismatch {
            reference: wrong_id,
            field: "conversion reference",
        },
        "the conversion role is the one the refusal names"
    );
}

fn inverse_future(
    tracked: PriceReferenceId,
    conversion_id: PriceReferenceId,
    conversion: PriceReferenceTerms,
) -> Instrument {
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(token(VENUE, "BTC")), tracked);
    let terms = FutureTerms::new(
        underlying,
        token(VENUE, "USD"),
        DerivativePnlModel::Inverse,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(100_u8), token(VENUE, "USD"))
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        expiry_at(1_766_102_400_000),
        None,
        DeliveryMode::Cash {
            asset: token(VENUE, "BTC"),
            reference: tracked,
            conversion_reference: Some(conversion_id),
        },
        Some(&conversion),
    )
    .expect("the fixture inverse future terms are valid");
    Instrument::new(InstrumentTerms::Future(terms))
        .expect("the fixture inverse future instrument is valid")
}

fn future_payload(instrument: &Instrument, conversion_reference: PriceReferenceId) -> String {
    let InstrumentTerms::Future(terms) = instrument.terms() else {
        panic!("the fixture is a dated contract");
    };
    let json = serde_json::json!({
        "kind": "future",
        "spec": {
            "underlying": {
                "reference": {
                    "kind": "asset",
                    "reference": serde_json::to_value(token(VENUE, "BTC"))
                        .expect("the fixture asset serializes"),
                },
                "price_reference": serde_json::to_value(terms.underlying().price_reference())
                    .expect("the fixture reference serializes"),
            },
            "price_quote": serde_json::to_value(terms.price_quote())
                .expect("the fixture asset serializes"),
            "pnl_model": serde_json::to_value(terms.pnl_model())
                .expect("the fixture pnl model serializes"),
            "contract_size": {
                "unit": serde_json::to_value(terms.contract_size().unit())
                    .expect("the fixture contract unit serializes"),
            },
            "expiry": {
                "expiration_at": terms.expiry().expiration_at(),
                "last_trading_at": terms.expiry().last_trading_at(),
                "settlement_at": terms.expiry().settlement_at(),
            },
            "first_notice_at": terms.first_notice_at(),
            "delivery": {
                "kind": "cash",
                "mode": {
                    "asset": serde_json::to_value(token(VENUE, "BTC"))
                        .expect("the fixture asset serializes"),
                    "reference": serde_json::to_value(
                        terms.delivery().cash_reference()
                            .expect("the fixture settles in cash"),
                    )
                    .expect("the fixture reference serializes"),
                    "conversion_reference": serde_json::to_value(conversion_reference)
                        .expect("the fixture reference serializes"),
                },
            },
            "conversion_reference": serde_json::to_value(conversion_reference)
                .expect("the fixture reference serializes"),
        },
    });
    serde_json::to_string(&json).expect("the fixture payload serializes")
}

#[test]
fn a_missing_asset_term_refuses_the_publication() {
    let context = context(&spot_route());
    let base = base_generation();
    let error = CatalogBatch::pending(
        &context,
        CatalogBatch {
            listings: vec![entry(
                &spot_route(),
                test_support::spot_token(VENUE, "BTC", "USDT"),
                "BTCUSDT",
            )],
            ..CatalogBatch::default()
        },
        &base,
    )
    .map_err(|error| refused(spot_route(), error))
    .expect_err("an unresolvable asset refuses the batch");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::MissingAssetTerms { .. }
        ),
        "the missing terms are reported as such, got {error:?}"
    );
}

#[test]
fn an_unregistered_settlement_reference_refuses_the_publication() {
    let route = future_route();
    let (tracked, _reference) = crypto_reference("USD");
    let entry = crypto_future_entry_of(&route, tracked, 1, future_code_series("BTCUSD"));
    let error = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![entry],
            asset_terms: crypto_pair_terms(),
            ..CatalogBatch::default()
        },
    )
    .expect_err("a settlement reference the publication does not carry is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::MissingReferenceTerms { .. }
        ),
        "the missing reference terms are reported, got {error:?}"
    );
}

fn perpetual_entry(
    route: &VenueRoute,
    instrument: Instrument,
    symbol: &str,
    quote: &str,
) -> crate::catalog::batch::CatalogEntry {
    let mut entry = entry_of(
        route,
        instrument,
        symbol,
        BinanceProduct::Perpetual,
        BinanceListingIdentifiers {
            product: BinanceProduct::Perpetual,
            future_series: None,
            option_series: None,
        },
    );
    entry.snapshot.rules.product = std::sync::Arc::new(perpetual_rules(quote));
    entry.snapshot.rules.common.price.quote_asset = token(VENUE, quote);
    entry
}

fn perpetual_rules(quote: &str) -> market_participants_model::listing::rules::ProductTradingRules {
    use market_participants_model::listing::rules::{
        FundingInterval, FundingSchedule, MarginMode, MarginProfile, PerpetualMarginTerms,
        PerpetualRisk, PerpetualTradingRules,
    };
    let quote = token(VENUE, quote);
    market_participants_model::listing::rules::ProductTradingRules::Perpetual(
        PerpetualTradingRules {
            risk: PerpetualRisk::Published(
                PerpetualMarginTerms::new(
                    MarginProfile::new(
                        MarginMode::Cross,
                        fraction("0.1"),
                        fraction("0.05"),
                        quote.clone(),
                    )
                    .expect("the fixture margin profile is valid"),
                    vec![
                        market_participants_model::listing::rules::LeverageTier::new(
                            Quantity::from(1_000_u64),
                            10,
                            fraction("0.05"),
                        )
                        .expect("the fixture leverage tier is valid"),
                    ],
                )
                .expect("the fixture margin terms are valid"),
            ),
            funding: FundingSchedule::new(
                FundingInterval::Fixed {
                    interval_millis: 28_800_000,
                },
                None,
                None,
                quote,
            )
            .expect("the fixture funding schedule is valid"),
            open_interest_cap: None,
        },
    )
}

fn fraction(value: &str) -> market_participants_model::decimal::Fraction {
    market_participants_model::decimal::Fraction::new(
        value
            .parse::<market_participants_model::decimal::Ratio>()
            .expect("the fixture ratio parses"),
    )
    .expect("the fixture fraction is valid")
}

fn mic_of(code: &str) -> Mic {
    Mic::new(code).expect("the fixture MIC is complete")
}

fn equity_identifiers(mic: &Mic, ticker: &str, local_code: &str) -> EquityListingIdentifiers {
    EquityListingIdentifiers {
        mic: mic.clone(),
        board: None,
        ticker: SmolStr::new(ticker),
        local_code: EquityLocalCode::new(local_code).expect("the fixture local code is canonical"),
    }
}

fn equity_entry(
    route: &VenueRoute,
    security: &AssetId,
    ticker: &str,
    local_code: &str,
    mic: &Mic,
) -> CatalogEntry {
    let instrument = asset_spot(security.clone());
    let key = ListingContinuityKey {
        route: route.clone(),
        terms: market_participants_model::listing::continuity::ListingContinuityTerms::Equity {
            issuer: security_issuer(),
            share_class: security_share_class(),
            mic: mic.clone(),
            local_code: EquityLocalCode::new(local_code).expect("the fixture local code is valid"),
        },
    }
    .listing_key()
    .expect("the fixture equity listing key is valid");
    let listing = VenueListing::new(
        key.clone(),
        *instrument.id(),
        Some(NativeSymbol::new(ticker).expect("the fixture symbol is valid")),
        Vec::new(),
        VenueListingIdentifiers::Equity(equity_identifiers(mic, ticker, local_code)),
    )
    .expect("the fixture equity listing is valid");
    let snapshot = ListingSnapshot::new(
        key,
        test_support::rules(token(VENUE, "USDT"), security.clone()),
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the fixture equity snapshot is valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

fn security_issuer() -> SecurityIssuerId {
    SecurityIssuerId::new("cik/320193").expect("the fixture issuer is canonical")
}

fn security_share_class() -> ShareClassId {
    ShareClassId::new(&share_class_terms()).expect("the fixture share class is valid")
}

fn strike_of(value: &str, quote: AssetId) -> StrikePrice {
    StrikePrice::new(
        value.parse::<Price>().expect("the fixture strike parses"),
        quote,
        PriceBasis::PerAssetUnit,
    )
    .expect("the fixture strike is valid")
}

fn expiry_at(millis: UnixMillis) -> Expiry {
    Expiry::new(millis, None, None).expect("the fixture expiry is valid")
}

fn derivative_rules(
    underlying: AssetId,
    quote: &str,
    product: Option<ProductTradingRules>,
) -> TradingRules {
    let quote = token(VENUE, quote);
    let common = CommonTradingRules {
        price: PriceRule {
            quote_asset: quote,
            basis: PriceBasis::PerAssetUnit,
            tick: Tick::try_from(Quantity::from(1_u8)).expect("the fixture tick is valid"),
            minimum: None,
            maximum: None,
        },
        quantity: QuantityRule {
            unit: QuantityUnit::AssetUnits { asset: underlying },
            step: Quantity::from(1_u8),
            minimum: None,
            maximum: None,
        },
        notional: None,
        venue_policy: std::sync::Arc::new(VenueTradingPolicy::continuous()),
    };
    let product = match product {
        Some(product) => product,
        None => future_product_rules(),
    };
    TradingRules {
        common,
        product: std::sync::Arc::new(product),
    }
}

fn option_product_rules() -> ProductTradingRules {
    ProductTradingRules::Option(
        OptionTradingRules::new(fixture_margin_profile(), None, None)
            .expect("the fixture option rules are valid"),
    )
}

fn future_product_rules() -> ProductTradingRules {
    ProductTradingRules::Future(
        market_participants_model::listing::rules::FutureTradingRules::new(
            fixture_margin_profile(),
            None,
            None,
        )
        .expect("the fixture future rules are valid"),
    )
}

fn fixture_margin_profile() -> MarginProfile {
    MarginProfile::new(
        MarginMode::Cross,
        Fraction::new("0.1".parse::<Ratio>().expect("the fixture ratio parses"))
            .expect("the fixture initial margin is valid"),
        Fraction::new("0.05".parse::<Ratio>().expect("the fixture ratio parses"))
            .expect("the fixture maintenance margin is valid"),
        token(VENUE, "USD"),
    )
    .expect("the fixture margin profile is valid")
}

fn security_option_entry(
    route: &VenueRoute,
    security: &AssetId,
    occ: &str,
    multiplier: u32,
) -> (CatalogEntry, PriceReferenceTerms) {
    let (_price, tracked_terms) =
        security_reference_over(VENUE, UnderlyingRef::Asset(security.clone()));
    let tracked = tracked_terms
        .reference_id()
        .expect("the fixture tracked reference is valid");
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(security.clone()), tracked);
    let terms = OptionTerms::new(
        underlying,
        token(VENUE, "USD"),
        OptionRight::Call,
        strike_of("150", token(VENUE, "USD")),
        expiry_at(1_766_102_400_000),
        ExerciseStyle::American,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(multiplier), security.clone())
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        DeliveryMode::Cash {
            asset: token(VENUE, "USD"),
            reference: tracked_terms
                .reference_id()
                .expect("the fixture settlement reference is valid"),
            conversion_reference: None,
        },
        None,
    )
    .expect("the fixture option terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Option(terms))
        .expect("the fixture option instrument is valid");
    (
        series_entry(
            route,
            instrument,
            security.clone(),
            OptionSeriesIdentifiers {
                occ_symbol: Some(OccSymbol::new(occ).expect("the fixture OCC symbol is valid")),
                market_participant_series_id: None,
                local_symbol: None,
            },
        ),
        tracked_terms,
    )
}

fn security_reference_terms(source: &str, underlying: UnderlyingRef) -> PriceReferenceTerms {
    PriceReferenceTerms {
        underlying,
        quote_asset: token(VENUE, "USD"),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(source).expect("the fixture source is canonical"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the fixture version is valid"),
    }
}

fn security_reference_over(
    source: &str,
    underlying: UnderlyingRef,
) -> (PriceReferenceId, PriceReferenceTerms) {
    let terms = security_reference_terms(source, underlying);
    let id = terms
        .reference_id()
        .expect("the fixture reference is valid");
    (id, terms)
}

fn security_future_entry(
    route: &VenueRoute,
    security: &AssetId,
    series: FutureSeriesIdentifiers,
) -> (CatalogEntry, PriceReferenceTerms) {
    let (_price, tracked_terms) =
        security_reference_over(VENUE, UnderlyingRef::Asset(security.clone()));
    let tracked = tracked_terms
        .reference_id()
        .expect("the fixture tracked reference is valid");
    let underlying = DerivativeUnderlying::new(UnderlyingRef::Asset(security.clone()), tracked);
    let terms = FutureTerms::new(
        underlying,
        token(VENUE, "USD"),
        DerivativePnlModel::Linear,
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(1_u8), security.clone())
                .expect("the fixture contract unit is valid"),
        )
        .expect("the fixture contract size is valid"),
        expiry_at(1_766_102_400_000),
        None,
        DeliveryMode::Cash {
            asset: token(VENUE, "USD"),
            reference: tracked_terms
                .reference_id()
                .expect("the fixture settlement reference is valid"),
            conversion_reference: None,
        },
        None,
    )
    .expect("the fixture future terms are valid");
    let instrument = Instrument::new(InstrumentTerms::Future(terms))
        .expect("the fixture future instrument is valid");
    (
        series_entry(route, instrument, security.clone(), series),
        tracked_terms,
    )
}

fn crypto_reference(settled: &str) -> (PriceReferenceId, PriceReferenceTerms) {
    let terms = PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(token(VENUE, "BTC")),
        quote_asset: token(VENUE, settled),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(VENUE).expect("the fixture source is canonical"),
            kind: ReferenceObservationKind::Index,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the fixture version is valid"),
    };
    let id = terms
        .reference_id()
        .expect("the fixture reference is valid");
    (id, terms)
}

#[test]
fn an_unrenderable_text_refuses_the_whole_batch() {
    let route = spot_route();
    let group = outcome_group_of("Will it\nrain?");
    let entry = outcome_entry_of(&route, &group, "RAIN-YES");
    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![entry],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a question the naming step cannot render refuses the batch");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::Format {
                error: DisplayFormatError::InvalidText {
                    reason: InvalidTextReason::ControlCharacter,
                    ..
                }
            }
        ),
        "the formatter refusal is reported as such, got {error:?}"
    );
}

#[test]
fn a_typed_identifier_its_symbols_do_not_echo_refuses_the_batch() {
    let route = future_route();
    let (tracked, reference) = crypto_reference("USD");
    let mut entry = physical_future_entry(&route, tracked, DeliveryDirection::Deliver, "BTCMAR26");
    entry.listing.native_symbol =
        Some(NativeSymbol::new("BTCDEC25").expect("the fixture symbol is valid"));

    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![entry],
            asset_terms: crypto_pair_terms(),
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an identifier its symbols do not echo refuses the batch");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::ListingIdentifierMismatch { kind, value, .. }
            if kind.label() == "future_series" && value == "BTCMAR26"
        ),
        "the unechoed identifier is named with its kind and value, got {error:?}"
    );
}

#[test]
fn an_index_underlying_without_registered_terms_refuses_the_publication() {
    let route = perp_route();
    let (tracked, reference) = crypto_reference("USD");
    let index = market_participants_model::asset::IndexId::of_terms(&index_terms());
    let instrument = perpetual_over(
        UnderlyingRef::Index(index),
        tracked,
        index_contract_unit(index),
    );
    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![index_perpetual_entry(&route, instrument)],
            asset_terms: vec![AssetTerms::CryptoToken(token_terms(VENUE, "USD"))],
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an index the publication does not carry is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::MissingIndexTerms { index: found } if found.as_ref() == &index
        ),
        "the missing index terms name the index, got {error:?}"
    );
}

#[test]
fn an_external_underlying_without_registered_terms_refuses_the_publication() {
    let route = perp_route();
    let (tracked, reference) = crypto_reference("USD");
    let external = external_reference_terms()
        .reference_id()
        .expect("the fixture external reference is valid");
    let instrument = perpetual_over(
        UnderlyingRef::External(external),
        tracked,
        asset_contract_unit(),
    );
    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![perpetual_entry(&route, instrument, "FXXPERP", "USD")],
            asset_terms: vec![AssetTerms::CryptoToken(token_terms(VENUE, "USD"))],
            reference_terms: vec![reference],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an external reference the publication does not carry is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::MissingExternalReferenceTerms { reference } if reference.as_ref() == &external
        ),
        "the missing external reference terms name the reference, got {error:?}"
    );
}

#[test]
fn an_outcome_listing_without_its_group_is_refused_by_the_batch() {
    let route = spot_route();
    let group = outcome_group_of("Will it rain?");
    let entry = outcome_entry_of(&route, &group, "RAIN-YES");
    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![entry],
            ..CatalogBatch::default()
        },
    )
    .expect_err("an outcome listing whose group the batch does not carry is refused");

    assert!(
        matches!(
            &error,
            CatalogError::InvalidCatalog { error, .. }
                if matches!(error.as_ref(), CatalogValidationError::IncompleteOutcomeSet { .. })
        ),
        "the batch names the missing group, got {error:?}"
    );
}

fn perpetual_over(
    underlying: UnderlyingRef,
    reference: PriceReferenceId,
    unit: ContractUnit,
) -> Instrument {
    let contract_size = ContractSize::new(unit).expect("the fixture contract size is valid");
    Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            DerivativeUnderlying::new(underlying, reference),
            token(VENUE, "USD"),
            token(VENUE, "USD"),
            contract_size,
            DerivativePnlModel::Linear,
            None,
        )
        .expect("the fixture perpetual terms are valid"),
    ))
    .expect("the fixture perpetual instrument is valid")
}

fn index_contract_unit(index: market_participants_model::asset::IndexId) -> ContractUnit {
    ContractUnit::quote_notional_per_index_point(Quantity::from(50_u8), token(VENUE, "USD"), index)
        .expect("the fixture index contract unit is valid")
}

fn index_perpetual_entry(
    route: &VenueRoute,
    instrument: Instrument,
) -> crate::catalog::batch::CatalogEntry {
    let index = match instrument.terms() {
        InstrumentTerms::Perpetual(terms) => match terms.underlying().reference() {
            UnderlyingRef::Index(index) => *index,
            other => panic!("the fixture tracks an index, got {other:?}"),
        },
        other => panic!("the fixture is a perpetual, got {other:?}"),
    };
    let mut entry = perpetual_entry(route, instrument, "SPXPERP", "USD");
    entry.snapshot.rules.common.price.basis = PriceBasis::PerIndexPoint { index };
    entry
}

fn asset_contract_unit() -> ContractUnit {
    ContractUnit::asset_quantity(Quantity::from(1_u8), token(VENUE, "USD"))
        .expect("the fixture contract unit is valid")
}

fn series_entry(
    route: &VenueRoute,
    instrument: Instrument,
    underlying: AssetId,
    series: impl SeriesListing,
) -> CatalogEntry {
    let key = ListingContinuityKey {
        route: route.clone(),
        terms: series.continuity_terms(),
    }
    .listing_key()
    .expect("the fixture series listing key is valid");
    let listing = series.listing(key.clone(), *instrument.id());
    let product = match instrument.kind() {
        InstrumentKind::Option => option_product_rules(),
        _ => future_product_rules(),
    };
    let snapshot = ListingSnapshot::new(
        key,
        derivative_rules(underlying, "USD", Some(product)),
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the fixture series snapshot is valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

trait SeriesListing {
    fn continuity_terms(
        &self,
    ) -> market_participants_model::listing::continuity::ListingContinuityTerms;
    fn listing(&self, key: VenueListingKey, instrument: InstrumentId) -> VenueListing;
}

impl SeriesListing for OptionSeriesIdentifiers {
    fn continuity_terms(
        &self,
    ) -> market_participants_model::listing::continuity::ListingContinuityTerms {
        market_participants_model::listing::continuity::ListingContinuityTerms::OptionSeries(
            self.clone(),
        )
    }

    fn listing(&self, key: VenueListingKey, instrument: InstrumentId) -> VenueListing {
        VenueListing::new(
            key,
            instrument,
            None,
            Vec::new(),
            VenueListingIdentifiers::OptionSeries(self.clone()),
        )
        .expect("the fixture option series listing is valid")
    }
}

impl SeriesListing for FutureSeriesIdentifiers {
    fn continuity_terms(
        &self,
    ) -> market_participants_model::listing::continuity::ListingContinuityTerms {
        market_participants_model::listing::continuity::ListingContinuityTerms::FutureSeries(
            self.clone(),
        )
    }

    fn listing(&self, key: VenueListingKey, instrument: InstrumentId) -> VenueListing {
        VenueListing::new(
            key,
            instrument,
            None,
            Vec::new(),
            VenueListingIdentifiers::FutureSeries(self.clone()),
        )
        .expect("the fixture future series listing is valid")
    }
}

fn security_pair_terms(terms: SecurityTerms) -> Vec<AssetTerms> {
    vec![
        AssetTerms::Security {
            terms,
            share_class: share_class_terms(),
        },
        AssetTerms::CryptoToken(token_terms(VENUE, "USD")),
        AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
    ]
}

#[test]
fn a_security_listing_without_a_staged_code_is_refused() {
    let (security, terms) = fixture_security();
    let route = test_support::route(test_support::VENUE, "xnas");
    let entry = equity_entry(&route, &security, "AAPL", "AAPL", &mic_of("XNAS"));

    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![entry],
            asset_terms: security_pair_terms(terms),
            ..CatalogBatch::default()
        },
    )
    .expect_err("a named security without a code is refused");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::AssetDisplayCodeMissing { .. }
        ),
        "the refusal names the missing code, got {error:?}"
    );
}

#[test]
fn a_security_option_states_its_staged_ticker() {
    let (security, terms) = fixture_security();
    let route = test_support::route(test_support::VENUE, "equity-option");
    let (entry, tracked) = security_option_entry(&route, &security, "AAPL251219C00150000", 1);
    let generation = publish_bare_at(
        context_of(registration(&route)),
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_codes: vec![staged_code(security, "AAPL")],
            asset_terms: security_pair_terms(terms),
            reference_terms: vec![tracked],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    assert_eq!(
        generation
            .display_catalog()
            .listing_instrument(&entry.listing.key)
            .map(|name| name.as_str()),
        Some("AAPL/USD:USD-20251219-150-CALL"),
        "the staged ticker is the underlying, and the contract states its own fields"
    );
}

#[test]
fn a_security_future_states_its_staged_ticker() {
    let (security, terms) = fixture_security();
    let route = test_support::route(test_support::VENUE, "equity-future");
    let (entry, tracked) = security_future_entry(
        &route,
        &security,
        FutureSeriesIdentifiers {
            market_participant_series_id: None,
            local_symbol: None,
            contract_code: Some(
                ContractCode::new("ES").expect("the fixture contract code is valid"),
            ),
        },
    );
    let generation = publish(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_codes: vec![staged_code(security, "AAPL")],
            asset_terms: security_pair_terms(terms),
            reference_terms: vec![tracked],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");

    assert_eq!(
        generation
            .display_catalog()
            .listing_instrument(&entry.listing.key)
            .map(|name| name.as_str()),
        Some("AAPL/USD:USD-20251219"),
        "the future keeps the expiry and the settlement of the contract"
    );
}

#[test]
fn two_listings_of_one_name_are_published() {
    let instrument = test_support::spot_token(VENUE, "BTC", "USDT");
    let first = entry(&spot_route(), instrument.clone(), "BTCUSDT");
    let key = test_support::continuity_key(&spot_route(), "BTC-USDC", BinanceProduct::Spot);
    let second = VenueListing::new(
        key.clone(),
        *instrument.id(),
        Some(NativeSymbol::new("BTC-USDC").expect("the fixture symbol is valid")),
        Vec::new(),
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Spot {
            asset_id: HyperliquidAssetId(1),
            base_token_id: HyperliquidTokenId(1),
            quote_token_id: HyperliquidTokenId(2),
        }),
    )
    .expect("the fixture Hyperliquid spot listing is valid");
    let second_key = second.key.clone();
    let generation = publish(
        &spot_route(),
        CatalogBatch {
            listings: vec![
                first.clone(),
                CatalogEntry {
                    snapshot: ListingSnapshot::new(
                        key,
                        first.snapshot.rules.clone(),
                        ListingStatus::Trading,
                        None,
                        None,
                        0,
                    )
                    .expect("the fixture snapshot is valid"),
                    instrument: first.instrument.clone(),
                    listing: second,
                },
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("two listings of one name are publishable");

    let catalog = generation.display_catalog();
    for name in [
        catalog.listing_instrument(&first.listing.key),
        catalog.listing_instrument(&second_key),
    ] {
        assert_eq!(name.map(|name| name.as_str()), Some("BTC/USDT"));
    }
}

fn option_route() -> VenueRoute {
    test_support::route(VENUE, "european-option")
}

fn future_route() -> VenueRoute {
    test_support::route(VENUE, "usd-m-future")
}

fn crypto_pair_terms() -> Vec<AssetTerms> {
    vec![
        AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
        AssetTerms::CryptoToken(token_terms(VENUE, "USD")),
        AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
    ]
}

fn outcome_group_of(question: &str) -> OutcomeGroup {
    let seed = OutcomeGroupSeed {
        question: SmolStr::new(question),
        expiry: expiry_at(1_767_139_200_000),
        topology: OutcomeTopology::Binary,
        settlement_asset: token(VENUE, "USDT"),
        resolution: ResolutionRule::FixedAt {
            at: 1_767_139_200_000,
        },
        fraction_rule: SettlementFractionRule::Binary,
    };
    let seed_id = seed.seed_id();
    let yes = OutcomeSide::new(&seed_id, SmolStr::new_static("yes")).expect("the side is valid");
    let no = OutcomeSide::new(&seed_id, SmolStr::new_static("no")).expect("the side is valid");
    let member = OutcomeMember::new(&seed_id, SmolStr::new_static("winner"), vec![yes.id, no.id])
        .expect("the member is valid");
    OutcomeGroup::new(seed, vec![yes, no], vec![member]).expect("the group is valid")
}

fn outcome_entry_of(route: &VenueRoute, group: &OutcomeGroup, symbol: &str) -> CatalogEntry {
    let member = group.members[0].id;
    let side = group.members[0].sides[0];
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id, member, side,
    )))
    .expect("the fixture outcome instrument is valid");
    let key = test_support::continuity_key(route, symbol, BinanceProduct::Spot);
    let listing = VenueListing::new(
        key.clone(),
        *instrument.id(),
        Some(NativeSymbol::new(symbol).expect("the fixture symbol is valid")),
        Vec::new(),
        VenueListingIdentifiers::Polymarket(PolymarketListingIdentifiers {
            event_id: None,
            market_id: PolymarketMarketId::new("market-1").expect("the market id is valid"),
            condition_id: CtfConditionId::new("0x".to_owned() + &"1".repeat(64))
                .expect("the condition id is valid"),
            asset_id: CtfTokenId::new("12345").expect("the token id is valid"),
            protocol: market_participants_model::listing::venue::PolymarketProtocolVersion::V1,
            outcome_label: group
                .side(&side)
                .expect("the fixture group carries the side")
                .label
                .clone(),
        }),
    )
    .expect("the fixture outcome listing is valid");
    let snapshot = ListingSnapshot::new(
        key,
        outcome_rules(group, member, side),
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the fixture outcome snapshot is valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

fn outcome_rules(
    group: &OutcomeGroup,
    member: OutcomeMemberId,
    side: OutcomeSideId,
) -> TradingRules {
    TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset: token(VENUE, "USDT"),
                basis: PriceBasis::FractionOfCollateral,
                tick: Tick::try_from(Quantity::from(1_u8)).expect("the fixture tick is valid"),
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::OutcomeTokens {
                    group: group.id,
                    member,
                    side,
                },
                step: Quantity::from(1_u8),
                minimum: None,
                maximum: None,
            },
            notional: None,
            venue_policy: std::sync::Arc::new(VenueTradingPolicy::continuous()),
        },
        product: std::sync::Arc::new(ProductTradingRules::Outcome(
            market_participants_model::listing::rules::OutcomeTradingRules {
                order_book: true,
                negative_risk_conversion: true,
            },
        )),
    }
}

#[test]
fn a_protocol_outcome_label_that_disagrees_refuses_the_batch() {
    let route = spot_route();
    let group = outcome_group_of("Will it rain?");
    let mut entry = outcome_entry_of(&route, &group, "RAIN-YES");
    let VenueListingIdentifiers::Polymarket(identifiers) = &mut entry.listing.identifiers else {
        panic!("the fixture is a Polymarket listing");
    };
    identifiers.outcome_label = SmolStr::new_static("maybe");

    let error = publish(
        &route,
        CatalogBatch {
            listings: vec![entry],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect_err("a protocol label that disagrees with the registry refuses the batch");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::ListingIdentifierMismatch { .. }
        ),
        "the label disagreement is a mismatch, got {error:?}"
    );
}

#[test]
fn a_protocol_outcome_label_that_agrees_publishes_the_registry_name() {
    let route = spot_route();
    let group = outcome_group_of("Will it rain?");
    let entry = outcome_entry_of(&route, &group, "RAIN-YES");
    let generation = publish(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            outcome_groups: vec![group],
            ..CatalogBatch::default()
        },
    )
    .expect("the agreeing protocol label publishes the batch");

    let name = generation
        .display_catalog()
        .listing_instrument(&entry.listing.key)
        .expect("the listing is named");
    assert!(
        name.as_str().contains("/YES:USDT-"),
        "the name states the registry label, got {name}"
    );
    assert!(
        !name.as_str().contains("RAIN-YES"),
        "the protocol symbol never reaches the name, got {name}"
    );
}

#[test]
fn a_contradicted_echo_refuses_the_publication() {
    let (security, terms) = fixture_security();
    let route = test_support::route(test_support::VENUE, "xnas");
    let entry = equity_entry(&route, &security, "AAPL", "AAPL", &mic_of("XNAS"));
    let mut listing = entry.listing.clone();
    listing.native_symbol = Some(NativeSymbol::new("MSFT").expect("the fixture symbol is valid"));
    let error = publish_bare_at(
        context_of(registration(&route)),
        CatalogBatch {
            listings: vec![CatalogEntry {
                instrument: entry.instrument.clone(),
                listing,
                snapshot: entry.snapshot,
            }],
            asset_codes: vec![staged_code(security, "AAPL")],
            asset_terms: security_pair_terms(terms),
            ..CatalogBatch::default()
        },
    )
    .expect_err("a contradicted echo refuses the publication");

    assert!(
        matches!(
            display_error(&error),
            DisplayError::ListingIdentifierMismatch { .. }
        ),
        "the mismatch is reported as such, got {error:?}"
    );
}

#[test]
fn the_golden_perpetual_name_carries_no_qualifier() {
    let route = test_support::route("binance", "usd-m-perp");
    let terms = reference_terms("binance", "BTC", "USDT");
    let reference = terms
        .reference_id()
        .expect("the fixture reference is valid");
    let entry = perpetual_entry(
        &route,
        perpetual(reference, "BTC", "USDT"),
        "BTCUSDT",
        "USDT",
    );
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![entry.clone()],
            asset_terms: vec![
                AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
            ],
            reference_terms: vec![terms],
            ..CatalogBatch::default()
        },
    )
    .expect("the golden perpetual publishes");

    let catalog = generation.display_catalog();
    assert_eq!(
        catalog
            .listing_instrument(&entry.listing.key)
            .map(|name| name.as_str()),
        Some("BTC/USDT:USDT"),
        "the pnl model, the contract size and the tracked reference are not part of a name"
    );
}

#[test]
fn two_venues_of_one_code_keep_their_code_across_routes() {
    let binance = test_support::spot_route();
    let okx = test_support::route("okx", "spot");
    let binance_btc = token(VENUE, "BTC");
    let okx_btc = token("okx", "BTC");
    let generation = publish_pair(
        (
            &binance,
            CatalogBatch {
                listings: vec![entry(
                    &binance,
                    test_support::spot_token(VENUE, "BTC", "USDT"),
                    "BTCUSDT",
                )],
                asset_terms: vec![
                    AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                    AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
                ],
                ..CatalogBatch::default()
            },
        ),
        (
            &okx,
            CatalogBatch {
                listings: vec![entry(
                    &okx,
                    test_support::spot_token("okx", "BTC", "USDT"),
                    "BTC-USDT",
                )],
                asset_terms: vec![
                    AssetTerms::CryptoToken(token_terms("okx", "BTC")),
                    AssetTerms::CryptoToken(token_terms("okx", "USDT")),
                ],
                ..CatalogBatch::default()
            },
        ),
    )
    .expect("two venues of one code publish side by side");

    let catalog = generation.display_catalog();
    for asset in [&binance_btc, &okx_btc] {
        assert_eq!(
            catalog.asset_code(asset).map(|code| code.as_str()),
            Some("BTC"),
            "a token publishes its own native code, whatever another route publishes"
        );
    }
    for (route, symbol) in [(&binance, "BTCUSDT"), (&okx, "BTC-USDT")] {
        assert_eq!(
            catalog
                .listing_instrument(&test_support::listing_key(route, symbol))
                .map(|name| name.as_str()),
            Some("BTC/USDT")
        );
    }
}

#[test]
fn two_instruments_of_one_name_are_published() {
    let (security, terms, share_class) = common_stock("cik/320193", "AAPL", None);
    let first = reference_terms("binance", "BTC", "USDT");
    let first_id = first
        .reference_id()
        .expect("the fixture reference is valid");
    let mut second = reference_terms("binance", "BTC", "USDT");
    second.basis = PriceBasis::PerContract;
    let second_id = second
        .reference_id()
        .expect("the fixture reference is valid");
    assert_ne!(first_id, second_id, "the two scopes are distinct");

    let route = perp_route();
    let projected = security_perpetual_entry(&route, &security, &first_id, "AAPL-MIC");
    let plain = security_perpetual_entry(&route, &security, &second_id, "AAPL-PLAIN");
    assert_ne!(
        projected.instrument.id(),
        plain.instrument.id(),
        "the two instruments are distinct"
    );
    let generation = publish_bare(
        &route,
        CatalogBatch {
            listings: vec![projected.clone(), plain.clone()],
            asset_codes: vec![staged_code(security, "AAPL")],
            asset_terms: vec![
                AssetTerms::Security { terms, share_class },
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
            ],
            reference_terms: vec![first, second],
            ..CatalogBatch::default()
        },
    )
    .expect("two instruments of one name are publishable");

    let catalog = generation.display_catalog();
    let first_name = catalog
        .listing_instrument(&projected.listing.key)
        .expect("the first listing is named");
    let second_name = catalog
        .listing_instrument(&plain.listing.key)
        .expect("the second listing is named");
    assert_eq!(
        first_name, second_name,
        "the two instruments state the same fields, so they publish one name"
    );
    assert!(
        first_name.as_str().starts_with("AAPL/"),
        "the staged ticker is the underlying of both, got {first_name}"
    );
}

fn common_stock(
    issuer: &str,
    designation: &str,
    par_value: Option<Quantity>,
) -> (AssetId, SecurityTerms, ShareClassTerms) {
    let issuer = SecurityIssuerId::new(issuer).expect("the fixture issuer is canonical");
    let share_class = ShareClassTerms {
        issuer: issuer.clone(),
        designation: SmolStr::new(designation),
        form: SecurityForm::CommonStock,
        par_value,
        adr_ratio: None,
    };
    let terms = SecurityTerms {
        issuer,
        share_class: ShareClassId::new(&share_class).expect("the fixture share class is valid"),
        form: SecurityForm::CommonStock,
    };
    (
        AssetId::Security(market_participants_model::asset::SecurityId::of_terms(
            &terms,
        )),
        terms,
        share_class,
    )
}

fn security_perpetual(security: &AssetId, reference: &PriceReferenceId) -> Instrument {
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(1_u8), security.clone())
            .expect("the fixture contract unit is valid"),
    )
    .expect("the fixture contract size is valid");
    Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            market_participants_model::asset::DerivativeUnderlying::new(
                UnderlyingRef::Asset(security.clone()),
                *reference,
            ),
            token(VENUE, "USDT"),
            token(VENUE, "USDT"),
            contract_size,
            DerivativePnlModel::Linear,
            None,
        )
        .expect("the fixture perpetual terms are valid"),
    ))
    .expect("the fixture perpetual instrument is valid")
}

fn security_perpetual_entry(
    route: &VenueRoute,
    security: &AssetId,
    reference: &PriceReferenceId,
    symbol: &str,
) -> CatalogEntry {
    perpetual_entry(
        route,
        security_perpetual(security, reference),
        symbol,
        "USDT",
    )
}

fn code_of(catalog: &crate::catalog::display::catalog::DisplayCatalog, asset: &AssetId) -> SmolStr {
    SmolStr::new(
        catalog
            .asset_code(asset)
            .unwrap_or_else(|| panic!("the catalog publishes no code for {asset:?}"))
            .as_str(),
    )
}

fn currency_terms_of(code: &str) -> CurrencyTerms {
    CurrencyTerms {
        code: Iso4217Code::new(code).expect("the fixture ISO code is canonical"),
    }
}

fn currency(code: &str) -> AssetId {
    AssetId::Currency(CurrencyId::of_terms(&currency_terms_of(code)))
}

fn issuer_token_terms(issuer: &str, code: &str) -> AssetTerms {
    let issuer_id = TokenIssuerId::new(issuer).expect("the fixture issuer is canonical");
    AssetTerms::CryptoToken(CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer: issuer_id,
            namespace: IdentifierNamespace::new(issuer)
                .expect("the fixture namespace is canonical"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("the fixture code is canonical"),
            },
        },
    })
}

fn index_terms() -> market_participants_model::asset::IndexTerms {
    market_participants_model::asset::IndexTerms {
        namespace: IdentifierNamespace::new("sp").expect("the fixture namespace is canonical"),
        code: IdentifierValue::new("500").expect("the fixture code is canonical"),
    }
}

fn external_reference_terms() -> market_participants_model::reference::ExternalReferenceTerms {
    let fixture = reference_terms("binance", "BTC", "USD");
    market_participants_model::reference::ExternalReferenceTerms {
        issuer: IdentifierNamespace::new("oracle").expect("the fixture issuer is canonical"),
        reference: IdentifierValue::new("fx-1").expect("the fixture reference is canonical"),
        quote_asset: fixture.quote_asset,
        basis: fixture.basis,
        observation: fixture.observation,
        aggregation: fixture.aggregation,
        session: fixture.session,
        settlement: fixture.settlement,
        schema_version: fixture.schema_version,
    }
}

fn published_registries(
    assets: Vec<AssetTerms>,
    indices: Vec<market_participants_model::asset::IndexTerms>,
    externals: Vec<market_participants_model::reference::ExternalReferenceTerms>,
) -> (
    AssetRegistry,
    crate::catalog::registry::reference::ReferenceRegistry,
) {
    let mut pending = crate::catalog::registry::asset::AssetRegistryPending::new();
    let mut references = crate::catalog::registry::reference::ReferenceRegistryPending::new();
    let mut referenced: crate::catalog::batch::ReferencedTerms =
        crate::catalog::batch::ReferencedTerms::default();
    for terms in assets {
        referenced.assets.insert(terms.asset_id());
        pending.stage(terms);
    }
    for terms in indices {
        referenced
            .indices
            .insert(market_participants_model::asset::IndexId::of_terms(&terms));
        references.register_index(terms);
    }
    for terms in externals {
        let id = references
            .register_external_reference(terms)
            .expect("the fixture external reference is valid");
        referenced.external_references.insert(id);
    }
    (
        crate::catalog::batch::publish_assets(
            &std::sync::Arc::new(AssetRegistry::new()),
            &pending,
            &referenced,
        ),
        references.publish(
            &crate::catalog::registry::reference::ReferenceRegistry::new(),
            &referenced,
        ),
    )
}

fn registry_only(
    assets: Vec<AssetTerms>,
    indices: Vec<market_participants_model::asset::IndexTerms>,
    externals: Vec<market_participants_model::reference::ExternalReferenceTerms>,
) -> CatalogGeneration {
    let (assets, references) = published_registries(assets, indices, externals);
    base_generation()
        .with_registries(
            std::sync::Arc::new(assets),
            std::sync::Arc::new(references),
            std::sync::Arc::new(crate::catalog::registry::outcome::OutcomeRegistry::new()),
            &CatalogGenerationPublisher::new(),
        )
        .expect("a registry-only publication assigns a new generation")
}

#[test]
fn a_registered_asset_no_listing_references_is_named_by_the_catalog() {
    let unreferenced_currency = currency("JPY");
    let unreferenced_token = token(VENUE, "SOL");
    let generation = registry_only(
        vec![
            AssetTerms::Currency(currency_terms_of("JPY")),
            AssetTerms::CryptoToken(token_terms(VENUE, "SOL")),
        ],
        Vec::new(),
        Vec::new(),
    );
    let catalog = generation.display_catalog();
    assert!(
        generation.assets().terms(&unreferenced_currency).is_some()
            && generation.assets().terms(&unreferenced_token).is_some(),
        "the registry of the generation really carries the assets no listing references"
    );

    assert_eq!(
        code_of(catalog, &unreferenced_currency).as_str(),
        "JPY",
        "the currency no listing references is named by the catalog"
    );
    assert_eq!(
        code_of(catalog, &unreferenced_token).as_str(),
        "SOL",
        "the token no listing references is named by the catalog"
    );
}

#[test]
fn an_index_and_an_external_reference_no_listing_tracks_are_named_by_the_catalog() {
    let index = index_terms();
    let index_id = market_participants_model::asset::IndexId::of_terms(&index);
    let external = external_reference_terms();
    let external_id = external
        .reference_id()
        .expect("the fixture external reference is valid");
    let generation = registry_only(
        vec![AssetTerms::CryptoToken(token_terms(VENUE, "BTC"))],
        vec![index],
        vec![external],
    );
    let catalog = generation.display_catalog();
    assert!(
        generation.references().index(&index_id).is_some()
            && generation
                .references()
                .external_reference(&external_id)
                .is_some(),
        "the registry of the generation carries both references"
    );

    assert_eq!(
        catalog.index_code(&index_id).map(|code| code.as_str()),
        Some("SP/500"),
        "the index no listing tracks is named by the catalog"
    );
    assert_eq!(
        catalog
            .external_reference_code(&external_id)
            .map(|code| code.as_str()),
        Some("FX-1"),
        "the external reference no listing tracks is named by the catalog"
    );
}

#[test]
fn a_generation_without_listings_names_every_registered_asset() {
    let carried = currency("EUR");
    let absent = currency("GBP");
    let generation = registry_only(
        vec![AssetTerms::Currency(currency_terms_of("EUR"))],
        Vec::new(),
        Vec::new(),
    );
    let catalog = generation.display_catalog();

    assert!(
        generation.route_snapshots().is_empty(),
        "a registry-only publication carries no route snapshot at all"
    );
    assert_eq!(
        code_of(catalog, &carried).as_str(),
        "EUR",
        "the registered asset of a listing-less generation is named"
    );
    assert_eq!(
        catalog.asset_code(&absent).map(|code| code.as_str()),
        None,
        "an asset the registry does not carry has no code at all"
    );
}

#[test]
fn growing_the_registry_does_not_change_the_code_a_listing_publishes() {
    let route = spot_route();
    let tracked = token(VENUE, "BTC");
    let quote = token(VENUE, "USDT");
    let generation = publish(
        &route,
        CatalogBatch {
            listings: vec![entry(&route, asset_spot(tracked.clone()), "BTCUSDT")],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture publication is accepted");
    let name = generation
        .display_catalog()
        .listing_instrument(&listing_key(&route, "BTCUSDT"))
        .expect("the fixture listing is published")
        .clone();
    let grown = registry_only(
        vec![
            AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
            AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
            AssetTerms::Currency(currency_terms_of("JPY")),
            AssetTerms::CryptoToken(token_terms(VENUE, "SOL")),
            AssetTerms::CryptoToken(token_terms(VENUE, "ETH")),
        ],
        Vec::new(),
        Vec::new(),
    );
    let catalog = grown.display_catalog();

    assert!(
        grown.assets().terms(&token(VENUE, "SOL")).is_some()
            && generation.assets().terms(&token(VENUE, "SOL")).is_none(),
        "the later publication really carries an asset the earlier one did not"
    );
    assert_eq!(
        code_of(catalog, &tracked).as_str(),
        "BTC",
        "the base of the listing keeps its code"
    );
    assert_eq!(
        code_of(catalog, &quote).as_str(),
        "USDT",
        "the quote of the listing keeps its code"
    );
    assert_eq!(
        name.as_str(),
        "BTC/USDT",
        "the name the listing published before the registry grew is unchanged"
    );
}

#[test]
fn two_unreferenced_tokens_of_one_code_are_both_published() {
    let first = issuer_token_terms("alpha", "USD");
    let second = issuer_token_terms("beta", "USD");
    let first_id = first.asset_id();
    let second_id = second.asset_id();
    assert_ne!(first_id, second_id, "the two issuers are two identities");
    let generation = registry_only(vec![first, second], Vec::new(), Vec::new());
    let catalog = generation.display_catalog();

    for asset in [&first_id, &second_id] {
        assert_eq!(
            code_of(catalog, asset).as_str(),
            "USD",
            "the native code of the token is its display code"
        );
    }
}

#[test]
fn an_unreferenced_currency_of_a_named_token_code_keeps_the_published_name() {
    let placed = issuer_token_terms("alpha", "USD");
    let placed_id = placed.asset_id();
    let unreferenced_currency = currency("USD");
    let route = spot_route();
    let generation = publish_bare_on(
        context(&route),
        &registry_only(
            vec![
                placed,
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
                AssetTerms::Currency(currency_terms_of("USD")),
            ],
            Vec::new(),
            Vec::new(),
        ),
        CatalogBatch {
            listings: vec![entry(&route, asset_spot(placed_id.clone()), "USDUSDT")],
            asset_terms: vec![
                issuer_token_terms("alpha", "USD"),
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("the placed token keeps the code it published");
    let catalog = generation.display_catalog();

    assert_eq!(
        code_of(catalog, &placed_id).as_str(),
        "USD",
        "the token the listing names keeps its code"
    );
    assert_eq!(
        code_of(catalog, &unreferenced_currency).as_str(),
        "USD",
        "the currency publishes its own ISO code"
    );
    assert_eq!(
        catalog
            .listing_instrument(&listing_key(&route, "USDUSDT"))
            .map(|name| name.as_str()),
        Some("USD/USDT"),
        "the name the token published is the one the listing still states"
    );
}

#[test]
fn the_spot_row_of_the_name_table_is_published_word_for_word() {
    let route = spot_route();
    let generation = publish_bare_on(
        context(&route),
        &registry_only(
            vec![
                AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
                AssetTerms::Currency(currency_terms_of("JPY")),
                AssetTerms::CryptoToken(token_terms(VENUE, "SOL")),
            ],
            Vec::new(),
            Vec::new(),
        ),
        CatalogBatch {
            listings: vec![entry(&route, asset_spot(token(VENUE, "BTC")), "BTCUSDT")],
            asset_terms: vec![
                AssetTerms::CryptoToken(token_terms(VENUE, "BTC")),
                AssetTerms::CryptoToken(token_terms(VENUE, "USDT")),
            ],
            ..CatalogBatch::default()
        },
    )
    .expect("the fixture row is published");
    let catalog = generation.display_catalog();

    assert_eq!(
        catalog
            .listing_instrument(&listing_key(&route, "BTCUSDT"))
            .map(|name| name.as_str()),
        Some("BTC/USDT"),
        "the spot row of the name table, word for word"
    );
    assert_eq!(
        code_of(catalog, &currency("JPY")).as_str(),
        "JPY",
        "the registry the row was published against really carried an extra asset"
    );
    assert_eq!(
        code_of(catalog, &token(VENUE, "SOL")).as_str(),
        "SOL",
        "and an extra token too"
    );
}
