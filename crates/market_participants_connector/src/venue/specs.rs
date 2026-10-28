//! The static scope table of every venue this build carries an identity for.
//!
//! One row per statically known scope: its canonical name, the capability names
//! it serves with whether each of them needs the venue's credential, and its
//! identifier normalization rules. The table is crate-private, and it is read in
//! exactly two places: a loader turns a venue's rows into the scope entries a
//! load answers with, and the load transaction turns them into the route
//! descriptors the registry is written with.
//!
//! A scope a venue deploys rather than declares — a builder-deployed perpetual
//! DEX — has no row here: its entry comes from the venue's own discovery reading
//! and is carried into the registry by [`descriptor_of_entry`], which is why the
//! two readers above are the table's only ones.

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

/// One capability of a built-in route.
///
/// A capability states whether a request for it has to carry the venue's
/// credential. Which hosts it dials is not a property of a scope's own table: the
/// hosts are the venue's own settings, and whether a request travels over REST or a
/// socket is the request type's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RouteCapability {
    capability: CapabilityId,
    needs_auth: bool,
}

/// A capability whose requests are public.
const fn public(capability: CapabilityId) -> RouteCapability {
    RouteCapability {
        capability,
        needs_auth: false,
    }
}

/// A capability whose requests sign with the venue's credential.
const fn authenticated(capability: CapabilityId) -> RouteCapability {
    RouteCapability {
        capability,
        needs_auth: true,
    }
}
/// A normalization rule declared at compile time.
#[derive(Clone, Copy, Debug)]
struct NormalizationRule {
    identifier: ListingIdentifierKind,
    product: Option<IdentifierNormalizationProduct>,
    mode: SymbolNormalization,
}

/// Declares an ASCII-case-insensitive instrument identifier rule.
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

/// Declares an ASCII-case-insensitive display-symbol rule of one instrument kind.
///
/// A display symbol belongs to the route's own instrument kind, so it is declared
/// as an `Instrument` product: a venue product qualifier (`Binance`, `Okx`) only
/// ever applies to that venue's own instrument identifier, and the model refuses
/// a rule that pairs it with anything else.
const fn symbol_rule(kind: InstrumentKind) -> NormalizationRule {
    NormalizationRule {
        identifier: ListingIdentifierKind::Symbol,
        product: Some(IdentifierNormalizationProduct::Instrument(kind)),
        mode: SymbolNormalization::AsciiCaseInsensitive,
    }
}

/// Declares an exact-match identifier rule.
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

/// Binance spot normalization: instrument identifiers fold ASCII case.
const BINANCE_SPOT_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Spot),
    instrument_rule(
        ListingIdentifierKind::BinanceInstrument,
        IdentifierNormalizationProduct::Binance(BinanceProduct::Spot),
    ),
];

/// Binance USD-M perpetual normalization.
const BINANCE_PERP_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Perpetual),
    instrument_rule(
        ListingIdentifierKind::BinanceInstrument,
        IdentifierNormalizationProduct::Binance(BinanceProduct::Perpetual),
    ),
];

/// OKX spot normalization.
const OKX_SPOT_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Spot),
    instrument_rule(
        ListingIdentifierKind::OkxInstrument,
        IdentifierNormalizationProduct::Okx(OkxInstrumentType::Spot),
    ),
];

/// OKX swap normalization.
const OKX_SWAP_NORMALIZATION: &[NormalizationRule] = &[
    symbol_rule(InstrumentKind::Perpetual),
    instrument_rule(
        ListingIdentifierKind::OkxInstrument,
        IdentifierNormalizationProduct::Okx(OkxInstrumentType::Perpetual),
    ),
];

/// Hyperliquid core spot asset normalization.
///
/// Hyperliquid asset identifiers are indexed exactly, so a HIP-3 coin name can
/// never be folded into a core spot symbol. The rule is declared per product, so
/// a product without a rule falls back to `Exact` rather than to another
/// product's mode.
const HYPERLIQUID_SPOT_NORMALIZATION: &[NormalizationRule] = &[exact_rule(
    ListingIdentifierKind::HyperliquidAsset,
    IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Spot),
)];

/// Hyperliquid core perpetual asset normalization.
const HYPERLIQUID_PERP_NORMALIZATION: &[NormalizationRule] = &[exact_rule(
    ListingIdentifierKind::HyperliquidAsset,
    IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Perpetual),
)];

/// Longbridge US-equity normalization.
///
/// The display symbol folds ASCII case, which is what a Nasdaq code needs: the venue's own
/// codes are upper case, and a route addressed with a lower-case one asks for the same
/// listing.
///
/// There is no second rule, and the equity local code is why: an identifier the table does
/// not name is matched exactly, which is already what `EquityLocalCode` means. A rule
/// restating the default would be a second place to keep in step without changing an answer.
const LONGBRIDGE_US_NORMALIZATION: &[NormalizationRule] = &[symbol_rule(InstrumentKind::Spot)];

/// The static declaration of one built-in route.
#[derive(Clone, Copy, Debug)]
struct RouteSpec {
    scope: &'static str,
    capabilities: &'static [RouteCapability],
    normalization: &'static [NormalizationRule],
}

/// Everything the built-in Binance routes declare.
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

/// Everything the built-in OKX routes declare.
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

/// Everything the built-in Hyperliquid routes declare.
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

/// Everything the built-in Longbridge route declares.
///
/// The venue publishes one scope — the US equity market — so the table has one row.
///
/// The directory is public: it is the venue operator's own listing file rather than anything
/// behind the account's credentials. Candlesticks and tickers are authenticated: the venue
/// serves history, streams and real-time quotes on one authenticated session, so a request
/// for any of them is honest about needing the venue's credential.
///
/// [`CapabilityId::AllTickers`] is deliberately absent: the venue answers a ticker one
/// listing at a time and has no command for the whole-scope read, so a caller that needs
/// every listing's ticker in one answer cannot be served by this route.
const LONGBRIDGE_ROUTES: &[RouteSpec] = &[RouteSpec {
    scope: LONGBRIDGE_US_SCOPE,
    capabilities: &[
        public(CapabilityId::MarketCatalog),
        authenticated(CapabilityId::Candlesticks),
        authenticated(CapabilityId::Tickers),
    ],
    normalization: LONGBRIDGE_US_NORMALIZATION,
}];

/// Returns the static rows of one venue, in declaration order.
///
/// This is the dispatch from a venue identity to its scope table, and it is total over
/// [`VenueId`]: the four tables are the four venues the identity carries, so there is no fifth
/// answer for a venue the table happens not to carry. Being total over the identity is the same
/// set as being served — every one of the four has its row here and its definition in
/// [`crate::venue::definition`] — and a block does not state its own venue, because the block a
/// venue is answered by is the statement of it.
fn specs_of(venue: VenueId) -> &'static [RouteSpec] {
    match venue {
        VenueId::Binance => BINANCE_ROUTES,
        VenueId::Hyperliquid => HYPERLIQUID_ROUTES,
        VenueId::Longbridge => LONGBRIDGE_ROUTES,
        VenueId::Okx => OKX_ROUTES,
    }
}

/// Returns the row of one statically known scope of one venue.
fn spec_of(venue: VenueId, scope: &VenueScopeId) -> Option<&'static RouteSpec> {
    specs_of(venue)
        .iter()
        .find(|spec| spec.scope == scope.as_str())
}

/// Returns the scope entries one venue publishes from the static table.
///
/// A loader answers with these: the entries carry what a load publishes — a scope
/// name, the catalog it binds, its normalization rules and the capability names it
/// serves with whether each needs the venue's credential — and nothing about an
/// origin or a transport.
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

/// Returns every statically declared route descriptor of this build.
///
/// A fixture of this crate's own tests: a test that needs the built-in descriptor set
/// without a venue answering reads it here, and no build outside a unit test can reach
/// it.
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

/// Returns the descriptor of one scope entry of one venue.
///
/// A statically known scope is answered from its own row, so its capabilities and
/// normalization rules are the ones this build declares. A scope the venue deployed
/// has no row: its descriptor is built from the entry the venue's own reading
/// produced. Which origins either of them is reached on is not a descriptor's
/// business — the venue's own settings answer that for every scope it serves.
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

/// Returns the entry of a descriptor a venue deployed rather than declared.
///
/// The two shapes are one scope's facts: a deployed scope's entry is its
/// descriptor without the origin and the credential, which are the venue's own
/// settings rather than a scope's.
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

/// Returns the descriptor of one statically known scope.
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

/// Returns the catalog source id of one statically known catalog scope.
///
/// A catalog source id is unique inside the candidate set it is registered in, so
/// it names the route's venue *and* its scope: `binance/spot` and `okx/spot` are
/// two catalogs, and a scope-only id would make the second binding collide with
/// the first.
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

/// Validates a built-in scope name.
fn scope_name(name: &'static str) -> VenueScopeId {
    VenueScopeId::new(name).expect("a built-in scope name is canonical")
}
