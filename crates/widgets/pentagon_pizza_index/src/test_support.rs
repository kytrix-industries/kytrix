use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use kytrix_proto::types::v1 as proto_types;
use kytrix_service_provider::service::engine::KytrixServiceError;

use super::{
    model::{PizzaLevel, PizzaReading},
    service::{PizzaFetchError, PizzaSource},
};

pub(super) const ACCOUNT: &str = "test-account";

pub(super) const OBSERVED_AT: i64 = 1_792_821_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FetchPlan {
    Snapshot(u32),
    UnknownLevel,
    Failure,
    Pending,
}

pub(super) fn answer(level: u32) -> proto_types::PentagonPizzaIndex {
    proto_types::PentagonPizzaIndex {
        defcon_level: level,
        observed_at: OBSERVED_AT,
        timestamp: OBSERVED_AT + 60_000,
    }
}

pub(super) fn reading(level: u8) -> PizzaReading {
    PizzaReading::for_test(
        PizzaLevel::from_number(level).expect("the fixture names a rung"),
        OBSERVED_AT,
    )
}

#[derive(Clone)]
pub(super) struct FakeSource {
    state: Arc<FakeSourceState>,
}

struct FakeSourceState {
    plans: Mutex<VecDeque<FetchPlan>>,
    fetches: AtomicUsize,
    cancelled_fetches: AtomicUsize,
}

impl FakeSource {
    pub(super) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                fetches: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
            }),
        }
    }

    pub(super) fn fetches(&self) -> usize {
        self.state.fetches.load(Ordering::Acquire)
    }

    pub(super) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }

    fn begin(&self) -> (FetchPlan, FetchGuard) {
        let plan = self
            .state
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(FetchPlan::Pending);
        self.state.fetches.fetch_add(1, Ordering::AcqRel);

        (
            plan,
            FetchGuard {
                state: Arc::clone(&self.state),
                completed: false,
            },
        )
    }
}

impl Default for FakeSource {
    fn default() -> Self {
        Self::new([])
    }
}

impl PizzaSource for FakeSource {
    async fn fetch(&self) -> Result<proto_types::PentagonPizzaIndex, PizzaFetchError> {
        let (plan, mut guard) = self.begin();
        let result = match plan {
            FetchPlan::Snapshot(level) => Ok(answer(level)),
            FetchPlan::UnknownLevel => Ok(answer(9)),
            FetchPlan::Failure => Err(PizzaFetchError::Service(KytrixServiceError::Connect(
                "the fake source refused".into(),
            ))),
            FetchPlan::Pending => pending().await,
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
        if !self.completed {
            self.state.cancelled_fetches.fetch_add(1, Ordering::AcqRel);
        }
    }
}
