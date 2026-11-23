use market_participants_model::{
    asset::AssetId,
    error::ModelError,
    identifier::{NativeSymbol, PolymarketEventId, VenueId},
    instrument::InstrumentKind,
    listing::{
        continuity::ListingContinuityKey,
        native::{ListingIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
        venue::{PriceBasis, VenueListingId},
    },
    outcome::{OutcomeEventId, OutcomeGroupId, OutcomeMemberId, OutcomeRiskPoolId, OutcomeSideId},
    routing::ListingIdentifierKind,
    settlement::InstrumentId,
};
use smol_str::SmolStr;
use thiserror::Error;

use crate::catalog::{display::types::DisplayError, source::RouteInputRevision};

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SourceError {
    #[error("instrument catalog network request failed: {reason}")]
    Network { reason: SmolStr },
    #[error("instrument catalog authentication failed: {reason}")]
    Authentication { reason: SmolStr },
    #[error("instrument catalog request was rate limited")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("instrument catalog request was rejected ({code}): {message}")]
    Rejected {
        code: SmolStr,
        message: SmolStr,
        retryable: bool,
    },
    #[error("failed to decode instrument catalog response for {operation}")]
    Decode { operation: SmolStr },
    #[error("instrument catalog protocol error during {operation}: {reason}")]
    Protocol {
        operation: SmolStr,
        reason: SourceProtocolRejection,
    },
    #[error("instrument catalog configuration failed: {reason}")]
    Configuration { reason: SmolStr },
    #[error("the symbol directory of venue {venue} could not be read: {reason}")]
    SymbolDirectory { venue: VenueId, reason: SmolStr },
    #[error("the instrument catalog fetch was lost before it answered")]
    Lost,
    #[error("instrument catalog source failed: {reason}")]
    Other { reason: SmolStr },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SourceProtocolRejection {
    #[error("the adapter returned an unexpected response")]
    UnexpectedResponse,
    #[error("the venue terms were refused: {error}")]
    Model {
        #[source]
        error: Box<ModelError>,
    },
    #[error("listing {listing:?} carries no instrument family and settlement asset")]
    ListingTermsIncomplete { listing: Box<VenueListingKey> },
    #[error("the venue refused the answer: {reason}")]
    Venue { reason: SmolStr },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogValidationError {
    #[error("listing {listing:?} does not belong to the fetched route {route}")]
    ListingRouteMismatch {
        route: VenueRoute,
        listing: Box<VenueListingKey>,
    },
    #[error("snapshot {snapshot:?} does not describe its listing {listing:?}")]
    SnapshotKeyMismatch {
        listing: Box<VenueListingKey>,
        snapshot: Box<VenueListingKey>,
    },
    #[error("listing {listing:?} declares instrument {declared} but its terms produce {computed}")]
    InstrumentTermsMismatch {
        listing: Box<VenueListingKey>,
        declared: InstrumentId,
        computed: InstrumentId,
    },
    #[error("listing {listing:?} declares {declared:?} but its instrument is {kind:?}")]
    InstrumentKindMismatch {
        listing: Box<VenueListingKey>,
        declared: InstrumentKind,
        kind: InstrumentKind,
    },
    #[error("route {route} reuses the normalized native identifier {identifier}")]
    DuplicateNativeIdentifier {
        route: VenueRoute,
        identifier: SmolStr,
    },
    #[error("listing {listing:?} carries unusable {kind:?} identifiers: {error}")]
    InvalidListingIdentifiers {
        listing: Box<VenueListingKey>,
        kind: ListingIdentifierKind,
        error: Box<ModelError>,
    },
    #[error("listing {listing:?} declares rules that do not hold for its instrument: {error}")]
    InvalidTradingRules {
        listing: Box<VenueListingKey>,
        error: Box<ModelError>,
    },
    #[error("outcome listing {listing:?} quotes {found} but its group settles in {expected}")]
    OutcomePriceQuoteMismatch {
        listing: Box<VenueListingKey>,
        expected: Box<AssetId>,
        found: Box<AssetId>,
    },
    #[error(
        "outcome listing {listing:?} declares the price basis {basis:?}, which does not express a fraction of collateral"
    )]
    OutcomePriceBasisUnsupported {
        listing: Box<VenueListingKey>,
        basis: Box<PriceBasis>,
    },
    #[error("route {route} cannot hold the native identifier {identifier:?} of its own listing")]
    ListingIdentifierRouteMismatch {
        route: VenueRoute,
        identifier: Box<ListingIdentifier>,
    },
    #[error(
        "identifier kind {kind:?} does not match the declared product of listing {listing:?}: {error}"
    )]
    IdentifierNormalizationMismatch {
        listing: Box<VenueListingKey>,
        kind: ListingIdentifierKind,
        error: Box<ModelError>,
    },
    #[error(
        "listing {listing:?} references {count} missing {kind} objects the batch does not carry on route {route}"
    )]
    IncompleteOutcomeSet {
        route: VenueRoute,
        listing: Box<VenueListingKey>,
        kind: &'static str,
        count: usize,
    },
    #[error("outcome group {group} references a member or side the batch does not carry: {error}")]
    OutcomeGroupIncomplete {
        group: OutcomeGroupId,
        error: Box<ModelError>,
    },
    #[error(
        "outcome listing {listing:?} trades the member {member} and side {side}, which the group {group} does not carry"
    )]
    OutcomeListingLegUnknown {
        listing: Box<VenueListingKey>,
        group: OutcomeGroupId,
        member: OutcomeMemberId,
        side: OutcomeSideId,
    },
    #[error(
        "listing {listing:?} declares the native event {event:?}, which neither its batch nor the published generation carries"
    )]
    ListingEventUnknown {
        listing: Box<VenueListingKey>,
        event: PolymarketEventId,
    },
    #[error("outcome group lineage is not a valid graph at {group}")]
    OutcomeGroupLineageInvalid { group: OutcomeGroupId },
    #[error("risk pool {pool} references a group or conversion the batch does not carry: {error}")]
    OutcomeRiskPoolIncomplete {
        pool: OutcomeRiskPoolId,
        error: Box<ModelError>,
    },
    #[error("outcome group {group} belongs to more than one risk pool")]
    AmbiguousRiskPoolMembership { group: OutcomeGroupId },
    #[error(
        "outcome event {event} lists the missing group {group:?} or risk pool {pool:?} of its batch"
    )]
    OutcomeEventIncomplete {
        event: OutcomeEventId,
        group: Option<OutcomeGroupId>,
        pool: Option<OutcomeRiskPoolId>,
    },
    #[error("risk pool {pool} is not a valid lineage successor of a member group")]
    OutcomeRiskPoolLineageInvalid { pool: OutcomeRiskPoolId },
    #[error("instrument lineage is not a valid graph: {error}")]
    InstrumentLineageInvalid { error: Box<ModelError> },
    #[error("route {route} reuses the listing id {listing_id}")]
    DuplicateListingId {
        route: VenueRoute,
        listing_id: VenueListingId,
    },
    #[error(
        "listing {listing:?} declares unusable listing continuity terms {continuity:?}: {error}"
    )]
    InvalidListingContinuityTerms {
        listing: Box<VenueListingKey>,
        continuity: Box<ListingContinuityKey>,
        error: Box<ModelError>,
    },
    #[error("listing {listing:?} shares the continuity key {continuity:?} with another listing")]
    AmbiguousListingContinuity {
        listing: Box<VenueListingKey>,
        continuity: Box<ListingContinuityKey>,
    },
    #[error("listing {listing:?} carries neither a symbol nor typed identifiers")]
    ListingWithoutIdentity { listing: VenueListingKey },
    #[error("listing {listing:?} cannot be displayed: {error}")]
    DisplayRejected {
        listing: Option<Box<VenueListingKey>>,
        error: Box<DisplayError>,
    },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error(transparent)]
    Source(#[from] Box<SourceError>),
    #[error("the instrument catalog of {route} was refused: {error}")]
    InvalidCatalog {
        route: VenueRoute,
        error: Box<CatalogValidationError>,
    },
    #[error("the published instrument registries were refused: {error}")]
    InvalidRegistries { error: Box<CatalogValidationError> },
    #[error("route {route} is not a registered catalog route")]
    RouteNotRegistered { route: VenueRoute },
    #[error("route {route} has no published catalog yet")]
    CatalogNotLoaded { route: VenueRoute },
    #[error("listing {listing:?} is absent from the published generation")]
    ListingNotFound { listing: Box<VenueListingKey> },
    #[error("listing {found:?} belongs to another route than {expected}")]
    ListingRouteMismatch {
        expected: Box<VenueRoute>,
        found: Box<VenueListingKey>,
    },
    #[error("the native identifier {identifier:?} is not registered on route {route}")]
    NativeIdentifierNotFound {
        route: VenueRoute,
        identifier: Box<ListingIdentifier>,
    },
    #[error("route {route} has no outcome group for the native identifier {identifier:?}")]
    NativeGroupIdentifierNotFound {
        route: VenueRoute,
        identifier: Box<OutcomeGroupIdentifier>,
    },
    #[error("route {route} has no outcome event for the native identifier {event:?}")]
    NativeEventIdentifierNotFound {
        route: VenueRoute,
        event: PolymarketEventId,
    },
    #[error("the symbol {symbol} is not a usable native symbol on route {route}")]
    NativeSymbolInvalid { route: VenueRoute, symbol: SmolStr },
    #[error("the native identifier {identifier:?} names several listings on route {route}")]
    NativeIdentifierAmbiguous {
        route: VenueRoute,
        identifier: Box<ListingIdentifier>,
    },
    #[error("the native identifier {identifier:?} belongs to another route than {route}")]
    NativeIdentifierRouteMismatch {
        route: VenueRoute,
        identifier: Box<ListingIdentifier>,
    },
    #[error("the symbol {symbol} is not registered on route {route}")]
    NativeSymbolNotFound {
        route: VenueRoute,
        symbol: NativeSymbol,
    },
    #[error("listing {listing:?} is published without a native symbol")]
    NativeSymbolAbsent { listing: Box<VenueListingKey> },
    #[error("outcome group {group} is absent from the published generation")]
    OutcomeGroupNotFound { group: OutcomeGroupId },
    #[error("outcome event {event} is absent from the published generation")]
    OutcomeEventNotFound { event: OutcomeEventId },
    #[error("outcome risk pool {pool} is absent from the published generation")]
    OutcomeRiskPoolNotFound { pool: OutcomeRiskPoolId },
    #[error("the staged batch was built under another route input revision of {route}")]
    RouteRevisionChanged {
        route: VenueRoute,
        staged: Box<RouteInputRevision>,
        current: Box<RouteInputRevision>,
    },
    #[error("the catalog source id {id} is registered more than once")]
    DuplicateCatalogSourceId { id: SmolStr },
    #[error("the catalog source id {id} is not a valid namespace")]
    InvalidCatalogSourceId { id: SmolStr },
    #[error("route {route} provides {capability} but does not bind a catalog source")]
    MissingCatalogSource {
        route: VenueRoute,
        capability: SmolStr,
    },
    #[error(
        "route {route} binds catalog source {id} but does not provide the {capability} capability"
    )]
    CatalogSourceOnNonCatalogRoute {
        route: VenueRoute,
        id: SmolStr,
        capability: SmolStr,
    },
    #[error("route {route} binds the catalog source {id}, which is not registered")]
    UnregisteredCatalogSource { route: VenueRoute, id: SmolStr },
    #[error("the registered catalog route {route} declares inconsistent inputs")]
    InconsistentRouteRegistration { route: VenueRoute },
    #[error("the registered catalog route {route} does not match the fetched route")]
    RegisteredRouteMismatch { route: VenueRoute },
    #[error("the identifier normalization of listing {listing:?} is not declared by its route")]
    IdentifierNormalizationMismatch {
        listing: Box<VenueListingKey>,
        kind: ListingIdentifierKind,
        error: Box<ModelError>,
    },
    #[error("the catalog source {id} failed: {error}")]
    SourceFailed {
        id: SmolStr,
        error: Box<SourceError>,
    },
}

impl From<SourceError> for CatalogError {
    fn from(error: SourceError) -> Self {
        Self::Source(Box::new(error))
    }
}

impl CatalogError {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn source_failed(id: SmolStr, error: SourceError) -> Self {
        Self::SourceFailed {
            id,
            error: Box::new(error),
        }
    }

    pub(crate) fn invalid_catalog(route: VenueRoute, error: CatalogValidationError) -> Self {
        Self::InvalidCatalog {
            route,
            error: Box::new(error),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ListingInstrumentError {
    #[error(transparent)]
    Catalog(Box<CatalogError>),
    #[error(transparent)]
    Display(#[from] DisplayError),
}

impl From<CatalogError> for ListingInstrumentError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::{CatalogError, SourceError};

    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new("swap").expect("swap is a canonical scope id"),
        )
    }

    #[test]
    fn a_route_with_no_snapshot_is_not_a_route_that_serves_nothing() {
        assert_ne!(
            CatalogError::CatalogNotLoaded { route: route() },
            CatalogError::RouteNotRegistered { route: route() }
        );
    }

    #[test]
    fn a_source_failure_keeps_its_own_variant() {
        let error = CatalogError::Source(Box::new(SourceError::RateLimited {
            retry_after_ms: Some(1_000),
        }));

        assert!(matches!(
            error,
            CatalogError::Source(failure)
                if matches!(*failure, SourceError::RateLimited { .. })
        ));
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error(transparent)]
pub struct RegistryError(Box<ModelError>);
impl RegistryError {
    pub fn model(&self) -> &ModelError {
        &self.0
    }

    pub fn into_model(self) -> ModelError {
        *self.0
    }
}

impl From<ModelError> for RegistryError {
    fn from(error: ModelError) -> Self {
        Self(Box::new(error))
    }
}
