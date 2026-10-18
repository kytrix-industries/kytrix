//! The catalog sources and the exact set of routes they are bound to.
//!
//! A route binds at most one catalog source, and only a route that provides the
//! `MarketCatalog` capability may bind one. The bridge that owns the route
//! registry submits a whole candidate update — the source map and the route map
//! together — and this module either accepts all of it or none of it: a route
//! can never be fetchable while its source is missing, and a revoked route is
//! revoked for fetching and resolving at once.

use std::{collections::BTreeSet, sync::Arc};

use ahash::AHashMap;
use futures::future::BoxFuture;
use market_participants_model::{
    error::ModelError,
    identifier::IdentifierNamespace,
    listing::{native::ListingIdentifier, route::VenueRoute},
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization, normalization_mode,
        validate_normalization_rules,
    },
};
use smol_str::{SmolStr, format_smolstr};

use crate::catalog::{
    batch::CatalogBatch,
    error::{CatalogError, SourceError},
    registry::staging::CatalogStagingHandle,
};

/// The identity of one catalog source inside one catalog.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogSourceId(SmolStr);

impl CatalogSourceId {
    /// Validates a catalog source id.
    pub fn new(value: impl AsRef<str>) -> Result<Self, CatalogError> {
        IdentifierNamespace::new(value.as_ref())
            .map(|identifier| Self(identifier.as_str().into()))
            .map_err(|_| CatalogError::InvalidCatalogSourceId {
                id: SmolStr::new(value.as_ref()),
            })
    }

    /// Returns the id exactly as registered.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the id as a reusable short string.
    pub fn to_smol_str(&self) -> SmolStr {
        self.0.clone()
    }
}

impl std::fmt::Display for CatalogSourceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One registered catalog source.
#[derive(Clone)]
pub struct CatalogSourceBinding {
    id: CatalogSourceId,
    source: Arc<dyn CatalogSource>,
}

impl CatalogSourceBinding {
    /// Binds a source to its id.
    pub fn new(id: CatalogSourceId, source: Arc<dyn CatalogSource>) -> Self {
        Self { id, source }
    }

    /// Returns the id of this source.
    pub fn id(&self) -> &CatalogSourceId {
        &self.id
    }

    /// Returns the source itself.
    pub fn source(&self) -> &Arc<dyn CatalogSource> {
        &self.source
    }
}

impl std::fmt::Debug for CatalogSourceBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogSourceBinding")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The identity of the declared inputs a route's catalog was built under.
///
/// Two revisions are equal exactly when the declaration a fetch was built from is
/// equal, which is what makes a published snapshot still this route's: a route
/// re-registered under another declaration moves its revision, and a batch staged
/// under the declaration it replaced is refused at publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteInputRevision {
    catalog_descriptor: u64,
    normalization_rules: u64,
}

impl RouteInputRevision {
    /// Describes the inputs of one route.
    pub fn describe() -> RouteInputRevisionBuilder {
        RouteInputRevisionBuilder::default()
    }

    /// The inputs one declared route is registered under.
    ///
    /// What a catalog fetch is built from is stated by the registration itself:
    /// the route it addresses, the catalog source it binds and the identifier
    /// normalization rules its index is folded under. A route re-registered with
    /// any of them changed therefore moves its revision, and a batch a fetch
    /// staged under the declaration it replaced is refused at publication instead
    /// of being written under an index it was not built for.
    ///
    /// The identity of the route takes part in the comparison so that a route
    /// removed and registered again under another declaration is a moved
    /// revision, while the same declaration stays the same registration.
    pub fn of_declaration(
        route: &VenueRoute,
        catalog_source: Option<&CatalogSourceId>,
        normalization: &[IdentifierNormalizationRule],
    ) -> Self {
        Self::describe()
            .catalog_descriptor(content_fingerprint([
                route.venue.as_str(),
                route.scope.as_str(),
                catalog_source.map_or("", CatalogSourceId::as_str),
            ]))
            .normalization_rules(content_fingerprint(normalization.iter().map(|rule| {
                format_smolstr!(
                    "{}|{}|{}",
                    rule.identifier.label(),
                    rule.product.map_or_else(
                        || SmolStr::new_static(""),
                        |product| format_smolstr!("{product:?}")
                    ),
                    rule.mode.label(),
                )
            })))
            .build()
    }

    /// Returns the catalog descriptor generation.
    pub const fn catalog_descriptor(self) -> u64 {
        self.catalog_descriptor
    }

    /// Returns the identifier normalization rule generation.
    pub const fn normalization_rules(self) -> u64 {
        self.normalization_rules
    }
}

impl std::fmt::Display for RouteInputRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "descriptor:{} normalization:{}",
            self.catalog_descriptor, self.normalization_rules,
        )
    }
}

/// Collects the inputs of one route's catalog fetch into a [`RouteInputRevision`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RouteInputRevisionBuilder {
    revision: RouteInputRevision,
}

/// Returns a stable content fingerprint of one declared input dimension.
///
/// A revision field is an opaque comparison, so the only requirements are that
/// equal inputs hash equally and different inputs differ. FNV-1a over the
/// canonical field text is deterministic, which is what the comparison needs.
fn content_fingerprint<I, T>(fields: I) -> u64
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for field in fields {
        for byte in field.as_ref().as_bytes().iter().chain(&[0xff]) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

impl RouteInputRevisionBuilder {
    /// Sets the catalog descriptor generation.
    pub const fn catalog_descriptor(mut self, generation: u64) -> Self {
        self.revision.catalog_descriptor = generation;
        self
    }

    /// Sets the identifier normalization rule generation.
    pub const fn normalization_rules(mut self, generation: u64) -> Self {
        self.revision.normalization_rules = generation;
        self
    }

    /// Returns the revision these inputs describe.
    pub const fn build(self) -> RouteInputRevision {
        self.revision
    }
}

/// The inputs a registered catalog route was accepted under.
///
/// Every field is either owned by this crate or an opaque revision supplied by
/// the bridge, so a registered route can only be built through
/// [`CatalogSourceRegistry::plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRegistration {
    /// The identifier normalization rules of the route.
    pub normalization: Vec<IdentifierNormalizationRule>,
    /// The capabilities the route's own descriptor declares.
    ///
    /// They are the bridge's verdict, passed in as facts: a reader that needs to
    /// know whether a route serves something at all — an on-demand read whose
    /// capability is not part of the catalog — asks the route instead of sending a
    /// request the gateway would refuse.
    pub capabilities: Vec<CapabilityId>,
    /// The input revision of the route.
    pub revision: RouteInputRevision,
}

impl RouteRegistration {
    /// Returns the identifier normalization rules of this route.
    pub fn normalization(&self) -> &[IdentifierNormalizationRule] {
        &self.normalization
    }

    /// Returns the input revision of this route.
    pub const fn revision(&self) -> RouteInputRevision {
        self.revision
    }

    /// Resolves the normalization mode of one typed identifier kind on its own.
    ///
    /// This is the kind-only form, for the identifiers whose own variant carries
    /// no product: the product the route declared for that kind is the one that
    /// applies, so a product-qualified rule is what folds the route's own symbols
    /// instead of being a dead rule. A route that declared no product-qualified
    /// rule for the kind falls back to a route-wide rule, and one that declared
    /// none at all is `Exact`.
    ///
    /// A kind the route declared more than one product for is answered by the
    /// route-wide rule or by `Exact`: two products disagree about the same kind,
    /// and picking one of them would fold another product's identifier under it.
    pub fn normalization_mode(&self, kind: ListingIdentifierKind) -> SymbolNormalization {
        let product = self.declared_product(kind);
        self.normalization_mode_at(kind, product)
            .unwrap_or(SymbolNormalization::Exact)
    }

    /// Resolves the normalization mode of one typed identifier, product included.
    ///
    /// The kind comes from the variant and the product from its own fields, which
    /// is how the index a route publishes and the lookups against it are folded
    /// under the same rule.
    pub fn identifier_normalization_mode(
        &self,
        identifier: &ListingIdentifier,
    ) -> SymbolNormalization {
        match listing_normalization_product(identifier) {
            Some(product) => self
                .normalization_mode_at(identifier.kind(), Some(product))
                .unwrap_or(SymbolNormalization::Exact),
            None => self.normalization_mode(identifier.kind()),
        }
    }

    /// Resolves one kind at one explicit product granularity.
    ///
    /// A product-qualified rule wins, a kind with no rule for that product falls
    /// back to the route-wide rule, and a kind the route declares no rule for is
    /// `Exact`. A product that does not belong to the kind is reported as
    /// [`ModelError::IdentifierNormalizationMismatch`] rather than answered with
    /// another product's mode.
    pub fn normalization_mode_at(
        &self,
        kind: ListingIdentifierKind,
        product: Option<IdentifierNormalizationProduct>,
    ) -> Result<SymbolNormalization, ModelError> {
        normalization_mode(&self.normalization, kind, product)
    }

    /// Returns the one product this route declared for one identifier kind.
    ///
    /// A kind the route declared no product for, or declared two products for,
    /// has no single product of its own and answers `None`.
    fn declared_product(
        &self,
        kind: ListingIdentifierKind,
    ) -> Option<IdentifierNormalizationProduct> {
        let mut products = self
            .normalization
            .iter()
            .filter(|rule| rule.identifier == kind)
            .filter_map(|rule| rule.product);
        let product = products.next()?;
        products.next().is_none().then_some(product)
    }
}

/// The product one typed identifier carries in its own variant.
///
/// The index a route publishes is built by reading the identifier kind from the
/// variant and the product from its fields, so a product-qualified rule is looked
/// up at exactly the granularity it was declared at. `Symbol` and the
/// series-internal codes carry no product of their own, and a Polymarket token
/// bypasses symbol normalization entirely; their kind-only mode is the route's own
/// declaration for that kind.
pub fn listing_normalization_product(
    identifier: &ListingIdentifier,
) -> Option<IdentifierNormalizationProduct> {
    match identifier {
        ListingIdentifier::BinanceInstrument { product, .. } => {
            Some(IdentifierNormalizationProduct::Binance(*product))
        }
        ListingIdentifier::OkxInstrument {
            instrument_type, ..
        } => Some(IdentifierNormalizationProduct::Okx(*instrument_type)),
        ListingIdentifier::HyperliquidAsset { product, .. } => {
            Some(IdentifierNormalizationProduct::Hyperliquid(*product))
        }
        ListingIdentifier::HyperliquidOutcome { .. } => {
            Some(IdentifierNormalizationProduct::HyperliquidOutcome)
        }
        ListingIdentifier::Symbol(_)
        | ListingIdentifier::PolymarketToken(_)
        | ListingIdentifier::EquityCode { .. }
        | ListingIdentifier::FutureSeries(_)
        | ListingIdentifier::OccSymbol(_)
        | ListingIdentifier::MarketParticipantSeries(_)
        | ListingIdentifier::OptionLocalSymbol(_) => None,
    }
}

/// A route a source may fetch, together with the exact inputs it was accepted
/// under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredCatalogRoute {
    route: VenueRoute,
    catalog_source: CatalogSourceId,
    registration: Arc<RouteRegistration>,
}

impl RegisteredCatalogRoute {
    /// Returns the route this registration is about.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the catalog source bound to this route.
    pub fn catalog_source(&self) -> &CatalogSourceId {
        &self.catalog_source
    }

    /// Returns the registered inputs of this route.
    pub fn registration(&self) -> &RouteRegistration {
        &self.registration
    }

    /// Returns the input revision of this route.
    pub fn route_revision(&self) -> RouteInputRevision {
        self.registration.revision
    }

    /// Returns the identifier normalization rules of this route.
    pub fn identifier_normalization(&self) -> &[IdentifierNormalizationRule] {
        &self.registration.normalization
    }
}

/// One candidate route binding of a registry update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRouteRegistration {
    route: VenueRoute,
    catalog_source: Option<CatalogSourceId>,
    normalization: Vec<IdentifierNormalizationRule>,
    capabilities: Vec<CapabilityId>,
    revision: RouteInputRevision,
}

impl CatalogRouteRegistration {
    /// Describes a catalog route that binds one registered source.
    pub fn catalog(
        route: VenueRoute,
        source: CatalogSourceId,
        normalization: Vec<IdentifierNormalizationRule>,
        capabilities: Vec<CapabilityId>,
        revision: RouteInputRevision,
    ) -> Self {
        Self {
            route,
            catalog_source: Some(source),
            normalization,
            capabilities,
            revision,
        }
    }

    /// Describes a route that provides no catalog and binds no source.
    ///
    /// Named for what it is rather than for the discovery route it used to carry:
    /// discovery dials the venue's configured origin and has no route of its own.
    pub fn without_catalog(
        route: VenueRoute,
        normalization: Vec<IdentifierNormalizationRule>,
        capabilities: Vec<CapabilityId>,
        revision: RouteInputRevision,
    ) -> Self {
        Self {
            route,
            catalog_source: None,
            normalization,
            capabilities,
            revision,
        }
    }

    /// Returns the route of this candidate.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the input revision this candidate is registered under.
    pub const fn route_revision(&self) -> RouteInputRevision {
        self.revision
    }

    /// Returns the candidate source binding, absent for a route without a catalog.
    pub fn catalog_source(&self) -> Option<&CatalogSourceId> {
        self.catalog_source.as_ref()
    }

    /// Returns the capabilities this candidate declares.
    ///
    /// A commit derives the routes that must bind a catalog source from these: the
    /// candidate is the whole route set the commit is about to publish, so the
    /// verdict is read from the value being written rather than from a set a
    /// caller assembled before another venue committed.
    pub fn capabilities(&self) -> &[CapabilityId] {
        &self.capabilities
    }
}

/// A whole candidate update of the catalog source registry.
///
/// Both maps are the complete candidate state: a route or source that is absent
/// from the update is revoked. There is no partial update.
#[derive(Clone, Debug, Default)]
pub struct CatalogRouteUpdate {
    sources: Vec<CatalogSourceBinding>,
    routes: Vec<CatalogRouteRegistration>,
}

impl CatalogRouteUpdate {
    /// Builds a candidate update from its complete source and route maps.
    pub fn new(
        sources: impl IntoIterator<Item = CatalogSourceBinding>,
        routes: impl IntoIterator<Item = CatalogRouteRegistration>,
    ) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            routes: routes.into_iter().collect(),
        }
    }

    /// Adds one source binding to the candidate source map.
    pub fn with_source(mut self, source: CatalogSourceBinding) -> Self {
        self.sources.push(source);
        self
    }

    /// Returns the candidate source map.
    pub fn sources(&self) -> &[CatalogSourceBinding] {
        &self.sources
    }

    /// Returns the candidate route map.
    pub fn routes(&self) -> &[CatalogRouteRegistration] {
        &self.routes
    }
}

/// The accepted catalog source and route maps of one generation.
#[derive(Clone)]
pub struct CatalogRouteSet {
    sources: Arc<AHashMap<CatalogSourceId, CatalogSourceBinding>>,
    routes: Arc<AHashMap<VenueRoute, Arc<CatalogRouteEntry>>>,
    catalog_routes: Arc<[VenueRoute]>,
}

impl CatalogRouteSet {
    /// Returns every registered source binding.
    pub fn sources(&self) -> &AHashMap<CatalogSourceId, CatalogSourceBinding> {
        &self.sources
    }

    /// Returns every registered route, whether it provides a catalog or not.
    pub fn routes(&self) -> &AHashMap<VenueRoute, Arc<CatalogRouteEntry>> {
        &self.routes
    }

    /// Returns the routes that provide a catalog, in canonical route order.
    pub fn catalog_routes(&self) -> &[VenueRoute] {
        &self.catalog_routes
    }

    /// Returns the entry of one registered route.
    pub fn route(&self, route: &VenueRoute) -> Option<&Arc<CatalogRouteEntry>> {
        self.routes.get(route)
    }

    /// Returns the source bound to one registered catalog route.
    pub fn source_of(&self, route: &VenueRoute) -> Option<&CatalogSourceBinding> {
        let entry = self.routes.get(route)?;
        self.sources.get(entry.catalog_source.as_ref()?)
    }

    /// Returns the registration of one registered catalog route.
    pub fn registered_route(&self, route: &VenueRoute) -> Option<RegisteredCatalogRoute> {
        self.routes.get(route)?.as_registered_catalog_route()
    }
}

impl std::fmt::Debug for CatalogRouteSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogRouteSet")
            .field("sources", &self.sources.keys().collect::<Vec<_>>())
            .field("routes", &self.routes.keys().collect::<Vec<_>>())
            .field("catalog_routes", &self.catalog_routes)
            .finish()
    }
}

/// One registered route of a [`CatalogRouteSet`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRouteEntry {
    route: VenueRoute,
    catalog_source: Option<CatalogSourceId>,
    registration: Arc<RouteRegistration>,
}

impl CatalogRouteEntry {
    /// Returns the route of this entry.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the bound catalog source, absent for a route without a catalog.
    pub fn catalog_source(&self) -> Option<&CatalogSourceId> {
        self.catalog_source.as_ref()
    }

    /// Returns the registered inputs of this route.
    pub fn registration(&self) -> &RouteRegistration {
        &self.registration
    }

    /// Returns the route as a source may see it, when it provides a catalog.
    pub fn as_registered_catalog_route(&self) -> Option<RegisteredCatalogRoute> {
        Some(RegisteredCatalogRoute {
            route: self.route.clone(),
            catalog_source: self.catalog_source.clone()?,
            registration: Arc::clone(&self.registration),
        })
    }

    /// Returns this entry as a candidate registration of a later update.
    ///
    /// One venue's commit rebuilds the whole candidate route set inside the
    /// registry's write lock: every other venue's entries are carried over exactly
    /// as they were accepted — normalization, capabilities and revision alike — so
    /// a commit of one venue never re-registers another's routes under inputs it
    /// did not declare.
    pub fn as_registration(&self) -> CatalogRouteRegistration {
        match &self.catalog_source {
            Some(source) => CatalogRouteRegistration::catalog(
                self.route.clone(),
                source.clone(),
                self.registration.normalization.clone(),
                self.registration.capabilities.clone(),
                self.registration.revision,
            ),
            None => CatalogRouteRegistration::without_catalog(
                self.route.clone(),
                self.registration.normalization.clone(),
                self.registration.capabilities.clone(),
                self.registration.revision,
            ),
        }
    }
}

/// The already verified `(route, capability)` pairs of a candidate update.
///
/// The routing bridge owns the route registry and the runtime capability matrix,
/// so whether a route provides `MarketCatalog` is its verdict — passed in as a
/// parameter rather than re-derived here, because this crate must not decide
/// capabilities on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogRouteRequirements {
    catalog_routes: BTreeSet<VenueRoute>,
}

impl CatalogRouteRequirements {
    /// Builds the requirements from every verified pair.
    pub fn new(pairs: impl IntoIterator<Item = (VenueRoute, CapabilityId)>) -> Self {
        Self {
            catalog_routes: pairs
                .into_iter()
                .filter(|(_, capability)| *capability == CapabilityId::MarketCatalog)
                .map(|(route, _)| route)
                .collect(),
        }
    }

    /// Builds the requirements from the routes that provide a catalog.
    pub fn from_catalog_routes(routes: impl IntoIterator<Item = VenueRoute>) -> Self {
        Self {
            catalog_routes: routes.into_iter().collect(),
        }
    }

    /// Returns `true` when the route must bind exactly one catalog source.
    pub fn requires_catalog_source(&self, route: &VenueRoute) -> bool {
        self.catalog_routes.contains(route)
    }

    /// Returns the routes that provide a catalog.
    pub fn catalog_routes(&self) -> impl Iterator<Item = &VenueRoute> {
        self.catalog_routes.iter()
    }

    /// Returns the number of routes that provide a catalog.
    pub fn len(&self) -> usize {
        self.catalog_routes.len()
    }

    /// Returns `true` when no route provides a catalog.
    pub fn is_empty(&self) -> bool {
        self.catalog_routes.is_empty()
    }
}

/// The current source and route maps of one catalog.
#[derive(Clone, Debug, Default)]
pub struct CatalogSourceRegistry {
    route_set: Option<CatalogRouteSet>,
}

impl CatalogSourceRegistry {
    /// Creates an empty registry: no route is registered and nothing can be fetched.
    pub fn new() -> Self {
        Self { route_set: None }
    }

    /// Returns the accepted maps, absent until the first update.
    pub fn route_set(&self) -> Option<&CatalogRouteSet> {
        self.route_set.as_ref()
    }

    /// Validates a whole candidate update.
    ///
    /// `catalog_routes` is the already verified set of `(route, capability)`
    /// pairs the bridge accepted: a route in that set must bind exactly one
    /// registered source, and a route outside it must bind none. Nothing is
    /// committed here, so a refused candidate leaves the accepted maps exactly
    /// as they were.
    pub fn plan(
        &self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
    ) -> Result<CatalogRouteSet, CatalogError> {
        let sources = self.accepted_sources(&update)?;
        let claimed: BTreeSet<VenueRoute> = catalog_routes.catalog_routes().cloned().collect();
        let routes = accepted_routes(&update, &sources, &claimed)?;
        let mut catalog_routes: Vec<VenueRoute> = routes
            .iter()
            .filter(|(_, entry)| entry.catalog_source.is_some())
            .map(|(route, _)| route.clone())
            .collect();
        catalog_routes.sort();
        Ok(CatalogRouteSet {
            sources: Arc::new(sources),
            routes: Arc::new(routes),
            catalog_routes: catalog_routes.into(),
        })
    }

    /// Accepts a validated candidate update, replacing both maps at once.
    pub fn commit(&mut self, route_set: CatalogRouteSet) {
        self.route_set = Some(route_set);
    }

    /// Validates and commits a candidate update in one step.
    pub fn publish(
        &mut self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
    ) -> Result<CatalogRouteSet, CatalogError> {
        let route_set = self.plan(update, catalog_routes)?;
        self.commit(route_set.clone());
        Ok(route_set)
    }

    /// Returns whether the route is a registered catalog route.
    pub fn is_registered(&self, route: &VenueRoute) -> bool {
        self.route_set
            .as_ref()
            .is_some_and(|set| set.catalog_routes.binary_search(route).is_ok())
    }

    /// Returns the registration of one route, catalog or not.
    pub fn route_entry(&self, route: &VenueRoute) -> Option<&Arc<CatalogRouteEntry>> {
        self.route_set.as_ref()?.routes.get(route)
    }

    /// Returns the registration of one registered catalog route.
    pub fn registered_route(&self, route: &VenueRoute) -> Option<RegisteredCatalogRoute> {
        self.route_entry(route)?.as_registered_catalog_route()
    }

    /// Returns the source bound to one registered catalog route.
    pub fn source_of(&self, route: &VenueRoute) -> Option<&CatalogSourceBinding> {
        self.route_set.as_ref()?.source_of(route)
    }

    /// Returns the bound source of a route that must be registered.
    pub fn require_source(&self, route: &VenueRoute) -> Result<CatalogSourceBinding, CatalogError> {
        if !self.is_registered(route) {
            return Err(CatalogError::RouteNotRegistered {
                route: route.clone(),
            });
        }
        self.source_of(route)
            .cloned()
            .ok_or_else(|| CatalogError::RouteNotRegistered {
                route: route.clone(),
            })
    }

    /// Returns the registered route that must be present.
    pub fn require_registered_route(
        &self,
        route: &VenueRoute,
    ) -> Result<RegisteredCatalogRoute, CatalogError> {
        if !self.is_registered(route) {
            return Err(CatalogError::RouteNotRegistered {
                route: route.clone(),
            });
        }
        self.registered_route(route)
            .ok_or_else(|| CatalogError::RouteNotRegistered {
                route: route.clone(),
            })
    }

    /// Builds the accepted source map, refusing duplicate ids.
    fn accepted_sources(
        &self,
        update: &CatalogRouteUpdate,
    ) -> Result<AHashMap<CatalogSourceId, CatalogSourceBinding>, CatalogError> {
        let mut sources: AHashMap<CatalogSourceId, CatalogSourceBinding> =
            AHashMap::with_capacity(update.sources.len());
        for binding in &update.sources {
            if sources
                .insert(binding.id.clone(), binding.clone())
                .is_some()
            {
                return Err(CatalogError::DuplicateCatalogSourceId {
                    id: binding.id.to_smol_str(),
                });
            }
        }
        Ok(sources)
    }
}

/// Validates every candidate route binding against the accepted source map.
fn accepted_routes(
    update: &CatalogRouteUpdate,
    sources: &AHashMap<CatalogSourceId, CatalogSourceBinding>,
    claimed: &BTreeSet<VenueRoute>,
) -> Result<AHashMap<VenueRoute, Arc<CatalogRouteEntry>>, CatalogError> {
    let mut routes: AHashMap<VenueRoute, Arc<CatalogRouteEntry>> =
        AHashMap::with_capacity(update.routes.len());
    for candidate in &update.routes {
        validate_normalization_rules(&candidate.normalization).map_err(|_| {
            CatalogError::InconsistentRouteRegistration {
                route: candidate.route.clone(),
            }
        })?;
        let provides_catalog = claimed.contains(&candidate.route);
        let entry = match (&candidate.catalog_source, provides_catalog) {
            (Some(id), true) => {
                if !sources.contains_key(id) {
                    return Err(CatalogError::UnregisteredCatalogSource {
                        route: candidate.route.clone(),
                        id: id.to_smol_str(),
                    });
                }
                CatalogRouteEntry {
                    route: candidate.route.clone(),
                    catalog_source: Some(id.clone()),
                    registration: Arc::new(RouteRegistration {
                        normalization: candidate.normalization.clone(),
                        capabilities: candidate.capabilities.clone(),
                        revision: candidate.revision,
                    }),
                }
            }
            (Some(id), false) => {
                return Err(CatalogError::CatalogSourceOnNonCatalogRoute {
                    route: candidate.route.clone(),
                    id: id.to_smol_str(),
                    capability: CapabilityId::MarketCatalog.label().into(),
                });
            }
            (None, true) => {
                return Err(CatalogError::MissingCatalogSource {
                    route: candidate.route.clone(),
                    capability: CapabilityId::MarketCatalog.label().into(),
                });
            }
            (None, false) => CatalogRouteEntry {
                route: candidate.route.clone(),
                catalog_source: None,
                registration: Arc::new(RouteRegistration {
                    normalization: candidate.normalization.clone(),
                    capabilities: candidate.capabilities.clone(),
                    revision: candidate.revision,
                }),
            },
        };
        if routes
            .insert(candidate.route.clone(), Arc::new(entry))
            .is_some()
        {
            return Err(CatalogError::InconsistentRouteRegistration {
                route: candidate.route.clone(),
            });
        }
    }
    Ok(routes)
}

/// What one source fetch is given: the registered route it must answer for and
/// the staging view it may resolve canonical identities in.
#[derive(Clone, Debug)]
pub struct CatalogFetchContext {
    /// The registered route being fetched.
    pub route: RegisteredCatalogRoute,
    /// The copy-on-write staging view of the canonical registries.
    pub staging: CatalogStagingHandle,
}

impl CatalogFetchContext {
    /// Builds a fetch context.
    pub fn new(route: RegisteredCatalogRoute, staging: CatalogStagingHandle) -> Self {
        Self { route, staging }
    }

    /// Returns the route being fetched.
    pub fn route(&self) -> &RegisteredCatalogRoute {
        &self.route
    }

    /// Returns the route being fetched.
    pub fn venue_route(&self) -> &VenueRoute {
        self.route.route()
    }

    /// Returns the staging view.
    pub fn staging(&self) -> &CatalogStagingHandle {
        &self.staging
    }
}

/// Produces a fully protocol-parsed catalog batch for one registered route.
///
/// The catalog owns validation, indexing and publication, and a catalog is
/// fetched only by the connection action. A source translates an adapter
/// response into market-participant-neutral entries and may resolve canonical identities
/// through the staging view it is given; it can never publish a generation
/// itself, and nothing asks it for anything outside a load.
pub trait CatalogSource: Send + Sync + 'static {
    /// Fetches the whole catalog batch of one registered route.
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>>;
}

/// Binds one source instance to every catalog source id a registration set
/// names.
///
/// The factory is the caller's: the market participants root binds its own adapter-backed
/// source, so the catalog layer never learns what a venue's directory is fetched
/// through.
pub(crate) fn bindings_of(
    registrations: &[CatalogRouteRegistration],
    sources: impl Fn(&CatalogSourceId) -> Arc<dyn CatalogSource>,
) -> Vec<CatalogSourceBinding> {
    let mut bindings: Vec<CatalogSourceBinding> = Vec::new();
    for id in registrations
        .iter()
        .filter_map(|registration| registration.catalog_source())
    {
        if bindings.iter().any(|binding| binding.id() == id) {
            continue;
        }
        bindings.push(CatalogSourceBinding::new(id.clone(), sources(id)));
    }
    bindings
}
