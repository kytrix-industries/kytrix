//! The fake source the service's and the pane's tests answer through.
//!
//! It is shared rather than copied because both halves of the widget test the same
//! seam: the loop's tests drive the answers it gives, and the pane's tests count the
//! reads it was asked for. A source that runs out of plans never answers again, so a
//! test that expects an absence can tell "the read never happened" from "the answer
//! went somewhere else" by the count.

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
use time::{Date, Month};

use crate::{
    model::{CalendarSnapshot, Importance},
    service::{CalendarFetchError, CalendarSource},
};

/// The account every fixture pane is scoped to.
pub(crate) const ACCOUNT: &str = "test-account";

/// The instant the fixtures were collected at.
pub(crate) const COLLECTED_AT: i64 = 1_790_000_000_000;

/// The instant of `day` October 2026 at `hour` UTC, in Unix milliseconds.
pub(crate) fn at(day: u8, hour: u8) -> i64 {
    Date::from_calendar_date(2026, Month::October, day)
        .expect("a day of October 2026")
        .with_hms(hour, 0, 0)
        .expect("an hour of the day")
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

/// One event of a fixture answer.
pub(crate) fn proto_event(
    at: i64,
    country: &str,
    name: &str,
    importance: Importance,
    reference: Option<&str>,
) -> proto_types::FinancialCalendarEvent {
    let importance = match importance {
        Importance::Unspecified => {
            proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceUnspecified
        }
        Importance::Low => {
            proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceLow
        }
        Importance::Medium => {
            proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceMedium
        }
        Importance::High => {
            proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceHigh
        }
    };

    proto_types::FinancialCalendarEvent {
        timestamp: at,
        country_code: country.to_owned(),
        name: name.to_owned(),
        importance: importance as i32,
        reference: reference.map(str::to_owned),
    }
}

/// One answer of `events` events, one an hour from 09:00 UTC on the fixture day.
pub(crate) fn snapshot(events: usize) -> Arc<CalendarSnapshot> {
    let events = (0..events)
        .map(|index| {
            proto_event(
                at(25, 9) + index as i64 * 60 * 60 * 1_000,
                if index % 2 == 0 { "US" } else { "DE" },
                "event",
                Importance::Medium,
                None,
            )
        })
        .collect();
    read(events)
}

/// The answer whose day boundary moves with the display zone: two events, the first
/// late in the UTC day and the second early in the next one, which are one day eight
/// hours east of UTC.
pub(crate) fn boundary_snapshot() -> Arc<CalendarSnapshot> {
    read(vec![
        proto_event(at(25, 23), "US", "late", Importance::High, Some("SEP")),
        proto_event(at(26, 1), "CN", "early", Importance::Low, None),
    ])
}

/// Reads a fixture answer through the production path, so a fixture is as valid as
/// the data a service sends.
fn read(events: Vec<proto_types::FinancialCalendarEvent>) -> Arc<CalendarSnapshot> {
    Arc::new(
        CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events,
            timestamp: COLLECTED_AT,
        })
        .expect("a fixture is a readable calendar"),
    )
}

/// How the fake answers one read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    /// Answers with an answer of this many events.
    Snapshot(usize),
    /// Answers with a service failure.
    Failure,
    /// Never answers, so the read stays in flight.
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
    cancelled_fetches: AtomicUsize,
}

impl FakeSource {
    pub(crate) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                calls: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
            }),
        }
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

    /// How many reads were started.
    pub(crate) fn fetch_count(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }

    /// How many reads have not answered yet.
    pub(crate) fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }

    /// How many reads were dropped before they answered.
    pub(crate) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }
}

impl CalendarSource for FakeSource {
    async fn fetch_calendar(&self) -> Result<Arc<CalendarSnapshot>, CalendarFetchError> {
        let (plan, mut guard) = self.begin();
        let result = match plan {
            FetchPlan::Snapshot(events) => Ok(snapshot(events)),
            FetchPlan::Failure => Err(CalendarFetchError::Service(KytrixServiceError::Connect(
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
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        if !self.completed {
            self.state.cancelled_fetches.fetch_add(1, Ordering::AcqRel);
        }
    }
}
