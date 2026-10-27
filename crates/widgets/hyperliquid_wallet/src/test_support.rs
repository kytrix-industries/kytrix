//! What the service and the pane are tested through.
//!
//! The venue is faked rather than the service: a fake answers what Hyperliquid would answer, and
//! every reading the service makes — which routes it read, how often it re-read a timeline,
//! whether a subscription is still held — is then something a test can assert against the fake's
//! own record.
//!
//! A test session carries the catalog the reads are validated and displayed against, so a test
//! names the listings a route publishes and gets listings, ticks and asset names from the same
//! publication. The listings it publishes are Hyperliquid's own: a listing is identified by the
//! route and the venue's asset index, so a fixture that used another venue's identifiers would
//! be a catalog this build refuses.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ahash::{AHashMap, AHashSet};
use gpui_kit::{App, TestAppContext};
use market_participants_connector::{
    account::{
        domain::{
            HyperliquidAssetContext, HyperliquidHistoricalOrder, HyperliquidLedgerEvent,
            HyperliquidOpenOrder, HyperliquidPerpetualPosition, HyperliquidPerpetualState,
            HyperliquidSpotBalance, HyperliquidSpotPair, HyperliquidSpotState, HyperliquidUserFill,
            HyperliquidWalletUpdate,
        },
        error::AccountError,
        shared::PositionDirection,
    },
    catalog::{
        batch::CatalogEntry,
        display::catalog::DisplaySnapshot,
        registry::asset::AssetTerms,
        source::{CatalogRouteRegistration, CatalogRouteUpdate, CatalogSourceBinding},
        store::Catalog,
        test_support,
    },
};
use market_participants_model::{
    asset::{AssetId, CryptoTokenId, CryptoTokenOrigin, TokenReference},
    decimal::{Fraction, Quantity, Ratio, SignedQuantity, Tick},
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, TokenIssuerId, VenueId, VenueScopeId,
        WalletAddress,
    },
    instrument::{Instrument, InstrumentTerms, PerpetualTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::{HyperliquidListingIdentifiers, VenueListingIdentifiers},
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, FundingInterval, FundingSchedule, LeverageTier, LotPolicy,
            MarginMode, MarginProfile, PerpetualMarginTerms, PerpetualRisk, PerpetualTradingRules,
            PriceRule, ProductTradingRules, QuantityRule, ShortSalePolicy, SpotTradingRules,
            TradingRules, VenueTradingPolicy,
        },
        venue::{HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidTokenId, PriceBasis},
    },
    market::{
        account::AssetAmount,
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit, Side},
    },
};
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::mpsc;
use widget::{
    runtime::WidgetRuntime,
    stream::{LatestStream, ReliableStream},
};

use super::{
    model::{DetailData, DetailSnapshot, DisplayPrecision, TimelineSnapshot, WalletDataError},
    service::{
        WalletProtocol, WalletSource, WalletStatus, WalletStreamKey, WalletStreamMessage,
        WalletSubscription,
    },
    view::HyperliquidWalletConfig,
};

/// The venue every test route belongs to.
pub(super) const VENUE: &str = "hyperliquid";
/// The scope of the test perpetual route.
///
/// The scopes are the market participant's own: the pane asks the registry which routes may be watched,
/// and a test route under a scope the market participant does not publish is a route no watch may name.
pub(super) const PERPETUAL_SCOPE: &str = "core-perp";
/// The scope of the test spot route.
pub(super) const SPOT_SCOPE: &str = "core-spot";
/// A wallet that is monitored whenever a test needs one.
pub(super) const ACCOUNT: &str = "0x1111111111111111111111111111111111111111";
/// The token codes every fixture catalog registers, which is what a test reading names.
const FIXTURE_CODES: &[&str] = &["ETH", "BTC", "HYPE", "SOL", "USDC", "USDT", "USDH"];

/// A test route of the venue.
pub(super) fn route(scope: &str) -> VenueRoute {
    VenueRoute::new(
        venue(),
        VenueScopeId::new(scope).expect("the test scope should be canonical"),
    )
}

/// The venue identity every test route belongs to.
pub(super) fn venue() -> VenueId {
    VenueId::Hyperliquid
}

/// The test perpetual route.
pub(super) fn perpetual_route() -> VenueRoute {
    route(PERPETUAL_SCOPE)
}

/// The test spot route.
pub(super) fn spot_route() -> VenueRoute {
    route(SPOT_SCOPE)
}

/// A wallet address from a short, readable suffix, repeated to the address's own length.
///
/// The digits are the ones a test wrote, so an address in a fixture is readable as the reading
/// it stands for rather than as a run of zeroes.
pub(super) fn address(suffix: &str) -> WalletAddress {
    let repeated = suffix.repeat(40_usize.div_ceil(suffix.len()));
    format!("0x{}", &repeated[..40])
        .parse()
        .expect("a test address should be valid")
}

/// The canonical identity of a test token the venue issues.
pub(super) fn token(code: &str) -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new(VENUE).expect("the test issuer should be canonical"),
            namespace: IdentifierNamespace::new(VENUE).expect("the test namespace is canonical"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("the test code should be canonical"),
            },
        })
        .expect("the test token should be valid"),
    )
}

/// The venue's own index of one test token.
///
/// A code the fixture does not register is not a token this fixture can publish; the shared
/// fallback index would alias it onto whichever other unregistered code a test used.
fn token_index(code: &str) -> HyperliquidAssetId {
    let index = FIXTURE_CODES
        .iter()
        .position(|candidate| *candidate == code)
        .unwrap_or_else(|| panic!("the test token {code} is not registered"));
    HyperliquidAssetId(100 + index as u32)
}

/// The venue's own index of one test spot book.
///
/// A spot book is indexed by the pair it trades, not by the two codes run together: the venue
/// indexes every book it lists, so two books of one base are two indexes. It only has to be
/// stable within a test, and every test pair is drawn from this table, so the order here is the
/// fixture's own book numbering.
const FIXTURE_SPOT_BOOKS: &[(&str, &str)] = &[
    ("HYPE", "USDC"),
    ("HYPE", "USDT"),
    ("HYPE", "USDH"),
    ("SOL", "USDC"),
    ("USDC", "USDT"),
    ("BTC", "USDT"),
];

/// The venue's own index of one test spot pair.
///
/// A pair the table does not name is not a pair this fixture can publish: answering with a shared
/// fallback index would silently alias two pairs into one listing, which is exactly the shape that
/// hid the production defect this fixture exists to guard.
fn spot_book_index(base: &str, quote: &str) -> HyperliquidAssetId {
    let index = FIXTURE_SPOT_BOOKS
        .iter()
        .position(|(candidate_base, candidate_quote)| {
            *candidate_base == base && *candidate_quote == quote
        })
        .unwrap_or_else(|| panic!("the test spot book {base}/{quote} is not registered"));
    HyperliquidAssetId(200 + index as u32)
}

/// A perpetual instrument tracking one test token, settled in the venue's stablecoin.
pub(super) fn perp_instrument(asset: &str) -> Instrument {
    let underlying = market_participants_model::asset::DerivativeUnderlying::new(
        market_participants_model::asset::UnderlyingRef::Asset(token(asset)),
        price_reference(asset),
    );
    Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            underlying,
            token("USDC"),
            token("USDC"),
            market_participants_model::settlement::ContractSize::new(
                market_participants_model::settlement::ContractUnit::asset_quantity(
                    Quantity::from(1_u8),
                    token(asset),
                )
                .expect("a one-unit contract should be valid"),
            )
            .expect("a one-unit contract size should be valid"),
            market_participants_model::settlement::DerivativePnlModel::Linear,
            None,
        )
        .expect("the test perpetual terms should be valid"),
    ))
    .expect("the test perpetual instrument should be valid")
}

/// A spot instrument of two test tokens.
pub(super) fn spot_instrument(base: &str, quote: &str) -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(token(base), token(quote)).expect("the test spot terms should be valid"),
    ))
    .expect("the test spot instrument should be valid")
}

/// The canonical price reference of one test asset.
fn price_reference(asset: &str) -> market_participants_model::reference::PriceReferenceId {
    use market_participants_model::{
        identifier::ReferenceTermsVersion,
        reference::{
            PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
            ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
        },
    };

    PriceReferenceTerms {
        underlying: market_participants_model::asset::UnderlyingRef::Asset(token(asset)),
        quote_asset: token("USDC"),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: market_participants_model::identifier::ReferenceSourceId::new(VENUE)
                .expect("the test source should be canonical"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new("v1").expect("the test version is canonical"),
    }
    .reference_id()
    .expect("the test price reference should be valid")
}

/// The listing key of one Hyperliquid listing.
fn hyperliquid_key(
    route: &VenueRoute,
    product: HyperliquidAssetProduct,
    asset: &str,
) -> VenueListingKey {
    let asset_id = match product {
        HyperliquidAssetProduct::Spot => {
            let (base, quote) = asset
                .split_once('/')
                .expect("a test spot book should be named `BASE/QUOTE`");
            spot_book_index(base, quote)
        }
        _ => token_index(asset),
    };
    ListingContinuityKey {
        route: route.clone(),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product,
            scope: route.scope.clone(),
            asset_id,
        },
    }
    .listing_key()
    .expect("the test continuity key should be valid")
}

/// The venue's identifiers of one Hyperliquid listing.
fn hyperliquid_identifiers(route: &VenueRoute, instrument: &Instrument) -> VenueListingIdentifiers {
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => {
            let base = code_of(terms.base()).expect("a spot base should be a test token");
            let quote = code_of(terms.quote()).expect("a spot quote should be a test token");
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Spot {
                asset_id: spot_book_index(base, quote),
                base_token_id: HyperliquidTokenId(token_index(base).0),
                quote_token_id: HyperliquidTokenId(token_index(quote).0),
            })
        }
        InstrumentTerms::Perpetual(terms) => {
            let asset = terms
                .underlying()
                .reference()
                .asset()
                .and_then(code_of)
                .expect("a perpetual underlying should be a test token");
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Perpetual {
                dex: route.scope.clone(),
                asset_id: token_index(asset),
                coin: NativeSymbol::new(asset).expect("a test symbol should be canonical"),
            })
        }
        InstrumentTerms::Future(_) | InstrumentTerms::Option(_) | InstrumentTerms::Outcome(_) => {
            panic!("the test fixture publishes no series listings")
        }
    }
}

/// The test token code one test asset was built from.
fn code_of(asset: &AssetId) -> Option<&'static str> {
    let AssetId::CryptoToken(id) = asset else {
        return None;
    };
    FIXTURE_CODES
        .iter()
        .copied()
        .find(|code| &CryptoTokenId::of_terms(&test_support::token_terms(VENUE, code)) == id)
}

/// The listing key of one test perpetual listing.
pub(super) fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
    hyperliquid_key(route, HyperliquidAssetProduct::Perpetual, symbol)
}

/// The perpetual listing of one test route, as the fixture publishes it.
pub(super) fn published_perpetual(asset: &str) -> VenueListingKey {
    listing_key(&perpetual_route(), asset)
}

/// The spot listing of one test route, as the fixture publishes it.
pub(super) fn published_spot(base: &str, quote: &str) -> VenueListingKey {
    let symbol = format_smolstr!("{base}/{quote}");
    hyperliquid_key(
        &spot_route(),
        HyperliquidAssetProduct::Spot,
        symbol.as_str(),
    )
}

/// The listing key of the spot pair two test assets form.
pub(super) fn pair_listing(base: &str, quote: &str) -> VenueListingKey {
    published_spot(base, quote)
}

/// One listing of one test route, as the venue publishes it.
///
/// The price rule is quoted in the asset the listing's own terms are quoted in, which is what a
/// venue's catalog states: a spot pair is quoted in its own quote token, and a perpetual in the
/// collateral of the DEX that lists it. A fixture that hardcoded one quote asset would make two
/// pairs of one base look like one market.
pub(super) fn entry(
    route: &VenueRoute,
    instrument: Instrument,
    symbol: &str,
    price_tick: &str,
    quantity_tick: &str,
) -> CatalogEntry {
    let product = match instrument.kind() {
        market_participants_model::instrument::InstrumentKind::Spot => {
            HyperliquidAssetProduct::Spot
        }
        _ => HyperliquidAssetProduct::Perpetual,
    };
    let key = hyperliquid_key(route, product, symbol);
    let (quantity_unit, quote_asset) = match instrument.terms() {
        InstrumentTerms::Spot(terms) => (
            QuantityUnit::AssetUnits {
                asset: terms.base().clone(),
            },
            terms.quote().clone(),
        ),
        _ => (QuantityUnit::Contracts, token("USDC")),
    };
    let rules = TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset,
                basis: PriceBasis::PerAssetUnit,
                tick: Tick::try_from(
                    price_tick
                        .parse::<Quantity>()
                        .expect("a test price tick should parse"),
                )
                .expect("a test price tick should be valid"),
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: quantity_unit,
                step: quantity_tick
                    .parse::<Quantity>()
                    .expect("a test quantity tick should parse"),
                minimum: None,
                maximum: None,
            },
            notional: None,
            venue_policy: std::sync::Arc::new(VenueTradingPolicy::continuous()),
        },
        product: std::sync::Arc::new(match instrument.terms() {
            InstrumentTerms::Spot(_) => ProductTradingRules::Spot(
                SpotTradingRules::new(
                    LotPolicy::FractionalAllowed {
                        lot_size: Quantity::from(1_u8),
                        minimum_fraction: None,
                    },
                    ShortSalePolicy::Allowed,
                )
                .expect("the test spot rules should be valid"),
            ),
            _ => ProductTradingRules::Perpetual(PerpetualTradingRules {
                risk: PerpetualRisk::Published(
                    PerpetualMarginTerms::new(
                        MarginProfile::new(
                            MarginMode::Cross,
                            Fraction::new(
                                "0.1".parse::<Ratio>().expect("a margin ratio should parse"),
                            )
                            .expect("the test initial margin should be a fraction"),
                            Fraction::new(
                                "0.05"
                                    .parse::<Ratio>()
                                    .expect("a margin ratio should parse"),
                            )
                            .expect("the test maintenance margin should be a fraction"),
                            token("USDC"),
                        )
                        .expect("the test margin profile should be valid"),
                        vec![
                            LeverageTier::new(
                                Quantity::from(1_000_u64),
                                10,
                                Fraction::new(
                                    "0.05"
                                        .parse::<Ratio>()
                                        .expect("a margin ratio should parse"),
                                )
                                .expect("the test maintenance margin should be a fraction"),
                            )
                            .expect("the test leverage tier should be valid"),
                        ],
                    )
                    .expect("the test margin terms should be valid"),
                ),
                funding: FundingSchedule::new(FundingInterval::Adaptive, None, None, token("USDC"))
                    .expect("the test funding schedule should be valid"),
                open_interest_cap: None,
            }),
        }),
    };
    let instrument_id = *instrument.id();
    let listing = VenueListing::new(
        key.clone(),
        instrument_id,
        Some(NativeSymbol::new(symbol).expect("a test symbol should be canonical")),
        Vec::new(),
        hyperliquid_identifiers(route, &instrument),
    )
    .expect("the test listing should be valid");
    let snapshot = ListingSnapshot::new(key, rules, ListingStatus::Trading, None, None, 0)
        .expect("the test listing snapshot should be valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

/// One perpetual listing of the test perpetual route with the ticks a test names.
pub(super) fn perp_entry(
    route: &VenueRoute,
    asset: &str,
    price_tick: &str,
    quantity_tick: &str,
) -> CatalogEntry {
    entry(
        route,
        perp_instrument(asset),
        asset,
        price_tick,
        quantity_tick,
    )
}

/// One spot listing of the test spot route with the ticks a test names.
pub(super) fn spot_entry(
    route: &VenueRoute,
    base: &str,
    quote: &str,
    price_tick: &str,
    quantity_tick: &str,
) -> CatalogEntry {
    entry(
        route,
        spot_instrument(base, quote),
        format_smolstr!("{base}/{quote}").as_str(),
        price_tick,
        quantity_tick,
    )
}

/// A catalog whose routes publish the given listings.
///
/// Every route is published in one commit at one generation, which is what the display and the
/// market-participant registry both read: a test that reports readings about two routes reads one venue's
/// listings either way, and the route set it installs carries the generation the registry was
/// built at.
pub(super) fn fixture(routes: &[(VenueRoute, Vec<CatalogEntry>)]) -> Arc<Catalog> {
    let catalog = Arc::new(Catalog::new());
    let sources: Vec<_> = routes
        .iter()
        .map(|(route, entries)| {
            test_support::binding(&source_id(route), test_support::listing(entries.clone()))
        })
        .collect();
    let registrations: Vec<_> = routes
        .iter()
        .map(|(route, _)| test_support::registration(route, &source_id(route)))
        .collect();
    let routes_of_update: Vec<VenueRoute> = routes.iter().map(|(route, _)| route.clone()).collect();
    publish_test_routes(&catalog, sources, registrations, &routes_of_update);
    publish_test_terms(&catalog, FIXTURE_CODES);
    // A display code belongs to the generation that published a listing using the
    // asset, so the fixture loads every route's listings the way the connection
    // action does: the names a test asserts are then the codes of a generation that
    // published something. A test that is already inside a runtime has no thread to
    // block on, and a route whose entries are only meant to be registered may not be
    // publishable at all: both leave the terms this fixture staged, which is what
    // those tests read.
    let loaded: Vec<VenueRoute> = routes.iter().map(|(route, _)| route.clone()).collect();
    load_routes(Arc::clone(&catalog), routes, loaded);
    catalog
}

/// Publishes every route's listings into one fixture catalog.
///
/// The batch goes through the catalog's own publication path, so a fixture
/// snapshot is the same kind of value a fetch produces, and the terms of every
/// asset the fixture names travel with it — a canonical identity is a digest of its
/// terms, so a batch that carried only identities could not be displayed. Nothing
/// here awaits: a fixture is built from inside the test's own context, which has no
/// thread to block on.
fn load_routes(
    catalog: Arc<Catalog>,
    entries: &[(VenueRoute, Vec<CatalogEntry>)],
    routes: Vec<VenueRoute>,
) {
    for route in routes {
        let listings = entries
            .iter()
            .find(|(owner, _)| owner == &route)
            .map(|(_, entries)| entries.clone())
            .unwrap_or_default();
        test_support::publish_fixture_route(&catalog, &route, listings)
            .expect("the fixture route should publish its listings");
    }
}

/// Publishes one candidate set of test routes into a catalog.
pub(super) fn publish_test_routes(
    catalog: &Catalog,
    sources: Vec<CatalogSourceBinding>,
    registrations: Vec<CatalogRouteRegistration>,
    routes: &[VenueRoute],
) {
    test_support::commit_fixture_routes(
        catalog,
        CatalogRouteUpdate::new(sources, registrations),
        &test_support::requirements(routes),
    )
    .expect("the test routes should publish");
}

/// Publishes the terms of every asset a fixture catalog is read with.
fn publish_test_terms(catalog: &Catalog, codes: &[&str]) {
    let terms: Vec<AssetTerms> = codes
        .iter()
        .map(|code| AssetTerms::CryptoToken(test_support::token_terms(VENUE, code)))
        .collect();
    test_support::install_asset_terms(catalog, &terms);
}

/// The catalog source id one test route publishes under.
fn source_id(route: &VenueRoute) -> SmolStr {
    let scope = route.scope.as_str().replace('/', "-");
    format_smolstr!("{VENUE}-{scope}")
}

/// The display precision of one route's listings.
pub(super) fn precision_of(route: &VenueRoute, entries: Vec<CatalogEntry>) -> DisplayPrecision {
    precision_for(vec![(route.clone(), entries)])
}

/// The display precision of several routes' listings.
pub(super) fn precision_for(routes: Vec<(VenueRoute, Vec<CatalogEntry>)>) -> DisplayPrecision {
    let catalog = fixture(&routes);
    DisplayPrecision::from_catalogs(
        super::model::AssetLabels::new(catalog.cached_display()),
        catalogs_of(routes),
    )
    .expect("the test catalog should be valid")
}

/// Why one set of catalogs cannot be indexed.
///
/// The catalogs are handed to the index exactly as the caller built them, without
/// publishing them into a catalog first: the refusal under test is the index's own
/// answer to a catalog whose listing belongs to another route, and the catalog
/// refuses such a batch before it could ever be published. The refusal is returned
/// as it is, so the caller asserts on the error rather than on a literal beside it.
pub(super) fn precision_failure(routes: Vec<(VenueRoute, Vec<CatalogEntry>)>) -> WalletDataError {
    let placeholder = Arc::new(Catalog::new());
    DisplayPrecision::from_catalogs(
        super::model::AssetLabels::new(placeholder.cached_display()),
        catalogs_of(routes),
    )
    .expect_err("the test catalogs should be refused")
}

/// The catalogs of one set of test routes, as the display indexes them.
fn catalogs_of(
    routes: Vec<(VenueRoute, Vec<CatalogEntry>)>,
) -> Vec<(VenueRoute, Arc<[CatalogEntry]>)> {
    routes
        .into_iter()
        .map(|(route, entries)| (route, Arc::from(entries)))
        .collect()
}

/// Money without a market behind it.
pub(super) fn money() -> DisplayPrecision {
    DisplayPrecision::money_only(super::model::AssetLabels::new(
        Catalog::new().cached_display(),
    ))
    .expect("money precision should be valid")
}

pub(super) fn wallet_balance(asset: &str, total: &str) -> HyperliquidSpotBalance {
    HyperliquidSpotBalance {
        asset: token(asset),
        total: total.parse().expect("total should parse"),
        hold: SignedQuantity::ZERO,
    }
}

pub(super) fn spot_state(balances: Vec<HyperliquidSpotBalance>) -> HyperliquidSpotState {
    HyperliquidSpotState { balances }
}

pub(super) fn position(
    listing: &VenueListingKey,
    size: &str,
    entry_price: Option<&str>,
) -> HyperliquidPerpetualPosition {
    let signed: SignedQuantity = size.parse().expect("a size should be valid");
    HyperliquidPerpetualPosition {
        listing: listing.clone(),
        size: MarketQuantity::traded(
            Quantity::from_inner(signed.inner().unsigned_abs()),
            QuantityUnit::Contracts,
        )
        .expect("a position size is a traded quantity"),
        direction: if signed.is_negative() {
            PositionDirection::Short
        } else {
            PositionDirection::Long
        },
        entry_price: entry_price.map(|value| price_in("USDC", value)),
        mark_price: None,
        liquidation_price: None,
        position_value: None,
        unrealized_pnl: amount_of("USDC", "0"),
        margin: None,
        funding_since_open: amount_of("USDC", "0"),
    }
}

/// One signed amount of one test asset.
pub(super) fn amount_of(asset: &str, value: &str) -> AssetAmount {
    AssetAmount {
        asset: token(asset),
        value: value.parse().expect("an amount should be valid"),
    }
}

/// One price of one test asset, quoted in the venue's stablecoin.
pub(super) fn price_in(quote: &str, value: &str) -> MarketPrice {
    MarketPrice {
        value: value.parse().expect("a price should be valid"),
        quote_asset: token(quote),
        basis: PriceBasis::PerAssetUnit,
    }
}

pub(super) fn perpetual_state(
    positions: Vec<HyperliquidPerpetualPosition>,
) -> HyperliquidPerpetualState {
    HyperliquidPerpetualState { positions }
}

pub(super) fn spot_pair(base: &str, quote: &str) -> HyperliquidSpotPair {
    HyperliquidSpotPair {
        coin: format_smolstr!("{base}/{quote}"),
        base_token: token(base),
        quote_token: token(quote),
        asset_id: spot_book_index(base, quote),
    }
}

pub(super) fn context(
    listing: &VenueListingKey,
    mark: Option<&str>,
    mid: Option<&str>,
) -> HyperliquidAssetContext {
    HyperliquidAssetContext {
        listing: listing.clone(),
        mark_price: mark.map(|value| price_in("USDC", value)),
        mid_price: mid.map(|value| price_in("USDC", value)),
    }
}

pub(super) fn fill(
    address: &market_participants_model::identifier::WalletAddress,
    listing: &VenueListingKey,
    side: Side,
    quantity: &str,
    price: &str,
    timestamp: i64,
    tid: u64,
) -> HyperliquidUserFill {
    HyperliquidUserFill {
        address: address.clone(),
        listing: listing.clone(),
        side,
        quantity: MarketQuantity::traded(
            quantity.parse().expect("quantity should be valid"),
            QuantityUnit::Contracts,
        )
        .expect("a fill quantity is a traded quantity"),
        price: market_price(price),
        timestamp,
        start_position: SignedQuantity::ZERO,
        closed_pnl: amount_of("USDC", "0"),
        fee: amount_of("USDC", "0"),
        hash: None,
        tid,
        liquidation: false,
        direction: None,
    }
}

/// One market price in the venue's stablecoin.
pub(super) fn market_price(price: &str) -> MarketPrice {
    price_in("USDC", price)
}

pub(super) fn deposit(
    address: &market_participants_model::identifier::WalletAddress,
    amount: &str,
    timestamp: i64,
) -> HyperliquidLedgerEvent {
    HyperliquidLedgerEvent {
        address: address.clone(),
        timestamp,
        hash: SmolStr::new(format_smolstr!("hash-{timestamp}").as_str()),
        asset: Some(token("USDC")),
        kind: SmolStr::new_static("deposit"),
        amount: Some(amount_of("USDC", amount)),
        value: Some(amount_of("USDC", amount)),
        transfer_direction: Some(
            market_participants_connector::account::domain::HyperliquidTransferDirection::Incoming,
        ),
        related_wallet: None,
        liquidated_notional: None,
        liquidated_positions: Vec::new(),
    }
}

pub(super) fn open_order(
    listing: &VenueListingKey,
    order_id: &str,
    timestamp: i64,
) -> HyperliquidOpenOrder {
    HyperliquidOpenOrder {
        listing: listing.clone(),
        side: Side::Buy,
        quantity: MarketQuantity::traded(
            "2".parse().expect("quantity should be valid"),
            QuantityUnit::Contracts,
        )
        .expect("an order quantity is a traded quantity"),
        price: market_price("100"),
        status: market_participants_connector::account::shared::OrderStatus::Open,
        kind: market_participants_connector::account::shared::OrderKind::Limit {
            price: "100".parse().expect("price should be valid"),
            time_in_force:
                market_participants_connector::account::shared::TimeInForce::GoodTillCancelled,
        },
        reduce_only: false,
        timestamp,
        order_id: SmolStr::new(order_id),
    }
}

pub(super) fn historical_order(
    listing: &VenueListingKey,
    order_id: &str,
    timestamp: i64,
) -> HyperliquidHistoricalOrder {
    HyperliquidHistoricalOrder {
        listing: listing.clone(),
        side: Side::Sell,
        quantity: MarketQuantity::traded(
            "3".parse().expect("quantity should be valid"),
            QuantityUnit::Contracts,
        )
        .expect("an order quantity is a traded quantity"),
        price: market_price("110"),
        kind: market_participants_connector::account::shared::OrderKind::Limit {
            price: "110".parse().expect("price should be valid"),
            time_in_force:
                market_participants_connector::account::shared::TimeInForce::GoodTillCancelled,
        },
        reduce_only: true,
        status: SmolStr::new_static("filled"),
        timestamp,
        order_id: SmolStr::new(order_id),
    }
}

/// Which live subscription a test is pushing to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SubscriptionKind {
    Fills,
    Ledger,
    FastContexts,
}

/// One live subscription the venue would have opened.
pub(super) struct FakeSubscription {
    events: mpsc::Receiver<Result<HyperliquidWalletUpdate, AccountError>>,
    drops: Arc<Mutex<usize>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        let mut drops = self
            .drops
            .lock()
            .expect("the drop counter should not be poisoned");
        *drops += 1;
    }
}

impl WalletSubscription for FakeSubscription {
    async fn recv(&mut self) -> Option<Result<HyperliquidWalletUpdate, AccountError>> {
        self.events.recv().await
    }
}

/// Everything a fake source answers with, and everything it was asked.
struct FakeState {
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    /// The perpetual state one route answers with, when a test set one.
    perpetual_by_route: AHashMap<VenueRoute, HyperliquidPerpetualState>,
    pairs: Vec<HyperliquidSpotPair>,
    fills: Vec<HyperliquidUserFill>,
    ledger: Vec<HyperliquidLedgerEvent>,
    open_orders: Vec<HyperliquidOpenOrder>,
    historical_orders: Vec<HyperliquidHistoricalOrder>,
    fills_by_time: VecDeque<Result<Vec<HyperliquidUserFill>, AccountError>>,
    /// Every address a wallet read was made for, in the order the reads were made.
    read_addresses: Vec<WalletAddress>,
    /// The routes whose account state this venue answers with a failure.
    failed_states: AHashSet<VenueRoute>,
    /// The routes whose catalog this venue answers with a failure.
    failed_catalogs: AHashSet<VenueRoute>,
    counters: Counters,
    /// Every subscription this fake has opened, by the kind of update it carries.
    subscriptions: AHashMap<
        SubscriptionKind,
        Vec<mpsc::Sender<Result<HyperliquidWalletUpdate, AccountError>>>,
    >,
    subscription_drops: Arc<Mutex<usize>>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            spot: HyperliquidSpotState {
                balances: Vec::new(),
            },
            perpetual: HyperliquidPerpetualState {
                positions: Vec::new(),
            },
            perpetual_by_route: AHashMap::new(),
            pairs: Vec::new(),
            fills: Vec::new(),
            ledger: Vec::new(),
            open_orders: Vec::new(),
            historical_orders: Vec::new(),
            fills_by_time: VecDeque::new(),
            read_addresses: Vec::new(),
            failed_states: AHashSet::new(),
            failed_catalogs: AHashSet::new(),
            counters: Counters::default(),
            subscriptions: AHashMap::new(),
            subscription_drops: Arc::new(Mutex::new(0)),
        }
    }
}

/// What one fake source was asked for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Counters {
    pub spot_states: usize,
    pub perpetual_states: usize,
    pub pairs: usize,
    pub fills: usize,
    pub fills_by_time: usize,
    pub ledger: usize,
    pub open_orders: usize,
    pub historical_orders: usize,
    /// How many route catalogs were read. One route publishes one catalog, so a reading that
    /// covers N routes reads N of them.
    pub catalogs: usize,
    pub fills_subscriptions: usize,
    pub ledger_subscriptions: usize,
    pub fast_context_subscriptions: usize,
}

impl Counters {
    /// Every wallet subscription this fake opened.
    pub(super) fn subscriptions(&self) -> usize {
        self.fills_subscriptions + self.ledger_subscriptions
    }

    /// Every stream this fake opened, the route-level one included.
    pub(super) fn streams(&self) -> usize {
        self.subscriptions() + self.fast_context_subscriptions
    }
}

/// The venue, as a test drives it.
#[derive(Clone)]
pub(super) struct TestSource {
    catalog: Arc<Catalog>,
    /// The routes this fake venue publishes, which is what one reading of an address covers.
    routes: Vec<VenueRoute>,
    state: Arc<Mutex<FakeState>>,
}

impl TestSource {
    /// A venue whose routes publish the given listings.
    pub(super) fn new(routes: &[(VenueRoute, Vec<CatalogEntry>)]) -> Self {
        let catalog = fixture(routes);
        Self {
            catalog,
            routes: routes.iter().map(|(route, _)| route.clone()).collect(),
            state: Arc::new(Mutex::new(FakeState {
                subscription_drops: Arc::new(Mutex::new(0)),
                ..FakeState::default()
            })),
        }
    }

    /// Republishes these routes' listings, which is what a venue that restated them answers with.
    ///
    /// A reading is resolved from the catalog its stream fetches, so a test about what a stream
    /// opened *after* a market was loaded again resolves at republishes here: the next stream reads
    /// the listings the venue is publishing now.
    pub(super) fn restate(&self, routes: &[(VenueRoute, Vec<CatalogEntry>)]) {
        for (route, entries) in routes {
            test_support::publish_fixture_route(&self.catalog, route, entries.clone())
                .expect("the test route should publish its listings");
        }
    }

    pub(super) fn spot(&self, state: HyperliquidSpotState) -> &Self {
        self.with(|state_mut| state_mut.spot = state);
        self
    }

    pub(super) fn perpetual(&self, state: HyperliquidPerpetualState) -> &Self {
        self.with(|state_mut| state_mut.perpetual = state);
        self
    }

    /// The perpetual state one route answers with.
    ///
    /// One partition's positions are not another's, so a test that reads a merge across partitions
    /// says what each of them holds; a route with nothing set here answers with the plain
    /// `perpetual` state, which is what every single-route test uses.
    pub(super) fn perpetual_on(
        &self,
        route: &VenueRoute,
        state: HyperliquidPerpetualState,
    ) -> &Self {
        self.with(|state_mut| {
            state_mut.perpetual_by_route.insert(route.clone(), state);
        });
        self
    }

    pub(super) fn pairs(&self, pairs: Vec<HyperliquidSpotPair>) -> &Self {
        self.with(|state_mut| state_mut.pairs = pairs);
        self
    }

    pub(super) fn fills(&self, fills: Vec<HyperliquidUserFill>) -> &Self {
        self.with(|state_mut| state_mut.fills = fills);
        self
    }

    pub(super) fn open_orders(&self, orders: Vec<HyperliquidOpenOrder>) -> &Self {
        self.with(|state_mut| state_mut.open_orders = orders);
        self
    }

    /// Queues the answer of the next timeline read.
    pub(super) fn push_timeline(
        &self,
        answer: Result<Vec<HyperliquidUserFill>, AccountError>,
    ) -> &Self {
        self.with(|state| state.fills_by_time.push_back(answer));
        self
    }

    pub(super) fn counters(&self) -> Counters {
        self.with(|state| state.counters)
    }

    /// The address every wallet read was made for, in the order the reads were made.
    pub(super) fn read_addresses(&self) -> Vec<WalletAddress> {
        self.with(|state| state.read_addresses.clone())
    }

    /// Reads one route's catalog as the catalog failure the route answers with.
    pub(super) fn fail_catalog_of(&self, route: &VenueRoute) -> &Self {
        self.with(|state| {
            state.failed_catalogs.insert(route.clone());
        });
        self
    }

    /// Records one wallet read.
    fn read_address(&self, address: &WalletAddress) {
        self.with(|state| state.read_addresses.push(address.clone()));
    }

    /// Reads one route's account state as the provider failure the route answers with.
    ///
    /// It is what pins the attribution of a reading that covers several partitions: the failure
    /// belongs to the partition that could not answer, not to the address as a whole.
    pub(super) fn fail_state_of(&self, route: &VenueRoute) -> &Self {
        self.with(|state| {
            state.failed_states.insert(route.clone());
        });
        self
    }

    /// Pushes one live update to a subscription that is open.
    pub(super) async fn publish(&self, kind: SubscriptionKind, update: HyperliquidWalletUpdate) {
        for sender in self.senders(kind) {
            let _ = sender.send(Ok(update.clone())).await;
        }
    }

    /// Pushes one live update to a subscription that is open, as a value a runtime can own.
    pub(super) async fn deliver(self, kind: SubscriptionKind, update: HyperliquidWalletUpdate) {
        self.publish(kind, update).await;
    }

    /// Reports a subscription failure, as a refused subscription would.
    pub(super) async fn fail_subscription(&self, kind: SubscriptionKind) {
        for sender in self.senders(kind) {
            let _ = sender
                .send(Err(AccountError::Protocol { operation: "test" }))
                .await;
        }
    }

    /// How many subscriptions have been dropped.
    pub(super) fn subscription_drops(&self) -> usize {
        *self
            .state
            .lock()
            .expect("the fake state should not be poisoned")
            .subscription_drops
            .lock()
            .expect("the drop counter should not be poisoned")
    }

    fn senders(
        &self,
        kind: SubscriptionKind,
    ) -> Vec<mpsc::Sender<Result<HyperliquidWalletUpdate, AccountError>>> {
        self.with(|state| state.subscriptions.get(&kind).cloned().unwrap_or_default())
    }

    fn subscription(&self, kind: SubscriptionKind) -> FakeSubscription {
        let (sender, events) = mpsc::channel(16);
        self.with(|state| {
            match kind {
                SubscriptionKind::Fills => state.counters.fills_subscriptions += 1,
                SubscriptionKind::Ledger => state.counters.ledger_subscriptions += 1,
                SubscriptionKind::FastContexts => state.counters.fast_context_subscriptions += 1,
            }
            state.subscriptions.entry(kind).or_default().push(sender);
            FakeSubscription {
                events,
                drops: Arc::clone(&state.subscription_drops),
            }
        })
    }

    fn with<T>(&self, update: impl FnOnce(&mut FakeState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .expect("the fake state should not be poisoned");
        update(&mut state)
    }
}

impl WalletSource for TestSource {
    type Subscription = FakeSubscription;

    fn wallet_routes(&self) -> Vec<VenueRoute> {
        self.routes.clone()
    }

    async fn fetch_spot_state(
        &self,
        _route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<HyperliquidSpotState, AccountError> {
        self.read_address(address);
        Ok(self.with(|state| {
            state.counters.spot_states += 1;
            state.spot.clone()
        }))
    }

    async fn fetch_perpetual_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<HyperliquidPerpetualState, AccountError> {
        self.read_address(address);
        if self.with(|state| state.failed_states.contains(route)) {
            return Err(AccountError::Protocol {
                operation: "read the fake wallet state",
            });
        }
        Ok(self.with(|state| {
            state.counters.perpetual_states += 1;
            state
                .perpetual_by_route
                .get(route)
                .cloned()
                .unwrap_or_else(|| state.perpetual.clone())
        }))
    }

    async fn fetch_spot_pairs(
        &self,
        _route: &VenueRoute,
    ) -> Result<Vec<HyperliquidSpotPair>, AccountError> {
        Ok(self.with(|state| {
            state.counters.pairs += 1;
            state.pairs.clone()
        }))
    }

    async fn fetch_fills(
        &self,
        address: &WalletAddress,
        _aggregate_by_time: bool,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        self.read_address(address);
        Ok(self.with(|state| {
            state.counters.fills += 1;
            state.fills.clone()
        }))
    }

    async fn fetch_fills_by_time(
        &self,
        address: &WalletAddress,
        _start_timestamp: i64,
        _end_timestamp: i64,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        self.read_address(address);
        self.with(|state| {
            state.counters.fills_by_time += 1;
            state
                .fills_by_time
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        })
    }

    async fn fetch_ledger(
        &self,
        address: &WalletAddress,
        _start_time: i64,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidLedgerEvent>, AccountError> {
        self.read_address(address);
        Ok(self.with(|state| {
            state.counters.ledger += 1;
            state.ledger.clone()
        }))
    }

    async fn fetch_open_orders(
        &self,
        _route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<Vec<HyperliquidOpenOrder>, AccountError> {
        self.read_address(address);
        Ok(self.with(|state| {
            state.counters.open_orders += 1;
            state.open_orders.clone()
        }))
    }

    async fn fetch_historical_orders(
        &self,
        address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidHistoricalOrder>, AccountError> {
        self.read_address(address);
        Ok(self.with(|state| {
            state.counters.historical_orders += 1;
            state.historical_orders.clone()
        }))
    }

    async fn fetch_catalog(&self, route: &VenueRoute) -> Result<Arc<[CatalogEntry]>, AccountError> {
        self.with(|state| state.counters.catalogs += 1);
        if self.with(|state| state.failed_catalogs.contains(route)) {
            return Err(AccountError::Protocol {
                operation: "read the fake wallet catalog",
            });
        }
        self.catalog
            .listings(route)
            .map_err(|_| AccountError::Protocol {
                operation: "read the fake wallet catalog",
            })
    }

    fn display_generation(&self) -> DisplaySnapshot {
        self.catalog.cached_display()
    }

    async fn subscribe_fills(
        &self,
        address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        self.read_address(address);
        Ok(self.subscription(SubscriptionKind::Fills))
    }

    async fn subscribe_ledger(
        &self,
        address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        self.read_address(address);
        Ok(self.subscription(SubscriptionKind::Ledger))
    }

    async fn subscribe_fast_contexts(
        &self,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        Ok(self.subscription(SubscriptionKind::FastContexts))
    }
}

/// A venue that never answers a wallet read, for a pane that is driven by hand.
///
/// Opening a subscription is not a wallet read: a venue that answers nothing yet still has
/// sockets, so a pane under this source has streams that report they are connecting and no
/// reading behind them.
#[derive(Clone, Copy)]
pub(super) struct PendingSource;

pub(super) struct PendingSubscription;

impl WalletSource for PendingSource {
    type Subscription = PendingSubscription;

    /// The fixture route a reading is addressed to, so a stream parks on its reads rather than
    /// failing for having no route to read.
    fn wallet_routes(&self) -> Vec<VenueRoute> {
        vec![perpetual_route()]
    }

    async fn fetch_spot_state(
        &self,
        _route: &VenueRoute,
        _address: &WalletAddress,
    ) -> Result<HyperliquidSpotState, AccountError> {
        std::future::pending().await
    }

    async fn fetch_perpetual_state(
        &self,
        _route: &VenueRoute,
        _address: &WalletAddress,
    ) -> Result<HyperliquidPerpetualState, AccountError> {
        std::future::pending().await
    }

    async fn fetch_spot_pairs(
        &self,
        _route: &VenueRoute,
    ) -> Result<Vec<HyperliquidSpotPair>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_fills(
        &self,
        _address: &WalletAddress,
        _aggregate_by_time: bool,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_fills_by_time(
        &self,
        _address: &WalletAddress,
        _start_timestamp: i64,
        _end_timestamp: i64,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_ledger(
        &self,
        _address: &WalletAddress,
        _start_time: i64,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidLedgerEvent>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_open_orders(
        &self,
        _route: &VenueRoute,
        _address: &WalletAddress,
    ) -> Result<Vec<HyperliquidOpenOrder>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_historical_orders(
        &self,
        _address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidHistoricalOrder>, AccountError> {
        std::future::pending().await
    }

    async fn fetch_catalog(
        &self,
        _route: &VenueRoute,
    ) -> Result<Arc<[CatalogEntry]>, AccountError> {
        std::future::pending().await
    }

    fn display_generation(&self) -> DisplaySnapshot {
        Catalog::new().cached_display()
    }

    async fn subscribe_fills(
        &self,
        _address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        Ok(PendingSubscription)
    }

    async fn subscribe_ledger(
        &self,
        _address: &WalletAddress,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        Ok(PendingSubscription)
    }

    async fn subscribe_fast_contexts(
        &self,
        _routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        Ok(PendingSubscription)
    }
}

impl WalletSubscription for PendingSubscription {
    async fn recv(&mut self) -> Option<Result<HyperliquidWalletUpdate, AccountError>> {
        std::future::pending().await
    }
}

/// Installs a market participants connector whose routes publish the test listings.
///
/// The pane asks the registry which routes a watch may name and which one an unconfigured pane
/// starts on, so a test loads the venue whose routes the fake venue answers for and then
/// publishes those routes' catalogs into the same directory the pane reads them from.
pub(super) fn install_connector(cx: &mut App, routes: &[(VenueRoute, Vec<CatalogEntry>)]) {
    install_connector_with(cx, routes, true);
}

/// The same, with the venue loaded or left out of the loaded venues entirely.
///
/// A venue that is not loaded is the state a pane nobody has loaded yet is
/// in, and the catalogs the fixture published for its routes are then beside the
/// point: the pane waits for the routes, not for the listings.
pub(super) fn install_connector_with(
    cx: &mut App,
    routes: &[(VenueRoute, Vec<CatalogEntry>)],
    loaded: bool,
) {
    // The directory the pane reads is the one the root owns, so it carries the
    // terms of the assets this venue's fixtures are denominated in: a canonical
    // identity is a digest of its terms, and a catalog that named an asset the
    // directory cannot describe would be refused rather than published.
    let catalog = Arc::new(Catalog::new());
    publish_test_terms(&catalog, FIXTURE_CODES);
    let connector =
        market_participants_connector::test_support::install_root(cx, Arc::clone(&catalog));
    // The pane reads the routes of a venue the user loaded, so the test loads the one the
    // fake venue answers for: the scopes of the test set, not of the whole venue list.
    if loaded {
        connector
            .load_fixture(
                &market_participants_connector::test_support::definitions(&[venue()]),
                &[],
                &[],
            )
            .expect("the venue's routes should load");
    }
    test_support::commit_fixture_routes(
        &catalog,
        CatalogRouteUpdate::new(test_route_sources(routes), test_route_registrations(routes)),
        &test_support::requirements(&test_route_list(routes)),
    )
    .expect("the test routes should publish");
    // Loading the venue is one commit of every catalog route: the pane reads the
    // catalog of a market the user loaded, so the test loads it the way the load
    // action does.
    test_support::publish_fixture_catalogs(cx, routes);
}

/// The catalog sources of one candidate test route set.
pub(super) fn test_route_sources(
    routes: &[(VenueRoute, Vec<CatalogEntry>)],
) -> Vec<CatalogSourceBinding> {
    routes
        .iter()
        .map(|(route, entries)| {
            test_support::binding(&source_id(route), test_support::listing(entries.clone()))
        })
        .collect()
}

/// The route registrations of one candidate test route set.
pub(super) fn test_route_registrations(
    routes: &[(VenueRoute, Vec<CatalogEntry>)],
) -> Vec<CatalogRouteRegistration> {
    routes
        .iter()
        .map(|(route, _)| test_support::registration(route, &source_id(route)))
        .collect()
}

/// The routes of one candidate test route set.
pub(super) fn test_route_list(routes: &[(VenueRoute, Vec<CatalogEntry>)]) -> Vec<VenueRoute> {
    routes.iter().map(|(route, _)| route.clone()).collect()
}

/// Installs what a windowed test draws: the metadata a list or a canvas asks for.
pub(super) fn install_metadata(cx: &mut TestAppContext) {
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        widget_tokio_runtime::global::install(cx).expect("the test Tokio runtime should build");
    });
}

/// The routes one test session publishes, with the listings every test reading names.
///
/// One route publishes a perpetual and the other a spot pair, so a test that reports a reading
/// about a perpetual position and one about a spot balance uses one venue for both.
pub(super) fn test_routes() -> Vec<(VenueRoute, Vec<CatalogEntry>)> {
    vec![
        (
            perpetual_route(),
            vec![perp_entry(&perpetual_route(), "ETH", "0.1", "0.001")],
        ),
        (
            spot_route(),
            vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
        ),
    ]
}

/// The routes of a venue that also deployed a DEX: the two core partitions and one deployed one.
///
/// `test_routes` stays the two-route set most tests are about, and this is the shape a real
/// Hyperliquid account has — which is the only way to read a fan-out, a three-catalog union or a
/// partition-level failure the way the venue actually produces them.
pub(super) fn deployed_routes() -> Vec<(VenueRoute, Vec<CatalogEntry>)> {
    let deployed = deployed_route();
    vec![
        (
            spot_route(),
            vec![spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001")],
        ),
        (
            perpetual_route(),
            vec![perp_entry(&perpetual_route(), "ETH", "0.1", "0.001")],
        ),
        (
            deployed.clone(),
            vec![perp_entry(&deployed, "SOL", "0.01", "0.001")],
        ),
    ]
}

/// The deployed DEX the tests read beside the core partitions.
pub(super) fn deployed_route() -> VenueRoute {
    route("perp-dex/xyz")
}

/// The config that follows the given addresses.
pub(super) fn config(addresses: Vec<WalletAddress>) -> HyperliquidWalletConfig {
    HyperliquidWalletConfig::monitoring(addresses)
}

/// One overview reading a test can hand to a pane.
pub(super) fn overview_snapshot(
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    contexts: Vec<HyperliquidAssetContext>,
    precision: DisplayPrecision,
) -> Arc<super::model::WalletOverviewSnapshot> {
    use super::model::{AccountSummary, WalletOverviewSnapshot};

    Arc::new(WalletOverviewSnapshot {
        summary: AccountSummary::from_states(
            &spot,
            &perpetual,
            &[spot_pair("HYPE", "USDC")],
            &contexts,
            &precision,
        ),
        precision,
    })
}

/// A timeline reading with no fills.
pub(super) fn empty_timeline() -> Arc<TimelineSnapshot> {
    Arc::new(TimelineSnapshot {
        window_start: 0,
        window_end: super::timeline::WINDOW_MILLIS,
        fills: Arc::from([]),
    })
}

/// A reading of the open-orders tab.
pub(super) fn open_orders_snapshot(
    orders: Vec<HyperliquidOpenOrder>,
    precision: DisplayPrecision,
) -> Arc<DetailSnapshot> {
    Arc::new(
        DetailSnapshot::new(DetailData::OpenOrders(orders), precision)
            .expect("a published listing should validate"),
    )
}

/// A reading of the spot tab.
pub(super) fn spot_snapshot(
    balances: Vec<HyperliquidSpotBalance>,
    precision: DisplayPrecision,
) -> Arc<DetailSnapshot> {
    Arc::new(
        DetailSnapshot::new(
            DetailData::Spot(
                super::model::SpotSnapshot::new(
                    spot_state(balances),
                    vec![spot_pair("HYPE", "USDC")],
                    Vec::new(),
                    &precision,
                )
                .expect("the spot reading should value"),
            ),
            precision,
        )
        .expect("a published listing should validate"),
    )
}

/// The messages one stream publishes, as a pane receives them.
pub(super) fn message(message: WalletStreamMessage) -> Arc<WalletStreamMessage> {
    Arc::new(message)
}

/// The status one stream reports.
pub(super) fn status(status: WalletStatus) -> Arc<WalletStreamMessage> {
    Arc::new(WalletStreamMessage::Status(status))
}

/// A key of the overview of one wallet.
pub(super) fn overview_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Overview {
        address: address.clone(),
    }
}

/// A key of the timeline of one wallet.
pub(super) fn timeline_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Timeline {
        address: address.clone(),
    }
}

/// A key of one tab of one wallet.
pub(super) fn detail_key(address: &WalletAddress, tab: super::model::DetailTab) -> WalletStreamKey {
    WalletStreamKey::Detail {
        address: address.clone(),
        tab,
    }
}

/// The handle of the runtime this test runs on.
///
/// A test drives its subject on its own runtime rather than on a shared one. The
/// clock these tests read is the runtime's: `#[tokio::test(start_paused = true)]`
/// pauses it and `tokio::time::advance` moves it, and both only affect the runtime
/// whose clock was paused. Work started on another runtime keeps waiting on real
/// time while the test advances a clock it never reads.
#[allow(
    clippy::disallowed_methods,
    reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
)]
fn current_test_handle() -> &'static tokio::runtime::Handle {
    Box::leak(Box::new(tokio::runtime::Handle::current()))
}

/// Runs the widget service until it stops, for a service a test drives end to end.
pub(super) fn runtime<S>(source: S) -> WidgetRuntime<WalletProtocol>
where
    S: WalletSource,
{
    WidgetRuntime::spawn(
        current_test_handle(),
        super::service::WalletService::new(source),
    )
}

/// Waits for one reliable message, and returns it.
///
/// The wait is counted in yields rather than in wall time: every test here drives a paused
/// clock, so a stream that has something to say says it on the next turn of the runtime rather
/// than a second later.
pub(super) async fn next_reliable(
    stream: &mut ReliableStream<WalletProtocol>,
) -> (WalletStreamKey, Arc<WalletStreamMessage>) {
    for _ in 0..512 {
        tokio::select! {
            biased;
            envelope = stream.recv() => {
                let envelope = envelope.expect("the reliable stream should remain open");
                return (envelope.key().clone(), envelope.message().clone());
            }
            () = tokio::task::yield_now() => {}
        }
    }
    panic!("a reliable message should arrive");
}

/// Waits until the reliable stream reports a status for one key.
pub(super) async fn wait_for_status(
    stream: &mut ReliableStream<WalletProtocol>,
    key: &WalletStreamKey,
    expected: &WalletStatus,
) {
    for _ in 0..64 {
        let (received, message) = next_reliable(stream).await;
        if &received == key
            && let WalletStreamMessage::Status(status) = message.as_ref()
            && status == expected
        {
            return;
        }
    }
    panic!("the expected status should arrive");
}

/// Waits until a predicate holds, driving the widget runtime in between.
pub(super) async fn until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..128 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "the expected condition should be reached");
}

/// Waits for the next reading published under one key.
pub(super) async fn latest_reading(
    latest: &mut LatestStream<WalletProtocol>,
    key: &WalletStreamKey,
) -> Arc<WalletStreamMessage> {
    for _ in 0..512 {
        while let Some(envelope) = latest.next_unseen() {
            if envelope.key() == key {
                return envelope.message().clone();
            }
        }
        tokio::task::yield_now().await;
    }
    panic!("the expected reading should be published");
}

/// Pushes one live update to a subscription, from a test that owns a GPUI frame.
///
/// The send goes through the widget's own runtime rather than being awaited inline: a GPUI test
/// thread is not a Tokio worker, so pumping the runtime is what lets the frame that receives it run.
/// This only *schedules* the delivery — it does not wait for anything, because there is nothing
/// here to wait for: the send completing is not the reading arriving, and every caller states the
/// effect it wants with its own `settle_until` right after (for example "the totals to follow the
/// venue"). A predicate that is true on the first round would be an empty wait wearing the name of
/// a condition.
pub(super) fn publish(
    cx: &mut TestAppContext,
    source: &TestSource,
    kind: SubscriptionKind,
    update: HyperliquidWalletUpdate,
) {
    let handle = cx.update(|cx: &mut App| widget_tokio_runtime::global::handle(cx));
    handle.run(source.clone().deliver(kind, update));
}

/// Drives a pane until a predicate about it holds.
///
/// The pane's work lives on two runtimes — its own Tokio runtime and GPUI's — so a test that
/// wants a reading to land gives both a turn: the sleep lets the widget runtime make progress,
/// and `run_until_parked` is what runs the frame that received it.
pub(super) fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut rounds = 0_usize;
    loop {
        cx.run_until_parked();
        if cx.update(|cx| ready(cx)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what} should happen after {rounds} rounds"
        );
        rounds += 1;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
