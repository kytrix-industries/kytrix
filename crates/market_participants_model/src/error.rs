//! Strongly typed failures raised by the canonical market-participants model.
//!
//! Decimal, timestamp and time-window failures live in the same closed enum as
//! the identity, unit and settlement failures, so a caller can match one error
//! type instead of stringly typed reasons. Every variant carries the concrete
//! field, typed identifier or route that failed validation.

use smol_str::SmolStr;
use thiserror::Error;

use crate::{
    asset::AssetId,
    decimal::{Quantity, UnixMillis, validate_timestamp},
    identifier::{ChainId, VenueId, VenueScopeId},
    instrument::InstrumentKind,
    listing::{
        native::ListingIdentifier,
        route::{VenueListingKey, VenueRoute},
    },
    market::units::QuantityUnit,
    outcome::{OutcomeGroupId, OutcomeRiskPoolId},
    routing::{IdentifierNormalizationProduct, ListingIdentifierKind},
};

/// Why one typed native listing identifier did not resolve on one route.
///
/// This is the resolver-level failure of a [`ListingIdentifier`]: it names the
/// route it was looked up in and the identifier that failed, so a caller can
/// classify the failure without reading a sentence. It is folded into
/// [`ModelError`] through [`ModelError::ListingIdentifier`], so the crate never
/// carries two parallel vocabularies for one failure.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ListingIdentifierError {
    /// The identifier text is not a usable value of its kind.
    #[error("listing identifier `{value}` is not a valid {kind}")]
    InvalidValue {
        kind: ListingIdentifierKind,
        value: SmolStr,
    },
    /// No listing of the looked-up route carries this identifier.
    #[error("listing identifier {identifier:?} is not present on route {route}")]
    IdentifierNotFound {
        route: VenueRoute,
        identifier: ListingIdentifier,
    },
    /// The identifier echoes a route of its own that is not the looked-up route.
    #[error("listing identifier {identifier:?} belongs to route {owner}, not to route {route}")]
    NativeIdentifierRouteMismatch {
        route: VenueRoute,
        identifier: ListingIdentifier,
        owner: VenueRoute,
    },
    /// The identifier kind or product is not the one the route declares.
    #[error(
        "listing identifier {identifier:?} does not match the {product:?} product of route {route}"
    )]
    IdentifierProductMismatch {
        route: VenueRoute,
        identifier: ListingIdentifier,
        product: IdentifierNormalizationProduct,
    },
    /// The identifier resolves to more than one listing on the route.
    #[error("listing identifier {identifier:?} resolves to {matches} listings on route {route}")]
    NativeIdentifierAmbiguous {
        route: VenueRoute,
        identifier: ListingIdentifier,
        matches: usize,
    },
}

/// Why a listing and its snapshot were refused at the publication boundary.
///
/// The payload of this enum is larger than a pointer, so [`ModelError`] carries
/// it boxed; the variants themselves stay unboxed and carry the concrete keys,
/// kinds and values that failed.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ListingPublicationError {
    /// The snapshot key is not the key of the listing it was published with.
    #[error("snapshot key {snapshot:?} does not describe listing {listing:?}")]
    SnapshotKeyMismatch {
        listing: VenueListingKey,
        snapshot: VenueListingKey,
    },
    /// A typed identifier echoes a symbol the listing does not declare.
    #[error(
        "listing identifier {kind} echoes `{value}` that is not a display symbol of the listing"
    )]
    IdentifierEchoMismatch {
        kind: ListingIdentifierKind,
        value: SmolStr,
    },
}

/// Which part of a conversion reference disagreed with the instrument it was
/// declared for.
///
/// A conversion reference is only valid in one direction: its registry terms must
/// track the settlement asset as the underlying, quote that asset in the price
/// quote of the instrument, and be expressed per asset unit. The direction is
/// carried as a value so a caller can report which of the three broke without
/// parsing a sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversionDirection {
    /// The reference does not track the settlement asset.
    Underlying,
    /// The reference does not quote the instrument price quote.
    Quote,
    /// The reference is not expressed per asset unit.
    Basis,
}

impl ConversionDirection {
    /// Returns the fixed label used in error rendering.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Underlying => "underlying",
            Self::Quote => "quote",
            Self::Basis => "basis",
        }
    }
}

impl std::fmt::Display for ConversionDirection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// What a rejected settlement asset was asked to settle.
///
/// One rule decides whether an asset can be settled in — only a currency or a
/// crypto token can, because a security or a commodity cannot denominate a payoff —
/// and more than one place applies it. The role is carried as a value so the
/// rejection names the settlement that was actually refused instead of describing
/// one of the other places the same rule runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementAssetRole {
    /// The asset of a `DeliveryMode::Cash` settlement.
    CashDelivery,
    /// The settlement asset of an outcome group.
    OutcomeGroup,
    /// The settlement asset of an outcome risk pool.
    OutcomeRiskPool,
}

impl SettlementAssetRole {
    /// Returns the label used in error rendering.
    ///
    /// The label is a noun phrase and carries its own article, because the rejection
    /// reads "cannot denominate a settlement for {role}" and an article chosen by the
    /// template would be wrong for at least one of the three.
    pub const fn label(self) -> &'static str {
        match self {
            Self::CashDelivery => "a cash delivery",
            Self::OutcomeGroup => "an outcome group",
            Self::OutcomeRiskPool => "an outcome risk pool",
        }
    }
}

impl std::fmt::Display for SettlementAssetRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// What a quantity comparison expected to find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuantityUnitExpectation {
    /// A traded quantity: asset units, contracts or outcome tokens.
    TradedQuantity,
    /// A monetary amount expressed as a notional of one asset.
    Notional,
    /// The quantity unit declared by the resolved listing rules.
    ListedUnit,
    /// A quantity denominated in the asset of the listing notional rule.
    NotionalAsset,
    /// The group, member and side of the listed outcome instrument.
    OutcomeInstrument,
}

impl QuantityUnitExpectation {
    /// Returns the label used in error rendering.
    ///
    /// The label is a noun phrase that names what a quantity of this kind is, because it
    /// is rendered into messages a user reads: the type name of the variant is an
    /// internal identifier and would say nothing about the quantity it stands for.
    pub const fn label(self) -> &'static str {
        match self {
            Self::TradedQuantity => "asset units, contracts or outcome tokens",
            Self::Notional => "a monetary amount of one asset",
            Self::ListedUnit => "the quantity unit the listing rules declare",
            Self::NotionalAsset => "an amount of the asset the listing notional rule names",
            Self::OutcomeInstrument => "a quantity of the listed outcome instrument",
        }
    }
}

impl std::fmt::Display for QuantityUnitExpectation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Which index term of a contract or a price reference did not line up.
///
/// An index contract has to agree with the index it tracks in more than one place — the
/// contract unit names it, the unit is quoted in the price quote of the contract, and the
/// price reference behind the underlying is expressed per index point of the same index —
/// and one rule covers all of them. The term is carried as a value so the rejection names
/// the one that broke instead of describing the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexField {
    /// The basis of a price reference that tracks an index.
    ReferenceBasis,
    /// The contract unit of an index contract, which has to be an index-point unit.
    ContractUnit,
    /// The index a contract unit names.
    ContractUnitIndex,
    /// The quote asset an index-point contract unit is denominated in.
    ContractUnitQuoteAsset,
}

impl IndexField {
    /// Returns the label used in error rendering.
    ///
    /// The label is a noun phrase, because the rendering reads "{label} does not line up
    /// with the index terms of the contract".
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReferenceBasis => "the basis of the price reference",
            Self::ContractUnit => "the contract unit",
            Self::ContractUnitIndex => "the index of the contract unit",
            Self::ContractUnitQuoteAsset => "the quote asset of the contract unit",
        }
    }
}

impl std::fmt::Display for IndexField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Validation failure for a canonical market-participants model value or operation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("decimal value is empty")]
    EmptyDecimal,
    #[error("decimal value is invalid")]
    InvalidDecimal,
    #[error("decimal value cannot be negative")]
    NegativeDecimal,
    #[error("decimal value has more than {maximum} fractional digits")]
    DecimalPrecisionExceeded { maximum: usize },
    #[error("decimal value is outside the supported range")]
    DecimalOverflow,
    #[error("tick must be positive")]
    ZeroTick,
    #[error("identifier `{field}` must not be empty")]
    EmptyIdentifier { field: &'static str },
    #[error("{field} must be greater than zero")]
    ZeroValue { field: &'static str },
    #[error("{field} timestamp must be a non-negative Unix millisecond value")]
    InvalidTimestamp { field: &'static str },
    #[error("start timestamp must not be later than end timestamp")]
    InvalidTimeRange,
    #[error("{field} must not be later than {limit}")]
    InvalidTimeOrder {
        field: &'static str,
        limit: &'static str,
    },
    #[error("time window exceeds the maximum of {maximum} milliseconds")]
    TimeWindowTooLarge { maximum: UnixMillis },
    #[error("{kind} is not a currency in the built-in ISO 4217 table")]
    InvalidIso4217Code { kind: SmolStr },
    #[error("security form `{form}` is unsupported")]
    UnsupportedSecurityForm { form: SmolStr },
    #[error("instrument kind `{kind}` is unsupported by this operation")]
    UnsupportedInstrumentKind { kind: InstrumentKind },
    #[error("ratio must be within [0, 1]")]
    FractionOutOfRange,
    #[error("ratio must be greater than zero")]
    NonPositiveRatio,
    #[error("collection `{field}` must not be empty")]
    EmptyCollection { field: &'static str },
    #[error("collection `{field}` must be strictly increasing")]
    UnsortedCollection { field: &'static str },
    #[error("collection `{field}` must not contain duplicates")]
    DuplicateCollectionEntry { field: &'static str },
    #[error("{field} quantity must be greater than zero")]
    NonPositiveQuantity { field: &'static str },
    #[error("delivery legs contradict each other for the same asset")]
    ContradictoryDeliveryLegs,
    #[error("{field} does not match the canonical instrument terms")]
    InstrumentTermsMismatch { field: &'static str },
    #[error("strike quote asset does not match the option price quote asset")]
    StrikeQuoteMismatch,
    #[error("spot base asset must differ from the quote asset")]
    IdenticalSpotAssets,
    #[error("{field} does not line up with the index terms of the contract")]
    IndexMismatch { field: IndexField },
    #[error("price basis is not compatible with the instrument contract unit")]
    IncompatiblePriceBasis,
    #[error("delivery mode is not compatible with the instrument pnl model or contract size")]
    IncompatibleDeliveryMode,
    #[error("conversion reference {direction} does not match the instrument it converts")]
    ConversionDirectionMismatch { direction: ConversionDirection },
    #[error("cross-asset settlement requires a conversion reference")]
    RequiredConversionReference,
    #[error("a contract that performs no conversion must not declare a conversion reference")]
    UnexpectedConversionReference,
    #[error("conversion reference does not match the one the contract declares")]
    ConversionReferenceMismatch,
    #[error("converted settlement amount is not denominated in the settlement asset")]
    SettlementAssetMismatch,
    #[error("pnl model requires an asset underlying")]
    IncompatiblePnlModel,
    #[error("underlying is not an asset, so the contract cannot settle in it")]
    UnderlyingWithoutAsset,
    #[error("an index cannot be delivered, so the contract has to settle in cash")]
    IndexUnderlyingRequiresCashDelivery,
    #[error("contract size is not denominated in an asset")]
    ContractSizeWithoutAsset,
    #[error("inverse contract size must be denominated in the price quote asset")]
    InverseContractSizeMismatch,
    #[error("inverse settlement asset must be the underlying asset")]
    InverseSettlementMismatch,
    #[error("inverse contracts must settle in cash")]
    InverseRequiresCashDelivery,
    #[error("physical delivery does not carry a leg for the underlying asset")]
    DeliveryLegMismatch,
    #[error("contract unit is denominated in {denomination}, not in the {underlying} it tracks")]
    PayoffUnitMismatch {
        underlying: AssetId,
        denomination: AssetId,
    },
    #[error("index-point contract cannot be valued from a single per-asset price")]
    IndexPointPayoffUnsupported,
    #[error("outcome question is longer than the maximum of {maximum} characters")]
    OutcomeQuestionTooLong { maximum: usize },
    #[error("{field} must be expressed in {expected}")]
    QuantityUnitMismatch {
        field: &'static str,
        expected: QuantityUnitExpectation,
    },
    #[error("value `{value}` is not a canonical identity of version `{version}`")]
    InvalidIdentityValue {
        version: &'static str,
        value: SmolStr,
    },
    #[error("identifier namespace `{value}` is not a lowercase canonical namespace")]
    InvalidIdentifierNamespace { value: SmolStr },
    #[error("identifier value `{value}` is not a canonical protocol value")]
    InvalidIdentifierValue { value: SmolStr },
    #[error("native symbol `{symbol}` is invalid")]
    InvalidNativeIdentifier { symbol: SmolStr },
    #[error("venue `{value}` is not one of the venues this build speaks")]
    UnknownVenue { value: SmolStr },
    #[error("venue scope `{value}` is not a canonical `/`-separated scope")]
    InvalidVenueScopeId { value: SmolStr },
    #[error("chain id `{value}` is not a canonical chain identity")]
    InvalidChainId { value: SmolStr },
    #[error("wallet address `{value}` is not a canonical chain address")]
    InvalidWalletAddress { value: SmolStr },
    #[error("contract address `{value}` is not an EVM-style address")]
    InvalidContractAddress { value: SmolStr },
    #[error("security issuer `{value}` is not a typed issuer identity")]
    InvalidSecurityIssuerId { value: SmolStr },
    #[error("share class designation `{value}` is not a valid class identity")]
    InvalidShareClassDesignation { value: SmolStr },
    #[error("ADR share class `{value}` requires an ADR ratio")]
    MissingAdrRatio { value: SmolStr },
    #[error("commodity code `{value}` is not a canonical commodity identity")]
    InvalidCommodityCode { value: SmolStr },
    #[error("market identifier code `{value}` is not a valid ISO 10383 code")]
    InvalidMic { value: SmolStr },
    #[error("CTF condition id `{value}` is not a 32-byte hexadecimal value")]
    InvalidCtfConditionId { value: SmolStr },
    #[error("CTF token id `{value}` is not a uint256 identifier")]
    InvalidCtfTokenId { value: SmolStr },
    #[error("reference terms version `{value}` is not a canonical version label")]
    InvalidReferenceTermsVersion { value: SmolStr },
    #[error("IANA time-zone identifier `{value}` is invalid")]
    InvalidTimeZoneId { value: SmolStr },
    #[error("trading calendar `{value}` is not a canonical calendar identity")]
    InvalidTradingCalendarId { value: SmolStr },
    #[error("chain `{chain}` has no verified identity for token `{token}`")]
    UnverifiedTokenOrigin { chain: ChainId, token: SmolStr },
    #[error("listing continuity terms are invalid: {field}")]
    InvalidListingContinuityTerms { field: &'static str },
    #[error("listing continuity terms do not determine a single listing on route {venue}/{scope}")]
    AmbiguousListingContinuity { venue: VenueId, scope: VenueScopeId },
    #[error("listing {listing:?} is absent from route {venue}/{scope}")]
    ListingNotFound {
        venue: VenueId,
        scope: VenueScopeId,
        listing: VenueListingKey,
    },
    #[error("route {venue}/{scope} is not registered")]
    RouteNotRegistered { venue: VenueId, scope: VenueScopeId },
    #[error("listing {listing:?} does not belong to route {venue}/{scope}")]
    ListingRouteMismatch {
        venue: VenueId,
        scope: VenueScopeId,
        listing: VenueListingKey,
    },
    #[error("identifier normalization rule for {kind} does not match the typed identifier product")]
    IdentifierNormalizationMismatch { kind: ListingIdentifierKind },
    #[error("listing identifier resolution failed: {0}")]
    ListingIdentifier(#[source] Box<ListingIdentifierError>),
    #[error("listing publication was refused: {0}")]
    ListingPublication(#[source] Box<ListingPublicationError>),
    #[error("outcome group {group} is incomplete: {field}")]
    OutcomeGroupIncomplete {
        group: OutcomeGroupId,
        field: &'static str,
    },
    #[error("outcome group terms have no canonical identity yet: {field}")]
    OutcomeGroupUnresolved { field: &'static str },
    #[error("outcome risk pool {pool} is incomplete: {field}")]
    OutcomeRiskPoolIncomplete {
        pool: OutcomeRiskPoolId,
        field: &'static str,
    },
    #[error("outcome risk pool has no canonical identity yet: {field}")]
    OutcomeRiskPoolUnresolved { field: &'static str },
    #[error("outcome risk pool lineage is invalid: {field}")]
    OutcomeRiskPoolLineageInvalid { field: &'static str },
    #[error("outcome group lineage is invalid: {field}")]
    OutcomeGroupLineageInvalid { field: &'static str },
    #[error("instrument lineage is invalid: {field}")]
    InstrumentLineageInvalid { field: &'static str },
    #[error("outcome settlement does not cover every tradable side of group {group}")]
    OutcomeSettlementIncomplete { group: OutcomeGroupId },
    #[error("outcome settlement fraction is only {fraction_kind} for group {group}")]
    OutcomeSettlementFractionInvalid {
        group: OutcomeGroupId,
        fraction_kind: &'static str,
    },
    #[error("outcome conversion reference {field} is not a member of risk pool {pool}")]
    OutcomeConversionUnresolved {
        pool: OutcomeRiskPoolId,
        field: &'static str,
    },
    #[error("outcome conversion graph contains a cycle")]
    OutcomeConversionCycle,
    #[error("settlement asset `{asset}` cannot denominate a settlement for {role}")]
    UnsupportedSettlementAsset {
        asset: AssetId,
        role: SettlementAssetRole,
    },
    #[error("listing snapshot is scheduled without a future `available_from`")]
    ScheduledListingWithoutAvailability,
    #[error("listing snapshot status {status} is not tradable")]
    ListingNotTradable { status: &'static str },
    #[error("listing snapshot rules do not match the listed instrument: {field}")]
    TradingRulesMismatch { field: &'static str },
    #[error("order book {side} levels are not in best-to-worst price order")]
    UnsortedOrderBook { side: &'static str },
    #[error("order book level quantity must be greater than zero")]
    EmptyOrderBookLevel,
    #[error("order book is crossed")]
    CrossedOrderBook,
    #[error("invalid `{field}` value `{value}`")]
    InvalidField { field: &'static str, value: SmolStr },
    #[error("candlestick prices do not form a valid OHLC range")]
    InvalidCandlestickPrices,
}

impl From<ListingIdentifierError> for ModelError {
    fn from(error: ListingIdentifierError) -> Self {
        Self::ListingIdentifier(Box::new(error))
    }
}

impl From<ListingPublicationError> for ModelError {
    fn from(error: ListingPublicationError) -> Self {
        Self::ListingPublication(Box::new(error))
    }
}

/// Ensures a Unix millisecond contract term is non-negative.
pub fn require_non_negative_time(
    timestamp: UnixMillis,
    field: &'static str,
) -> Result<(), ModelError> {
    validate_timestamp(timestamp, field)
}

/// Ensures a lower time bound is not later than an upper time bound.
pub fn require_ordered_time(
    earlier: UnixMillis,
    later: UnixMillis,
    field: &'static str,
    limit: &'static str,
) -> Result<(), ModelError> {
    if earlier > later {
        Err(ModelError::InvalidTimeOrder { field, limit })
    } else {
        Ok(())
    }
}

/// Ensures every element of a non-empty sorted collection is strictly ordered.
pub fn require_strictly_sorted<T: Ord>(
    values: &[T],
    field: &'static str,
) -> Result<(), ModelError> {
    if values.is_empty() {
        return Err(ModelError::EmptyCollection { field });
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ModelError::UnsortedCollection { field });
    }
    Ok(())
}

/// Ensures every element of a sorted collection is unique, allowing an empty one.
pub fn require_sorted_and_unique<T: Ord>(
    values: &[T],
    field: &'static str,
) -> Result<(), ModelError> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ModelError::UnsortedCollection { field });
    }
    Ok(())
}

/// Validates that a quantity is strictly positive.
pub fn require_positive_quantity(
    quantity: Quantity,
    field: &'static str,
) -> Result<(), ModelError> {
    if quantity.is_zero() {
        Err(ModelError::NonPositiveQuantity { field })
    } else {
        Ok(())
    }
}

/// Rejects a [`QuantityUnit`] that is not an amount of the given asset.
pub fn require_notional_unit(unit: &QuantityUnit, field: &'static str) -> Result<(), ModelError> {
    match unit {
        QuantityUnit::Notional { .. } => Ok(()),
        _ => Err(ModelError::QuantityUnitMismatch {
            field,
            expected: QuantityUnitExpectation::Notional,
        }),
    }
}

/// Rejects a [`QuantityUnit`] that is not a traded asset or contract quantity.
pub fn require_traded_quantity_unit(
    unit: &QuantityUnit,
    field: &'static str,
) -> Result<(), ModelError> {
    match unit {
        QuantityUnit::AssetUnits { .. }
        | QuantityUnit::Contracts
        | QuantityUnit::OutcomeTokens { .. } => Ok(()),
        QuantityUnit::Notional { .. } => Err(ModelError::QuantityUnitMismatch {
            field,
            expected: QuantityUnitExpectation::TradedQuantity,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::{
        asset::{AssetId, CryptoTokenId, CryptoTokenOrigin, TokenReference},
        decimal::{Quantity, UnixMillis},
        error::{
            ListingIdentifierError, ListingPublicationError, ModelError, QuantityUnitExpectation,
            require_notional_unit, require_ordered_time, require_positive_quantity,
            require_strictly_sorted, require_traded_quantity_unit,
        },
        identifier::{IdentifierNamespace, IdentifierValue, TokenIssuerId, VenueId, VenueScopeId},
        listing::{
            native::ListingIdentifier,
            route::{VenueListingKey, VenueRoute},
            venue::{HyperliquidAssetId, HyperliquidAssetProduct, VenueListingId},
        },
        market::units::QuantityUnit,
        outcome::OutcomeGroupId,
        routing::IdentifierNormalizationProduct,
    };

    fn hyperliquid_route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new(scope).expect("scope should be valid"),
        )
    }

    fn asset_identifier(scope: &str) -> ListingIdentifier {
        ListingIdentifier::HyperliquidAsset {
            scope: VenueScopeId::new(scope).expect("scope should be valid"),
            product: HyperliquidAssetProduct::Perpetual,
            asset_id: HyperliquidAssetId(7),
        }
    }

    #[test]
    fn listing_identifier_not_found_should_carry_the_route_and_identifier() {
        let route = hyperliquid_route("core-perp");
        let identifier = asset_identifier("core-perp");
        let error = ListingIdentifierError::IdentifierNotFound {
            route: route.clone(),
            identifier: identifier.clone(),
        };

        assert_eq!(
            error.to_string(),
            format!("listing identifier {identifier:?} is not present on route {route}")
        );
        assert_eq!(
            ModelError::from(error.clone()),
            ModelError::ListingIdentifier(Box::new(error))
        );
    }

    #[test]
    fn listing_identifier_route_mismatch_should_name_the_owner_route() {
        let route = hyperliquid_route("core-perp");
        let owner = hyperliquid_route("perp-dex/xyz");
        let error = ListingIdentifierError::NativeIdentifierRouteMismatch {
            route,
            identifier: asset_identifier("perp-dex/xyz"),
            owner: owner.clone(),
        };

        assert!(error.to_string().contains(&owner.to_string()));
        assert!(matches!(
            ModelError::from(error),
            ModelError::ListingIdentifier(boxed)
                if matches!(*boxed, ListingIdentifierError::NativeIdentifierRouteMismatch { .. })
        ));
    }

    #[test]
    fn listing_identifier_product_mismatch_should_carry_the_declared_product() {
        let error = ListingIdentifierError::IdentifierProductMismatch {
            route: hyperliquid_route("core-perp"),
            identifier: asset_identifier("core-perp"),
            product: IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Spot),
        };

        assert!(error.to_string().contains("Spot"));
        assert!(matches!(
            ModelError::from(error),
            ModelError::ListingIdentifier(boxed)
                if matches!(*boxed, ListingIdentifierError::IdentifierProductMismatch {
                    product: IdentifierNormalizationProduct::Hyperliquid(
                        HyperliquidAssetProduct::Spot
                    ),
                    ..
                })
        ));
    }

    #[test]
    fn listing_identifier_ambiguity_should_carry_the_match_count() {
        let error = ListingIdentifierError::NativeIdentifierAmbiguous {
            route: hyperliquid_route("core-perp"),
            identifier: asset_identifier("core-perp"),
            matches: 2,
        };

        assert!(error.to_string().contains('2'));
        assert_ne!(
            ModelError::from(error),
            ModelError::from(ListingIdentifierError::IdentifierNotFound {
                route: hyperliquid_route("core-perp"),
                identifier: asset_identifier("core-perp"),
            })
        );
    }

    #[test]
    fn an_invalid_identifier_value_should_carry_its_kind() {
        let error = ListingIdentifierError::InvalidValue {
            kind: crate::routing::ListingIdentifierKind::OccSymbol,
            value: "AAPL".into(),
        };

        assert_eq!(
            error.to_string(),
            "listing identifier `AAPL` is not a valid occ_symbol"
        );
    }

    #[test]
    fn publication_errors_should_fold_into_the_model_error() {
        let listing = VenueListingKey {
            route: hyperliquid_route("core-perp"),
            listing_id: VenueListingId::from_identity_value(&format!(
                "listing-id/v1/{}",
                "2".repeat(64)
            ))
            .expect("fixture identity"),
        };
        let snapshot = VenueListingKey {
            route: hyperliquid_route("core-perp"),
            listing_id: VenueListingId::from_identity_value(&format!(
                "listing-id/v1/{}",
                "3".repeat(64)
            ))
            .expect("fixture identity"),
        };
        let error = ListingPublicationError::SnapshotKeyMismatch { listing, snapshot };
        let boxed = ModelError::from(error);

        assert!(boxed.to_string().contains("does not describe listing"));
        assert!(matches!(
            boxed,
            ModelError::ListingPublication(inner)
                if matches!(*inner, ListingPublicationError::SnapshotKeyMismatch { .. })
        ));
        assert!(
            ModelError::from(ListingPublicationError::IdentifierEchoMismatch {
                kind: crate::routing::ListingIdentifierKind::HyperliquidAsset,
                value: "ETH".into(),
            })
            .to_string()
            .contains("ETH")
        );
    }

    #[test]
    fn folded_errors_should_expose_their_typed_source() {
        let identifier = ListingIdentifierError::IdentifierNotFound {
            route: hyperliquid_route("core-perp"),
            identifier: asset_identifier("core-perp"),
        };
        let folded = ModelError::from(identifier.clone());
        let source = folded
            .source()
            .expect("a folded identifier error is a source");

        // `thiserror` resolves `#[source]` through `AsDynError`, and `std`
        // implements `Error` for `Box<T>`, so the source object is the boxed
        // payload; `&**` recovers the typed error without copying it.
        assert_eq!(
            source
                .downcast_ref::<Box<ListingIdentifierError>>()
                .map(Box::as_ref),
            Some(&identifier)
        );
        assert_eq!(source.to_string(), identifier.to_string());
        assert!(source.downcast_ref::<ListingPublicationError>().is_none());

        let publication = ListingPublicationError::IdentifierEchoMismatch {
            kind: crate::routing::ListingIdentifierKind::HyperliquidAsset,
            value: "ETH".into(),
        };
        let folded = ModelError::from(publication.clone());
        let source = folded
            .source()
            .expect("a folded publication error is a source");

        assert_eq!(
            source
                .downcast_ref::<Box<ListingPublicationError>>()
                .map(Box::as_ref),
            Some(&publication)
        );
        assert_eq!(source.to_string(), publication.to_string());
        assert!(source.downcast_ref::<ListingIdentifierError>().is_none());
    }

    #[test]
    fn model_error_should_fit_the_result_large_err_budget() {
        let size = size_of::<ModelError>();

        assert!(
            size <= 128,
            "ModelError is {size} bytes, which exceeds the clippy::result_large_err threshold; box or reuse a variant instead"
        );
    }

    fn token(code: &str) -> AssetId {
        AssetId::CryptoToken(
            CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
                token: TokenReference::Native {
                    code: IdentifierValue::new(code).expect("code should be valid"),
                },
            })
            .expect("token should be valid"),
        )
    }

    #[test]
    fn ordered_time_should_accept_equal_bounds() {
        assert_eq!(
            require_ordered_time(5, 5, "last_trading_at", "expiration_at"),
            Ok(())
        );
    }

    #[test]
    fn ordered_time_should_reject_inverted_bounds() {
        assert_eq!(
            require_ordered_time(6, 5, "last_trading_at", "expiration_at"),
            Err(ModelError::InvalidTimeOrder {
                field: "last_trading_at",
                limit: "expiration_at",
            })
        );
    }

    #[test]
    fn strictly_sorted_should_reject_empty_and_duplicate_values() {
        assert_eq!(
            require_strictly_sorted::<UnixMillis>(&[], "exercise_at"),
            Err(ModelError::EmptyCollection {
                field: "exercise_at"
            })
        );
        assert_eq!(
            require_strictly_sorted(&[1, 1], "exercise_at"),
            Err(ModelError::UnsortedCollection {
                field: "exercise_at"
            })
        );
    }

    #[test]
    fn notional_unit_should_reject_asset_units() {
        assert_eq!(
            require_notional_unit(
                &QuantityUnit::AssetUnits {
                    asset: token("BTC")
                },
                "traded_notional"
            ),
            Err(ModelError::QuantityUnitMismatch {
                field: "traded_notional",
                expected: QuantityUnitExpectation::Notional,
            })
        );
    }

    #[test]
    fn traded_quantity_unit_should_reject_notional() {
        assert_eq!(
            require_traded_quantity_unit(
                &QuantityUnit::Notional {
                    asset: token("USDT")
                },
                "traded_quantity"
            ),
            Err(ModelError::QuantityUnitMismatch {
                field: "traded_quantity",
                expected: QuantityUnitExpectation::TradedQuantity,
            })
        );
    }

    #[test]
    fn identifier_errors_should_expose_the_listing_identity() {
        let listing = VenueListingKey {
            route: VenueRoute::new(
                VenueId::Binance,
                VenueScopeId::new("spot").expect("scope should be valid"),
            ),
            listing_id: VenueListingId::from_identity_value(&format!(
                "listing-id/v1/{}",
                "0".repeat(64)
            ))
            .expect("fixture identity"),
        };
        let error = ModelError::ListingNotFound {
            venue: VenueId::Binance,
            scope: VenueScopeId::new("spot").expect("scope should be valid"),
            listing,
        };

        assert!(error.to_string().contains("binance/spot"));
    }

    #[test]
    fn outcome_errors_should_carry_typed_identities() {
        let group = OutcomeGroupId::from_identity_value(&format!("v1/{}", "1".repeat(64)))
            .expect("fixture identity");
        let error = ModelError::OutcomeGroupIncomplete {
            group: group.clone(),
            field: "empty question",
        };

        assert!(error.to_string().contains(group.as_str()));
        assert!(
            ModelError::OutcomeGroupUnresolved {
                field: "unresolved"
            }
            .to_string()
            .contains("unresolved")
        );
    }

    #[test]
    fn quantity_unit_mismatch_should_carry_typed_expectations() {
        let error = ModelError::QuantityUnitMismatch {
            field: "traded_notional",
            expected: QuantityUnitExpectation::Notional,
        };

        assert_eq!(
            error,
            ModelError::QuantityUnitMismatch {
                field: "traded_notional",
                expected: QuantityUnitExpectation::Notional,
            },
            "the mismatch carries the field and the typed unit it expected"
        );
        assert_eq!(
            require_traded_quantity_unit(
                &QuantityUnit::Notional {
                    asset: token("USDT")
                },
                "traded_notional"
            ),
            Err(ModelError::QuantityUnitMismatch {
                field: "traded_notional",
                expected: QuantityUnitExpectation::TradedQuantity,
            }),
            "the requirement reports the typed expectation rather than a message"
        );
        assert_eq!(
            error.to_string(),
            "traded_notional must be expressed in a monetary amount of one asset"
        );
    }

    #[test]
    fn listing_identifier_kind_errors_should_carry_the_kind() {
        let error = ModelError::IdentifierNormalizationMismatch {
            kind: crate::routing::ListingIdentifierKind::HyperliquidAsset,
        };

        assert!(error.to_string().contains("hyperliquid_asset"));
    }

    #[test]
    fn positive_quantity_should_reject_zero() {
        assert_eq!(
            require_positive_quantity(Quantity::ZERO, "contract_size"),
            Err(ModelError::NonPositiveQuantity {
                field: "contract_size"
            })
        );
    }
}
