use market_participants_model::{
    asset::{
        AssetId, CryptoTokenOrigin, CryptoTokenTerms, DerivativeUnderlying, TokenReference,
        UnderlyingRef,
    },
    decimal::{Fraction, Quantity, Ratio},
    error::ModelError,
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, ReferenceSourceId,
        ReferenceTermsVersion, TokenIssuerId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, PerpetualTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::HyperliquidListingIdentifiers,
        route::{VenueListingKey, VenueRoute},
        venue::{
            HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidOutcomeId, HyperliquidTokenId,
            PriceBasis,
        },
    },
    reference::{
        PriceReferenceTerms, ReferenceAggregation, ReferenceObservation, ReferenceObservationKind,
        ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
    settlement::{ContractSize, ContractUnit, DerivativePnlModel},
};
#[cfg(test)]
use market_participants_model::{
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeMember, OutcomeSide, OutcomeSideId, OutcomeTopology,
        ResolutionRule, SettlementFractionRule,
    },
    settlement::Expiry,
};
use smol_str::{SmolStr, format_smolstr};

#[cfg(test)]
use super::types::HyperliquidOutcomeSpec;
use super::types::{HyperliquidPerpDex, HyperliquidSpotToken};
use crate::{
    catalog::{registry::asset::AssetTerms, source::CatalogSourceId},
    error::MarketParticipantError,
    routing::{HYPERLIQUID_VENUE, RouteCapabilityDescriptor, VenueRouteDescriptor},
};

pub(super) const VENUE: &str = HYPERLIQUID_VENUE;

pub(super) const PERP_DEX_SCOPE_PREFIX: &str = "perp-dex/";

pub(super) const OUTCOME_VENUE_SCOPE_PREFIX: &str = "outcome-venue/";

const SPOT_ASSET_ID_BASE: u32 = 10_000;
const PERP_DEX_ASSET_ID_BASE: u32 = 100_000;
const PERP_DEX_ASSET_ID_STRIDE: u32 = 10_000;
const OUTCOME_ASSET_ID_BASE: u64 = 100_000_000;

const MIN_SCOPE_NAME_LENGTH: usize = 2;
const MAX_SCOPE_NAME_LENGTH: usize = 4;

pub(crate) fn perp_dex_name(scope: &VenueScopeId) -> Option<&str> {
    scope
        .as_str()
        .strip_prefix(PERP_DEX_SCOPE_PREFIX)
        .filter(|name| !name.is_empty())
}

pub(crate) fn perp_dex_scope(name: &str) -> Result<VenueScopeId, MarketParticipantError> {
    if !is_deployable_scope_name(name) {
        return Err(protocol_error(
            "build a Hyperliquid perpetual DEX scope",
            format_smolstr!("the venue reserves no scope shape for the name {name}"),
        ));
    }
    VenueScopeId::new(format_smolstr!("{PERP_DEX_SCOPE_PREFIX}{name}"))
        .map_err(|error| protocol_error("build a Hyperliquid perpetual DEX scope", error))
}

pub(crate) fn outcome_venue_scope(name: &str) -> Result<VenueScopeId, MarketParticipantError> {
    VenueScopeId::new(format_smolstr!("{OUTCOME_VENUE_SCOPE_PREFIX}{name}"))
        .map_err(|error| protocol_error("build a Hyperliquid outcome venue scope", error))
}

pub(crate) fn is_deployable_scope_name(name: &str) -> bool {
    (MIN_SCOPE_NAME_LENGTH..=MAX_SCOPE_NAME_LENGTH).contains(&name.len())
        && name.bytes().all(|byte| byte.is_ascii_lowercase())
}

pub(super) fn spot_asset_id(index: u32) -> Result<HyperliquidAssetId, MarketParticipantError> {
    SPOT_ASSET_ID_BASE
        .checked_add(index)
        .map(HyperliquidAssetId)
        .ok_or_else(|| {
            protocol_error(
                "derive a Hyperliquid spot asset ID",
                SmolStr::new_static("the spot asset ID overflowed"),
            )
        })
}

pub(super) fn perp_asset_id(
    perp_dex_index: u32,
    index_in_meta: u32,
) -> Result<HyperliquidAssetId, MarketParticipantError> {
    if perp_dex_index == 0 {
        return Ok(HyperliquidAssetId(index_in_meta));
    }
    perp_dex_index
        .checked_mul(PERP_DEX_ASSET_ID_STRIDE)
        .and_then(|offset| PERP_DEX_ASSET_ID_BASE.checked_add(offset))
        .and_then(|base| base.checked_add(index_in_meta))
        .map(HyperliquidAssetId)
        .ok_or_else(|| {
            protocol_error(
                "derive a Hyperliquid perpetual asset ID",
                SmolStr::new_static("the perpetual asset ID overflowed"),
            )
        })
}

pub(crate) fn outcome_encoding(outcome: u64, side: u64) -> Result<u64, MarketParticipantError> {
    if side > 1 {
        return Err(protocol_error(
            "derive a Hyperliquid outcome encoding",
            SmolStr::new_static("an outcome has exactly the sides 0 and 1"),
        ));
    }
    outcome
        .checked_mul(10)
        .and_then(|encoding| encoding.checked_add(side))
        .ok_or_else(|| {
            protocol_error(
                "derive a Hyperliquid outcome encoding",
                SmolStr::new_static("the outcome encoding overflowed"),
            )
        })
}

pub(crate) fn outcome_asset_id(
    encoding: u64,
) -> Result<HyperliquidAssetId, MarketParticipantError> {
    OUTCOME_ASSET_ID_BASE
        .checked_add(encoding)
        .and_then(|asset| u32::try_from(asset).ok())
        .map(HyperliquidAssetId)
        .ok_or_else(|| {
            protocol_error(
                "derive a Hyperliquid outcome asset ID",
                SmolStr::new_static("the outcome asset ID overflowed"),
            )
        })
}

pub(crate) fn outcome_coin(encoding: u64) -> SmolStr {
    format_smolstr!("#{encoding}")
}

#[cfg(test)]
pub(crate) fn outcome_token_name(encoding: u64) -> SmolStr {
    format_smolstr!("+{encoding}")
}

pub(super) fn token_asset(code: &str) -> Result<AssetTerms, MarketParticipantError> {
    let issuer = TokenIssuerId::new(VENUE)
        .map_err(|error| protocol_error("build a Hyperliquid token issuer", error))?;
    let namespace = IdentifierNamespace::new(VENUE)
        .map_err(|error| protocol_error("build a Hyperliquid token namespace", error))?;
    let reference = IdentifierValue::new(code)
        .map_err(|error| protocol_error("build a Hyperliquid token code", error))?;
    let terms = CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer,
            namespace,
            token: TokenReference::Native { code: reference },
        },
    };
    terms.validate().map_err(model_error)?;
    Ok(AssetTerms::CryptoToken(terms))
}

pub(super) fn spot_instrument(
    base: &str,
    quote: &str,
) -> Result<(Instrument, Vec<AssetTerms>, Vec<PriceReferenceTerms>), MarketParticipantError> {
    let base = token_asset(base)?;
    let quote = token_asset(quote)?;
    let instrument = Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(base.asset_id(), quote.asset_id()).map_err(model_error)?,
    ))
    .map_err(model_error)?;
    Ok((instrument, vec![base, quote], Vec::new()))
}

pub(super) fn perpetual_instrument(
    coin: &str,
    collateral: &str,
) -> Result<(Instrument, Vec<AssetTerms>, Vec<PriceReferenceTerms>), MarketParticipantError> {
    let base = token_asset(coin)?;
    let collateral = token_asset(collateral)?;
    let mark = price_reference(&base.asset_id(), &collateral.asset_id())?;
    let underlying = DerivativeUnderlying {
        reference: UnderlyingRef::Asset(base.asset_id()),
        price_reference: mark.reference_id().map_err(model_error)?,
    };
    let contract_size = ContractSize::new(
        ContractUnit::asset_quantity(Quantity::from(1_u8), base.asset_id()).map_err(model_error)?,
    )
    .map_err(model_error)?;
    let instrument = Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            underlying,
            collateral.asset_id(),
            collateral.asset_id(),
            contract_size,
            DerivativePnlModel::Linear,
            None,
        )
        .map_err(model_error)?,
    ))
    .map_err(model_error)?;
    Ok((instrument, vec![base, collateral], vec![mark]))
}

pub(super) fn price_reference(
    asset: &AssetId,
    quote: &AssetId,
) -> Result<PriceReferenceTerms, MarketParticipantError> {
    let source = ReferenceSourceId::new(VENUE)
        .map_err(|error| protocol_error("build a Hyperliquid reference source", error))?;
    let version = ReferenceTermsVersion::new("v1")
        .map_err(|error| protocol_error("build a Hyperliquid reference version", error))?;
    let terms = PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(asset.clone()),
        quote_asset: quote.clone(),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source,
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: version,
    };
    terms.reference_id().map_err(model_error)?;
    Ok(terms)
}

pub(super) fn listing_key(
    route: &VenueRoute,
    terms: ListingContinuityTerms,
) -> Result<VenueListingKey, MarketParticipantError> {
    ListingContinuityKey {
        route: route.clone(),
        terms,
    }
    .listing_key()
    .map_err(model_error)
}

pub(super) fn spot_continuity(
    scope: &VenueScopeId,
    asset_id: HyperliquidAssetId,
) -> ListingContinuityTerms {
    ListingContinuityTerms::HyperliquidAsset {
        product: HyperliquidAssetProduct::Spot,
        scope: scope.clone(),
        asset_id,
    }
}

pub(super) fn perpetual_continuity(
    scope: &VenueScopeId,
    asset_id: HyperliquidAssetId,
) -> ListingContinuityTerms {
    ListingContinuityTerms::HyperliquidAsset {
        product: HyperliquidAssetProduct::Perpetual,
        scope: scope.clone(),
        asset_id,
    }
}

pub(crate) fn outcome_continuity(
    scope: &VenueScopeId,
    outcome_id: HyperliquidOutcomeId,
    asset_id: HyperliquidAssetId,
) -> ListingContinuityTerms {
    ListingContinuityTerms::HyperliquidOutcome {
        scope: scope.clone(),
        outcome_id,
        asset_id,
    }
}

pub(super) fn spot_assets<'a>(
    tokens: &'a [HyperliquidSpotToken],
    indices: [u32; 2],
    operation: &'static str,
) -> Result<(&'a str, &'a str, HyperliquidTokenId, HyperliquidTokenId), MarketParticipantError> {
    let base = tokens
        .iter()
        .find(|token| token.index == indices[0])
        .ok_or_else(|| protocol_error(operation, "the pair names an unknown base token"))?;
    let quote = tokens
        .iter()
        .find(|token| token.index == indices[1])
        .ok_or_else(|| protocol_error(operation, "the pair names an unknown quote token"))?;
    Ok((
        base.name.as_str(),
        quote.name.as_str(),
        HyperliquidTokenId(indices[0]),
        HyperliquidTokenId(indices[1]),
    ))
}

pub(super) fn spot_identifiers(
    asset_id: HyperliquidAssetId,
    base_token_id: HyperliquidTokenId,
    quote_token_id: HyperliquidTokenId,
) -> HyperliquidListingIdentifiers {
    HyperliquidListingIdentifiers::Spot {
        asset_id,
        base_token_id,
        quote_token_id,
    }
}

pub(super) fn perpetual_identifiers(
    scope: &VenueScopeId,
    asset_id: HyperliquidAssetId,
    coin: SmolStr,
) -> Result<HyperliquidListingIdentifiers, MarketParticipantError> {
    Ok(HyperliquidListingIdentifiers::Perpetual {
        dex: scope.clone(),
        asset_id,
        coin: NativeSymbol::new(coin)
            .map_err(|error| protocol_error("decode a Hyperliquid perpetual coin", error))?,
    })
}

#[cfg(test)]
pub(crate) struct OutcomeIdentity {
    pub scope: VenueScopeId,
    pub outcome_id: HyperliquidOutcomeId,
    pub side: OutcomeSideId,
    pub asset_id: HyperliquidAssetId,
    pub coin: SmolStr,
    pub token_name: SmolStr,
}

#[cfg(test)]
impl OutcomeIdentity {
    pub(crate) fn identifiers(
        &self,
    ) -> Result<HyperliquidListingIdentifiers, MarketParticipantError> {
        Ok(HyperliquidListingIdentifiers::Outcome {
            venue: self.scope.clone(),
            outcome_id: self.outcome_id,
            side: self.side,
            asset_id: self.asset_id,
            coin: Some(
                NativeSymbol::new(self.coin.clone())
                    .map_err(|error| protocol_error("decode a Hyperliquid outcome coin", error))?,
            ),
            token_name: IdentifierValue::new(self.token_name.clone()).map_err(|error| {
                protocol_error("decode a Hyperliquid outcome token name", error)
            })?,
        })
    }
}

pub(super) fn margin_fractions(
    max_leverage: u32,
) -> Result<(Fraction, Fraction), MarketParticipantError> {
    if max_leverage == 0 {
        return Err(protocol_error(
            "decode a Hyperliquid leverage",
            SmolStr::new_static("a leverage is never zero"),
        ));
    }
    let one = Ratio::from(1_u8);
    let initial = Fraction::new(Ratio::from_inner(one.inner() / u128::from(max_leverage)))
        .map_err(model_error)?;
    let maintenance =
        Fraction::new(Ratio::from_inner(initial.ratio().inner() / 2)).map_err(model_error)?;
    Ok((initial, maintenance))
}

pub(super) fn unbounded_notional() -> Quantity {
    Quantity::from_inner(u128::MAX)
}

pub(super) fn parse_notional(
    value: &str,
    operation: &'static str,
) -> Result<Quantity, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

pub(super) fn funding_rate_cap() -> Result<Fraction, MarketParticipantError> {
    "0.04"
        .parse::<Ratio>()
        .map_err(|error| protocol_error("decode the Hyperliquid funding cap", error))
        .and_then(|ratio| Fraction::new(ratio).map_err(model_error))
}

#[cfg(test)]
pub(crate) fn outcome_group(
    question: SmolStr,
    expiry: Expiry,
    settlement_asset: AssetId,
    resolution: ResolutionRule,
    fraction_rule: SettlementFractionRule,
    spec: &HyperliquidOutcomeSpec,
) -> Result<OutcomeGroup, MarketParticipantError> {
    if spec.side_specs.len() != 2 {
        return Err(protocol_error(
            "build a Hyperliquid outcome group",
            SmolStr::new_static("an outcome publishes exactly two sides"),
        ));
    }
    let seed = OutcomeGroupSeed {
        question,
        expiry,
        topology: OutcomeTopology::Binary,
        settlement_asset,
        resolution,
        fraction_rule,
    };
    let seed_id = seed.seed_id();
    let mut sides = Vec::with_capacity(spec.side_specs.len());
    for side in &spec.side_specs {
        sides.push(OutcomeSide::new(&seed_id, side.name.clone()).map_err(model_error)?);
    }
    let side_ids: Vec<OutcomeSideId> = sides.iter().map(|side| side.id).collect();
    let member = OutcomeMember::new(&seed_id, spec.name.clone(), side_ids).map_err(model_error)?;
    OutcomeGroup::new(seed, sides, vec![member]).map_err(model_error)
}

#[cfg(test)]
pub(crate) fn outcome_side_id(
    group: &OutcomeGroup,
    side_index: usize,
) -> Result<OutcomeSideId, MarketParticipantError> {
    group
        .sides
        .get(side_index)
        .map(|side| side.id)
        .ok_or_else(|| {
            protocol_error(
                "decode a Hyperliquid outcome side",
                SmolStr::new_static("the side index is outside the declared sides"),
            )
        })
}

fn perp_dex_capabilities() -> Vec<RouteCapabilityDescriptor> {
    vec![
        RouteCapabilityDescriptor::new(CapabilityId::MarketCatalog, false),
        RouteCapabilityDescriptor::new(CapabilityId::Candlesticks, false),
        RouteCapabilityDescriptor::new(CapabilityId::MarketTrade, false),
        RouteCapabilityDescriptor::new(CapabilityId::Tickers, false),
        RouteCapabilityDescriptor::new(CapabilityId::AllTickers, false),
        RouteCapabilityDescriptor::new(CapabilityId::OrderBook, false),
        RouteCapabilityDescriptor::new(CapabilityId::Wallet, false),
        RouteCapabilityDescriptor::new(CapabilityId::AuthenticatedTrading, true),
        RouteCapabilityDescriptor::new(CapabilityId::AccountOrders, true),
        RouteCapabilityDescriptor::new(CapabilityId::AccountPositions, true),
        RouteCapabilityDescriptor::new(CapabilityId::AccountBalances, true),
        RouteCapabilityDescriptor::new(CapabilityId::AccountTrades, true),
    ]
}

fn perp_dex_normalization() -> Vec<IdentifierNormalizationRule> {
    let mut rules = vec![
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::HyperliquidAsset,
            Some(IdentifierNormalizationProduct::Hyperliquid(
                HyperliquidAssetProduct::Perpetual,
            )),
            SymbolNormalization::Exact,
        ),
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            None,
            SymbolNormalization::Exact,
        ),
    ];
    rules.sort_by_key(|rule| (rule.identifier, rule.product));
    rules
}

pub(crate) fn perp_dex_descriptor(
    name: &str,
) -> Result<VenueRouteDescriptor, MarketParticipantError> {
    let route = VenueRoute::new(
        market_participants_model::identifier::VenueId::from_name(VENUE)
            .map_err(|error| protocol_error("build a Hyperliquid venue identity", error))?,
        perp_dex_scope(name)?,
    );
    let catalog_source = CatalogSourceId::new(format_smolstr!("perp-dex-{name}-catalog"))
        .map_err(|error| protocol_error("build a Hyperliquid catalog source id", error))?;
    Ok(VenueRouteDescriptor::new(
        route,
        perp_dex_capabilities(),
        perp_dex_normalization(),
        Some(catalog_source),
    ))
}

fn outcome_capabilities() -> Vec<RouteCapabilityDescriptor> {
    vec![
        RouteCapabilityDescriptor::new(CapabilityId::PredictionEvents, false),
        RouteCapabilityDescriptor::new(CapabilityId::PredictionSeries, false),
    ]
}

fn outcome_normalization() -> Vec<IdentifierNormalizationRule> {
    vec![IdentifierNormalizationRule::new(
        ListingIdentifierKind::HyperliquidOutcome,
        Some(IdentifierNormalizationProduct::HyperliquidOutcome),
        SymbolNormalization::Exact,
    )]
}

pub(crate) fn outcome_venue_descriptor(
    scope: VenueScopeId,
) -> Result<VenueRouteDescriptor, MarketParticipantError> {
    let route = VenueRoute::new(
        market_participants_model::identifier::VenueId::from_name(VENUE)
            .map_err(|error| protocol_error("build a Hyperliquid venue identity", error))?,
        scope,
    );
    Ok(VenueRouteDescriptor::new(
        route,
        outcome_capabilities(),
        outcome_normalization(),
        None,
    ))
}

pub(crate) fn deployed_perp_dexes(dexes: &[Option<HyperliquidPerpDex>]) -> Vec<SmolStr> {
    dexes
        .iter()
        .skip(1)
        .filter_map(|dex| dex.as_ref())
        .map(|dex| dex.name.clone())
        .collect()
}

fn model_error(error: ModelError) -> MarketParticipantError {
    MarketParticipantError::MarketModel(Box::new(error))
}

fn protocol_error(
    operation: &'static str,
    reason: impl std::fmt::Display,
) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: format_smolstr!("{reason}"),
        raw_data: None,
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        asset::AssetId,
        decimal::{Fraction, Ratio},
        identifier::{VenueId, VenueScopeId},
        listing::{
            identifiers::{HyperliquidListingIdentifiers, VenueListingIdentifiers},
            route::{VenueListingKey, VenueRoute},
            venue::HyperliquidOutcomeId,
        },
        outcome::{OutcomeTopology, ResolutionRule, SettlementFractionRule},
        routing::{CapabilityId, SymbolNormalization},
        settlement::Expiry,
    };
    use smol_str::SmolStr;

    use super::{
        super::types::{
            HyperliquidOutcomeMetaResponse, HyperliquidOutcomeSideSpec, HyperliquidSettledOutcome,
        },
        HyperliquidOutcomeSpec, HyperliquidPerpDex, HyperliquidSpotToken, OutcomeIdentity,
        RouteCapabilityDescriptor, VENUE, deployed_perp_dexes, is_deployable_scope_name,
        listing_key, margin_fractions, outcome_asset_id, outcome_coin, outcome_continuity,
        outcome_encoding, outcome_group, outcome_side_id, outcome_token_name, outcome_venue_scope,
        perp_asset_id, perp_dex_descriptor, perp_dex_name, perp_dex_scope, spot_asset_id,
        spot_assets, token_asset,
    };

    fn perp_dex(name: &str) -> Option<HyperliquidPerpDex> {
        Some(HyperliquidPerpDex {
            name: SmolStr::new(name),
            full_name: Some(SmolStr::new(format!("{name} dex"))),
        })
    }

    fn outcome_spec(outcome: u64, sides: [&str; 2]) -> HyperliquidOutcomeSpec {
        HyperliquidOutcomeSpec {
            outcome,
            name: SmolStr::new_static("Recurring"),
            description: SmolStr::new_static("class:priceBinary|underlying:BTC"),
            quote_token: Some(SmolStr::new_static("USDC")),
            venue: None,
            side_specs: sides
                .iter()
                .map(|name| HyperliquidOutcomeSideSpec {
                    name: SmolStr::new(name),
                })
                .collect(),
        }
    }

    struct FixtureTerms {
        question: SmolStr,
        expiry: Expiry,
        settlement_asset: AssetId,
        resolution: ResolutionRule,
        fraction_rule: SettlementFractionRule,
    }

    fn fixture_terms() -> FixtureTerms {
        let token = token_asset("USDC")
            .expect("the fixture settlement token is valid")
            .asset_id();
        FixtureTerms {
            question: SmolStr::new_static("will BTC close above 77363 on 2026-05-26"),
            expiry: Expiry::new(1_774_000_000_000, None, None)
                .expect("the fixture expiry is valid"),
            settlement_asset: token,
            resolution: ResolutionRule::FixedAt {
                at: 1_773_000_000_000,
            },
            fraction_rule: SettlementFractionRule::AnyFraction,
        }
    }

    fn fixture_group(sides: [&str; 2]) -> market_participants_model::outcome::OutcomeGroup {
        let terms = fixture_terms();
        outcome_group(
            terms.question,
            terms.expiry,
            terms.settlement_asset,
            terms.resolution,
            terms.fraction_rule,
            &outcome_spec(1, sides),
        )
        .expect("the fixture group should build")
    }

    #[test]
    fn asset_ids_follow_the_published_formulas() {
        assert_eq!(spot_asset_id(0).expect("the index should fit").0, 10_000);
        assert_eq!(spot_asset_id(7).expect("the index should fit").0, 10_007);
        assert_eq!(perp_asset_id(0, 0).expect("the index should fit").0, 0);
        assert_eq!(perp_asset_id(0, 150).expect("the index should fit").0, 150);
        assert_eq!(
            perp_asset_id(1, 0).expect("the index should fit").0,
            110_000
        );
        assert_eq!(
            perp_asset_id(1, 2).expect("the index should fit").0,
            110_002
        );
        assert!(perp_asset_id(u32::MAX, 1).is_err());
        assert!(spot_asset_id(u32::MAX).is_err());
    }

    #[test]
    fn deployed_scope_names_are_two_to_four_lowercase_letters() {
        assert!(is_deployable_scope_name("ab"));
        assert!(is_deployable_scope_name("xyz"));
        assert!(is_deployable_scope_name("test"));
        assert!(!is_deployable_scope_name("a"));
        assert!(!is_deployable_scope_name("abcde"));
        assert!(!is_deployable_scope_name("XYZ"));
        assert!(!is_deployable_scope_name("x1"));
        assert!(!is_deployable_scope_name(""));
    }

    #[test]
    fn deployed_scopes_round_trip_through_their_names() {
        let dex = perp_dex_scope("xyz").expect("the scope should be canonical");
        assert_eq!(dex.as_str(), "perp-dex/xyz");
        assert_eq!(perp_dex_name(&dex), Some("xyz"));
        assert_eq!(
            perp_dex_name(&VenueScopeId::new("core-perp").expect("the scope is canonical")),
            None
        );

        let venue = outcome_venue_scope("ab").expect("the scope should be canonical");
        assert_eq!(venue.as_str(), "outcome-venue/ab");
        assert_eq!(perp_dex_name(&venue), None);
    }

    #[test]
    fn deployed_perp_dexes_keeps_every_name_the_venue_published() {
        let dexes = [None, perp_dex("xyz"), perp_dex("A"), perp_dex("toolong")];

        assert_eq!(
            deployed_perp_dexes(&dexes),
            vec![
                SmolStr::new_static("xyz"),
                SmolStr::new_static("A"),
                SmolStr::new_static("toolong"),
            ]
        );
        assert!(
            perp_dex_scope("toolong").is_err(),
            "a name outside the reserved shape builds no scope"
        );
    }

    #[test]
    fn a_perp_dex_descriptor_declares_the_core_perpetual_set() {
        let descriptor = perp_dex_descriptor("xyz").expect("the descriptor should build");
        assert_eq!(descriptor.route().scope.as_str(), "perp-dex/xyz");
        assert_eq!(descriptor.route().venue.as_str(), VENUE);

        let declared: Vec<CapabilityId> = descriptor
            .capabilities()
            .iter()
            .map(|capability| capability.capability())
            .collect();
        for expected in [
            CapabilityId::MarketCatalog,
            CapabilityId::Candlesticks,
            CapabilityId::MarketTrade,
            CapabilityId::Tickers,
            CapabilityId::AllTickers,
            CapabilityId::OrderBook,
            CapabilityId::Wallet,
            CapabilityId::AuthenticatedTrading,
            CapabilityId::AccountOrders,
            CapabilityId::AccountPositions,
            CapabilityId::AccountBalances,
            CapabilityId::AccountTrades,
        ] {
            assert!(
                declared.contains(&expected),
                "{expected:?} should be declared"
            );
        }
        for absent in [CapabilityId::AccountHistory, CapabilityId::ForceOrder] {
            assert!(!declared.contains(&absent), "{absent:?} is not implemented");
        }
        assert_eq!(declared.len(), 12);
        assert!(descriptor.catalog_source().is_some());
        assert!(
            descriptor
                .identifier_normalization()
                .iter()
                .all(|rule| rule.mode == SymbolNormalization::Exact),
            "a builder-deployed coin is never case folded"
        );
        let authenticated: Vec<CapabilityId> = descriptor
            .capabilities()
            .iter()
            .filter(|capability| capability.needs_auth())
            .map(RouteCapabilityDescriptor::capability)
            .collect();
        assert_eq!(
            authenticated,
            vec![
                CapabilityId::AuthenticatedTrading,
                CapabilityId::AccountOrders,
                CapabilityId::AccountPositions,
                CapabilityId::AccountBalances,
                CapabilityId::AccountTrades,
            ],
            "only the trading and account capabilities sign with the venue's credential"
        );
    }

    #[test]
    fn outcome_encodings_follow_the_published_formula() {
        assert_eq!(outcome_encoding(1, 0).expect("the encoding should fit"), 10);
        assert_eq!(outcome_encoding(1, 1).expect("the encoding should fit"), 11);
        assert!(outcome_encoding(1, 2).is_err());
        assert_eq!(
            outcome_asset_id(10).expect("the asset ID should fit").0,
            100_000_010
        );
        assert_eq!(outcome_coin(10).as_str(), "#10");
        assert_eq!(outcome_token_name(10).as_str(), "+10");
    }

    #[test]
    fn the_outcome_wire_shapes_parse() {
        let meta: HyperliquidOutcomeMetaResponse = serde_json::from_str(
            r#"{"outcomes":[{"outcome":123,"name":"Recurring","description":"class:priceBinary|underlying:HYPE|expiry:20260310-1100|targetPrice:34.5|period:3m","sideSpecs":[{"name":"Yes"},{"name":"No"}]}]}"#,
        )
        .expect("the outcome metadata should decode");
        assert_eq!(meta.outcomes.len(), 1);
        assert_eq!(meta.outcomes[0].outcome, 123);
        assert_eq!(meta.outcomes[0].name.as_str(), "Recurring");
        assert_eq!(
            meta.outcomes[0]
                .side_specs
                .iter()
                .map(|side| side.name.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No"]
        );

        let settled: HyperliquidSettledOutcome = serde_json::from_str(
            r#"{"spec":{"outcome":95,"name":"Recurring","description":"class:priceBinary","sideSpecs":[{"name":"Yes"},{"name":"No"}],"quoteToken":"USDC"},"settleFraction":"0.0","details":"price:76876.9"}"#,
        )
        .expect("the settled outcome should decode");
        assert_eq!(settled.spec.outcome, 95);
        assert_eq!(settled.spec.quote_token.as_str(), "USDC");
        assert_eq!(settled.settle_fraction.as_str(), "0.0");
        assert_eq!(settled.details.as_deref(), Some("price:76876.9"));
    }

    #[test]
    fn the_outcome_requests_serialize_their_wire_spelling() {
        assert_eq!(
            serde_json::to_string(&super::super::HyperliquidInfoRequest::OutcomeMeta)
                .expect("the request should serialize"),
            r#"{"type":"outcomeMeta"}"#
        );
        assert_eq!(
            serde_json::to_string(&super::super::HyperliquidInfoRequest::SettledOutcome {
                outcome: 95
            })
            .expect("the request should serialize"),
            r#"{"type":"settledOutcome","outcome":95}"#
        );
    }

    #[test]
    fn an_outcome_group_derives_its_identity_from_its_terms() {
        let group = fixture_group(["Yes", "No"]);

        assert_eq!(group.topology, OutcomeTopology::Binary);
        assert_eq!(group.sides.len(), 2);
        assert_eq!(group.members.len(), 1);
        assert_eq!(group.members[0].sides.len(), 2);
        assert_eq!(
            group
                .sides
                .iter()
                .map(|side| side.label.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No"]
        );

        let rebuilt = fixture_group(["Yes", "No"]);
        assert_eq!(
            rebuilt.id, group.id,
            "the identity is stable for the same terms"
        );

        let renamed = fixture_group(["Over", "Under"]);
        assert_ne!(
            renamed.id, group.id,
            "a side label is part of the canonical terms"
        );

        let yes = outcome_side_id(&group, 0).expect("the first side is declared");
        let no = outcome_side_id(&group, 1).expect("the second side is declared");
        assert_ne!(yes, no);
        assert_eq!(yes, group.sides[0].id);
        assert_eq!(no, group.sides[1].id);
        assert!(
            outcome_side_id(&group, 2).is_err(),
            "a side index outside the declared sides is a protocol error"
        );
    }

    #[test]
    fn each_outcome_side_is_its_own_listing() {
        let group = fixture_group(["Yes", "No"]);
        let scope = outcome_venue_scope("ab").expect("the scope should be canonical");
        let route = VenueRoute::new(VenueId::Hyperliquid, scope.clone());
        let yes_asset = outcome_asset_id(outcome_encoding(1, 0).expect("the encoding should fit"))
            .expect("the asset ID should fit");
        let no_asset = outcome_asset_id(outcome_encoding(1, 1).expect("the encoding should fit"))
            .expect("the asset ID should fit");

        let yes = OutcomeIdentity {
            scope: scope.clone(),
            outcome_id: HyperliquidOutcomeId(1),
            side: outcome_side_id(&group, 0).expect("the first side is declared"),
            asset_id: yes_asset,
            coin: outcome_coin(10),
            token_name: outcome_token_name(10),
        };
        let no = OutcomeIdentity {
            scope,
            outcome_id: HyperliquidOutcomeId(1),
            side: outcome_side_id(&group, 1).expect("the second side is declared"),
            asset_id: no_asset,
            coin: outcome_coin(11),
            token_name: outcome_token_name(11),
        };

        let yes_identifiers = yes.identifiers().expect("the identifiers should build");
        let no_identifiers = no.identifiers().expect("the identifiers should build");
        assert_ne!(yes_identifiers, no_identifiers);
        VenueListingIdentifiers::Hyperliquid(yes_identifiers.clone())
            .validate(&route)
            .expect("the identifiers belong to the route scope");

        let yes_key: VenueListingKey = listing_key(
            &route,
            outcome_continuity(&route.scope, HyperliquidOutcomeId(1), yes_asset),
        )
        .expect("the first side should have a listing");
        let no_key = listing_key(
            &route,
            outcome_continuity(&route.scope, HyperliquidOutcomeId(1), no_asset),
        )
        .expect("the second side should have a listing");
        assert_eq!(yes_key.route, no_key.route);
        assert_ne!(
            yes_key.listing_id, no_key.listing_id,
            "the sides share liquidity but never identity"
        );

        let HyperliquidListingIdentifiers::Outcome {
            outcome_id,
            coin,
            token_name,
            asset_id,
            ..
        } = yes_identifiers
        else {
            panic!("an outcome identity is a Hyperliquid outcome identifier");
        };
        assert_eq!(outcome_id, HyperliquidOutcomeId(1));
        assert_eq!(asset_id.0, 100_000_010);
        assert_eq!(
            coin.map(|coin| coin.as_str().to_owned()),
            Some("#10".to_owned())
        );
        assert_eq!(token_name.as_str(), "+10");
    }

    #[test]
    fn margin_fractions_are_half_of_the_initial_margin() {
        let (initial, maintenance) = margin_fractions(50).expect("a leverage of 50 is valid");

        assert_eq!(initial.to_string(), "0.02");
        assert_eq!(maintenance.to_string(), "0.01");
        assert!(margin_fractions(0).is_err());

        let (third, _) = margin_fractions(3).expect("a leverage of 3 is valid");
        assert_eq!(
            third,
            Fraction::new(Ratio::from_inner(333_333_333_333_333_333)).expect("valid")
        );
    }

    #[test]
    fn a_spot_pair_resolves_both_token_indices() {
        let tokens = vec![
            HyperliquidSpotToken {
                name: SmolStr::new_static("USDC"),
                sz_decimals: 8,
                index: 0,
            },
            HyperliquidSpotToken {
                name: SmolStr::new_static("BTC"),
                sz_decimals: 3,
                index: 1,
            },
        ];
        let (base, quote, base_id, quote_id) =
            spot_assets(&tokens, [1, 0], "decode a fixture pair").expect("the pair should resolve");

        assert_eq!(base, "BTC");
        assert_eq!(quote, "USDC");
        assert_eq!(base_id.0, 1);
        assert_eq!(quote_id.0, 0);
        assert!(spot_assets(&tokens, [9, 0], "decode a fixture pair").is_err());
    }
}
