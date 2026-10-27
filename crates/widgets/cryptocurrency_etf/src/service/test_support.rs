//! The fake source the service and the pane both answer through.
//!
//! It lives in a module of its own because two test files need it: the loop's
//! tests drive it to produce snapshots, and the pane's tests drive it to answer a
//! load. Everything in here is test scaffolding and is compiled only for tests.

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
use time::{Date, Month};

use super::stream::{CryptocurrencyEtfFetchError, CryptocurrencyEtfSource};
use crate::model::{
    CryptocurrencyEtfAsset, CryptocurrencyEtfDataset, CryptocurrencyEtfDay,
    CryptocurrencyEtfSnapshot,
};

/// How the fake answers one fetch.
///
/// A plan is consumed per fetch, and a source that runs out of plans keeps
/// answering with a later snapshot: a test that watches how many fetches happened
/// has to be able to tell a fetch that failed from a fetch that never ran, and a
/// fake that fails once its queue is empty cannot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FetchPlan {
    /// The whole snapshot a history of one day with this total flow makes.
    Snapshot(f64),
    Failure,
    /// Never answers, so the attempt times out.
    Pending,
}

/// The answer one fetch is given.
#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Snapshot(Arc<CryptocurrencyEtfSnapshot>),
    Failure,
    Pending,
}

#[derive(Clone)]
pub(crate) struct FakeSource {
    state: Arc<FakeSourceState>,
}

struct FakeSourceState {
    plans: Mutex<VecDeque<FetchPlan>>,
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    maximum_in_flight: AtomicUsize,
    cancelled_fetches: AtomicUsize,
}

impl FakeSource {
    pub(crate) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                calls: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                maximum_in_flight: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
            }),
        }
    }

    /// Takes the next plan, counting the fetch as started.
    fn begin(&self) -> (Answer, FetchGuard) {
        let call = self.state.calls.fetch_add(1, Ordering::AcqRel);
        let plan = self
            .state
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| {
                // Past the end of the plans every fetch is a distinct snapshot, so
                // a fetch that happened is visible in the data as well as in the
                // count.
                let flow = i64::try_from(call).unwrap_or(i64::MAX) as f64;
                FetchPlan::Snapshot(flow)
            });
        let in_flight = self.state.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.state
            .maximum_in_flight
            .fetch_max(in_flight, Ordering::AcqRel);
        let answer = match plan {
            FetchPlan::Snapshot(flow) => Answer::Snapshot(snapshot(flow)),
            FetchPlan::Failure => Answer::Failure,
            FetchPlan::Pending => Answer::Pending,
        };
        (
            answer,
            FetchGuard {
                state: self.state.clone(),
                completed: false,
            },
        )
    }

    async fn execute(&self) -> Result<Arc<CryptocurrencyEtfSnapshot>, CryptocurrencyEtfFetchError> {
        let (answer, mut guard) = self.begin();
        let result = match answer {
            Answer::Snapshot(snapshot) => Ok(snapshot),
            Answer::Failure => Err(CryptocurrencyEtfFetchError::Service(
                KytrixServiceError::Connect("test failure".into()),
            )),
            Answer::Pending => pending().await,
        };
        guard.completed = true;
        result
    }

    pub(crate) fn fetch_count(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }

    pub(crate) fn maximum_in_flight(&self) -> usize {
        self.state.maximum_in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }

    /// How many fetches were dropped before they answered.
    pub(crate) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }
}

impl CryptocurrencyEtfSource for FakeSource {
    async fn fetch_snapshot(
        &self,
    ) -> Result<Arc<CryptocurrencyEtfSnapshot>, CryptocurrencyEtfFetchError> {
        self.execute().await
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

/// A whole snapshot whose every asset has the same one-day history.
pub(crate) fn snapshot(flow: f64) -> Arc<CryptocurrencyEtfSnapshot> {
    snapshot_of(core::array::from_fn(|index| {
        asset(ALL[index], &[flow], DEFAULT_FUNDS)
    }))
}

/// The assets a snapshot carries, in the order the model stores them.
const ALL: [CryptocurrencyEtfAsset; 4] = [
    CryptocurrencyEtfAsset::Btc,
    CryptocurrencyEtfAsset::Eth,
    CryptocurrencyEtfAsset::Sol,
    CryptocurrencyEtfAsset::Hyp,
];

/// The funds a fixture asset has when a test does not name its own.
pub(crate) const DEFAULT_FUNDS: &[&str] = &["FUND"];

/// One asset's history: a fund list, and one day per flow.
pub(crate) fn asset(
    asset: CryptocurrencyEtfAsset,
    flows: &[f64],
    funds: &[&str],
) -> Arc<CryptocurrencyEtfDataset> {
    // Julian days from a fixed start rather than ordinals of one year: a fixture is
    // allowed to be longer than a year, and an ordinal is not.
    let first = Date::from_calendar_date(2024, Month::January, 1)
        .expect("the fixture start should be valid")
        .to_julian_day();
    let mut net_assets = 0.0;
    let days = flows
        .iter()
        .enumerate()
        .map(|(index, flow)| {
            net_assets += flow;
            CryptocurrencyEtfDay {
                date: Date::from_julian_day(first + i32::try_from(index).unwrap_or(i32::MAX))
                    .expect("fixture date should be valid"),
                fund_flows: Arc::from([*flow]),
                total_flow: *flow,
                net_assets,
            }
        })
        .collect::<Vec<_>>();
    Arc::new(CryptocurrencyEtfDataset {
        asset,
        funds: funds
            .iter()
            .map(|fund| SmolStr::new(*fund))
            .collect::<Vec<_>>()
            .into(),
        days: days.into(),
    })
}

/// A whole snapshot from four assets' histories.
pub(crate) fn snapshot_of(
    datasets: [Arc<CryptocurrencyEtfDataset>; 4],
) -> Arc<CryptocurrencyEtfSnapshot> {
    Arc::new(CryptocurrencyEtfSnapshot { datasets })
}
