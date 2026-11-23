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
use time::{Date, Month, OffsetDateTime, UtcOffset};

use crate::{
    model::{CalendarSnapshot, Importance},
    service::{CalendarFetchError, CalendarSource},
};

pub(crate) const ACCOUNT: &str = "test-account";

pub(crate) const COLLECTED_AT: i64 = 1_790_000_000_000;

pub(crate) fn at(day: u8, hour: u8) -> i64 {
    Date::from_calendar_date(2026, Month::October, day)
        .expect("a day of October 2026")
        .with_hms(hour, 0, 0)
        .expect("an hour of the day")
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

pub(crate) fn proto_event(
    at: i64,
    country: &str,
    name: &str,
    importance: Importance,
    reference: Option<&str>,
) -> proto_types::FinancialCalendarEvent {
    let importance = match importance {
        Importance::Unspecified => proto_types::FinancialCalendarEventImportance::Unspecified,
        Importance::Low => proto_types::FinancialCalendarEventImportance::Low,
        Importance::Medium => proto_types::FinancialCalendarEventImportance::Medium,
        Importance::High => proto_types::FinancialCalendarEventImportance::High,
    };
    let datetime = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        .expect("a fixture instant is one the calendar holds")
        .to_offset(UtcOffset::UTC);

    proto_types::FinancialCalendarEvent {
        year: datetime.year() as u32,
        month: datetime.month() as u32,
        day: datetime.day() as u32,
        seconds_of_day: Some(
            u32::from(datetime.hour()) * 3_600
                + u32::from(datetime.minute()) * 60
                + u32::from(datetime.second()),
        ),
        country_code: country.to_owned(),
        name: name.to_owned(),
        importance: importance as i32,
        reference: reference.map(str::to_owned),
    }
}

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

pub(crate) fn boundary_snapshot() -> Arc<CalendarSnapshot> {
    read(vec![
        proto_event(at(25, 23), "US", "late", Importance::High, Some("SEP")),
        proto_event(at(26, 1), "CN", "early", Importance::Low, None),
    ])
}

fn read(events: Vec<proto_types::FinancialCalendarEvent>) -> Arc<CalendarSnapshot> {
    Arc::new(
        CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events,
            timestamp: COLLECTED_AT,
        })
        .expect("a fixture is a readable calendar"),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    Snapshot(usize),
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
