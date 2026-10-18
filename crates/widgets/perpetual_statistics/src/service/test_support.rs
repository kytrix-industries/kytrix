//! The fake source the service and the pane both answer through.
//!
//! It lives in a module of its own because two test files need it: the loop's
//! tests drive it to produce windows, and the pane's tests drive it to answer a
//! load. Everything in here is test scaffolding and is compiled only for tests.

use std::{
    collections::VecDeque,
    future::pending,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use market_participants_connector::{catalog::test_support, market::error::MarketError};
use market_participants_model::{
    asset::{AssetId, DerivativeUnderlying, UnderlyingRef},
    decimal::{Quantity, Ratio, Tick},
    identifier::{
        IDENTITY_VERSION, NativeSymbol, ReferenceSourceId, ReferenceTermsVersion, VenueId,
        VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, PerpetualTerms},
    listing::{
        identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        route::{VenueListingKey, VenueRoute},
        rules::{
            FundingInterval, FundingSchedule, LeverageTier, ListingPrecision, MarginMode,
            MarginProfile, PerpetualMarginTerms, PerpetualRisk, PerpetualTradingRules,
            ProductTradingRules, TradingRules,
        },
        venue::{BinanceProduct, PriceBasis},
    },
    market::{
        book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
        units::{MarketQuantity, QuantityUnit},
    },
    reference::{
        PriceReferenceId, PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
        ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    settlement::{ContractSize, ContractUnit, DerivativePnlModel},
};

use super::stream::{StatisticsQuery, StatisticsSource};
use crate::model::MetricKind;

/// The fixture perpetual route the registry declares these statistics for.
///
/// It is the built-in Binance perpetual scope rather than the generic fixture one:
/// what a pane reads is the route registry's own answer, so a route the registry
/// does not declare is not a scope a test can drive.
pub(crate) fn perpetual_route() -> VenueRoute {
    VenueRoute::new(
        VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical"),
        VenueScopeId::new(market_participants_connector::routing::PERPETUAL_SCOPE)
            .expect("the perpetual scope is canonical"),
    )
}

/// The listing key of one fixture perpetual listing.
pub(crate) fn perpetual_listing(symbol: &str) -> VenueListingKey {
    test_support::continuity_key(&perpetual_route(), symbol, BinanceProduct::Perpetual)
}

/// One validated perpetual listing of two fixture tokens.
pub(crate) fn perpetual_entry(
    base: &str,
    quote: &str,
    symbol: &str,
) -> market_participants_connector::catalog::batch::CatalogEntry {
    let key = perpetual_listing(symbol);
    let base = test_support::token(test_support::VENUE, base);
    let quote = test_support::token(test_support::VENUE, quote);
    let instrument = Instrument::new(InstrumentTerms::Perpetual(
        PerpetualTerms::new(
            DerivativeUnderlying::new(
                UnderlyingRef::Asset(base.clone()),
                price_reference(&base, &quote),
            ),
            quote.clone(),
            quote.clone(),
            ContractSize::new(
                ContractUnit::asset_quantity(Quantity::from(1_u8), base.clone())
                    .expect("the fixture contract size is positive"),
            )
            .expect("the fixture contract size is valid"),
            DerivativePnlModel::Linear,
            None,
        )
        .expect("the fixture perpetual terms are valid"),
    ))
    .expect("the fixture perpetual instrument is valid");
    let listing = VenueListing::new(
        key.clone(),
        instrument.id().clone(),
        Some(NativeSymbol::new(symbol).expect("the fixture symbol is canonical")),
        Vec::new(),
        VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product: BinanceProduct::Perpetual,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("the fixture perpetual listing is valid");
    let snapshot = ListingSnapshot::new(
        key,
        perpetual_rules(quote, base),
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the fixture perpetual snapshot is valid");
    market_participants_connector::catalog::batch::CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

/// One perpetual rule set of two fixture assets.
fn perpetual_rules(quote: AssetId, base: AssetId) -> TradingRules {
    let mut rules = test_support::rules(quote.clone(), base);
    rules.common.price.basis = PriceBasis::PerAssetUnit;
    rules.product = ProductTradingRules::Perpetual(PerpetualTradingRules {
        risk: PerpetualRisk::Published(
            PerpetualMarginTerms::new(
                MarginProfile::new(
                    MarginMode::Cross,
                    fraction("0.1"),
                    fraction("0.05"),
                    quote.clone(),
                )
                .expect("the fixture margin profile is valid"),
                vec![
                    LeverageTier::new(Quantity::from(1_000_u64), 10, fraction("0.05"))
                        .expect("the fixture leverage tier is valid"),
                ],
            )
            .expect("the fixture margin terms are valid"),
        ),
        funding: FundingSchedule::new(
            FundingInterval::Fixed {
                interval_millis: 28_800_000,
            },
            None,
            None,
            quote,
        )
        .expect("the fixture funding schedule is valid"),
        open_interest_cap: None,
    });
    rules
}

/// One validated fraction, written the way a venue publishes a margin ratio.
fn fraction(value: &str) -> market_participants_model::decimal::Fraction {
    market_participants_model::decimal::Fraction::new(
        Ratio::from_str(value).expect("the fixture ratio should parse"),
    )
    .expect("the fixture fraction should be valid")
}

/// The canonical price reference of one fixture perpetual.
///
/// The reference is the real terms of the fixture pair rather than a synthetic
/// digest: a publication resolves every reference its listings name in the
/// reference registry, so a fixture naming an identity no terms can produce is a
/// publication the display gate refuses.
fn price_reference(base: &AssetId, quote: &AssetId) -> PriceReferenceId {
    PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(base.clone()),
        quote_asset: quote.clone(),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(test_support::VENUE)
                .expect("the fixture reference source is canonical"),
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new(IDENTITY_VERSION)
            .expect("the fixture reference version is canonical"),
    }
    .reference_id()
    .expect("the fixture price reference is valid")
}

/// How the fake answers one fetch.
///
/// A plan is consumed per fetch, and a source that runs out of plans keeps
/// answering successfully: a test that watches how many fetches happened has to
/// be able to tell a fetch that failed from a fetch that never ran, and a fake
/// that fails once its queue is empty cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    Success(i64),
    Failure,
    /// Never answers, so the attempt times out.
    Pending,
}

/// The plan a fetch is answered with, and the timestamp a synthetic success
/// carries.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Answer {
    Success(i64),
    Failure,
    Pending,
}

#[derive(Clone)]
pub(crate) struct FakeSource {
    state: Arc<FakeSourceState>,
}

pub(crate) struct FakeSourceState {
    plans: Mutex<VecDeque<FetchPlan>>,
    queries: Mutex<Vec<StatisticsQuery>>,
    metrics: Mutex<Vec<MetricKind>>,
    metadata_fetches: AtomicUsize,
    fetch_started: AtomicUsize,
    /// Whether the venue still lists the contract a test follows.
    ///
    /// `true` by default: a venue that answers with the listing is what an existing
    /// test expects, so only the test about a delisted contract turns it off.
    listed: AtomicBool,
}

impl FakeSource {
    pub(crate) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                queries: Mutex::new(Vec::new()),
                metrics: Mutex::new(Vec::new()),
                metadata_fetches: AtomicUsize::new(0),
                fetch_started: AtomicUsize::new(0),
                listed: AtomicBool::new(true),
            }),
        }
    }

    /// Makes the venue answer that it no longer publishes the followed listing.
    pub(crate) fn delist(&self) {
        self.state.listed.store(false, Ordering::Release);
    }

    /// The ratio series differ only in which endpoint asked, so one answer serves
    /// all three.
    pub(crate) async fn fetch_ratio(
        &self,
        query: StatisticsQuery,
        metric: MetricKind,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        match self.plan(metric, &query) {
            Answer::Success(timestamp) => Ok(vec![ListingLongShortRatio {
                listing: query.listing,
                timestamp,
                ratio: Ratio::from(timestamp.unsigned_abs()),
            }]),
            Answer::Failure => Err(MarketError::ServiceClosed),
            Answer::Pending => pending().await,
        }
    }

    pub(crate) fn plan(&self, metric: MetricKind, query: &StatisticsQuery) -> Answer {
        {
            let mut metrics = self.lock_metrics();
            metrics.push(metric);
        }
        {
            let mut queries = self.lock_queries();
            queries.push(query.clone());
        }
        let call = self.state.fetch_started.fetch_add(1, Ordering::AcqRel);
        match self.lock_plans().pop_front() {
            Some(FetchPlan::Success(timestamp)) => Answer::Success(timestamp),
            Some(FetchPlan::Failure) => Answer::Failure,
            Some(FetchPlan::Pending) => Answer::Pending,
            // Past the end of the plans every fetch is observable and distinctive.
            None => Answer::Success(i64::try_from(call).unwrap_or(i64::MAX) + 100),
        }
    }

    pub(crate) fn lock_plans(&self) -> std::sync::MutexGuard<'_, VecDeque<FetchPlan>> {
        self.state
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn lock_queries(&self) -> std::sync::MutexGuard<'_, Vec<StatisticsQuery>> {
        self.state
            .queries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn lock_metrics(&self) -> std::sync::MutexGuard<'_, Vec<MetricKind>> {
        self.state
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn fetch_count(&self) -> usize {
        self.state.fetch_started.load(Ordering::Acquire)
    }

    pub(crate) fn metadata_fetches(&self) -> usize {
        self.state.metadata_fetches.load(Ordering::Acquire)
    }

    pub(crate) fn metrics(&self) -> Vec<MetricKind> {
        self.lock_metrics().clone()
    }

    pub(crate) fn queries(&self) -> Vec<StatisticsQuery> {
        self.lock_queries().clone()
    }
}

/// A traded quantity of the fixture's own base token.
pub(crate) fn traded(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::AssetUnits {
            asset: test_support::token(test_support::VENUE, "BTC"),
        },
    }
}

/// A monetary amount in the fixture's own quote token.
pub(crate) fn amount(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::Notional {
            asset: test_support::token(test_support::VENUE, "USDT"),
        },
    }
}

/// The venue's answers, including what it does with a listing it no longer
/// carries: the pane's own question about a route is answered by the catalog its
/// test installs, and this flag is what the metadata fetch reads.
impl StatisticsSource for FakeSource {
    async fn fetch_listing_metadata(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ListingPrecision, MarketError> {
        self.state.metadata_fetches.fetch_add(1, Ordering::AcqRel);
        if !self.state.listed.load(Ordering::Acquire) {
            return Err(MarketError::Catalog(Box::new(
                market_participants_connector::catalog::error::CatalogError::ListingNotFound {
                    listing: Box::new(listing.clone()),
                },
            )));
        }
        Ok(ListingPrecision {
            price_tick: Tick::from_inner(10_000_000_000_000_000).expect("tick should be valid"),
            quantity_tick: Tick::from_inner(1_000_000_000_000_000).expect("tick should be valid"),
        })
    }

    async fn fetch_open_interest(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingOpenInterest>, MarketError> {
        match self.plan(MetricKind::OpenInterest, &query) {
            Answer::Success(timestamp) => Ok(vec![ListingOpenInterest {
                listing: query.listing,
                timestamp,
                quantity: traded(Quantity::from(timestamp.unsigned_abs())),
                notional: Some(amount(Quantity::from(timestamp.unsigned_abs()))),
            }]),
            Answer::Failure => Err(MarketError::ServiceClosed),
            Answer::Pending => pending().await,
        }
    }

    async fn fetch_top_trader_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.fetch_ratio(query, MetricKind::TopTraderAccountRatio)
            .await
    }

    async fn fetch_top_trader_position_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.fetch_ratio(query, MetricKind::TopTraderPositionRatio)
            .await
    }

    async fn fetch_long_short_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.fetch_ratio(query, MetricKind::LongShortAccountRatio)
            .await
    }

    async fn fetch_taker_volume(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingTakerVolume>, MarketError> {
        match self.plan(MetricKind::TakerVolume, &query) {
            Answer::Success(timestamp) => Ok(vec![ListingTakerVolume {
                listing: query.listing,
                timestamp,
                buy: traded(Quantity::from(timestamp.unsigned_abs())),
                sell: traded(Quantity::from(timestamp.unsigned_abs())),
            }]),
            Answer::Failure => Err(MarketError::ServiceClosed),
            Answer::Pending => pending().await,
        }
    }
}
