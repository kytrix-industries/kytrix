//! Hyperliquid catalog values, dynamic scope discovery and canonical identity.
//!
//! Everything here is a pure derivation from the venue's own published wire
//! values: an asset index, a listing continuity key, a route descriptor or the
//! canonical identity of one outcome side. Nothing in this module holds a
//! directory, and nothing derives an identity from an array position the venue
//! did not publish as an identity.

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
        venue::{HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidTokenId, PriceBasis},
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
    listing::venue::HyperliquidOutcomeId,
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeMember, OutcomeSide, OutcomeSideId, OutcomeTopology,
        ResolutionRule, SettlementFractionRule,
    },
    settlement::Expiry,
};
use smol_str::{SmolStr, format_smolstr};

#[cfg(test)]
use super::types::{
    HyperliquidOutcomeMetaResponse, HyperliquidOutcomeSideSpec, HyperliquidOutcomeSpec,
    HyperliquidSettledOutcome,
};
use super::types::{HyperliquidPerpDex, HyperliquidSpotToken};
use crate::{
    catalog::{registry::asset::AssetTerms, source::CatalogSourceId},
    error::MarketParticipantError,
    routing::{HYPERLIQUID_VENUE, RouteCapabilityDescriptor, VenueRouteDescriptor},
};

/// The venue these identities belong to.
pub(super) const VENUE: &str = HYPERLIQUID_VENUE;

/// The scope prefix of a builder-deployed perpetual DEX.
pub(super) const PERP_DEX_SCOPE_PREFIX: &str = "perp-dex/";

/// The scope prefix of a HIP-4 outcome venue, as the outcome-identity tests build it.
#[cfg(test)]
pub(super) const OUTCOME_VENUE_SCOPE_PREFIX: &str = "outcome-venue/";

/// The asset index of the first spot pair.
const SPOT_ASSET_ID_BASE: u32 = 10_000;
/// The asset index of the first builder-deployed perpetual.
const PERP_DEX_ASSET_ID_BASE: u32 = 100_000;
/// The number of asset indices one perpetual DEX reserves.
const PERP_DEX_ASSET_ID_STRIDE: u32 = 10_000;
/// The asset index of the first outcome side token.
#[cfg(test)]
const OUTCOME_ASSET_ID_BASE: u64 = 100_000_000;

/// The smallest scope name the venue accepts for a deployed scope.
const MIN_SCOPE_NAME_LENGTH: usize = 2;
/// The largest scope name the venue accepts for a deployed scope.
const MAX_SCOPE_NAME_LENGTH: usize = 4;

/// Returns the DEX name a perpetual scope names, when it names one.
pub(crate) fn perp_dex_name(scope: &VenueScopeId) -> Option<&str> {
    scope
        .as_str()
        .strip_prefix(PERP_DEX_SCOPE_PREFIX)
        .filter(|name| !name.is_empty())
}

/// Builds the scope of one builder-deployed perpetual DEX.
///
/// The name must have the shape the venue reserves for a deployed scope: a name
/// outside it is not a scope this build may register, and building one from it
/// would be inventing an address the venue never published. Whether such a name
/// fails a load is the reading's decision; this only refuses to build it.
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

/// Builds the scope of one HIP-4 outcome venue.
#[cfg(test)]
pub(crate) fn outcome_venue_scope(name: &str) -> Result<VenueScopeId, MarketParticipantError> {
    VenueScopeId::new(format_smolstr!("{OUTCOME_VENUE_SCOPE_PREFIX}{name}"))
        .map_err(|error| protocol_error("build a Hyperliquid outcome venue scope", error))
}

/// Returns whether one deployed scope name has the shape the venue reserves.
///
/// A builder-deployed DEX and an outcome venue share one namespace of two to four
/// lowercase ASCII letters, so a name outside that shape is not a scope this
/// adapter may register.
pub(crate) fn is_deployable_scope_name(name: &str) -> bool {
    (MIN_SCOPE_NAME_LENGTH..=MAX_SCOPE_NAME_LENGTH).contains(&name.len())
        && name.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// The asset index of one spot pair.
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

/// The asset index of one asset of a builder-deployed perpetual DEX.
///
/// The first (core) perpetual DEX is addressed by its `universe` position alone;
/// a builder-deployed one is `100000 + perp_dex_index * 10000 + index_in_meta`,
/// where the DEX index is the position of the DEX in the venue's own `perpDexs`
/// answer and the meta index is the position of the coin in that DEX's `universe`.
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

/// The wire encoding of one outcome side: `10 * outcome + side`.
#[cfg(test)]
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

/// The asset index of one outcome side token.
#[cfg(test)]
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

/// The spot coin of one outcome side: `#<encoding>`.
#[cfg(test)]
pub(crate) fn outcome_coin(encoding: u64) -> SmolStr {
    format_smolstr!("#{encoding}")
}

/// The token name of one outcome side: `+<encoding>`.
#[cfg(test)]
pub(crate) fn outcome_token_name(encoding: u64) -> SmolStr {
    format_smolstr!("+{encoding}")
}

/// The canonical asset identity of one Hyperliquid token code.
///
/// Hyperliquid issues its tokens inside its own namespace; the venue publishes no
/// chain identity for a token code, so a token is a venue-issued token of this
/// venue rather than a chain token this adapter could not verify.
///
/// The terms are built here, where the venue's own code is still authoritative,
/// and the identity is derived from them: a digest cannot be read back into the
/// terms the display contract renders, so a caller that needs the identity takes
/// it from [`AssetTerms::asset_id`] rather than the other way round.
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

/// The canonical spot instrument of two Hyperliquid tokens.
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
    // A spot pair exchanges its two assets and names no reference of its own.
    Ok((instrument, vec![base, quote], Vec::new()))
}

/// The canonical perpetual instrument of one Hyperliquid coin.
///
/// The contract is one coin of the base asset, quoted and settled in the DEX's own
/// collateral token, which is what the venue's `position_size * mark_price` margin
/// and pnl arithmetic describes.
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

/// The price reference the venue's own mark price of one asset observes.
///
/// The reference is the venue's mark observation of the asset, quoted in the
/// collateral token; the venue's oracle endpoints and refresh cadence are adapter
/// detail and never enter an identity. The terms are that identity, so they are
/// built here and handed on with the instrument that tracks them.
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

/// The listing key one route's continuity terms derive.
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

/// The continuity terms of one Hyperliquid spot listing.
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

/// The continuity terms of one Hyperliquid perpetual listing.
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

/// The continuity terms of one Hyperliquid outcome listing.
#[cfg(test)]
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

/// The base and quote token names and indices one spot pair exchange.
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

/// The identifiers of one Hyperliquid spot listing.
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

/// The identifiers of one Hyperliquid perpetual listing.
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

/// The identity of one Hyperliquid outcome side listing.
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
    /// Builds the identifiers of this outcome side listing.
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

/// The initial and maintenance margin fractions one maximum leverage implies.
///
/// The venue documents both: the margin required to open a position is the
/// position value over the leverage, and the maintenance margin is set to half of
/// the initial margin at maximum leverage.
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

/// The notional a leverage tier reaches when the venue publishes no ceiling.
///
/// Hyperliquid publishes a maximum leverage per asset, and a builder-deployed DEX
/// publishes an open-interest cap per coin, but the core DEX publishes no notional
/// cap at all. Such a tier is stated as reaching the largest notional the model
/// can express rather than an invented limit.
pub(super) fn unbounded_notional() -> Quantity {
    Quantity::from_inner(u128::MAX)
}

/// Reads one notional the venue published as a decimal string.
pub(super) fn parse_notional(
    value: &str,
    operation: &'static str,
) -> Result<Quantity, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

/// The funding rate cap the venue applies to every asset: four percent an hour.
pub(super) fn funding_rate_cap() -> Result<Fraction, MarketParticipantError> {
    "0.04"
        .parse::<Ratio>()
        .map_err(|error| protocol_error("decode the Hyperliquid funding cap", error))
        .and_then(|ratio| Fraction::new(ratio).map_err(model_error))
}

/// The canonical binary outcome group one wire outcome describes.
///
/// The question, the expiry, the settlement asset, the resolution rule and the
/// fraction rule are the caller's verified terms: the wire outcome metadata carries
/// none of them, so this function never invents one. The sides and the member are
/// derived *from* those terms, which is why a renamed side changes the canonical
/// identity instead of silently reusing it.
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

/// The canonical identity of one wire side of one outcome group.
///
/// The wire index only *selects* a declared side; it never constructs an identity,
/// so a side the group does not declare is a protocol error.
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

/// The capability set one builder-deployed perpetual DEX serves.
///
/// It is the core perpetual set: the venue serves `meta`, `clearinghouseState`
/// and `frontendOpenOrders` per DEX, and the same order and cancel actions.
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

/// The identifier normalization of a Hyperliquid asset scope.
///
/// A builder-deployed coin is `{dex}:{coin}` and a DEX name is lowercase while a
/// coin name is not, so the two are never folded into one another.
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

/// The descriptor of one builder-deployed perpetual DEX scope.
///
/// The descriptor declares the same capabilities and normalization the core
/// perpetual route declares, under its own scope, so a dynamic scope is isolated by
/// its route rather than by its origin: which hosts it dials is the venue's own
/// connection configuration. Its catalog source id folds the deployed
/// namespace's separator out, because a catalog source id is a lowercase identifier
/// and a scope is not.
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

/// Every deployment the venue's own `perpDexs` answer named, in the venue's order.
///
/// The first element of the venue's answer is `null` and stands for the core DEX,
/// which is already registered as `core-perp`, so it is not a deployment; every
/// other entry is one. Whether this build reserves a scope shape for a name is
/// deliberately **not** decided here: a venue that names a deployment this build
/// cannot serve is a load that must fail whole, and a name dropped here
/// would be a scope silently missing from an otherwise successful load — and would
/// not even count against the bound.
pub(crate) fn deployed_perp_dexes(dexes: &[Option<HyperliquidPerpDex>]) -> Vec<SmolStr> {
    dexes
        .iter()
        .skip(1)
        .filter_map(|dex| dex.as_ref())
        .map(|dex| dex.name.clone())
        .collect()
}

/// Turns one model failure into the market-participant error that carries it.
fn model_error(error: ModelError) -> MarketParticipantError {
    MarketParticipantError::MarketModel(Box::new(error))
}

/// Turns one protocol-shape failure into a typed protocol error.
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
        HyperliquidOutcomeMetaResponse, HyperliquidOutcomeSideSpec, HyperliquidOutcomeSpec,
        HyperliquidPerpDex, HyperliquidSettledOutcome, HyperliquidSpotToken, OutcomeIdentity,
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
            side_specs: sides
                .iter()
                .map(|name| HyperliquidOutcomeSideSpec {
                    name: SmolStr::new(name),
                })
                .collect(),
        }
    }

    /// The verified terms of one fixture outcome group.
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

    /// The asset indices are the venue's own arithmetic, not an array position.
    #[test]
    fn asset_ids_follow_the_published_formulas() {
        assert_eq!(spot_asset_id(0).expect("the index should fit").0, 10_000);
        assert_eq!(spot_asset_id(7).expect("the index should fit").0, 10_007);
        assert_eq!(perp_asset_id(0, 0).expect("the index should fit").0, 0);
        assert_eq!(perp_asset_id(0, 150).expect("the index should fit").0, 150);
        // The venue's own example: `test:ABC` is DEX 1, meta index 0, asset 110000.
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

    /// A deployed scope name is the two-to-four lowercase letters the venue
    /// reserves for builder-deployed DEXes and outcome venues.
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

    /// The scope of one deployed namespace round-trips through its name.
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

    /// The first `perpDexs` entry is the core DEX and is never a deployment; every
    /// other name is answered as the venue published it, whatever its shape.
    ///
    /// A name outside the reserved shape is the reading's refusal to make, not
    /// this list's to drop: dropping it here would leave a load successful with a
    /// scope missing, and would keep it out of the bound the reading counts.
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

    /// The descriptor of a deployed DEX serves exactly the core perpetual set under
    /// its own scope, with exact identifier matching and the venue's credential
    /// profile.
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

    /// An outcome side's three wire representations are the venue's encoding.
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

    /// The wire metadata parses into typed values, and the quote token appears only
    /// in the settlement.
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

    /// The outcome requests carry the venue's own type spelling.
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

    /// The sides and the member of an outcome group are derived from the verified
    /// terms, so a renamed side is a new identity and a wire index only selects.
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

    /// One outcome side is one listing, and its two sides never share a listing ID.
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

    /// The documented margin fractions come from the maximum leverage alone.
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

    /// A spot pair resolves both of its token indices to their published names.
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
