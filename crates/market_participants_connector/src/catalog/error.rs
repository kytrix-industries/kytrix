//! The classified failures of the shared catalogue.
//!
//! Every failure is a typed value that names the route, listing key, canonical
//! identity or field it is about, so a caller can decide what to do without
//! parsing a message. No catalog failure carries a free-form explanation.

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

/// A classified failure while a market-participant adapter is producing a catalog.
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
    /// The venue's own symbol directory could not be fetched or read.
    ///
    /// The source of a catalog is the market participant that publishes the directory, and
    /// that market participant is not the route whose listings are being read: the venue is
    /// carried as a typed identity so the message names the market in the reader's
    /// language instead of spelling the canonical identity into a sentence, and the
    /// reason stays the diagnostic detail.
    #[error("the symbol directory of venue {venue} could not be read: {reason}")]
    SymbolDirectory { venue: VenueId, reason: SmolStr },
    #[error("the instrument catalog fetch was lost before it answered")]
    Lost,
    #[error("instrument catalog source failed: {reason}")]
    Other { reason: SmolStr },
}

/// Why one catalog source step refused an answer it had already received.
///
/// The set is closed rather than a free-form reason, because these failures reach
/// a localized message: a sentence built by the step would carry whatever value it
/// was formatting — including a listing id — into a user-visible template. A
/// refusal that is about a listing therefore keeps the listing as a typed
/// diagnostic field, and the message states the failure without printing it.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SourceProtocolRejection {
    /// The adapter answered a request with a response the step cannot read.
    #[error("the adapter returned an unexpected response")]
    UnexpectedResponse,
    /// A venue answer does not form a value the market-participants model accepts.
    #[error("the venue terms were refused: {error}")]
    Model {
        /// The model refusal the venue's terms produced.
        #[source]
        error: Box<ModelError>,
    },
    /// A listing the source staged carries no instrument family and settlement
    /// asset, so its margin terms cannot be read.
    #[error("listing {listing:?} carries no instrument family and settlement asset")]
    ListingTermsIncomplete {
        /// The listing the refused read was about. It stays a typed diagnostic:
        /// the localized message does not print it.
        listing: Box<VenueListingKey>,
    },
    /// The venue's own protocol layer refused the answer.
    ///
    /// The reason is the venue's own statement about its own answer — the same
    /// class of text as a rejected request's message — and is presented as such
    /// rather than parsed.
    #[error("the venue refused the answer: {reason}")]
    Venue {
        /// The venue's own diagnostic message.
        reason: SmolStr,
    },
}

/// Why a staged catalog batch was refused.
///
/// Every variant names the route and the listing, identifier or field that
/// failed, so the source and the bridge can classify the refusal instead of
/// reading a sentence.
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
    /// The display publication gate refused the whole batch.
    ///
    /// The display gate is a second, catalogue-global validator that runs on the
    /// same staged batch as the identity validator: a missing registry entry, an
    /// asset no code was resolved for, two staged codes that disagree, a listing
    /// whose identifiers contradict their declared symbols or a name that is not the
    /// rendering of its own details refuses the publication before the generation is
    /// replaced, so nothing is silently stale.
    #[error("listing {listing:?} cannot be displayed: {error}")]
    DisplayRejected {
        listing: Option<Box<VenueListingKey>>,
        error: Box<DisplayError>,
    },
}

/// Errors returned by the shared catalogue.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error(transparent)]
    Source(#[from] Box<SourceError>),
    /// The candidate catalog was refused as a whole; nothing was published.
    #[error("the instrument catalog of {route} was refused: {error}")]
    InvalidCatalog {
        route: VenueRoute,
        error: Box<CatalogValidationError>,
    },
    /// The canonical registries were refused as a whole; nothing was published.
    ///
    /// A registry-only publication names no route: the registries it installs are
    /// the catalog's own rather than a route's, so this is the same refusal as
    /// [`Self::InvalidCatalog`] for the publication that belongs to no route.
    #[error("the published instrument registries were refused: {error}")]
    InvalidRegistries { error: Box<CatalogValidationError> },
    /// The route is not in the registered catalog route set, or was revoked.
    #[error("route {route} is not a registered catalog route")]
    RouteNotRegistered { route: VenueRoute },
    /// The route is registered but nothing has loaded its catalog yet.
    ///
    /// This is a waiting state rather than a refusal: the connection action
    /// loads the route's catalog, and a read that finds none is answered with
    /// this instead of a request. It is deliberately a variant of its own so
    /// that "not loaded yet" and "publishes no catalog" stay two answers.
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
    /// Wraps a source failure with the id of the source that produced it.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn source_failed(id: SmolStr, error: SourceError) -> Self {
        Self::SourceFailed {
            id,
            error: Box::new(error),
        }
    }

    /// Wraps a validation failure with the route whose batch was refused.
    pub(crate) fn invalid_catalog(route: VenueRoute, error: CatalogValidationError) -> Self {
        Self::InvalidCatalog {
            route,
            error: Box::new(error),
        }
    }
}

/// Why reading the name of one listing through the loading entry failed.
///
/// The two variants are the two failure domains of that read, and a reader acts
/// on them differently: the route's catalog could not be loaded at all — the
/// failure is the catalogue's own, classified as [`CatalogError`] —
/// or the publication that was then captured holds no name for the listing,
/// which is the [`DisplayError`] the cached read answers with. A name miss is
/// never reported as a generation mismatch: the read names no generation of its
/// own, it reads the one that is published.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ListingInstrumentError {
    /// The listing's route catalog could not be loaded.
    #[error(transparent)]
    Catalog(Box<CatalogError>),
    /// The captured generation holds no name for the listing.
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

    /// A route nothing has loaded is told apart from a route that serves
    /// nothing at all: the first is a wait, the second is a refusal.
    #[test]
    fn a_route_with_no_snapshot_is_not_a_route_that_serves_nothing() {
        assert_ne!(
            CatalogError::CatalogNotLoaded { route: route() },
            CatalogError::RouteNotRegistered { route: route() }
        );
    }

    /// A transport failure stays the classified failure it was, whichever read
    /// it reached the caller through.
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

/// A failure while a canonical registry derived or validated identity terms.
///
/// The model error is boxed because it is the model crate's own error type and
/// [`market_participants_model::error::ModelError`] is large; the registry never redefines
/// it, it only transports it without paying for its size on every result.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error(transparent)]
pub struct RegistryError(Box<ModelError>);
impl RegistryError {
    /// Returns the model failure this error carries.
    pub fn model(&self) -> &ModelError {
        &self.0
    }

    /// Returns the model failure this error carries.
    pub fn into_model(self) -> ModelError {
        *self.0
    }
}

impl From<ModelError> for RegistryError {
    fn from(error: ModelError) -> Self {
        Self(Box::new(error))
    }
}
