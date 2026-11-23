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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogSourceId(SmolStr);

impl CatalogSourceId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CatalogError> {
        IdentifierNamespace::new(value.as_ref())
            .map(|identifier| Self(identifier.as_str().into()))
            .map_err(|_| CatalogError::InvalidCatalogSourceId {
                id: SmolStr::new(value.as_ref()),
            })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn to_smol_str(&self) -> SmolStr {
        self.0.clone()
    }
}

impl std::fmt::Display for CatalogSourceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone)]
pub struct CatalogSourceBinding {
    id: CatalogSourceId,
    source: Arc<dyn CatalogSource>,
}

impl CatalogSourceBinding {
    pub fn new(id: CatalogSourceId, source: Arc<dyn CatalogSource>) -> Self {
        Self { id, source }
    }

    pub fn id(&self) -> &CatalogSourceId {
        &self.id
    }

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteInputRevision {
    catalog_descriptor: u64,
    normalization_rules: u64,
}

impl RouteInputRevision {
    pub fn describe() -> RouteInputRevisionBuilder {
        RouteInputRevisionBuilder::default()
    }

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

    pub const fn catalog_descriptor(self) -> u64 {
        self.catalog_descriptor
    }

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

#[derive(Clone, Copy, Debug, Default)]
pub struct RouteInputRevisionBuilder {
    revision: RouteInputRevision,
}

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
    pub const fn catalog_descriptor(mut self, generation: u64) -> Self {
        self.revision.catalog_descriptor = generation;
        self
    }

    pub const fn normalization_rules(mut self, generation: u64) -> Self {
        self.revision.normalization_rules = generation;
        self
    }

    pub const fn build(self) -> RouteInputRevision {
        self.revision
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRegistration {
    pub normalization: Vec<IdentifierNormalizationRule>,
    pub capabilities: Vec<CapabilityId>,
    pub revision: RouteInputRevision,
}

impl RouteRegistration {
    pub fn normalization(&self) -> &[IdentifierNormalizationRule] {
        &self.normalization
    }

    pub const fn revision(&self) -> RouteInputRevision {
        self.revision
    }

    pub fn normalization_mode(&self, kind: ListingIdentifierKind) -> SymbolNormalization {
        let product = self.declared_product(kind);
        self.normalization_mode_at(kind, product)
            .unwrap_or(SymbolNormalization::Exact)
    }

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

    pub fn normalization_mode_at(
        &self,
        kind: ListingIdentifierKind,
        product: Option<IdentifierNormalizationProduct>,
    ) -> Result<SymbolNormalization, ModelError> {
        normalization_mode(&self.normalization, kind, product)
    }

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredCatalogRoute {
    route: VenueRoute,
    catalog_source: CatalogSourceId,
    registration: Arc<RouteRegistration>,
}

impl RegisteredCatalogRoute {
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn catalog_source(&self) -> &CatalogSourceId {
        &self.catalog_source
    }

    pub fn registration(&self) -> &RouteRegistration {
        &self.registration
    }

    pub fn route_revision(&self) -> RouteInputRevision {
        self.registration.revision
    }

    pub fn identifier_normalization(&self) -> &[IdentifierNormalizationRule] {
        &self.registration.normalization
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRouteRegistration {
    route: VenueRoute,
    catalog_source: Option<CatalogSourceId>,
    normalization: Vec<IdentifierNormalizationRule>,
    capabilities: Vec<CapabilityId>,
    revision: RouteInputRevision,
}

impl CatalogRouteRegistration {
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

    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub const fn route_revision(&self) -> RouteInputRevision {
        self.revision
    }

    pub fn catalog_source(&self) -> Option<&CatalogSourceId> {
        self.catalog_source.as_ref()
    }

    pub fn capabilities(&self) -> &[CapabilityId] {
        &self.capabilities
    }
}

#[derive(Clone, Debug, Default)]
pub struct CatalogRouteUpdate {
    sources: Vec<CatalogSourceBinding>,
    routes: Vec<CatalogRouteRegistration>,
}

impl CatalogRouteUpdate {
    pub fn new(
        sources: impl IntoIterator<Item = CatalogSourceBinding>,
        routes: impl IntoIterator<Item = CatalogRouteRegistration>,
    ) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            routes: routes.into_iter().collect(),
        }
    }

    pub fn with_source(mut self, source: CatalogSourceBinding) -> Self {
        self.sources.push(source);
        self
    }

    pub fn sources(&self) -> &[CatalogSourceBinding] {
        &self.sources
    }

    pub fn routes(&self) -> &[CatalogRouteRegistration] {
        &self.routes
    }
}

#[derive(Clone)]
pub struct CatalogRouteSet {
    sources: Arc<AHashMap<CatalogSourceId, CatalogSourceBinding>>,
    routes: Arc<AHashMap<VenueRoute, Arc<CatalogRouteEntry>>>,
    catalog_routes: Arc<[VenueRoute]>,
}

impl CatalogRouteSet {
    pub fn sources(&self) -> &AHashMap<CatalogSourceId, CatalogSourceBinding> {
        &self.sources
    }

    pub fn routes(&self) -> &AHashMap<VenueRoute, Arc<CatalogRouteEntry>> {
        &self.routes
    }

    pub fn catalog_routes(&self) -> &[VenueRoute] {
        &self.catalog_routes
    }

    pub fn route(&self, route: &VenueRoute) -> Option<&Arc<CatalogRouteEntry>> {
        self.routes.get(route)
    }

    pub fn source_of(&self, route: &VenueRoute) -> Option<&CatalogSourceBinding> {
        let entry = self.routes.get(route)?;
        self.sources.get(entry.catalog_source.as_ref()?)
    }

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRouteEntry {
    route: VenueRoute,
    catalog_source: Option<CatalogSourceId>,
    registration: Arc<RouteRegistration>,
}

impl CatalogRouteEntry {
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn catalog_source(&self) -> Option<&CatalogSourceId> {
        self.catalog_source.as_ref()
    }

    pub fn registration(&self) -> &RouteRegistration {
        &self.registration
    }

    pub fn as_registered_catalog_route(&self) -> Option<RegisteredCatalogRoute> {
        Some(RegisteredCatalogRoute {
            route: self.route.clone(),
            catalog_source: self.catalog_source.clone()?,
            registration: Arc::clone(&self.registration),
        })
    }

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogRouteRequirements {
    catalog_routes: BTreeSet<VenueRoute>,
}

impl CatalogRouteRequirements {
    pub fn new(pairs: impl IntoIterator<Item = (VenueRoute, CapabilityId)>) -> Self {
        Self {
            catalog_routes: pairs
                .into_iter()
                .filter(|(_, capability)| *capability == CapabilityId::MarketCatalog)
                .map(|(route, _)| route)
                .collect(),
        }
    }

    pub fn from_catalog_routes(routes: impl IntoIterator<Item = VenueRoute>) -> Self {
        Self {
            catalog_routes: routes.into_iter().collect(),
        }
    }

    pub fn requires_catalog_source(&self, route: &VenueRoute) -> bool {
        self.catalog_routes.contains(route)
    }

    pub fn catalog_routes(&self) -> impl Iterator<Item = &VenueRoute> {
        self.catalog_routes.iter()
    }

    pub fn len(&self) -> usize {
        self.catalog_routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.catalog_routes.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct CatalogSourceRegistry {
    route_set: Option<CatalogRouteSet>,
}

impl CatalogSourceRegistry {
    pub fn new() -> Self {
        Self { route_set: None }
    }

    pub fn route_set(&self) -> Option<&CatalogRouteSet> {
        self.route_set.as_ref()
    }

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

    pub fn commit(&mut self, route_set: CatalogRouteSet) {
        self.route_set = Some(route_set);
    }

    pub fn publish(
        &mut self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
    ) -> Result<CatalogRouteSet, CatalogError> {
        let route_set = self.plan(update, catalog_routes)?;
        self.commit(route_set.clone());
        Ok(route_set)
    }

    pub fn is_registered(&self, route: &VenueRoute) -> bool {
        self.route_set
            .as_ref()
            .is_some_and(|set| set.catalog_routes.binary_search(route).is_ok())
    }

    pub fn route_entry(&self, route: &VenueRoute) -> Option<&Arc<CatalogRouteEntry>> {
        self.route_set.as_ref()?.routes.get(route)
    }

    pub fn registered_route(&self, route: &VenueRoute) -> Option<RegisteredCatalogRoute> {
        self.route_entry(route)?.as_registered_catalog_route()
    }

    pub fn source_of(&self, route: &VenueRoute) -> Option<&CatalogSourceBinding> {
        self.route_set.as_ref()?.source_of(route)
    }

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

#[derive(Clone, Debug)]
pub struct CatalogFetchContext {
    pub route: RegisteredCatalogRoute,
    pub staging: CatalogStagingHandle,
}

impl CatalogFetchContext {
    pub fn new(route: RegisteredCatalogRoute, staging: CatalogStagingHandle) -> Self {
        Self { route, staging }
    }

    pub fn route(&self) -> &RegisteredCatalogRoute {
        &self.route
    }

    pub fn venue_route(&self) -> &VenueRoute {
        self.route.route()
    }

    pub fn staging(&self) -> &CatalogStagingHandle {
        &self.staging
    }
}

pub trait CatalogSource: Send + Sync + 'static {
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>>;
}

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
