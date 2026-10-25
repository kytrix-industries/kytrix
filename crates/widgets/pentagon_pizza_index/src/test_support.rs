//! The fake index several test modules answer through.
//!
//! One fake is shared because every module asks the service the same questions and
//! answers them with the same document: a second copy per module would be the same
//! lines with a different name.
//!
//! It hands over the answer itself rather than a reading, which is what the
//! production source does: that is the seam the parse is on, so a fake that answered
//! with a reading could not say what the service does with an unreadable one.

use std::{
    collections::VecDeque,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use kytrix_proto::types as proto_types;
use kytrix_service_provider::service::engine::KytrixServiceError;

use super::{
    model::{PizzaLevel, PizzaReading},
    service::{PizzaFetchError, PizzaSource},
};

/// The account every fixture pane is scoped to.
pub(super) const ACCOUNT: &str = "test-account";

/// The moment every fixture answer is observed at.
pub(super) const OBSERVED_AT: i64 = 1_792_821_600_000;

/// How the fake answers one fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FetchPlan {
    /// Answers with a snapshot carrying this level.
    Snapshot(u32),
    /// Answers with a level the ladder does not have, which is an answer the pane
    /// must refuse rather than clamp.
    UnknownLevel,
    /// Answers with a service failure.
    Failure,
    /// Never answers, so the fetch stays in flight.
    Pending,
}

/// The answer one level is published as.
pub(super) fn answer(level: u32) -> proto_types::PentagonPizzaIndex {
    proto_types::PentagonPizzaIndex {
        overall_index: level,
        observed_at: OBSERVED_AT,
        timestamp: 1_790_000_000_000,
    }
}

/// The reading a fixture answer of `level` stands for.
pub(super) fn reading(level: u8) -> PizzaReading {
    PizzaReading::for_test(
        PizzaLevel::from_number(level).expect("the fixture names a rung"),
        OBSERVED_AT,
    )
}

/// A source whose answers, and whose number of reads, a test decides.
///
/// With nothing left to answer it never answers again, so a test that expects an
/// absence can tell "the fetch never happened" from "the answer went somewhere
/// else" by the count.
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

    /// How many times the index has been read.
    pub(super) fn fetches(&self) -> usize {
        self.state.fetches.load(Ordering::Acquire)
    }

    /// How many reads were dropped before they answered.
    pub(super) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }

    /// Takes the next plan, counting the read as started.
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

/// Counts the read it belongs to as in flight until it answers or is dropped.
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
