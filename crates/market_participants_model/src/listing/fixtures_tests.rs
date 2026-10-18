//! Model-layer fixtures for the product families this build trades nowhere yet.
//!
//! The five product families, the outcome relations, the equity identifiers and
//! the standard series identifiers are asserted against the real semantics of
//! Polymarket, US equities, SSE/SZSE, OCC options and standard futures. What
//! makes one of those families what it is is its continuity terms and its typed
//! identifiers — the MIC, the issuer, the share class, the OCC symbol, the
//! condition id — and those are what these fixtures pin.
//!
//! What they do **not** pin is the venue. A closed venue set means a fixture of a
//! market this build trades nowhere cannot be addressed at the market participant that really
//! lists it, so every fixture here is addressed at [`VenueId::Binance`] and the
//! family is carried by its scope and its own terms. The venue is still one of the
//! terms a venue listing is hashed from — the pinned listing identity in
//! `identity_tests` moved when the fixture's route did — so a fixture here is an
//! addressed-at-Binance market of that family rather than a venue-neutral one, and
//! it exercises no route-aware validation: no layer refuses a Polymarket condition
//! on a Binance route, which is the same combination production never produces.
//! Nothing registers a route, capability or placeholder for those markets.

use crate::{
    asset::{
        AssetId, CryptoTokenId, CryptoTokenOrigin, SecurityForm, SecurityId, SecurityIssuerId,
        ShareClassId, ShareClassTerms, TokenReference, UnderlyingRef,
    },
    decimal::{Fraction, PositiveRatio, Price, Quantity, Ratio},
    error::ModelError,
    identifier::{
        BoardId, ContractCode, CtfConditionId, CtfTokenId, EquityLocalCode, IdentifierNamespace,
        IdentifierValue, LocalSymbol, MarketParticipantSeriesId, Mic, NativeSymbol, OccSymbol,
        PolymarketEventId, PolymarketMarketId, ReferenceSourceId, ReferenceTermsVersion,
        TokenIssuerId, VenueId, VenueScopeId,
    },
    instrument::{FutureTerms, Instrument, InstrumentTerms, OptionTerms, OutcomeTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        identifiers::{EquityListingIdentifiers, VenueListingIdentifiers},
        listing_impl::VenueListing,
        route::{VenueListingKey, VenueRoute},
        venue::PriceBasis,
    },
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeMember, OutcomeRiskPool, OutcomeSide,
        OutcomeTopology, ResolutionRule, SelectionCardinality, SettlementFractionRule,
    },
    reference::{
        PriceReferenceId, PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
        ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    settlement::{
        ContractSize, ContractUnit, DeliveryDirection, DeliveryLeg, DeliveryMode,
        DerivativePnlModel, ExerciseStyle, Expiry, InstrumentAdjustment, InstrumentLineageEdge,
        OptionRight, StrikePrice, validate_lineage,
    },
};

fn usdc() -> AssetId {
    token("USDC")
}

fn token(code: &str) -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("hyperliquid").expect("issuer should be valid"),
            namespace: IdentifierNamespace::new("hyperliquid").expect("namespace should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("code should be valid"),
            },
        })
        .expect("token should be valid"),
    )
}

fn reference(underlying: UnderlyingRef) -> PriceReferenceId {
    PriceReferenceTerms {
        underlying,
        quote_asset: usdc(),
        basis: crate::listing::venue::PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new("binance").expect("source should be valid"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("version should be valid"),
    }
    .reference_id()
    .expect("reference should be valid")
}

fn expiry() -> Expiry {
    Expiry::new(2_000, Some(1_800), Some(2_100)).expect("expiry should be valid")
}

fn share_class(issuer: &str, designation: &str, form: SecurityForm) -> ShareClassId {
    let adr_ratio = (form == SecurityForm::Adr).then(|| Quantity::from(10_u8));
    ShareClassId::new(&ShareClassTerms {
        issuer: SecurityIssuerId::new(issuer).expect("issuer should be valid"),
        designation: designation.into(),
        form,
        par_value: None,
        adr_ratio,
    })
    .expect("share class should be valid")
}

fn equity_instrument(issuer: &str, designation: &str, form: SecurityForm) -> Instrument {
    let security = AssetId::Security(SecurityId::new(
        SecurityIssuerId::new(issuer).expect("issuer should be valid"),
        share_class(issuer, designation, form),
        form,
    ));
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(security, usdc()).expect("terms should be valid"),
    ))
    .expect("instrument should be valid")
}

/// The route a fixture of a market this build trades nowhere is addressed at.
///
/// The scope names the product family the fixture is really about, and the venue
/// is [`VenueId::Binance`] — a venue this build speaks — because a route is an
/// addressing key and it cannot name a venue outside the closed set.
///
/// The venue is **not** incidental to a listing's identity: it is one of the
/// continuity terms a venue listing is hashed from, so a fixture here is an
/// addressed-at-Binance market whose *family* is named by its scope and its own
/// typed identifiers. That is a combination production does not produce — no
/// adapter publishes a Polymarket condition or an OCC symbol under Binance — and
/// it is deliberate: these fixtures exist to pin each family's own terms and
/// identifier validation, which the venue does not take part in, and the closed
/// venue set leaves no other way to address them.
fn fixture_route(scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::Binance,
        VenueScopeId::new(scope).expect("scope should be valid"),
    )
}

fn equity_listing(
    mic: &str,
    board: Option<&str>,
    ticker: &str,
    instrument: &Instrument,
) -> VenueListing {
    let mic = Mic::new(mic).expect("mic should be valid");
    let local_code = EquityLocalCode::new(ticker).expect("local code should be valid");
    let key = ListingContinuityKey {
        route: fixture_route("equity"),
        terms: ListingContinuityTerms::Equity {
            issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
            share_class: share_class("cik/320193", "common", SecurityForm::CommonStock),
            mic: mic.clone(),
            local_code: local_code.clone(),
        },
    }
    .listing_key()
    .expect("listing key should be valid");

    VenueListing::new(
        key,
        instrument.id().clone(),
        Some(NativeSymbol::new(ticker).expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Equity(EquityListingIdentifiers {
            mic,
            board: board.map(|board| BoardId::new(board).expect("board should be valid")),
            ticker: ticker.into(),
            local_code,
        }),
    )
    .expect("listing should be valid")
}

fn polymarket_group(question: &str, labels: &[&str], topology: OutcomeTopology) -> OutcomeGroup {
    let seed = OutcomeGroupSeed {
        question: question.into(),
        expiry: expiry(),
        topology,
        settlement_asset: usdc(),
        resolution: ResolutionRule::FixedAt { at: 1_900 },
        fraction_rule: SettlementFractionRule::AnyFraction,
    };
    let seed_id = seed.seed_id();
    let sides: Vec<OutcomeSide> = labels
        .iter()
        .map(|label| OutcomeSide::new(&seed_id, (*label).into()).expect("side should be valid"))
        .collect();
    let members: Vec<OutcomeMember> = if labels.len() == 2 {
        // A binary market is exactly one member with two complementary sides.
        vec![
            OutcomeMember::new(
                &seed_id,
                "winner".into(),
                vec![sides[0].id.clone(), sides[1].id.clone()],
            )
            .expect("member should be valid"),
        ]
    } else {
        sides
            .iter()
            .enumerate()
            .map(|(index, side)| {
                OutcomeMember::new(
                    &seed_id,
                    format!("member-{index}").into(),
                    vec![side.id.clone()],
                )
                .expect("member should be valid")
            })
            .collect()
    };
    OutcomeGroup::new(seed, sides, members).expect("group should be valid")
}

#[test]
fn polymarket_binary_market_should_model_one_complementary_pair() {
    let group = polymarket_group(
        "Will it rain tomorrow?",
        &["Yes", "No"],
        OutcomeTopology::Binary,
    );
    let yes = OutcomeTerms::new(
        group.id.clone(),
        group.members[0].id.clone(),
        group.members[0].sides[0].clone(),
    );
    let instrument =
        Instrument::new(InstrumentTerms::Outcome(yes)).expect("instrument should be valid");

    assert_eq!(group.tradable_legs().len(), 2);
    assert_eq!(group.members.len(), 1);
    assert_eq!(group.members[0].sides.len(), 2);
    assert_eq!(
        instrument.kind(),
        crate::instrument::InstrumentKind::Outcome
    );
    assert_eq!(
        OutcomeTopology::MutuallyExclusive(SelectionCardinality::ExactlyOne).cardinality(),
        Some(SelectionCardinality::ExactlyOne)
    );
    assert_eq!(OutcomeTopology::Binary.cardinality(), None);
}

#[test]
fn polymarket_negative_risk_fixture_should_relate_two_groups_through_a_pool() {
    let first = polymarket_group(
        "Will it rain tomorrow?",
        &["Yes", "No"],
        OutcomeTopology::Binary,
    );
    let second = polymarket_group(
        "Will it snow tomorrow?",
        &["Yes", "No"],
        OutcomeTopology::Binary,
    );
    let pool = OutcomeRiskPool::new(
        usdc(),
        vec![first.id.clone(), second.id.clone()],
        vec![crate::outcome::OutcomeConversion {
            input: crate::outcome::OutcomeLegRef {
                group: first.id.clone(),
                member: first.members[0].id.clone(),
                side: first.members[0].sides[0].clone(),
            },
            outputs: vec![crate::outcome::OutcomeConversionOutput {
                leg: crate::outcome::OutcomeLegRef {
                    group: second.id.clone(),
                    member: second.members[0].id.clone(),
                    side: second.members[0].sides[0].clone(),
                },
                factor: PositiveRatio::new(Ratio::from(1_u8)).expect("ratio should be valid"),
            }],
        }],
    )
    .expect("pool should be valid");

    assert!(pool.contains(&first.id));
    assert!(pool.contains(&second.id));
    assert_eq!(pool.conversions.len(), 1);
}

#[test]
fn polymarket_fractional_settlement_should_accept_any_fraction() {
    let group = polymarket_group(
        "How much rain will fall?",
        &["a", "b", "c"],
        OutcomeTopology::MutuallyExclusive(SelectionCardinality::AtMostOne),
    );
    let half = Fraction::new("0.5".parse::<Ratio>().expect("ratio should parse"))
        .expect("half is a fraction");

    assert!(
        SettlementFractionRule::AnyFraction
            .validate_fraction(&group.id, half)
            .is_ok()
    );
    assert!(
        SettlementFractionRule::Binary
            .validate_fraction(&group.id, half)
            .is_err(),
        "a binary rule must reject a fractional settlement"
    );
}

#[test]
fn polymarket_listing_identifiers_should_keep_condition_and_token_apart() {
    let condition =
        CtfConditionId::new("0x".to_owned() + &"1".repeat(64)).expect("condition should be valid");
    let token = CtfTokenId::new("123456789").expect("token should be valid");
    let key = ListingContinuityKey {
        route: fixture_route("ctf-clob/polygon"),
        terms: ListingContinuityTerms::Polymarket {
            condition_id: condition.clone(),
            clob_token_id: token.clone(),
        },
    }
    .listing_id()
    .expect("listing should be valid");
    let event = PolymarketEventId::new("event-1").expect("event should be valid");
    let market = PolymarketMarketId::new("market-1").expect("market should be valid");

    assert!(key.as_str().starts_with("listing-id/v1/"));
    assert_ne!(condition.as_str(), token.as_str());
    assert_ne!(event.as_str(), market.as_str());
}

#[test]
fn us_equity_share_classes_should_keep_distinct_security_identities() {
    let common = equity_instrument("cik/320193", "common", SecurityForm::CommonStock);
    let preferred = equity_instrument("cik/320193", "preferred", SecurityForm::PreferredStock);
    let adr = equity_instrument("cik/320193", "adr", SecurityForm::Adr);

    assert_ne!(common.id(), preferred.id());
    assert_ne!(common.id(), adr.id());
    assert_eq!(
        equity_instrument("cik/320193", "common", SecurityForm::CommonStock).id(),
        common.id()
    );
}

#[test]
fn adr_listing_should_resolve_through_issuer_share_class_and_mic() {
    let common = equity_instrument("cik/320193", "common", SecurityForm::CommonStock);
    let listing = equity_listing("XNAS", Some("nasdaq-global-select"), "AAPL", &common);

    assert_eq!(listing.validate_for(&common), Ok(()));
    assert_eq!(listing.symbol().map(NativeSymbol::as_str), Some("AAPL"));
    assert!(matches!(
        listing.identifiers,
        VenueListingIdentifiers::Equity(_)
    ));
}

#[test]
fn same_local_code_on_two_markets_should_keep_two_listing_identities() {
    let instrument = equity_instrument("cik/900000", "common", SecurityForm::CommonStock);
    let sse = equity_listing("XSHG", Some("main-board"), "600000", &instrument);
    let szse = equity_listing("XSHE", Some("main-board"), "600000", &instrument);

    assert_ne!(sse.key.listing_id, szse.key.listing_id);
    assert_eq!(sse.key.route, szse.key.route);
}

#[test]
fn occ_option_series_should_keep_american_and_european_apart() {
    let underlying = crate::asset::DerivativeUnderlying {
        reference: UnderlyingRef::Asset(usdc()),
        price_reference: reference(UnderlyingRef::Asset(usdc())),
    };
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(100_u16), usdc())
            .expect("unit should be valid"),
    )
    .expect("contract size should be valid");
    let strike = StrikePrice::new(
        Price::from(150_u32),
        usdc(),
        crate::listing::venue::PriceBasis::PerAssetUnit,
    )
    .expect("strike should be valid");
    let cash = DeliveryMode::cash(usdc(), reference(UnderlyingRef::Asset(usdc())), None)
        .expect("delivery should be valid");
    let american = OptionTerms::new(
        underlying.clone(),
        usdc(),
        OptionRight::Call,
        strike.clone(),
        expiry(),
        ExerciseStyle::American,
        contract_size.clone(),
        cash.clone(),
        None,
    )
    .expect("terms should be valid");
    let european = OptionTerms::new(
        underlying,
        usdc(),
        OptionRight::Call,
        strike,
        expiry(),
        ExerciseStyle::European,
        contract_size,
        cash,
        None,
    )
    .expect("terms should be valid");

    assert_ne!(
        Instrument::new(InstrumentTerms::Option(american))
            .expect("instrument should be valid")
            .id(),
        Instrument::new(InstrumentTerms::Option(european))
            .expect("instrument should be valid")
            .id()
    );
    assert_eq!(ExerciseStyle::American.label(), "american");
    assert!(matches!(
        ExerciseStyle::Bermudan {
            exercise_at: vec![1_900]
        }
        .validate(&expiry()),
        Ok(())
    ));
}

#[test]
fn adjusted_option_deliverable_should_create_a_successor_instrument() {
    let underlying = |stock: &str| crate::asset::DerivativeUnderlying {
        reference: UnderlyingRef::Asset(token(stock)),
        price_reference: reference(UnderlyingRef::Asset(usdc())),
    };
    let option = |stock: u8, cash: u8| {
        let deliveries = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                token("AAPL"),
                Quantity::from(stock),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
            DeliveryLeg::new(usdc(), Quantity::from(cash), DeliveryDirection::Deliver)
                .expect("leg should be valid"),
        ])
        .expect("legs should not contradict each other");
        OptionTerms::new(
            underlying("AAPL"),
            usdc(),
            OptionRight::Call,
            StrikePrice::new(Price::from(150_u32), usdc(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::American,
            ContractSize::new(
                ContractUnit::asset_quantity(Quantity::from(stock), token("AAPL"))
                    .expect("unit should be valid"),
            )
            .expect("contract size should be valid"),
            deliveries,
            None,
        )
        .expect("terms should be valid")
    };
    let original = Instrument::new(InstrumentTerms::Option(option(100, 100)))
        .expect("instrument should be valid");
    let adjusted = Instrument::new(InstrumentTerms::Option(option(150, 150)))
        .expect("instrument should be valid");
    let original_id = original.id().clone();
    let edge = InstrumentLineageEdge::new(
        original_id.clone(),
        adjusted.id().clone(),
        InstrumentAdjustment::DeliverableAdjustment,
        2_000,
    )
    .expect("the adjustment lineage edge should be valid");

    assert_ne!(
        original.id(),
        adjusted.id(),
        "an adjusted deliverable is a new canonical contract"
    );
    assert_eq!(original.kind(), crate::instrument::InstrumentKind::Option);
    assert_eq!(edge.predecessor, original_id);
    assert_eq!(edge.successor, *adjusted.id());
    assert_eq!(edge.reason, InstrumentAdjustment::DeliverableAdjustment);
    assert_eq!(
        validate_lineage(&[edge], |field| ModelError::InstrumentLineageInvalid {
            field
        }),
        Ok(()),
        "the adjustment lineage must be a valid one-edge graph"
    );
    assert_eq!(
        original.id(),
        &original_id,
        "lineage never rewrites the predecessor identity"
    );
    assert_eq!(
        InstrumentLineageEdge::new(
            original_id.clone(),
            original_id,
            InstrumentAdjustment::DeliverableAdjustment,
            2_000,
        ),
        Err(ModelError::InstrumentLineageInvalid {
            field: "successor equals predecessor"
        })
    );
}

#[test]
fn standard_future_series_should_require_a_typed_identifier() {
    let empty = FutureSeriesIdentifiers::default();
    let typed = FutureSeriesIdentifiers {
        market_participant_series_id: Some(
            MarketParticipantSeriesId::new("ES-202612").expect("series id should be valid"),
        ),
        local_symbol: Some(LocalSymbol::new("ESZ6").expect("local symbol should be valid")),
        contract_code: Some(ContractCode::new("ES").expect("contract code should be valid")),
    };

    assert!(empty.validate().is_err());
    assert!(typed.validate().is_ok());
}

#[test]
fn future_series_continuity_should_follow_the_typed_identifier() {
    let route = fixture_route("futures");
    let series = |local_symbol: &str| {
        ListingContinuityKey {
            route: route.clone(),
            terms: ListingContinuityTerms::FutureSeries(FutureSeriesIdentifiers {
                market_participant_series_id: None,
                local_symbol: Some(
                    LocalSymbol::new(local_symbol).expect("local symbol should be valid"),
                ),
                contract_code: None,
            }),
        }
        .listing_id()
        .expect("listing should be valid")
    };

    assert_eq!(series("ESZ6"), series("ESZ6"));
    assert_ne!(series("ESZ6"), series("ESH7"));
}

#[test]
fn future_expiry_delivery_and_settlement_window_should_be_typed_terms() {
    let underlying = crate::asset::DerivativeUnderlying {
        reference: UnderlyingRef::Asset(usdc()),
        price_reference: reference(UnderlyingRef::Asset(usdc())),
    };
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(50_u8), usdc()).expect("unit should be valid"),
    )
    .expect("contract size should be valid");
    let future = FutureTerms::new(
        underlying,
        usdc(),
        DerivativePnlModel::Linear,
        contract_size,
        Expiry::new(2_000, Some(1_800), Some(2_100)).expect("expiry should be valid"),
        Some(1_700),
        DeliveryMode::cash(usdc(), reference(UnderlyingRef::Asset(usdc())), None)
            .expect("delivery should be valid"),
        None,
    )
    .expect("terms should be valid");

    assert_eq!(future.expiry().expiration_at(), 2_000);
    assert_eq!(future.expiry().last_trading_at(), Some(1_800));
    assert_eq!(future.first_notice_at(), Some(1_700));
    assert!(future.delivery().cash_asset().is_some());
    assert!(matches!(future.delivery(), DeliveryMode::Cash { .. }));
}

#[test]
fn option_series_identifiers_should_allow_each_series_field() {
    let occ = OptionSeriesIdentifiers {
        occ_symbol: Some(OccSymbol::new("AAPL261218C00150000").expect("occ should be valid")),
        market_participant_series_id: None,
        local_symbol: None,
    };
    let local = OptionSeriesIdentifiers {
        occ_symbol: None,
        market_participant_series_id: Some(
            MarketParticipantSeriesId::new("AAPL-20261218-C150").expect("series should be valid"),
        ),
        local_symbol: Some(LocalSymbol::new("AAPL261218C150").expect("symbol should be valid")),
    };

    assert!(occ.validate().is_ok());
    assert!(local.validate().is_ok());
    assert!(OptionSeriesIdentifiers::default().validate().is_err());
}

#[test]
fn listing_key_of_every_fixture_should_be_unique_per_route() {
    let instrument = equity_instrument("cik/320193", "common", SecurityForm::CommonStock);
    let listing = equity_listing("XNAS", None, "AAPL", &instrument);
    let keys: Vec<VenueListingKey> = crate::listing::navigation::sorted_listing_keys(vec![
        listing.key.clone(),
        listing.key.clone(),
    ]);

    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].listing_id, listing.key.listing_id);
}
