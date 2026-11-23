use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    instrument::InstrumentKind,
    listing::{
        route::VenueRoute,
        venue::{BinanceProduct, HyperliquidAssetProduct, OkxInstrumentType},
    },
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
};

use crate::{
    catalog::source::CatalogSourceId,
    routing::{
        CORE_PERP_SCOPE, CORE_SPOT_SCOPE, LONGBRIDGE_US_SCOPE, PERPETUAL_SCOPE,
        RouteCapabilityDescriptor, SPOT_SCOPE, SWAP_SCOPE, VenueRouteDescriptor,
    },
    venue::definition::{CapabilitySupport, ScopeEntry},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RouteCapability {
    capability: CapabilityId,
    needs_auth: bool,
}

const fn public(capability: CapabilityId) -> RouteCapability {
    RouteCapability {
        capability,
        needs_auth: false,
    }
}

const fn authenticated(capability: CapabilityId) -> RouteCapability {
    RouteCapability {
        capability,
        needs_auth: true,
    }
}
#[derive(Clone, Copy, Debug)]
struct NormalizationRule {
    identifier: ListingIdentifierKind,
    product: Option<IdentifierNormalizationProduct>,
    mode: SymbolNormalization,
}

const fn instrument_rule(
    identifier: ListingIdentifierKind,
    product: IdentifierNormalizationProduct,
) -> NormalizationRule {
    NormalizationRule {
        identifier,
        product: Some(product),
        mode: SymbolNormalization::AsciiCaseInsensitive,
    }
}

const fn symbol_rule(kind: InstrumentKind) -> NormalizationRule {
    NormalizationRule {
        identifier: ListingIdentifierKind::Symbol,
        product: Some(IdentifierNormalizationProduct::Instrument(kind)),
        mode: SymbolNormalization::AsciiCaseInsensitive,
    }
}

const fn exact_rule(
    identifier: ListingIdentifierKind,
    product: IdentifierNormalizationProduct,
) -> NormalizationRule {
    NormalizationRule {
        identifier,
        product: Some(product),
        mode: SymbolNormalization::Exact,
    }
}

const BINANCE_SPOT_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Spot),
    instrument_rule(
        ListingIdentifierKind::BinanceInstrument,
        IdentifierNormalizationProduct::Binance(BinanceProduct::Spot),
    ),
];

const BINANCE_PERP_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Perpetual),
    instrument_rule(
        ListingIdentifierKind::BinanceInstrument,
        IdentifierNormalizationProduct::Binance(BinanceProduct::Perpetual),
    ),
];

const OKX_SPOT_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Spot),
    instrument_rule(
        ListingIdentifierKind::OkxInstrument,
        IdentifierNormalizationProduct::Okx(OkxInstrumentType::Spot),
    ),
];

const OKX_SWAP_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Perpetual),
    instrument_rule(
        ListingIdentifierKind::OkxInstrument,
        IdentifierNormalizationProduct::Okx(OkxInstrumentType::Perpetual),
    ),
];

const HYPERLIQUID_SPOT_NORMALIZATION: &[NormalizationRule] = &[exact_rule(
    ListingIdentifierKind::HyperliquidAsset,
    IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Spot),
)];

const HYPERLIQUID_PERP_NORMALIZATION: &[NormalizationRule] = &[exact_rule(
    ListingIdentifierKind::HyperliquidAsset,
    IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Perpetual),
)];

const LONGBRIDGE_US_NORMALIZATION: &[NormalizationRule] = &[symbol_rule(InstrumentKind::Spot)];

#[derive(Clone, Copy, Debug)]
struct RouteSpec {
    scope: &'static str,
    capabilities: &'static [RouteCapability],
    normalization: &'static [NormalizationRule],
}

const BINANCE_ROUTES: &[RouteSpec] = &[
    RouteSpec {
        scope: SPOT_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
            authenticated(CapabilityId::AccountHistory),
        ],
        normalization: BINANCE_SPOT_NORMALIZATION,
    },
    RouteSpec {
        scope: PERPETUAL_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            public(CapabilityId::ForceOrder),
            public(CapabilityId::OpenInterest),
            public(CapabilityId::LongShortAccountRatio),
            public(CapabilityId::TopTraderAccountRatio),
            public(CapabilityId::TopTraderPositionRatio),
            public(CapabilityId::TakerVolume),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountPositions),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
            authenticated(CapabilityId::AccountHistory),
        ],
        normalization: BINANCE_PERP_NORMALIZATION,
    },
];

const OKX_ROUTES: &[RouteSpec] = &[
    RouteSpec {
        scope: SPOT_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
            authenticated(CapabilityId::AccountHistory),
        ],
        normalization: OKX_SPOT_NORMALIZATION,
    },
    RouteSpec {
        scope: SWAP_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            public(CapabilityId::ForceOrder),
            public(CapabilityId::OpenInterest),
            public(CapabilityId::LongShortAccountRatio),
            public(CapabilityId::TopTraderAccountRatio),
            public(CapabilityId::TopTraderPositionRatio),
            public(CapabilityId::TakerVolume),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountPositions),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
            authenticated(CapabilityId::AccountHistory),
        ],
        normalization: OKX_SWAP_NORMALIZATION,
    },
];

const HYPERLIQUID_ROUTES: &[RouteSpec] = &[
    RouteSpec {
        scope: CORE_SPOT_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            public(CapabilityId::Wallet),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
        ],
        normalization: HYPERLIQUID_SPOT_NORMALIZATION,
    },
    RouteSpec {
        scope: CORE_PERP_SCOPE,
        capabilities: &[
            public(CapabilityId::MarketCatalog),
            public(CapabilityId::Candlesticks),
            public(CapabilityId::MarketTrade),
            public(CapabilityId::Tickers),
            public(CapabilityId::AllTickers),
            public(CapabilityId::OrderBook),
            public(CapabilityId::Wallet),
            authenticated(CapabilityId::AuthenticatedTrading),
            authenticated(CapabilityId::AccountOrders),
            authenticated(CapabilityId::AccountPositions),
            authenticated(CapabilityId::AccountBalances),
            authenticated(CapabilityId::AccountTrades),
        ],
        normalization: HYPERLIQUID_PERP_NORMALIZATION,
    },
];

const POLYMARKET_ROUTES: &[RouteSpec] = &[RouteSpec {
    scope: crate::routing::PREDICT_SCOPE,
    capabilities: &[
        public(CapabilityId::PredictionEvents),
        public(CapabilityId::PredictionSeries),
        public(CapabilityId::PredictionOrderBook),
    ],
    normalization: &[],
}];

const LONGBRIDGE_ROUTES: &[RouteSpec] = &[RouteSpec {
    scope: LONGBRIDGE_US_SCOPE,
    capabilities: &[
        public(CapabilityId::MarketCatalog),
        authenticated(CapabilityId::Candlesticks),
        authenticated(CapabilityId::Tickers),
    ],
    normalization: LONGBRIDGE_US_NORMALIZATION,
}];

fn specs_of(venue: VenueId) -> &'static [RouteSpec] {
    match venue {
        VenueId::Binance => BINANCE_ROUTES,
        VenueId::Hyperliquid => HYPERLIQUID_ROUTES,
        VenueId::Longbridge => LONGBRIDGE_ROUTES,
        VenueId::Okx => OKX_ROUTES,
        VenueId::Polymarket => POLYMARKET_ROUTES,
    }
}

fn spec_of(venue: VenueId, scope: &VenueScopeId) -> Option<&'static RouteSpec> {
    specs_of(venue)
        .iter()
        .find(|spec| spec.scope == scope.as_str())
}

pub(crate) fn scope_entries(venue: VenueId) -> Vec<ScopeEntry> {
    specs_of(venue)
        .iter()
        .map(|spec| ScopeEntry {
            scope: scope_name(spec.scope),
            catalog_source: catalog_source_of(&VenueRoute::new(venue, scope_name(spec.scope))),
            normalization: spec
                .normalization
                .iter()
                .map(|rule| {
                    IdentifierNormalizationRule::new(rule.identifier, rule.product, rule.mode)
                })
                .collect(),
            capabilities: spec
                .capabilities
                .iter()
                .map(|capability| CapabilitySupport {
                    capability: capability.capability,
                    needs_auth: capability.needs_auth,
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn all_descriptors() -> Vec<VenueRouteDescriptor> {
    VenueId::ALL
        .into_iter()
        .flat_map(|venue| {
            specs_of(venue)
                .iter()
                .map(move |spec| {
                    let route = VenueRoute::new(venue, scope_name(spec.scope));
                    descriptor_of_spec(&route, spec)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn descriptor_of_entry(venue: VenueId, entry: &ScopeEntry) -> VenueRouteDescriptor {
    let route = VenueRoute::new(venue, entry.scope.clone());
    if let Some(spec) = spec_of(venue, &entry.scope) {
        return descriptor_of_spec(&route, spec);
    }
    let capabilities = entry
        .capabilities
        .iter()
        .map(|entry| RouteCapabilityDescriptor::new(entry.capability, entry.needs_auth))
        .collect();
    VenueRouteDescriptor::new(
        route,
        capabilities,
        entry.normalization.clone(),
        entry.catalog_source.clone(),
    )
}

pub(crate) fn entry_of_descriptor(descriptor: &VenueRouteDescriptor) -> ScopeEntry {
    ScopeEntry {
        scope: descriptor.route().scope.clone(),
        catalog_source: descriptor.catalog_source().cloned(),
        normalization: descriptor.identifier_normalization().to_vec(),
        capabilities: descriptor
            .capabilities()
            .iter()
            .map(|capability| CapabilitySupport {
                capability: capability.capability(),
                needs_auth: capability.needs_auth(),
            })
            .collect(),
    }
}

fn descriptor_of_spec(route: &VenueRoute, spec: &'static RouteSpec) -> VenueRouteDescriptor {
    VenueRouteDescriptor::new(
        route.clone(),
        spec.capabilities
            .iter()
            .map(|capability| {
                RouteCapabilityDescriptor::new(capability.capability, capability.needs_auth)
            })
            .collect(),
        spec.normalization
            .iter()
            .map(|rule| IdentifierNormalizationRule::new(rule.identifier, rule.product, rule.mode))
            .collect(),
        catalog_source_of(route),
    )
}

fn catalog_source_of(route: &VenueRoute) -> Option<CatalogSourceId> {
    let spec = spec_of(route.venue, &route.scope)?;
    spec.capabilities
        .iter()
        .any(|capability| capability.capability == CapabilityId::MarketCatalog)
        .then(|| {
            CatalogSourceId::new(smol_str::format_smolstr!(
                "{}-{}-catalog",
                route.venue.as_str(),
                route.scope.as_str()
            ))
            .expect("a statically known venue and scope are catalog source id characters")
        })
}

fn scope_name(name: &'static str) -> VenueScopeId {
    VenueScopeId::new(name).expect("a built-in scope name is canonical")
}
