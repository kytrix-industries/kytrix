use std::sync::Arc;

use ahash::AHashMap;
use market_participants_model::{
    identifier::VenueId,
    listing::route::VenueRoute,
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization, normalization_mode,
        validate_normalization_rules,
    },
};

use crate::{
    catalog::{
        error::CatalogError,
        source::{
            CatalogRouteRegistration, CatalogRouteRequirements, CatalogSourceId, RouteInputRevision,
        },
    },
    protocol::RequestIdentity,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialGeneration(u64);

impl CredentialGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteCapabilityDescriptor {
    capability: CapabilityId,
    needs_auth: bool,
}

impl RouteCapabilityDescriptor {
    pub const fn new(capability: CapabilityId, needs_auth: bool) -> Self {
        Self {
            capability,
            needs_auth,
        }
    }

    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    pub const fn needs_auth(&self) -> bool {
        self.needs_auth
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueRouteDescriptor {
    route: VenueRoute,
    capabilities: Vec<RouteCapabilityDescriptor>,
    identifier_normalization: Vec<IdentifierNormalizationRule>,
    catalog_source: Option<CatalogSourceId>,
}

impl VenueRouteDescriptor {
    pub fn new(
        route: VenueRoute,
        capabilities: Vec<RouteCapabilityDescriptor>,
        identifier_normalization: Vec<IdentifierNormalizationRule>,
        catalog_source: Option<CatalogSourceId>,
    ) -> Self {
        Self {
            route,
            capabilities,
            identifier_normalization,
            catalog_source,
        }
    }

    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn capabilities(&self) -> &[RouteCapabilityDescriptor] {
        &self.capabilities
    }

    pub fn identifier_normalization(&self) -> &[IdentifierNormalizationRule] {
        &self.identifier_normalization
    }

    pub fn catalog_source(&self) -> Option<&CatalogSourceId> {
        self.catalog_source.as_ref()
    }

    pub fn capability(&self, capability: CapabilityId) -> Option<&RouteCapabilityDescriptor> {
        self.capabilities
            .binary_search_by_key(&capability, RouteCapabilityDescriptor::capability)
            .ok()
            .map(|index| &self.capabilities[index])
    }

    pub fn supports(&self, capability: CapabilityId) -> bool {
        self.capability(capability).is_some()
    }

    pub fn provides_catalog(&self) -> bool {
        self.supports(CapabilityId::MarketCatalog)
    }

    pub fn identity(&self, capability: CapabilityId) -> RequestIdentity {
        RequestIdentity::new(self.route.clone(), capability)
    }

    pub fn identities(&self) -> impl Iterator<Item = RequestIdentity> + '_ {
        self.capabilities
            .iter()
            .map(|descriptor| self.identity(descriptor.capability))
    }

    pub fn normalization_mode(
        &self,
        identifier: ListingIdentifierKind,
        product: Option<IdentifierNormalizationProduct>,
    ) -> Result<SymbolNormalization, DescriptorError> {
        normalization_mode(&self.identifier_normalization, identifier, product).map_err(|_| {
            DescriptorError::IdentifierNormalizationMismatch {
                route: self.route.clone(),
                identifier,
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DescriptorError {
    #[error("route {route} is registered twice")]
    DuplicateRoute { route: VenueRoute },
    #[error("capability {capability} of route {route} has more than one descriptor")]
    DuplicateCapability {
        route: VenueRoute,
        capability: CapabilityId,
    },
    #[error("route {route} declares inconsistent identifier normalization rules")]
    InvalidNormalization { route: VenueRoute },
    #[error("route {route} provides MarketCatalog without a catalog source")]
    MissingCatalogSource { route: VenueRoute },
    #[error("route {route} declares a catalog source without providing MarketCatalog")]
    CatalogSourceWithoutCatalog { route: VenueRoute },
    #[error("route {route} has no scope whose name is a catalog source id")]
    InvalidCatalogSourceId { route: VenueRoute },
    #[error(
        "identifier {identifier:?} has no normalization rule matching its product on route {route}"
    )]
    IdentifierNormalizationMismatch {
        route: VenueRoute,
        identifier: ListingIdentifierKind,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Descriptor(#[from] DescriptorError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

#[derive(Clone, Debug, Default)]
pub struct RegisteredRoutes {
    descriptors: Arc<[VenueRouteDescriptor]>,
    by_route: AHashMap<VenueRoute, Arc<VenueRouteDescriptor>>,
}

impl RegisteredRoutes {
    fn build(descriptors: Vec<VenueRouteDescriptor>) -> Self {
        let mut by_route = AHashMap::with_capacity(descriptors.len());
        for descriptor in &descriptors {
            by_route.insert(descriptor.route.clone(), Arc::new(descriptor.clone()));
        }
        Self {
            descriptors: descriptors.into(),
            by_route,
        }
    }

    pub fn routes(&self) -> &[VenueRouteDescriptor] {
        &self.descriptors
    }

    pub fn descriptor(&self, route: &VenueRoute) -> Option<&Arc<VenueRouteDescriptor>> {
        self.by_route.get(route)
    }

    pub fn supports(&self, route: &VenueRoute, capability: CapabilityId) -> bool {
        self.by_route
            .get(route)
            .is_some_and(|descriptor| descriptor.supports(capability))
    }

    pub fn capability_descriptor(
        &self,
        route: &VenueRoute,
        capability: CapabilityId,
    ) -> Option<&RouteCapabilityDescriptor> {
        self.by_route
            .get(route)
            .and_then(|descriptor| descriptor.capability(capability))
    }

    pub fn catalog_routes(&self) -> impl Iterator<Item = &VenueRouteDescriptor> {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.provides_catalog())
    }

    pub fn authenticated_routes(&self) -> impl Iterator<Item = &VenueRouteDescriptor> {
        self.descriptors.iter().filter(|descriptor| {
            descriptor
                .capabilities
                .iter()
                .any(|capability| capability.needs_auth)
        })
    }

    pub fn requirements(&self) -> CatalogRouteRequirements {
        CatalogRouteRequirements::new(self.descriptors.iter().flat_map(|descriptor| {
            descriptor
                .capabilities
                .iter()
                .map(move |capability| (descriptor.route.clone(), capability.capability))
        }))
    }
}

pub struct VenueRouteRegistry {
    venues: AHashMap<VenueId, Arc<[VenueRouteDescriptor]>>,
    index: Arc<RegisteredRoutes>,
}

impl VenueRouteRegistry {
    pub fn new() -> Self {
        Self {
            venues: AHashMap::new(),
            index: Arc::new(RegisteredRoutes::default()),
        }
    }

    pub fn routes(&self) -> &Arc<RegisteredRoutes> {
        &self.index
    }

    pub fn has_venue(&self, venue: &VenueId) -> bool {
        self.venues.contains_key(venue)
    }

    pub fn supports(&self, route: &VenueRoute, capability: CapabilityId) -> bool {
        self.index.supports(route, capability)
    }

    pub fn plan_venue_routes(
        &self,
        venue: &VenueId,
        descriptors: Vec<VenueRouteDescriptor>,
    ) -> Result<RegisteredRoutes, RegistryError> {
        candidate_routes(self.candidate(venue, descriptors)).map_err(RegistryError::Descriptor)
    }

    pub fn commit_venue_routes(
        &mut self,
        venue: &VenueId,
        descriptors: Vec<VenueRouteDescriptor>,
    ) -> Arc<RegisteredRoutes> {
        let candidate = self.candidate(venue, descriptors);
        let index = candidate_routes(candidate)
            .expect("a venue commit validates its candidate before it writes");
        let own: Vec<VenueRouteDescriptor> = index
            .routes()
            .iter()
            .filter(|descriptor| descriptor.route().venue == *venue)
            .cloned()
            .collect();
        if own.is_empty() {
            self.venues.remove(venue);
        } else {
            self.venues.insert(*venue, own.into());
        }
        self.index = Arc::new(index);
        Arc::clone(&self.index)
    }

    fn candidate(
        &self,
        venue: &VenueId,
        descriptors: Vec<VenueRouteDescriptor>,
    ) -> Vec<VenueRouteDescriptor> {
        let mut candidate: Vec<VenueRouteDescriptor> =
            Vec::with_capacity(descriptors.len() + self.index.routes().len());
        for (other, routes) in &self.venues {
            if other != venue {
                candidate.extend(routes.iter().cloned());
            }
        }
        candidate.extend(descriptors);
        candidate
    }

    pub fn source_of(&self, route: &VenueRoute) -> Option<CatalogSourceId> {
        let routes = &self.index;
        let descriptor = routes.descriptor(route)?;
        descriptor.catalog_source().cloned()
    }
}

impl Default for VenueRouteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn candidate_routes(
    descriptors: Vec<VenueRouteDescriptor>,
) -> Result<RegisteredRoutes, DescriptorError> {
    let mut routes: Vec<VenueRouteDescriptor> = descriptors;

    routes.sort_by(|left, right| left.route.cmp(&right.route));
    for pair in routes.windows(2) {
        if pair[0].route == pair[1].route {
            return Err(DescriptorError::DuplicateRoute {
                route: pair[1].route.clone(),
            });
        }
    }
    for descriptor in &mut routes {
        validate_descriptor(descriptor)?;
    }
    Ok(RegisteredRoutes::build(routes))
}

fn validate_descriptor(descriptor: &mut VenueRouteDescriptor) -> Result<(), DescriptorError> {
    let route = descriptor.route.clone();
    descriptor
        .capabilities
        .sort_by_key(|entry| entry.capability);
    for pair in descriptor.capabilities.windows(2) {
        if pair[0].capability == pair[1].capability {
            return Err(DescriptorError::DuplicateCapability {
                route,
                capability: pair[1].capability,
            });
        }
    }
    validate_normalization_rules(&descriptor.identifier_normalization).map_err(|_| {
        DescriptorError::InvalidNormalization {
            route: route.clone(),
        }
    })?;
    let provides_catalog = descriptor.provides_catalog();
    match (&descriptor.catalog_source, provides_catalog) {
        (Some(_), true) => {}
        (None, true) => {
            return Err(DescriptorError::MissingCatalogSource { route });
        }
        (Some(_), false) => {
            return Err(DescriptorError::CatalogSourceWithoutCatalog { route });
        }
        (None, false) => {}
    }
    Ok(())
}

pub(crate) fn venue_registrations(
    index: &RegisteredRoutes,
    venue: &VenueId,
) -> Vec<CatalogRouteRegistration> {
    index
        .routes()
        .iter()
        .filter(|descriptor| descriptor.route().venue == *venue)
        .map(|descriptor| {
            let source_id = descriptor.catalog_source().cloned();
            let revision = RouteInputRevision::of_declaration(
                descriptor.route(),
                descriptor.catalog_source(),
                descriptor.identifier_normalization(),
            );
            let capabilities: Vec<CapabilityId> = descriptor
                .capabilities()
                .iter()
                .map(RouteCapabilityDescriptor::capability)
                .collect();
            match source_id {
                Some(id) => CatalogRouteRegistration::catalog(
                    descriptor.route().clone(),
                    id,
                    descriptor.identifier_normalization().to_vec(),
                    capabilities,
                    revision,
                ),
                None => CatalogRouteRegistration::without_catalog(
                    descriptor.route().clone(),
                    descriptor.identifier_normalization().to_vec(),
                    capabilities,
                    revision,
                ),
            }
        })
        .collect()
}

pub const BINANCE_VENUE: &str = "binance";
pub const OKX_VENUE: &str = "okx";
pub const HYPERLIQUID_VENUE: &str = "hyperliquid";
pub const LONGBRIDGE_VENUE: &str = "longbridge";
pub const SPOT_SCOPE: &str = "spot";
pub const PERPETUAL_SCOPE: &str = "usd-m-perp";
pub const SWAP_SCOPE: &str = "swap";
pub const CORE_SPOT_SCOPE: &str = "core-spot";
pub const CORE_PERP_SCOPE: &str = "core-perp";
pub const PREDICT_SCOPE: &str = "predict";
pub const LONGBRIDGE_US_SCOPE: &str = "us";
pub const HYPERLIQUID_WALLET_SCOPE: &str = CORE_PERP_SCOPE;

#[cfg(test)]
mod tests {

    fn commit(registry: &mut VenueRouteRegistry, venue: &str, routes: Vec<VenueRouteDescriptor>) {
        registry
            .plan_venue_routes(&self::venue(venue), routes.clone())
            .expect("the fixture candidate is a valid route set");
        registry.commit_venue_routes(&self::venue(venue), routes);
    }

    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        routing::{CapabilityId, IdentifierNormalizationRule, ListingIdentifierKind},
    };

    use super::*;
    use crate::catalog::source::CatalogSourceId;

    fn route(venue: &str, scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::from_name(venue).expect("the venue should be canonical"),
            VenueScopeId::new(scope).expect("the scope should be canonical"),
        )
    }

    fn catalog_source() -> CatalogSourceId {
        CatalogSourceId::new("binance-spot-catalog").expect("the source id should be canonical")
    }

    fn catalog_route() -> VenueRouteDescriptor {
        VenueRouteDescriptor::new(
            route("binance", "spot"),
            vec![
                RouteCapabilityDescriptor::new(CapabilityId::MarketCatalog, false),
                RouteCapabilityDescriptor::new(CapabilityId::Tickers, false),
            ],
            vec![IdentifierNormalizationRule::new(
                ListingIdentifierKind::Symbol,
                None,
                SymbolNormalization::AsciiCaseInsensitive,
            )],
            Some(catalog_source()),
        )
    }

    fn validate(descriptor: VenueRouteDescriptor) -> Result<RegisteredRoutes, DescriptorError> {
        candidate_routes(vec![descriptor])
    }

    fn validate_outcome(descriptor: VenueRouteDescriptor) -> Result<(), DescriptorError> {
        validate(descriptor).map(|_| ())
    }

    fn validate_all(descriptors: Vec<VenueRouteDescriptor>) -> Result<(), DescriptorError> {
        candidate_routes(descriptors).map(|_| ())
    }

    #[test]
    fn a_capability_descriptor_set_is_sorted_unique_and_complete() {
        let routes = validate(catalog_route()).expect("a valid descriptor should be accepted");
        let descriptor = routes
            .descriptor(&route("binance", "spot"))
            .expect("the route should be registered");
        let capabilities = descriptor
            .capabilities()
            .iter()
            .map(RouteCapabilityDescriptor::capability)
            .collect::<Vec<_>>();

        assert_eq!(
            capabilities,
            vec![CapabilityId::MarketCatalog, CapabilityId::Tickers]
        );
        assert_eq!(descriptor.catalog_source(), Some(&catalog_source()));
    }

    #[test]
    fn an_unsorted_capability_list_is_accepted_and_normalized() {
        let mut descriptor = catalog_route();
        descriptor.capabilities.reverse();
        let routes = validate(descriptor).expect("the list is sorted, not required to be sorted");

        let capabilities = routes
            .descriptor(&route("binance", "spot"))
            .expect("registered")
            .capabilities()
            .iter()
            .map(RouteCapabilityDescriptor::capability)
            .collect::<Vec<_>>();
        assert_eq!(
            capabilities,
            vec![CapabilityId::MarketCatalog, CapabilityId::Tickers]
        );
    }

    #[test]
    fn a_duplicated_capability_is_refused() {
        let mut descriptor = catalog_route();
        descriptor
            .capabilities
            .push(RouteCapabilityDescriptor::new(CapabilityId::Tickers, false));

        assert_eq!(
            validate_outcome(descriptor),
            Err(DescriptorError::DuplicateCapability {
                route: route("binance", "spot"),
                capability: CapabilityId::Tickers,
            })
        );
    }

    #[test]
    fn a_route_without_capabilities_is_accepted() {
        let empty =
            VenueRouteDescriptor::new(route("binance", "spot"), Vec::new(), Vec::new(), None);
        let routes = validate(empty).expect("an empty capability list is accepted");
        let descriptor = routes
            .descriptor(&route("binance", "spot"))
            .expect("the route is registered");

        assert!(descriptor.capabilities().is_empty());
        assert!(!descriptor.provides_catalog());
    }

    #[test]
    fn a_catalog_route_must_bind_a_source_and_a_route_without_one_must_not_serve_a_catalog() {
        let mut without_source = catalog_route();
        without_source.catalog_source = None;
        assert_eq!(
            validate_outcome(without_source),
            Err(DescriptorError::MissingCatalogSource {
                route: route("binance", "spot"),
            })
        );

        let no_catalog = VenueRouteDescriptor::new(
            route("hyperliquid", "core-perp"),
            vec![RouteCapabilityDescriptor::new(CapabilityId::Tickers, false)],
            Vec::new(),
            None,
        );
        let routes = validate(no_catalog).expect("a route that serves no catalog binds no source");
        assert!(
            routes.catalog_routes().next().is_none(),
            "a route without a catalog source never enters the catalog route set"
        );

        let with_source = VenueRouteDescriptor::new(
            route("hyperliquid", "core-perp"),
            vec![RouteCapabilityDescriptor::new(CapabilityId::Tickers, false)],
            Vec::new(),
            Some(catalog_source()),
        );
        assert_eq!(
            validate_outcome(with_source),
            Err(DescriptorError::CatalogSourceWithoutCatalog {
                route: route("hyperliquid", "core-perp"),
            })
        );
    }

    #[test]
    fn a_duplicated_normalization_rule_is_refused() {
        let rule = IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            None,
            SymbolNormalization::Exact,
        );
        let mut descriptor = catalog_route();
        descriptor.identifier_normalization = vec![rule, rule];

        assert_eq!(
            validate_outcome(descriptor),
            Err(DescriptorError::InvalidNormalization {
                route: route("binance", "spot"),
            })
        );
    }

    #[test]
    fn a_duplicated_route_is_refused() {
        assert_eq!(
            validate_all(vec![catalog_route(), catalog_route()]),
            Err(DescriptorError::DuplicateRoute {
                route: route("binance", "spot"),
            })
        );
    }

    #[test]
    fn every_registered_pair_resolves_to_its_descriptor() {
        let routes = validate(catalog_route()).expect("a valid descriptor should be accepted");
        let spot = route("binance", "spot");

        assert!(routes.supports(&spot, CapabilityId::MarketCatalog));
        assert!(routes.supports(&spot, CapabilityId::Tickers));
        assert_eq!(
            routes
                .capability_descriptor(&spot, CapabilityId::MarketCatalog)
                .map(RouteCapabilityDescriptor::capability),
            Some(CapabilityId::MarketCatalog)
        );
        assert!(
            !routes.supports(&spot, CapabilityId::OrderBook),
            "a capability the route does not declare is not supported"
        );
        assert!(
            routes
                .capability_descriptor(&spot, CapabilityId::OrderBook)
                .is_none(),
            "and answers no descriptor for it"
        );
        assert!(
            !routes.supports(&route("binance", "usd-m-perp"), CapabilityId::MarketCatalog),
            "a route that is not registered at all answers nothing"
        );
    }

    #[test]
    fn a_capability_descriptor_states_whether_it_needs_the_venues_credential() {
        let mut descriptor = catalog_route();
        descriptor.capabilities.push(RouteCapabilityDescriptor::new(
            CapabilityId::AuthenticatedTrading,
            true,
        ));
        let routes = validate(descriptor).expect("a valid descriptor should be accepted");
        let spot = route("binance", "spot");
        let descriptor = routes
            .descriptor(&spot)
            .expect("the route should be registered");

        assert!(
            !descriptor
                .capability(CapabilityId::MarketCatalog)
                .expect("the catalog capability is declared")
                .needs_auth(),
            "a public market read signs with nothing"
        );
        assert!(
            descriptor
                .capability(CapabilityId::AuthenticatedTrading)
                .expect("the trading capability is declared")
                .needs_auth(),
            "a command signs with the venue's credential"
        );
        assert_eq!(
            descriptor.identity(CapabilityId::AuthenticatedTrading),
            RequestIdentity::new(spot.clone(), CapabilityId::AuthenticatedTrading),
            "the identity a request acts under is its route and capability together"
        );
        assert_eq!(
            descriptor.identities().collect::<Vec<_>>(),
            vec![
                RequestIdentity::new(spot.clone(), CapabilityId::MarketCatalog),
                RequestIdentity::new(spot.clone(), CapabilityId::Tickers),
                RequestIdentity::new(spot, CapabilityId::AuthenticatedTrading),
            ],
            "every declared capability answers its own identity, in capability order"
        );
    }

    fn okx_route() -> VenueRouteDescriptor {
        VenueRouteDescriptor::new(
            route("okx", "spot"),
            vec![RouteCapabilityDescriptor::new(
                CapabilityId::MarketCatalog,
                false,
            )],
            vec![IdentifierNormalizationRule::new(
                ListingIdentifierKind::Symbol,
                None,
                SymbolNormalization::Exact,
            )],
            Some(
                CatalogSourceId::new("okx-spot-catalog")
                    .expect("the fixture source id is canonical"),
            ),
        )
    }

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue is canonical")
    }

    #[test]
    fn an_empty_registry_is_a_legal_state() {
        let registry = VenueRouteRegistry::new();

        assert!(registry.routes().routes().is_empty());
        assert!(!registry.has_venue(&venue("binance")));
        assert!(
            !registry.supports(&route("binance", "spot"), CapabilityId::MarketCatalog),
            "a venue that was never loaded holds no route"
        );
    }

    #[test]
    fn publishing_a_venue_only_replaces_its_own_slot() {
        let mut registry = VenueRouteRegistry::new();
        commit(&mut registry, "binance", vec![catalog_route()]);
        commit(&mut registry, "okx", vec![okx_route()]);

        assert!(registry.has_venue(&venue("binance")));
        assert!(registry.has_venue(&venue("okx")));
        assert!(registry.supports(&route("binance", "spot"), CapabilityId::MarketCatalog));
        assert!(registry.supports(&route("okx", "spot"), CapabilityId::MarketCatalog));
    }

    #[test]
    fn a_route_absent_from_the_next_candidate_is_revoked() {
        let mut registry = VenueRouteRegistry::new();
        commit(&mut registry, "binance", vec![catalog_route(), okx_route()]);
        assert!(registry.supports(&route("okx", "spot"), CapabilityId::MarketCatalog));

        commit(&mut registry, "binance", vec![catalog_route()]);

        assert!(registry.supports(&route("binance", "spot"), CapabilityId::MarketCatalog));
        assert!(
            !registry.supports(&route("okx", "spot"), CapabilityId::MarketCatalog),
            "a route the candidate omits is revoked"
        );
        assert!(
            registry.source_of(&route("okx", "spot")).is_none(),
            "the revoked route's catalog source binding is dropped with it"
        );
    }

    #[test]
    fn an_empty_candidate_revokes_the_venue() {
        let mut registry = VenueRouteRegistry::new();
        commit(&mut registry, "binance", vec![catalog_route()]);
        commit(&mut registry, "okx", vec![okx_route()]);

        commit(&mut registry, "binance", Vec::new());

        assert!(!registry.has_venue(&venue("binance")));
        assert!(!registry.supports(&route("binance", "spot"), CapabilityId::MarketCatalog));
        assert!(
            registry.supports(&route("okx", "spot"), CapabilityId::MarketCatalog),
            "the other venue is untouched"
        );
    }

    #[test]
    fn a_refused_candidate_leaves_the_venue_slot_as_it_was() {
        let mut registry = VenueRouteRegistry::new();
        commit(&mut registry, "binance", vec![catalog_route()]);

        let refused =
            registry.plan_venue_routes(&venue("binance"), vec![catalog_route(), catalog_route()]);

        assert!(matches!(refused, Err(RegistryError::Descriptor(_))));
        assert!(registry.supports(&route("binance", "spot"), CapabilityId::MarketCatalog));
    }
}
