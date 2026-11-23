use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ahash::{AHashMap, AHashSet};
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

pub(super) const VENUE: &str = "hyperliquid";
pub(super) const PERPETUAL_SCOPE: &str = "core-perp";
pub(super) const SPOT_SCOPE: &str = "core-spot";
const FIXTURE_CODES: &[&str] = &["ETH", "BTC", "HYPE", "SOL", "USDC", "USDT", "USDH"];

pub(super) fn route(scope: &str) -> VenueRoute {
    VenueRoute::new(
        venue(),
        VenueScopeId::new(scope).expect("the test scope should be canonical"),
    )
}

pub(super) fn venue() -> VenueId {
    VenueId::Hyperliquid
}

pub(super) fn perpetual_route() -> VenueRoute {
    route(PERPETUAL_SCOPE)
}

pub(super) fn spot_route() -> VenueRoute {
    route(SPOT_SCOPE)
}

pub(super) fn address(suffix: &str) -> WalletAddress {
    let repeated = suffix.repeat(40_usize.div_ceil(suffix.len()));
    format!("0x{}", &repeated[..40])
        .parse()
        .expect("a test address should be valid")
}

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

fn token_index(code: &str) -> HyperliquidAssetId {
    let index = FIXTURE_CODES
        .iter()
        .position(|candidate| *candidate == code)
        .unwrap_or_else(|| panic!("the test token {code} is not registered"));
    HyperliquidAssetId(100 + index as u32)
}

const FIXTURE_SPOT_BOOKS: &[(&str, &str)] = &[
    ("HYPE", "USDC"),
    ("HYPE", "USDT"),
    ("HYPE", "USDH"),
    ("SOL", "USDC"),
    ("USDC", "USDT"),
    ("BTC", "USDT"),
];

fn spot_book_index(base: &str, quote: &str) -> HyperliquidAssetId {
    let index = FIXTURE_SPOT_BOOKS
        .iter()
        .position(|(candidate_base, candidate_quote)| {
            *candidate_base == base && *candidate_quote == quote
        })
        .unwrap_or_else(|| panic!("the test spot book {base}/{quote} is not registered"));
    HyperliquidAssetId(200 + index as u32)
}

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

pub(super) fn spot_instrument(base: &str, quote: &str) -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(token(base), token(quote)).expect("the test spot terms should be valid"),
    ))
    .expect("the test spot instrument should be valid")
}

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

fn code_of(asset: &AssetId) -> Option<&'static str> {
    let AssetId::CryptoToken(id) = asset else {
        return None;
    };
    FIXTURE_CODES
        .iter()
        .copied()
        .find(|code| &CryptoTokenId::of_terms(&test_support::token_terms(VENUE, code)) == id)
}

pub(super) fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
    hyperliquid_key(route, HyperliquidAssetProduct::Perpetual, symbol)
}

pub(super) fn published_perpetual(asset: &str) -> VenueListingKey {
    listing_key(&perpetual_route(), asset)
}

pub(super) fn published_spot(base: &str, quote: &str) -> VenueListingKey {
    let symbol = format_smolstr!("{base}/{quote}");
    hyperliquid_key(
        &spot_route(),
        HyperliquidAssetProduct::Spot,
        symbol.as_str(),
    )
}

pub(super) fn pair_listing(base: &str, quote: &str) -> VenueListingKey {
    published_spot(base, quote)
}

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
    let loaded: Vec<VenueRoute> = routes.iter().map(|(route, _)| route.clone()).collect();
    load_routes(Arc::clone(&catalog), routes, loaded);
    catalog
}

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

fn publish_test_terms(catalog: &Catalog, codes: &[&str]) {
    let terms: Vec<AssetTerms> = codes
        .iter()
        .map(|code| AssetTerms::CryptoToken(test_support::token_terms(VENUE, code)))
        .collect();
    test_support::install_asset_terms(catalog, &terms);
}

fn source_id(route: &VenueRoute) -> SmolStr {
    let scope = route.scope.as_str().replace('/', "-");
    format_smolstr!("{VENUE}-{scope}")
}

pub(super) fn precision_of(route: &VenueRoute, entries: Vec<CatalogEntry>) -> DisplayPrecision {
    precision_for(vec![(route.clone(), entries)])
}

pub(super) fn precision_for(routes: Vec<(VenueRoute, Vec<CatalogEntry>)>) -> DisplayPrecision {
    let catalog = fixture(&routes);
    DisplayPrecision::from_catalogs(
        super::model::AssetLabels::new(catalog.cached_display()),
        catalogs_of(routes),
    )
    .expect("the test catalog should be valid")
}

pub(super) fn precision_failure(routes: Vec<(VenueRoute, Vec<CatalogEntry>)>) -> WalletDataError {
    let placeholder = Arc::new(Catalog::new());
    DisplayPrecision::from_catalogs(
        super::model::AssetLabels::new(placeholder.cached_display()),
        catalogs_of(routes),
    )
    .expect_err("the test catalogs should be refused")
}

fn catalogs_of(
    routes: Vec<(VenueRoute, Vec<CatalogEntry>)>,
) -> Vec<(VenueRoute, Arc<[CatalogEntry]>)> {
    routes
        .into_iter()
        .map(|(route, entries)| (route, Arc::from(entries)))
        .collect()
}

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

pub(super) fn amount_of(asset: &str, value: &str) -> AssetAmount {
    AssetAmount {
        asset: token(asset),
        value: value.parse().expect("an amount should be valid"),
    }
}

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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SubscriptionKind {
    Fills,
    Ledger,
    FastContexts,
}

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

struct FakeState {
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    pairs: Vec<HyperliquidSpotPair>,
    fills: Vec<HyperliquidUserFill>,
    ledger: Vec<HyperliquidLedgerEvent>,
    open_orders: Vec<HyperliquidOpenOrder>,
    historical_orders: Vec<HyperliquidHistoricalOrder>,
    fills_by_time: VecDeque<Result<Vec<HyperliquidUserFill>, AccountError>>,
    read_addresses: Vec<WalletAddress>,
    failed_states: AHashSet<VenueRoute>,
    failed_catalogs: AHashSet<VenueRoute>,
    counters: Counters,
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
    pub catalogs: usize,
    pub fills_subscriptions: usize,
    pub ledger_subscriptions: usize,
    pub fast_context_subscriptions: usize,
}

impl Counters {
    pub(super) fn subscriptions(&self) -> usize {
        self.fills_subscriptions + self.ledger_subscriptions
    }

    pub(super) fn streams(&self) -> usize {
        self.subscriptions() + self.fast_context_subscriptions
    }
}

#[derive(Clone)]
pub(super) struct TestSource {
    catalog: Arc<Catalog>,
    routes: Vec<VenueRoute>,
    state: Arc<Mutex<FakeState>>,
}

impl TestSource {
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

    pub(super) fn spot(&self, state: HyperliquidSpotState) -> &Self {
        self.with(|state_mut| state_mut.spot = state);
        self
    }

    pub(super) fn perpetual(&self, state: HyperliquidPerpetualState) -> &Self {
        self.with(|state_mut| state_mut.perpetual = state);
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

    pub(super) fn read_addresses(&self) -> Vec<WalletAddress> {
        self.with(|state| state.read_addresses.clone())
    }

    pub(super) fn fail_catalog_of(&self, route: &VenueRoute) -> &Self {
        self.with(|state| {
            state.failed_catalogs.insert(route.clone());
        });
        self
    }

    fn read_address(&self, address: &WalletAddress) {
        self.with(|state| state.read_addresses.push(address.clone()));
    }

    pub(super) fn fail_state_of(&self, route: &VenueRoute) -> &Self {
        self.with(|state| {
            state.failed_states.insert(route.clone());
        });
        self
    }

    pub(super) async fn publish(&self, kind: SubscriptionKind, update: HyperliquidWalletUpdate) {
        for sender in self.senders(kind) {
            let _ = sender.send(Ok(update.clone())).await;
        }
    }

    pub(super) async fn fail_subscription(&self, kind: SubscriptionKind) {
        for sender in self.senders(kind) {
            let _ = sender
                .send(Err(AccountError::Protocol { operation: "test" }))
                .await;
        }
    }

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
            state.perpetual.clone()
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

pub(super) fn config(addresses: Vec<WalletAddress>) -> HyperliquidWalletConfig {
    HyperliquidWalletConfig::monitoring(addresses)
}

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

pub(super) fn empty_timeline() -> Arc<TimelineSnapshot> {
    Arc::new(TimelineSnapshot {
        window_start: 0,
        window_end: super::timeline::WINDOW_MILLIS,
        fills: Arc::from([]),
    })
}

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

pub(super) fn message(message: WalletStreamMessage) -> Arc<WalletStreamMessage> {
    Arc::new(message)
}

pub(super) fn status(status: WalletStatus) -> Arc<WalletStreamMessage> {
    Arc::new(WalletStreamMessage::Status(status))
}

pub(super) fn overview_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Overview {
        address: address.clone(),
    }
}

pub(super) fn timeline_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Timeline {
        address: address.clone(),
    }
}

pub(super) fn detail_key(address: &WalletAddress, tab: super::model::DetailTab) -> WalletStreamKey {
    WalletStreamKey::Detail {
        address: address.clone(),
        tab,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
)]
fn current_test_handle() -> &'static tokio::runtime::Handle {
    Box::leak(Box::new(tokio::runtime::Handle::current()))
}

pub(super) fn runtime<S>(source: S) -> WidgetRuntime<WalletProtocol>
where
    S: WalletSource,
{
    WidgetRuntime::spawn(
        current_test_handle(),
        super::service::WalletService::new(source),
    )
}

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

pub(super) async fn until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..128 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "the expected condition should be reached");
}

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
