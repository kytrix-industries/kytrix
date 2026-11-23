use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use market_participants_connector::{catalog::test_support, market::error::MarketError};
use market_participants_model::{
    decimal::{Quantity, Ratio, Tick},
    identifier::{VenueId, VenueScopeId},
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::BinanceProduct,
    },
    market::{
        book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
        units::{MarketQuantity, QuantityUnit},
    },
};

use super::stream::{StatisticsQuery, StatisticsSource};
use crate::model::MetricKind;

pub(crate) fn perpetual_route() -> VenueRoute {
    VenueRoute::new(
        VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical"),
        VenueScopeId::new(market_participants_connector::routing::PERPETUAL_SCOPE)
            .expect("the perpetual scope is canonical"),
    )
}

pub(crate) fn perpetual_listing(symbol: &str) -> VenueListingKey {
    test_support::continuity_key(&perpetual_route(), symbol, BinanceProduct::Perpetual)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    Success(i64),
    Failure,
    Pending,
}

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
            }),
        }
    }

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

pub(crate) fn traded(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::AssetUnits {
            asset: test_support::token(test_support::VENUE, "BTC"),
        },
    }
}

pub(crate) fn amount(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::Notional {
            asset: test_support::token(test_support::VENUE, "USDT"),
        },
    }
}

impl StatisticsSource for FakeSource {
    async fn fetch_listing_metadata(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ListingPrecision, MarketError> {
        self.state.metadata_fetches.fetch_add(1, Ordering::AcqRel);
        let _ = listing;
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
