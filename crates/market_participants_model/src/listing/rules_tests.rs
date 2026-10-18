//! Unit tests for venue trading rules, listing snapshots and listing aliases.

use crate::{
    asset::{AssetId, CryptoTokenId, CryptoTokenOrigin, TokenReference},
    decimal::{Fraction, Price, Quantity, Ratio, Tick},
    error::{ListingPublicationError, ModelError},
    identifier::{
        EquityLocalCode, IdentifierNamespace, IdentifierValue, Mic, NativeSymbol, TokenIssuerId,
        TradingCalendarId, VenueId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::{
            BinanceListingIdentifiers, EchoValidationStatus, EquityListingIdentifiers,
            VenueListingIdentifiers,
        },
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, FundingInterval, FundingSchedule, LeverageTier, ListingPrecision,
            LotPolicy, MarginMode, MarginProfile, NotionalRule, PerpetualMarginTerms,
            PerpetualRisk, PerpetualTradingRules, PriceLimitRule, PriceRule, ProductTradingRules,
            QuantityRule, SettlementCycle, ShortSalePolicy, SpotTradingRules, TimeWindow,
            TradingRules,
        },
        venue::{BinanceProduct, PriceBasis},
    },
    market::units::QuantityUnit,
    routing::{ListingIdentifierKind, SymbolNormalization},
};

fn venue() -> VenueId {
    VenueId::Binance
}

fn scope() -> VenueScopeId {
    VenueScopeId::new("spot").expect("scope should be valid")
}

fn route() -> VenueRoute {
    VenueRoute::new(venue(), scope())
}

fn listing_key(instrument: &str) -> VenueListingKey {
    ListingContinuityKey {
        route: route(),
        terms: ListingContinuityTerms::Binance {
            product: BinanceProduct::Spot,
            instrument_id: NativeSymbol::new(instrument).expect("symbol should be valid"),
        },
    }
    .listing_key()
    .expect("listing key should be valid")
}

fn btc() -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
            namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new("BTC").expect("code should be valid"),
            },
        })
        .expect("token should be valid"),
    )
}

fn usdt() -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
            namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new("USDT").expect("code should be valid"),
            },
        })
        .expect("token should be valid"),
    )
}

fn spot_instrument() -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(btc(), usdt()).expect("terms should be valid"),
    ))
    .expect("instrument should be valid")
}

fn spot_rules() -> TradingRules {
    TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset: usdt(),
                basis: PriceBasis::PerAssetUnit,
                tick: Tick::from_inner(10_000_000_000_000).expect("tick should be valid"),
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::AssetUnits { asset: btc() },
                step: Quantity::from_inner(1_000_000_000_000_000_000),
                minimum: None,
                maximum: None,
            },
            notional: Some(NotionalRule {
                asset: usdt(),
                minimum: Some(Quantity::from(10_u8)),
                maximum: None,
            }),
            sessions: TradingCalendarId::new("continuous-24x7").expect("calendar should be valid"),
            settlement_cycle: SettlementCycle::Immediate,
            price_limits: PriceLimitRule::None,
        },
        product: ProductTradingRules::Spot(
            SpotTradingRules::new(
                LotPolicy::FractionalAllowed {
                    lot_size: Quantity::from_inner(1),
                    minimum_fraction: None,
                },
                ShortSalePolicy::NotApplicable,
            )
            .expect("spot rules should be valid"),
        ),
    }
}

fn perpetual_rules() -> TradingRules {
    TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset: usdt(),
                basis: PriceBasis::PerContract,
                tick: Tick::from_inner(10_000_000_000_000).expect("tick should be valid"),
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::Contracts,
                step: Quantity::from(1_u8),
                minimum: Some(Quantity::from(1_u8)),
                maximum: None,
            },
            notional: None,
            sessions: TradingCalendarId::new("continuous-24x7").expect("calendar should be valid"),
            settlement_cycle: SettlementCycle::Immediate,
            price_limits: PriceLimitRule::None,
        },
        product: ProductTradingRules::Perpetual(PerpetualTradingRules {
            risk: PerpetualRisk::Published(
                PerpetualMarginTerms::new(
                    MarginProfile::new(
                        MarginMode::Cross,
                        Fraction::new("0.1".parse::<Ratio>().expect("ratio should parse"))
                            .expect("fraction should be valid"),
                        Fraction::new("0.05".parse::<Ratio>().expect("ratio should parse"))
                            .expect("fraction should be valid"),
                        usdt(),
                    )
                    .expect("margin should be valid"),
                    vec![
                        LeverageTier::new(
                            Quantity::from(50_000_u32),
                            20,
                            Fraction::new("0.05".parse::<Ratio>().expect("ratio should parse"))
                                .expect("fraction should be valid"),
                        )
                        .expect("tier should be valid"),
                    ],
                )
                .expect("margin terms should be valid"),
            ),
            funding: FundingSchedule::new(
                FundingInterval::Fixed {
                    interval_millis: 28_800_000,
                },
                None,
                None,
                usdt(),
            )
            .expect("funding should be valid"),
            open_interest_cap: None,
        }),
    }
}

#[test]
fn trading_rules_should_match_the_listed_instrument() {
    let instrument = spot_instrument();

    assert_eq!(spot_rules().validate_for(&instrument), Ok(()));
    assert_eq!(
        perpetual_rules().validate_for(&instrument),
        Err(ModelError::TradingRulesMismatch {
            field: "product rules do not match the instrument kind"
        })
    );
}

#[test]
fn notional_rule_should_require_the_price_quote_asset() {
    let instrument = spot_instrument();
    let mut rules = spot_rules();
    rules.common.notional = Some(NotionalRule {
        asset: btc(),
        minimum: None,
        maximum: None,
    });

    assert_eq!(
        rules.validate_for(&instrument),
        Err(ModelError::TradingRulesMismatch {
            field: "notional.asset"
        })
    );
}

#[test]
fn price_rule_should_require_the_instrument_quote_asset() {
    let instrument = spot_instrument();
    let mut rules = spot_rules();
    rules.common.price.quote_asset = btc();

    assert_eq!(
        rules.validate_for(&instrument),
        Err(ModelError::TradingRulesMismatch {
            field: "price.quote_asset"
        })
    );
}

#[test]
fn quantity_step_should_be_positive() {
    let mut rules = spot_rules();
    rules.common.quantity.step = Quantity::ZERO;

    assert_eq!(
        rules.common.validate(),
        Err(ModelError::NonPositiveQuantity {
            field: "quantity.step"
        })
    );
}

#[test]
fn quantity_unit_should_match_the_instrument() {
    let instrument = spot_instrument();
    let mut rules = spot_rules();
    rules.common.quantity.unit = QuantityUnit::Contracts;

    assert_eq!(
        rules.validate_for(&instrument),
        Err(ModelError::QuantityUnitMismatch {
            field: "quantity.unit",
            expected: crate::error::QuantityUnitExpectation::TradedQuantity,
        })
    );
}

/// A ladder is only a ladder when it rises: an empty one, a repeated cap and a
/// falling one are each refused where the terms are built.
#[test]
fn margin_terms_should_be_a_strictly_rising_ladder() {
    let margin = MarginProfile::new(
        MarginMode::Cross,
        Fraction::new("0.1".parse::<Ratio>().expect("ratio should parse"))
            .expect("fraction should be valid"),
        Fraction::new("0.05".parse::<Ratio>().expect("ratio should parse"))
            .expect("fraction should be valid"),
        usdt(),
    )
    .expect("margin should be valid");
    let tier = |cap: u32, leverage: u32, maintenance: &str| {
        LeverageTier::new(
            Quantity::from(cap),
            leverage,
            Fraction::new(maintenance.parse::<Ratio>().expect("ratio should parse"))
                .expect("fraction should be valid"),
        )
        .expect("tier should be valid")
    };

    assert_eq!(
        PerpetualMarginTerms::new(margin.clone(), Vec::new()),
        Err(ModelError::EmptyCollection {
            field: "perpetual.leverage"
        })
    );
    assert_eq!(
        PerpetualMarginTerms::new(
            margin.clone(),
            vec![tier(50_000, 20, "0.05"), tier(10_000, 50, "0.02")]
        ),
        Err(ModelError::UnsortedCollection {
            field: "perpetual.leverage"
        })
    );
    assert_eq!(
        PerpetualMarginTerms::new(
            margin.clone(),
            vec![tier(10_000, 50, "0.02"), tier(10_000, 20, "0.05")]
        ),
        Err(ModelError::UnsortedCollection {
            field: "perpetual.leverage"
        })
    );
    assert!(
        PerpetualMarginTerms::new(
            margin,
            vec![tier(10_000, 50, "0.02"), tier(50_000, 20, "0.05")]
        )
        .is_ok()
    );
}

#[test]
fn margin_profile_should_require_maintenance_below_initial() {
    assert!(matches!(
        MarginProfile::new(
            MarginMode::Cross,
            Fraction::new("0.05".parse::<Ratio>().expect("ratio should parse"))
                .expect("fraction should be valid"),
            Fraction::new("0.1".parse::<Ratio>().expect("ratio should parse"))
                .expect("fraction should be valid"),
            usdt(),
        ),
        Err(ModelError::TradingRulesMismatch {
            field: "margin.maintenance_margin"
        })
    ));
}

#[test]
fn funding_schedule_should_require_a_positive_interval() {
    assert!(matches!(
        FundingInterval::Fixed { interval_millis: 0 }.validate(),
        Err(ModelError::NonPositiveQuantity {
            field: "funding.interval_millis"
        })
    ));
}

#[test]
fn time_window_should_reject_an_inverted_range() {
    assert_eq!(TimeWindow::new(10, 5), Err(ModelError::InvalidTimeRange));
    assert_eq!(
        TimeWindow::new(5, 10)
            .expect("window should be valid")
            .duration_millis(),
        5
    );
}

#[test]
fn trading_rules_should_reject_a_zero_tick_on_the_wire() {
    assert!(
        serde_json::from_str::<TradingRules>(
            r#"{"common":{"price":{"quote_asset":{"kind":"currency","spec":"v1/0000000000000000000000000000000000000000000000000000000000000000"},"basis":{"kind":"per_asset_unit"},"tick":"0","minimum":null,"maximum":null},"quantity":{"unit":{"kind":"contracts"},"step":"1","minimum":null,"maximum":null},"notional":null,"sessions":"continuous-24x7","settlement_cycle":{"kind":"immediate"},"price_limits":{"kind":"none"}},"product":{"kind":"spot","rules":{"lot_policy":{"kind":"whole_lots","policy":{"lot_size":"1"}},"short_sale":"allowed"}}}"#
        )
        .is_err(),
        "a zero tick must be rejected while decoding"
    );
}

#[test]
fn trading_rules_should_round_trip_through_serde() {
    let rules = spot_rules();
    let encoded = serde_json::to_string(&rules).expect("rules should serialize");
    let decoded = serde_json::from_str::<TradingRules>(&encoded).expect("rules should decode");

    assert_eq!(decoded, rules);
}

/// Every state of a perpetual's margin terms is a valid rule set and survives a
/// round trip: `Unloaded` is what a venue that publishes them separately leaves
/// behind and `Published` what one that publishes them inline leaves, and a decode
/// that dropped either would lose what the venue published.
#[test]
fn every_perpetual_risk_state_should_round_trip_through_serde() {
    for risk in [
        PerpetualRisk::Unloaded,
        PerpetualRisk::Published(
            PerpetualMarginTerms::new(
                MarginProfile::new(
                    MarginMode::Cross,
                    Fraction::new("0.02".parse::<Ratio>().expect("ratio should parse"))
                        .expect("fraction should be valid"),
                    Fraction::new("0.01".parse::<Ratio>().expect("ratio should parse"))
                        .expect("fraction should be valid"),
                    usdt(),
                )
                .expect("margin should be valid"),
                vec![
                    LeverageTier::new(
                        Quantity::from(50_000_u32),
                        50,
                        Fraction::new("0.01".parse::<Ratio>().expect("ratio should parse"))
                            .expect("fraction should be valid"),
                    )
                    .expect("tier should be valid"),
                ],
            )
            .expect("margin terms should be valid"),
        ),
    ] {
        let mut rules = perpetual_rules();
        let ProductTradingRules::Perpetual(perpetual) = &mut rules.product else {
            panic!("fixture must be a perpetual listing")
        };
        perpetual.risk = risk.clone();

        assert_eq!(rules.validate(), Ok(()));
        let encoded = serde_json::to_string(&rules).expect("rules should serialize");
        let decoded = serde_json::from_str::<TradingRules>(&encoded).expect("rules should decode");
        assert_eq!(decoded, rules);
    }
}

#[test]
fn scheduled_listing_should_require_a_future_available_from() {
    let key = listing_key("BTCUSDT");

    assert_eq!(
        ListingSnapshot::new(
            key.clone(),
            spot_rules(),
            ListingStatus::Scheduled,
            None,
            None,
            1_000,
        ),
        Err(ModelError::ScheduledListingWithoutAvailability)
    );
    assert_eq!(
        ListingSnapshot::new(
            key.clone(),
            spot_rules(),
            ListingStatus::Scheduled,
            Some(1_000),
            None,
            1_000,
        ),
        Err(ModelError::ScheduledListingWithoutAvailability)
    );
    assert!(
        ListingSnapshot::new(
            key,
            spot_rules(),
            ListingStatus::Scheduled,
            Some(1_001),
            None,
            1_000,
        )
        .is_ok()
    );
}

#[test]
fn listing_window_should_be_half_open() {
    let snapshot = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        Some(100),
        Some(200),
        50,
    )
    .expect("snapshot should be valid");

    assert!(snapshot.is_available_at(100));
    assert!(snapshot.is_available_at(199));
    assert!(!snapshot.is_available_at(200));
    assert!(!snapshot.is_available_at(99));
    assert!(snapshot.allows_new_trading_at(150));
}

#[test]
fn halted_listing_should_not_allow_new_trading_but_still_settle() {
    let snapshot = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Halted,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");

    assert!(!snapshot.allows_new_trading_at(150));
    assert!(snapshot.status.allows_settlement());
    assert!(!ListingStatus::Settled.allows_settlement());
}

#[test]
fn listing_window_should_reject_an_inverted_range() {
    assert_eq!(
        ListingSnapshot::new(
            listing_key("BTCUSDT"),
            spot_rules(),
            ListingStatus::Trading,
            Some(200),
            Some(100),
            50,
        ),
        Err(ModelError::InvalidTimeOrder {
            field: "available_from",
            limit: "available_until"
        })
    );
}

#[test]
fn snapshot_should_validate_against_the_listed_instrument() {
    let snapshot = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");

    assert_eq!(snapshot.validate_for(&spot_instrument()), Ok(()));
}

#[test]
fn a_snapshot_of_another_listing_should_be_rejected() {
    let instrument = spot_instrument();
    let listing = VenueListing::new(
        listing_key("BTCUSDT"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");
    let foreign = ListingSnapshot::new(
        listing_key("ETHUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");
    let own = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");

    let mismatch = ModelError::from(ListingPublicationError::SnapshotKeyMismatch {
        listing: listing.key.clone(),
        snapshot: foreign.key.clone(),
    });

    assert_eq!(listing.validate_snapshot(&foreign), Err(mismatch.clone()));
    assert_eq!(foreign.validate_listing(&listing), Err(mismatch.clone()));
    assert_eq!(listing.validate_snapshot(&own), Ok(()));
    assert_eq!(own.validate_listing(&listing), Ok(()));
    assert_eq!(
        listing.validate_publication(&route(), &instrument, &foreign, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Err(mismatch),
        "the publication gate must reject a foreign snapshot instead of accepting it"
    );
    assert_eq!(
        listing.validate_publication(&route(), &instrument, &own, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Ok(()),
        "a listing with its own snapshot, route and instrument is publishable"
    );
}

#[test]
fn a_listing_of_another_route_should_be_rejected() {
    let instrument = spot_instrument();
    let listing = VenueListing::new(
        listing_key("BTCUSDT"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");
    let snapshot = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");
    let other = VenueRoute::new(
        VenueId::Binance,
        VenueScopeId::new("usd-m-perp").expect("scope should be valid"),
    );

    let route_mismatch = ModelError::ListingRouteMismatch {
        venue: VenueId::Binance,
        scope: VenueScopeId::new("usd-m-perp").expect("scope should be valid"),
        listing: listing.key.clone(),
    };

    assert_eq!(listing.validate_route(&other), Err(route_mismatch.clone()));
    assert_eq!(listing.validate_route(&route()), Ok(()));
    assert_eq!(
        listing.validate_publication(&other, &instrument, &snapshot, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Err(route_mismatch),
        "the publication gate must compare the listing route with the fetched route"
    );
}

#[test]
fn a_listing_of_another_instrument_should_be_rejected() {
    let instrument = spot_instrument();
    let other_instrument = Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(usdt(), btc()).expect("the swapped terms are still well formed"),
    ))
    .expect("instrument should be valid");
    let listing = VenueListing::new(
        listing_key("BTCUSDT"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");
    let snapshot = ListingSnapshot::new(
        listing_key("BTCUSDT"),
        spot_rules(),
        ListingStatus::Trading,
        None,
        None,
        50,
    )
    .expect("snapshot should be valid");

    assert_ne!(instrument.id(), other_instrument.id());
    assert_eq!(
        listing.validate_publication(&route(), &other_instrument, &snapshot, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Err(ModelError::InstrumentTermsMismatch {
            field: "venue_listing.instrument_id"
        }),
        "a listing must declare the instrument its canonical terms produce"
    );
    assert_eq!(
        listing.validate_publication(&route(), &instrument, &snapshot, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Ok(())
    );
}

/// A typed identifier that names a value none of the declared symbols carries is a
/// contradicted echo, and the publication gate reports it as the typed publication
/// error rather than only through the echo check it delegates to.
#[test]
fn publication_should_refuse_a_contradicted_echo_with_its_typed_reason() {
    let instrument = spot_instrument();
    let key = listing_key("BTCUSDT");
    let listing = VenueListing::new(
        key.clone(),
        instrument.id().clone(),
        Some(NativeSymbol::new("AAPL").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Equity(EquityListingIdentifiers {
            mic: Mic::new("xnas").expect("MIC should be valid"),
            board: None,
            ticker: "AAPL".into(),
            local_code: EquityLocalCode::new("MSFT").expect("local code should be valid"),
        }),
    )
    .expect("listing should be valid");
    let snapshot = ListingSnapshot::new(key, spot_rules(), ListingStatus::Trading, None, None, 50)
        .expect("snapshot should be valid");
    assert_eq!(
        listing.validate_echoed_identifiers(|_| SymbolNormalization::AsciiCaseInsensitive),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::EquityLocalCode,
            value: "MSFT".into(),
        })
    );
    assert_eq!(
        listing.validate_publication(&route(), &instrument, &snapshot, |_| {
            SymbolNormalization::AsciiCaseInsensitive
        }),
        Err(ListingPublicationError::IdentifierEchoMismatch {
            kind: ListingIdentifierKind::EquityLocalCode,
            value: "MSFT".into(),
        }
        .into()),
        "a contradicted echo must refuse publication with the typed reason that names it"
    );
}

#[test]
fn listing_should_accept_its_typed_identifiers() {
    let instrument = spot_instrument();
    let listing = VenueListing::new(
        listing_key("BTCUSDT"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        vec![NativeSymbol::new("BTC/USDT").expect("symbol should be valid")],
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");

    assert_eq!(listing.symbol().map(NativeSymbol::as_str), Some("BTCUSDT"));
    assert_eq!(listing.validate_for(&instrument), Ok(()));
}

#[test]
fn listing_should_reject_duplicate_aliases() {
    let instrument = spot_instrument();
    let listing = VenueListing::new(
        listing_key("BTCUSDT"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        vec![NativeSymbol::new("BTCUSDT").expect("symbol should be valid")],
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");

    assert_eq!(
        listing.validate_for(&instrument),
        Err(ModelError::DuplicateCollectionEntry {
            field: "venue_listing.aliases"
        })
    );
}

#[test]
fn tick_from_quantity_should_round_trip() {
    let tick = Tick::try_from("0.01".parse::<Quantity>().expect("quantity should parse"))
        .expect("tick should be valid");

    assert_eq!(tick.to_string(), "0.01");
}

#[test]
fn price_limits_should_reject_an_inverted_absolute_range() {
    assert_eq!(
        PriceLimitRule::Absolute {
            lower: Some(Price::from(10_u32)),
            upper: Some(Price::from(5_u32)),
        }
        .validate(),
        Err(ModelError::InvalidTimeRange)
    );
}

#[test]
fn listing_precision_should_read_both_increments_of_a_rule_set() {
    let precision = ListingPrecision::of(&spot_rules());

    assert_eq!(
        precision,
        ListingPrecision {
            price_tick: Tick::from_inner(10_000_000_000_000).expect("tick should be valid"),
            quantity_tick: Tick::try_from(Quantity::from(1_u8)).expect("tick should be valid"),
        }
    );
}

/// A label has to be written whatever a rule set carries, so a quantity step the
/// wire never states as positive is written at whole units.
#[test]
fn listing_precision_should_fall_back_to_whole_units_without_a_quantity_step() {
    let mut rules = spot_rules();
    rules.common.quantity.step = Quantity::ZERO;

    assert_eq!(
        ListingPrecision::of(&rules).quantity_tick,
        Tick::try_from(Quantity::from(1_u8)).expect("tick should be valid")
    );
}
