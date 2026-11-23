use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use kytrix_service_provider::service::engine::KytrixServiceError;
use smol_str::SmolStr;
use tokio::sync::Notify;

use crate::{
    model::{CoinMarketCapFearAndGreedIndexRow, CoinMarketCapFearAndGreedIndexSnapshot},
    service::{CoinMarketCapFearAndGreedIndexFetchError, CoinMarketCapFearAndGreedIndexSource},
};

pub(crate) const ACCOUNT: &str = "test-account";

pub(crate) const COLLECTED_AT: i64 = 1_790_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FetchPlan {
    Snapshot(u8),
    Failure,
    Pending,
    Held(u8),
}

pub(crate) fn row(date: &str, score: u8) -> CoinMarketCapFearAndGreedIndexRow {
    CoinMarketCapFearAndGreedIndexRow {
        date: SmolStr::new(date),
        score,
    }
}

pub(crate) fn snapshot(current: u8) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    Arc::new(CoinMarketCapFearAndGreedIndexSnapshot {
        current,
        yesterday: Some(row("2026-10-23", 45)),
        last_week: Some(row("2026-10-17", 40)),
        last_month: Some(row("2026-09-24", 28)),
        last_year: Some(row("2025-10-24", 72)),
        yearly_high: Some(row("2025-11-20", 88)),
        yearly_low: Some(row("2026-04-15", 12)),
        collected_at: COLLECTED_AT,
    })
}

pub(crate) fn empty_snapshot() -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
    Arc::new(CoinMarketCapFearAndGreedIndexSnapshot {
        current: 0,
        yesterday: None,
        last_week: None,
        last_month: None,
        last_year: None,
        yearly_high: None,
        yearly_low: None,
        collected_at: 0,
    })
}

#[derive(Clone)]
pub(crate) struct FakeSource {
    state: Arc<FakeSourceState>,
}

struct FakeSourceState {
    plans: Mutex<VecDeque<FetchPlan>>,
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    cancelled_fetches: AtomicUsize,
    release: Notify,
}

impl FakeSource {
    pub(crate) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                calls: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
                release: Notify::new(),
            }),
        }
    }

    pub(crate) fn release(&self) {
        self.state.release.notify_one();
    }

    fn begin(&self) -> (FetchPlan, FetchGuard) {
        let plan = self
            .state
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(FetchPlan::Pending);
        self.state.calls.fetch_add(1, Ordering::AcqRel);
        self.state.in_flight.fetch_add(1, Ordering::AcqRel);

        (
            plan,
            FetchGuard {
                state: self.state.clone(),
                completed: false,
            },
        )
    }

    pub(crate) fn fetch_count(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }
}

impl CoinMarketCapFearAndGreedIndexSource for FakeSource {
    async fn fetch_snapshot(
        &self,
    ) -> Result<Arc<CoinMarketCapFearAndGreedIndexSnapshot>, CoinMarketCapFearAndGreedIndexFetchError>
    {
        let (plan, mut guard) = self.begin();
        let result = match plan {
            FetchPlan::Snapshot(current) => Ok(snapshot(current)),
            FetchPlan::Failure => Err(CoinMarketCapFearAndGreedIndexFetchError::Service(
                KytrixServiceError::Connect("the fake source refused".into()),
            )),
            FetchPlan::Pending => pending().await,
            FetchPlan::Held(current) => {
                self.state.release.notified().await;

                Ok(snapshot(current))
            }
        };
        guard.completed = true;

        result
    }
}

struct FetchGuard {
    state: Arc<FakeSourceState>,
    completed: bool,
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        if !self.completed {
            self.state.cancelled_fetches.fetch_add(1, Ordering::AcqRel);
        }
    }
}
