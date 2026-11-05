//! The fixtures the model and service tests share.

use market_participants_model::{
    asset::{AssetId, CryptoTokenId, CryptoTokenOrigin, TokenReference},
    identifier::{IdentifierNamespace, IdentifierValue, TokenIssuerId, VenueId, VenueScopeId},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        route::VenueRoute,
        venue::PolymarketProtocolVersion,
    },
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeLegRef, OutcomeMember, OutcomeSide, OutcomeTopology,
        ResolutionRule, SettlementFractionRule,
    },
    prediction::{
        PredictionBookLevel, PredictionBookLevelChange, PredictionCategory, PredictionEventRef,
        PredictionEventView, PredictionLeg, PredictionMarket, PredictionMarketGroup,
        PredictionMarketRef, PredictionNativeLeg, PredictionOrderBook,
    },
    settlement::Expiry,
};
use smol_str::{SmolStr, format_smolstr};

/// One event on the prediction fixture route.
pub(crate) fn event_ref() -> PredictionEventRef {
    PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the test scope is canonical"),
        ),
        native_event: IdentifierValue::new("event").expect("the test event id is valid"),
    }
}

/// One market with no sides, for a test that only reads the header's choices.
pub(crate) fn market(
    event: &PredictionEventRef,
    id: &str,
    group: &str,
    open: bool,
) -> PredictionMarket {
    PredictionMarket {
        market: PredictionMarketRef {
            event: event.clone(),
            native_market: IdentifierValue::new(id).expect("the test market id is valid"),
        },
        question: format_smolstr!("Question {id}"),
        label: format_smolstr!("Market {id}"),
        group: PredictionMarketGroup::new(group, group),
        headline: false,
        open,
        default_leg: None,
        legs: Vec::new(),
    }
}

/// One event view carrying the given markets.
pub(crate) fn event_with(markets: Vec<PredictionMarket>) -> PredictionEventView {
    PredictionEventView {
        event: event_ref(),
        title: SmolStr::new_static("An event"),
        category: PredictionCategory::new("sports", "Sports"),
        ends_at: None,
        markets,
    }
}

fn settlement_asset() -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("kytrix").expect("the fixture issuer is valid"),
            namespace: IdentifierNamespace::new("kytrix").expect("the fixture namespace is valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new("USDC").expect("the fixture token code is valid"),
            },
        })
        .expect("the fixture settlement token is valid"),
    )
}

/// One binary market whose two sides a test can chart.
///
/// The returned legs are in the order the group publishes them, so a test can
/// name the side it ticks without reading the group.
pub(crate) fn charted_market(
    event: &PredictionEventRef,
    id: &str,
) -> (PredictionMarket, Vec<OutcomeLegRef>) {
    let seed = OutcomeGroupSeed {
        question: format_smolstr!("Question {id}"),
        expiry: Expiry::new(4_102_444_800_000, None, None).expect("the expiry is valid"),
        topology: OutcomeTopology::Binary,
        settlement_asset: settlement_asset(),
        resolution: ResolutionRule::FixedAt {
            at: 4_102_444_800_000,
        },
        fraction_rule: SettlementFractionRule::Binary,
    };
    let seed_id = seed.seed_id();
    let yes = OutcomeSide::new(&seed_id, SmolStr::new_static("Yes")).expect("the side is valid");
    let no = OutcomeSide::new(&seed_id, SmolStr::new_static("No")).expect("the side is valid");
    let member = OutcomeMember::new(&seed_id, SmolStr::new_static("winner"), vec![yes.id, no.id])
        .expect("the member is valid");
    let group = OutcomeGroup::new(seed, vec![yes, no], vec![member]).expect("the group is valid");
    let legs: Vec<OutcomeLegRef> = group
        .members
        .iter()
        .flat_map(|member| {
            member.sides.iter().map(|side| OutcomeLegRef {
                group: group.id,
                member: member.id,
                side: *side,
            })
        })
        .collect();
    let labels = [SmolStr::new_static("Yes"), SmolStr::new_static("No")];
    let sides = legs
        .iter()
        .zip(labels)
        .map(|(leg, label)| PredictionLeg {
            leg: leg.clone(),
            label,
            probability: None,
            listing: ListingContinuityKey {
                route: event.route.clone(),
                terms: ListingContinuityTerms::Polymarket {
                    condition_id:
                        "0x0000000000000000000000000000000000000000000000000000000000000001"
                            .parse()
                            .expect("the fixture condition id is valid"),
                    asset_id: "1".parse().expect("the fixture asset id is valid"),
                    protocol: PolymarketProtocolVersion::V1,
                },
            }
            .listing_key()
            .expect("the fixture listing key is valid"),
            native: PredictionNativeLeg::Polymarket {
                protocol: PolymarketProtocolVersion::V1,
                asset_id: "1".parse().expect("the fixture asset id is valid"),
            },
        })
        .collect();
    let market = PredictionMarket {
        market: PredictionMarketRef {
            event: event.clone(),
            native_market: IdentifierValue::new(id).expect("the test market id is valid"),
        },
        question: format_smolstr!("Question {id}"),
        label: format_smolstr!("Market {id}"),
        group: PredictionMarketGroup::new("markets", "Markets"),
        headline: true,
        open: true,
        default_leg: Some(legs[0].clone()),
        legs: sides,
    };
    (market, legs)
}

/// One price level of a fixture book.
pub(crate) fn level(price: &str, size: &str) -> PredictionBookLevel {
    PredictionBookLevel {
        price: price.parse().expect("the fixture price is a fraction"),
        size: size.parse().expect("the fixture size is a quantity"),
    }
}

/// One absolute size change of a fixture book level.
pub(crate) fn change(price: &str, size: &str) -> PredictionBookLevelChange {
    PredictionBookLevelChange {
        price: price.parse().expect("the fixture price is a fraction"),
        size: size.parse().expect("the fixture size is a quantity"),
    }
}

/// One fixture book for `leg`, with no last trade and a fixed timestamp.
pub(crate) fn order_book(
    leg: OutcomeLegRef,
    tick: &str,
    bids: Vec<PredictionBookLevel>,
    asks: Vec<PredictionBookLevel>,
) -> PredictionOrderBook {
    PredictionOrderBook {
        leg,
        tick_size: tick.parse().expect("the fixture tick is valid"),
        bids,
        asks,
        last_trade_price: None,
        timestamp: 1_000,
    }
}
