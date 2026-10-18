use crate::{
    asset::{AssetId, CryptoTokenId, CryptoTokenOrigin, CurrencyId, Iso4217Code, TokenReference},
    decimal::{Price, Quantity},
    error::{ModelError, QuantityUnitExpectation},
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, TokenIssuerId, VenueId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, OutcomeTerms, PerpetualTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
        route::VenueRoute,
        venue::{BinanceProduct, PriceBasis},
    },
    market::{
        account::{AssetAmount, AssetBalance},
        book::{
            ListingOrderBook, ListingOrderBookLevel, ListingOrderBookUpdate, ListingReferencePrice,
            MarketSequence, ReferencePriceRole,
        },
        price::MarketPrice,
        selection::{InstrumentSelection, ListingTicker, OrderQuantity},
        units::{MarketQuantity, PositionSide, QuantityUnit, Side},
    },
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeMember, OutcomeSide, OutcomeTopology,
        ResolutionRule, SettlementFractionRule,
    },
    reference::PriceReferenceId,
    settlement::{ContractSize, ContractUnit, DerivativePnlModel, Expiry},
};

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

fn spot_instrument() -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(btc(), usdt()).expect("terms should be valid"),
    ))
    .expect("instrument should be valid")
}

fn listing_key() -> crate::listing::route::VenueListingKey {
    ListingContinuityKey {
        route: VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("scope should be valid"),
        ),
        terms: ListingContinuityTerms::Binance {
            product: BinanceProduct::Spot,
            instrument_id: NativeSymbol::new("BTCUSDT").expect("symbol should be valid"),
        },
    }
    .listing_key()
    .expect("listing key should be valid")
}

fn price() -> MarketPrice {
    MarketPrice {
        value: Price::from(100_u32),
        quote_asset: usdt(),
        basis: PriceBasis::PerAssetUnit,
    }
}

fn outcome_fixture() -> (Instrument, OutcomeMember) {
    let seed = OutcomeGroupSeed {
        question: "Will it rain?".into(),
        expiry: Expiry::new(2_000, Some(1_500), Some(2_500)).expect("expiry should be valid"),
        topology: OutcomeTopology::Binary,
        settlement_asset: usdt(),
        resolution: ResolutionRule::FixedAt { at: 1_900 },
        fraction_rule: SettlementFractionRule::AnyFraction,
    };
    let seed_id = seed.seed_id();
    let yes = OutcomeSide::new(&seed_id, "Yes".into()).expect("side should be valid");
    let no = OutcomeSide::new(&seed_id, "No".into()).expect("side should be valid");
    let member = OutcomeMember::new(
        &seed_id,
        "winner".into(),
        vec![yes.id.clone(), no.id.clone()],
    )
    .expect("member should be valid");
    let group =
        OutcomeGroup::new(seed, vec![yes.clone(), no], vec![member.clone()]).expect("group");
    let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
        group.id,
        member.id.clone(),
        yes.id,
    )))
    .expect("instrument should be valid");
    (instrument, member)
}

#[test]
fn amount_fields_should_require_a_notional_unit() {
    assert_eq!(
        MarketQuantity::amount(
            Quantity::from(1_u8),
            QuantityUnit::AssetUnits { asset: usdt() }
        ),
        Err(ModelError::QuantityUnitMismatch {
            field: "market_amount",
            expected: QuantityUnitExpectation::Notional,
        })
    );
    assert!(
        MarketQuantity::amount(
            Quantity::from(1_u8),
            QuantityUnit::Notional { asset: usdt() }
        )
        .is_ok()
    );
}

#[test]
fn asset_quantity_fields_should_reject_a_notional_unit() {
    assert_eq!(
        MarketQuantity::traded(
            Quantity::from(1_u8),
            QuantityUnit::Notional { asset: usdt() }
        ),
        Err(ModelError::QuantityUnitMismatch {
            field: "market_quantity",
            expected: QuantityUnitExpectation::TradedQuantity,
        })
    );
    assert!(
        MarketQuantity::traded(
            Quantity::from(1_u8),
            QuantityUnit::AssetUnits { asset: btc() }
        )
        .is_ok()
    );
}

#[test]
fn ticker_should_reject_a_misused_amount_field() {
    let ticker = ListingTicker {
        listing: listing_key(),
        monotonic_id: None,
        open: price(),
        high: None,
        low: None,
        close: price(),
        traded_quantity: Some(MarketQuantity {
            value: Quantity::from(1_u8),
            unit: QuantityUnit::AssetUnits { asset: btc() },
        }),
        traded_notional: Some(MarketQuantity {
            value: Quantity::from(1_u8),
            unit: QuantityUnit::AssetUnits { asset: usdt() },
        }),
    };

    assert_eq!(
        ticker.validate(),
        Err(ModelError::QuantityUnitMismatch {
            field: "traded_notional",
            expected: QuantityUnitExpectation::Notional,
        })
    );
}

#[test]
fn order_quantity_should_match_the_listed_unit() {
    let instrument = spot_instrument();
    let listed = QuantityUnit::AssetUnits { asset: btc() };
    let matching = OrderQuantity::AssetUnits {
        asset: btc(),
        value: Quantity::from(1_u8),
    };
    let mismatched = OrderQuantity::Contracts(Quantity::from(1_u8));

    assert!(
        matching
            .validate_against(&listed, None, &instrument)
            .is_ok()
    );
    assert_eq!(
        mismatched.validate_against(&listed, None, &instrument),
        Err(ModelError::QuantityUnitMismatch {
            field: "order_quantity",
            expected: QuantityUnitExpectation::ListedUnit,
        })
    );
}

#[test]
fn order_quantity_should_accept_a_notional_in_the_listed_asset() {
    let instrument = spot_instrument();
    let listed = QuantityUnit::AssetUnits { asset: btc() };
    let notional = OrderQuantity::Notional {
        asset: usdt(),
        value: Quantity::from(100_u8),
    };

    assert!(
        notional
            .validate_against(&listed, Some(&usdt()), &instrument)
            .is_ok()
    );
    assert_eq!(
        notional.validate_against(&listed, None, &instrument),
        Err(ModelError::QuantityUnitMismatch {
            field: "order_quantity",
            expected: QuantityUnitExpectation::NotionalAsset,
        })
    );
}

#[test]
fn order_quantity_should_match_the_outcome_instrument_credentials() {
    let (instrument, member) = outcome_fixture();
    let terms = instrument.terms();
    let InstrumentTerms::Outcome(spec) = &terms else {
        panic!("fixture must be an outcome instrument")
    };
    let matching = OrderQuantity::OutcomeTokens {
        group: spec.group().clone(),
        member: spec.member().clone(),
        side: spec.side().clone(),
        value: Quantity::from(1_u8),
    };
    let wrong_side = OrderQuantity::OutcomeTokens {
        group: spec.group().clone(),
        member: member.id.clone(),
        side: member
            .sides
            .iter()
            .find(|side| *side != spec.side())
            .expect("the fixture must declare a complementary side")
            .clone(),
        value: Quantity::from(1_u8),
    };

    assert!(matching.unit().validate_for_instrument(&instrument).is_ok());
    assert_eq!(
        wrong_side.unit().validate_for_instrument(&instrument),
        Err(ModelError::QuantityUnitMismatch {
            field: "quantity.unit",
            expected: QuantityUnitExpectation::OutcomeInstrument,
        })
    );
}

#[test]
fn market_price_should_match_the_listing_rule() {
    assert!(
        price()
            .validate_against(&usdt(), &PriceBasis::PerAssetUnit)
            .is_ok()
    );
    assert_eq!(
        price().validate_against(&btc(), &PriceBasis::PerAssetUnit),
        Err(ModelError::InstrumentTermsMismatch {
            field: "market_price.quote_asset"
        })
    );
}

#[test]
fn order_book_should_reject_a_crossed_snapshot() {
    let level = |value: u32| ListingOrderBookLevel {
        price: MarketPrice {
            value: Price::from(value),
            ..price()
        },
        quantity: MarketQuantity {
            value: Quantity::from(1_u8),
            unit: QuantityUnit::AssetUnits { asset: btc() },
        },
    };
    let book = ListingOrderBook {
        listing: listing_key(),
        sequence: MarketSequence::Numeric(1),
        timestamp: Some(1_700_000_000_000),
        bids: vec![level(101)],
        asks: vec![level(100)],
    };

    assert_eq!(book.validate(), Err(ModelError::CrossedOrderBook));
}

#[test]
fn order_book_should_reject_an_unsorted_side() {
    let level = |value: u32| ListingOrderBookLevel {
        price: MarketPrice {
            value: Price::from(value),
            ..price()
        },
        quantity: MarketQuantity {
            value: Quantity::from(1_u8),
            unit: QuantityUnit::Contracts,
        },
    };
    let book = ListingOrderBook {
        listing: listing_key(),
        sequence: MarketSequence::Numeric(1),
        timestamp: None,
        bids: vec![level(100), level(101)],
        asks: Vec::new(),
    };

    assert_eq!(
        book.validate(),
        Err(ModelError::UnsortedOrderBook { side: "bids" })
    );
}

#[test]
fn market_sequence_should_only_compare_like_representations() {
    assert_eq!(
        MarketSequence::Numeric(2).is_after(&MarketSequence::Numeric(1)),
        Some(true)
    );
    assert_eq!(
        MarketSequence::Native(IdentifierValue::new("b").expect("value should be valid"))
            .is_after(&MarketSequence::Numeric(1)),
        None
    );
}

#[test]
fn asset_balance_should_reject_available_above_total() {
    let balance = AssetBalance {
        asset: usdt(),
        total: Quantity::from(1_u8),
        available: Quantity::from(2_u8),
    };

    assert!(balance.validate().is_err());
}

#[test]
fn asset_amount_should_report_its_magnitude() {
    let amount = AssetAmount {
        asset: usdt(),
        value: "-5".parse().expect("amount should parse"),
    };

    assert_eq!(amount.magnitude(), Quantity::from(5_u8));
}

#[test]
fn selection_should_round_trip_through_serde() {
    let selection = InstrumentSelection {
        listing: listing_key(),
    };
    let encoded = serde_json::to_string(&selection).expect("selection should serialize");
    let decoded =
        serde_json::from_str::<InstrumentSelection>(&encoded).expect("selection should decode");

    assert_eq!(decoded, selection);
    assert!(encoded.contains("listing"));
}

#[test]
fn market_side_should_round_trip() {
    assert_eq!(
        serde_json::from_str::<Side>("\"buy\"").expect("side should decode"),
        Side::Buy
    );
    assert_eq!(
        serde_json::from_str::<PositionSide>("\"long\"").expect("side should decode"),
        PositionSide::Long
    );
}

#[test]
fn perpetual_instrument_should_remain_reachable_from_market_types() {
    let instrument = Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            crate::asset::DerivativeUnderlying::new(
                crate::asset::UnderlyingRef::Asset(btc()),
                PriceReferenceId::from_identity_value(&format!("v1/{}", "0".repeat(64)))
                    .expect("reference identity"),
            ),
            usdt(),
            usdt(),
            ContractSize::new(
                ContractUnit::asset_quantity(Quantity::from(1_u8), btc())
                    .expect("unit should be valid"),
            )
            .expect("contract size should be valid"),
            DerivativePnlModel::Linear,
            None,
        )
        .expect("terms should be valid"),
    ))
    .expect("instrument should be valid");

    assert_eq!(
        instrument.kind(),
        crate::instrument::InstrumentKind::Perpetual
    );
}

#[test]
fn ticker_should_round_trip_through_serde() {
    let ticker = ListingTicker {
        listing: listing_key(),
        monotonic_id: Some(7),
        open: price(),
        high: None,
        low: None,
        close: price(),
        traded_quantity: None,
        traded_notional: Some(MarketQuantity {
            value: Quantity::from(1_000_u32),
            unit: QuantityUnit::Notional { asset: usdt() },
        }),
    };
    let encoded = serde_json::to_string(&ticker).expect("ticker should serialize");
    let decoded = serde_json::from_str::<ListingTicker>(&encoded).expect("ticker should decode");

    assert_eq!(decoded, ticker);
    assert_eq!(ticker.validate(), Ok(()));
}

#[test]
fn identifiers_should_serialize_with_explicit_names() {
    let identifiers = VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
        product: BinanceProduct::Spot,
        future_series: None,
        option_series: None,
    });
    let encoded = serde_json::to_string(&identifiers).expect("identifiers should serialize");

    assert!(encoded.contains("\"product\""));
    assert!(encoded.contains("\"spot\""));
    assert!(!encoded.contains("futureSeries"));
}

#[test]
fn currency_assets_should_be_usable_as_a_quote() {
    let usd = AssetId::Currency(CurrencyId::new(
        Iso4217Code::new("USD").expect("USD should be valid"),
    ));
    let currency_price = MarketPrice {
        value: Price::from(1_u32),
        quote_asset: usd.clone(),
        basis: PriceBasis::PerAssetUnit,
    };

    assert!(
        currency_price
            .validate_against(&usd, &PriceBasis::PerAssetUnit)
            .is_ok()
    );
    assert_eq!(
        MarketQuantity::amount(Quantity::ZERO, QuantityUnit::Contracts),
        Err(ModelError::QuantityUnitMismatch {
            field: "market_amount",
            expected: QuantityUnitExpectation::Notional,
        })
    );
}

#[test]
fn reference_price_should_require_a_registered_reference() {
    let reference = PriceReferenceId::from_identity_value(&format!("v1/{}", "ab".repeat(32)))
        .expect("reference identity should be well formed");
    let priced = ListingReferencePrice {
        listing: listing_key(),
        reference: reference.clone(),
        role: Some(ReferencePriceRole::Mark),
        price: price(),
        timestamp: 7,
    };
    let encoded = serde_json::to_string(&priced).expect("reference price should serialize");

    assert!(encoded.contains(reference.as_str()));
    assert!(encoded.contains("\"role\":\"mark\""));
    assert_eq!(
        serde_json::from_str::<ListingReferencePrice>(&encoded).expect("should decode"),
        priced
    );

    let roleless = ListingReferencePrice {
        role: None,
        ..priced.clone()
    };
    let roleless_encoded =
        serde_json::to_string(&roleless).expect("a roleless reference price should serialize");

    assert!(roleless_encoded.contains("\"role\":null"));
    assert_eq!(
        serde_json::from_str::<ListingReferencePrice>(&roleless_encoded).expect("should decode"),
        roleless,
        "the display role is optional"
    );
    assert!(
        serde_json::from_str::<ListingReferencePrice>(
            &encoded.replace("\"mark\"", "\"settlement\"")
        )
        .is_err(),
        "settlement is not a public reference price role"
    );
    assert!(
        serde_json::from_str::<ListingReferencePrice>(&encoded.replace("\"mark\"", "\"last\""))
            .is_err(),
        "last is not a public reference price role"
    );
    let without_reference = format!(
        r#"{{"listing":{},"role":"mark","price":{},"timestamp":7}}"#,
        serde_json::to_string(&priced.listing).expect("listing should serialize"),
        serde_json::to_string(&priced.price).expect("price should serialize")
    );

    assert!(
        serde_json::from_str::<ListingReferencePrice>(&without_reference).is_err(),
        "a reference price without its registered reference must be rejected"
    );
}

#[test]
fn order_book_update_timestamp_should_stay_optional() {
    let sequence = MarketSequence::Numeric(1);
    let update = ListingOrderBookUpdate {
        listing: listing_key(),
        snapshot: true,
        first_sequence: sequence.clone(),
        last_sequence: sequence,
        previous_sequence: None,
        timestamp: None,
        bids: Vec::new(),
        asks: Vec::new(),
    };
    let encoded = serde_json::to_string(&update).expect("update should serialize");

    assert!(encoded.contains("\"timestamp\":null"));
    assert_eq!(
        serde_json::from_str::<ListingOrderBookUpdate>(&encoded).expect("update should decode"),
        update
    );
    assert_eq!(update.validate(), Ok(()));
    let stamped = ListingOrderBookUpdate {
        timestamp: Some(9),
        ..update
    };

    assert_eq!(
        serde_json::from_str::<ListingOrderBookUpdate>(
            &serde_json::to_string(&stamped).expect("stamped update should serialize")
        )
        .expect("stamped update should decode"),
        stamped,
        "a venue that publishes a timestamp still decodes"
    );
}
