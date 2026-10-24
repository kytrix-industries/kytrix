//! The fake source the service's and the pane's tests answer through.
//!
//! It is shared rather than copied because both halves of the widget test the same
//! seam: the loop's tests drive the answers it gives, and the pane's tests count the
//! fetches it was asked for. A source that runs out of plans never answers again, so
//! a test that expects an absence can tell "the fetch never happened" from "the
//! answer went somewhere else" by the count.

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
    model::{FgiRow, FgiSnapshot},
    service::{FgiFetchError, FgiSource},
};

/// The account every fixture pane is scoped to.
pub(crate) const ACCOUNT: &str = "test-account";

/// The collection time every non-empty fixture was collected at.
pub(crate) const COLLECTED_AT: i64 = 1_790_000_000_000;

/// How the fake answers one fetch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FetchPlan {
    /// Answers with a complete snapshot of this current value.
    Snapshot(u8),
    /// Answers with a service failure.
    Failure,
    /// Never answers, so the fetch stays in flight.
    Pending,
    /// Waits until the test releases it, so a test can look at what the pane does
    /// while a fetch is still in flight, then answer it with this value.
    Held(u8),
}

/// One point of a fixture snapshot.
pub(crate) fn row(date: &str, score: u8) -> FgiRow {
    FgiRow {
        date: SmolStr::new(date),
        score,
    }
}

/// A complete snapshot of one current value.
pub(crate) fn snapshot(current: u8) -> Arc<FgiSnapshot> {
    Arc::new(FgiSnapshot {
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

/// The answer the service gives before its collector has run.
pub(crate) fn empty_snapshot() -> Arc<FgiSnapshot> {
    Arc::new(FgiSnapshot {
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
    /// Wakes the fetch a [`FetchPlan::Held`] left waiting.
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

    /// Answers the fetch a [`FetchPlan::Held`] left waiting.
    ///
    /// The wake-up is kept even if the fetch has not started waiting yet, so a test
    /// that releases as soon as the fetch is counted cannot lose the answer.
    pub(crate) fn release(&self) {
        self.state.release.notify_one();
    }

    /// Takes the next plan, counting the fetch as started.
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

    /// How many fetches were started.
    pub(crate) fn fetch_count(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }

    /// How many fetches have not answered yet.
    pub(crate) fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }

    /// How many fetches were dropped before they answered.
    pub(crate) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }
}

impl FgiSource for FakeSource {
    async fn fetch_snapshot(&self) -> Result<Arc<FgiSnapshot>, FgiFetchError> {
        let (plan, mut guard) = self.begin();
        let result = match plan {
            FetchPlan::Snapshot(current) => Ok(snapshot(current)),
            FetchPlan::Failure => Err(FgiFetchError::Service(KytrixServiceError::Connect(
                "the fake source refused".into(),
            ))),
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

/// Counts the fetch it belongs to as in flight until it answers or is dropped.
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
