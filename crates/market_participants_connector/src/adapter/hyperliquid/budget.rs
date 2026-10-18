//! The weight budget of the venue's `info` endpoint, as one process-wide allowance.
//!
//! Hyperliquid aggregates every REST request one IP makes into a budget of 1200
//! weight, refilled once a minute, and answers a request over it with `429` and a
//! body that carries no reason at all. A budget is a property of the venue and of
//! the IP rather than of a route, a connection or a caller, so this process keeps
//! one of them and spends it on every `info` request it posts — the catalog and the
//! discovery.
//!
//! The bucket is a token bucket rather than a fixed window: weight is spent when a
//! request is made and drips back at the venue's own rate, so a burst that is
//! larger than one minute's worth is spread over the minutes it needs instead of
//! being cut off at a window boundary. Waiting is on a mutex rather than on a
//! semaphore because weight is spent in units the callers choose: a request whose
//! turn has not come holds the budget, and the wait is not charged to it.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::Notify;

/// What this process spends per minute, as a share of the venue's own allowance of
/// 1200 weight per minute.
///
/// The venue charges extra weight for the number of rows a fills or orders answer
/// carries, which is not known before the request is sent, so the budget keeps the
/// difference in hand rather than modelling that surcharge.
#[cfg(not(test))]
const PROCESS_BUDGET: u32 = 1_000;

/// The allowance one test process spends, which is the production pace scaled up.
///
/// One test binary runs every fixture conversation of this crate at once, and the
/// budget is a property of the venue rather than of a test: at the production pace a
/// suite would spend its allowance on waiting for itself, and the tests that state
/// the venue's own limits would be measuring the suite instead. The pace is still
/// this type's — a hundred times the rate rather than no budget at all — and the
/// venue's own arithmetic is what [`Budget::new`] states for a test that wants it.
#[cfg(test)]
const PROCESS_BUDGET: u32 = 100_000;
/// How long one request waits for weight before it is sent anyway.
///
/// A caller that waits longer than this is waiting for a budget that another
/// process on this IP is also spending; sending is the honest answer, and the
/// venue's own refusal is what the caller then reports.
const LONGEST_WAIT: Duration = Duration::from_secs(30);

/// The weight of one `info` request, refundable when the venue refused it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Weight(u32);

impl Weight {
    pub(crate) const fn of(weight: u32) -> Self {
        Self(weight)
    }
}

/// The weight one request reserved, which is returned unless the request served it.
///
/// A refused request is the venue saying it served nothing: the allowance it would
/// have spent is still this process's to spend, and a retry must not be charged
/// twice for one request. The refund is on the guard rather than on the failure path
/// because every way out of a request that did not serve it — a refused status, a
/// transport error, an undecodable answer, a cancelled caller — leaves the same
/// weight unspent.
///
/// A request the budget could not pay for within [`LONGEST_WAIT`] is sent anyway and
/// is not charged: it was never granted any weight, so returning weight for it would
/// hand the next waiter an allowance this process never had.
pub(crate) struct Spent {
    weight: Weight,
    /// The allowance this reserved from, so the refund goes back where it came from.
    budget: &'static Budget,
    charged: bool,
    served: bool,
}

impl Spent {
    /// Reserves one request's weight, waiting until the budget can pay for it.
    pub(crate) async fn reserve(weight: Weight) -> Self {
        info_budget().reserve(weight).await
    }

    /// [`Self::reserve`] against `budget`, waiting at most `longest_wait`.
    #[cfg(test)]
    async fn reserve_within(
        budget: &'static Budget,
        weight: Weight,
        longest_wait: Duration,
    ) -> Self {
        budget.reserve_within(weight, longest_wait).await
    }

    /// Whether this reservation took weight from the budget.
    #[cfg(test)]
    const fn charged(&self) -> bool {
        self.charged
    }

    /// Returns this reservation's weight as its own end of scope does.
    #[cfg(test)]
    fn returned(self) {
        drop(self);
    }

    /// Marks the request this reserved weight for as served, so it is not refunded.
    pub(crate) fn served(&mut self) {
        self.served = true;
    }
}
impl Drop for Spent {
    fn drop(&mut self) {
        if self.charged && !self.served {
            self.budget.refund(self.weight);
        }
    }
}

/// The allowance this process spends its `info` requests on.
pub(crate) struct Budget {
    state: Mutex<State>,
    refilled: Notify,
}

struct State {
    /// The weight available right now, and the drip rate, both in thousandths of
    /// a weight: one request's weight is a large part of a minute's allowance, so
    /// the small increments that drip back inside one wait have to be kept.
    available: u64,
    capacity: u64,
    /// How much weight drips back per second.
    per_second: u64,
    last_refill: Instant,
}

/// The thousandths one weight is accounted in.
const SCALE: u64 = 1_000;

impl State {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        // Milliseconds of one wait: a `u128` that cannot reach a `u64`'s limit.
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let dripped = millis.saturating_mul(self.per_second).saturating_div(SCALE);
        if dripped == 0 {
            return;
        }
        self.available = self.available.saturating_add(dripped).min(self.capacity);
        self.last_refill = now;
    }

    /// How long this request waits for the weight it is missing, in thousandths.
    fn wait_for(&self, needed: u64) -> Duration {
        let missing = needed.saturating_sub(self.available);
        Duration::from_millis(
            missing
                .saturating_mul(SCALE)
                .div_ceil(self.per_second.max(1)),
        )
    }
}

impl Budget {
    /// The budget one process spends on one venue's `info` endpoint.
    fn production() -> Self {
        // The allowance drips back over the minute the venue states it over.
        Self::new(PROCESS_BUDGET, PROCESS_BUDGET / 60)
    }

    fn new(capacity: u32, per_second: u32) -> Self {
        let capacity = u64::from(capacity).saturating_mul(SCALE);
        Self {
            state: Mutex::new(State {
                available: capacity,
                capacity,
                per_second: u64::from(per_second).saturating_mul(SCALE),
                last_refill: Instant::now(),
            }),
            refilled: Notify::new(),
        }
    }

    /// Takes one request's weight and answers the guard over it.
    ///
    /// Every reservation this process makes goes through here, so the refund in
    /// [`Spent`] goes back to the one budget that granted it.
    async fn reserve(&'static self, weight: Weight) -> Spent {
        self.reserve_within(weight, LONGEST_WAIT).await
    }

    /// [`Self::reserve`] with the wait this caller is willing to take.
    async fn reserve_within(&'static self, weight: Weight, longest_wait: Duration) -> Spent {
        Spent {
            weight,
            budget: self,
            charged: self.acquire_within(weight, longest_wait).await,
            served: false,
        }
    }

    /// Takes one request's weight, waiting until the budget can pay for it.
    ///
    /// Answers whether the weight was taken: a request the budget could not pay for
    /// within [`LONGEST_WAIT`] is sent anyway and is not charged, so the caller must
    /// not return weight it never spent.
    ///
    /// The wait releases the budget between attempts, and a request that is woken
    /// without the weight it is missing waits for what is still missing: weight is
    /// spent in units the callers choose, so being told that weight dripped back is
    /// not the same as being able to pay.
    ///
    /// Every request this process makes reserves through [`Spent`], so this entry is the
    /// one a test states the budget's own arithmetic with.
    #[cfg(test)]
    async fn acquire(&self, weight: Weight) -> bool {
        self.acquire_within(weight, LONGEST_WAIT).await
    }

    /// `Self::acquire` with the wait this caller is willing to take.
    async fn acquire_within(&self, weight: Weight, longest_wait: Duration) -> bool {
        let needed = u64::from(weight.0).saturating_mul(SCALE);
        let deadline = Instant::now() + longest_wait;
        loop {
            // Everything is taken under the budget, so nothing but plain values
            // crosses the wait below — a guard is not held over an await.
            let (wait, notified) = {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.refill(now);
                if state.available >= needed {
                    state.available -= needed;
                    return true;
                }
                // Registered before the budget is released, so weight that drips
                // back in between is not missed.
                let notified = self.refilled.notified();
                let remaining = deadline.saturating_duration_since(now);
                if remaining.is_zero() {
                    return false;
                }
                (state.wait_for(needed).min(remaining), notified)
            };
            let slept = tokio::time::timeout(wait, notified).await;
            if slept.is_err() && Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Returns one refused request's weight to the budget.
    ///
    /// A refusal is the venue saying the request was not served, so the allowance
    /// it would have spent is still this process's to spend — and the refusal is
    /// the only evidence available that the two accounts of the budget disagree.
    pub(crate) fn refund(&self, weight: Weight) {
        {
            let mut state = self.state.lock();
            state.available = state
                .available
                .saturating_add(u64::from(weight.0).saturating_mul(SCALE))
                .min(state.capacity);
        }
        self.refilled.notify_waiters();
    }

    /// What this budget can pay for right now, in thousandths of a weight.
    #[cfg(test)]
    fn available(&self) -> u64 {
        let mut state = self.state.lock();
        state.refill(Instant::now());
        state.available
    }
}

/// The budget every `info` request of this process draws on.
fn info_budget() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(Budget::production)
}

/// Returns how long a refusal asked the caller to wait, when it did.
///
/// `Retry-After` is the venue's own answer and is preferred over any wait this
/// process would choose; it is read as seconds, which is the delay-date and
/// delay-seconds form both reduce to for the values a venue sends here.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds.min(LONGEST_WAIT.as_secs())))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Budget, LONGEST_WAIT, SCALE, Spent, Weight, retry_after};
    use crate::adapter::hyperliquid::HyperliquidInfoRequest;

    /// The venue's own weight table, as this adapter spends it: an unlisted request
    /// is not silently free.
    #[test]
    fn a_request_is_charged_the_weight_the_venue_publishes() {
        assert_eq!(
            HyperliquidInfoRequest::L2Book { coin: "BTC".into() }.weight(),
            2
        );
        assert_eq!(
            HyperliquidInfoRequest::ClearinghouseState {
                user: "0x0".into(),
                dex: None,
            }
            .weight(),
            2
        );
        assert_eq!(
            HyperliquidInfoRequest::SpotClearinghouseState { user: "0x0".into() }.weight(),
            2
        );
        assert_eq!(HyperliquidInfoRequest::PerpDexs.weight(), 20);
        assert_eq!(HyperliquidInfoRequest::SpotMeta.weight(), 20);
        assert_eq!(
            HyperliquidInfoRequest::MetaAndAssetCtxs { dex: None }.weight(),
            20
        );
        assert_eq!(
            HyperliquidInfoRequest::UserFills {
                user: "0x0".into(),
                aggregate_by_time: Some(false),
            }
            .weight(),
            20
        );
    }

    /// Weight drips back at the venue's rate and never past the capacity.
    #[test]
    fn weight_drips_back_at_the_venue_rate_and_stops_at_the_capacity() {
        let budget = Budget::new(1_000, 20);
        let mut state = budget.state.lock();
        state.available = 0;
        let started = state.last_refill;

        state.last_refill = started - Duration::from_secs(3);
        state.refill(started);
        assert_eq!(
            state.available,
            60 * SCALE,
            "three seconds of the minute's allowance drip back"
        );

        state.last_refill = started - Duration::from_secs(600);
        state.refill(started);
        assert_eq!(
            state.available, state.capacity,
            "the budget never exceeds its cap"
        );
    }

    /// A request whose weight is not available waits for it rather than being
    /// refused, and the weight it waited for is what it spends.
    #[tokio::test]
    async fn a_request_waits_for_weight_it_cannot_pay_for_yet() {
        // A minute's allowance that drips back within a second, so the test states
        // the venue's own shape without waiting out the real minute.
        let budget = Budget::new(60, 60);

        // The first request takes the whole allowance ...
        assert!(
            budget.acquire(Weight::of(60)).await,
            "the allowance is paid"
        );
        // ... and the second is answered only once 20 weight has dripped back.
        let started = std::time::Instant::now();
        assert!(budget.acquire(Weight::of(20)).await);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(300),
            "a request over the budget waits for the weight to drip back, waited {waited:?}"
        );
        assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    }

    /// A request the budget could not pay for within its longest wait is sent anyway
    /// and is not charged — a caller that returned weight for it would be handing the
    /// next waiter an allowance this process never had.
    #[tokio::test]
    async fn a_request_the_budget_could_not_pay_for_is_not_charged() {
        // An allowance of one weight a second, so a request for a hundred cannot be
        // paid for inside a wait of any length this test is willing to take.
        let budget = Budget::new(60, 1);

        assert!(
            !budget
                .acquire_within(Weight::of(100), Duration::from_millis(50))
                .await,
            "a request past the wait is sent anyway and is not charged"
        );
    }

    /// A refund is what a refused request is worth, so a retry is charged once rather
    /// than twice.
    #[tokio::test]
    async fn a_refused_requests_weight_is_returned_to_the_budget() {
        let budget = test_budget(60, 60);

        assert!(budget.acquire(Weight::of(60)).await);
        budget.refund(Weight::of(60));
        // A third of a second of drip is not enough to pay for this: only the refund is.
        let started = std::time::Instant::now();
        assert!(budget.acquire(Weight::of(60)).await);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// A reservation the budget gave up on returns nothing: the weight was never taken,
    /// so returning it would hand the next waiter an allowance this process never had.
    ///
    /// The budget holds nothing when the reservation is made, so the phantom credit of one
    /// refused reservation is what the balance after it would show — a balance the cap
    /// could absorb the credit into would show nothing either way.
    #[tokio::test]
    async fn an_uncharged_reservation_returns_nothing() {
        // An allowance of one weight a second, so nothing drips back in time to help.
        let budget = test_budget(60, 1);

        budget.acquire(Weight::of(60)).await;
        budget.refund(Weight::of(60));
        budget.acquire(Weight::of(60)).await;
        assert_eq!(budget.available(), 0, "the allowance is spent");

        let uncharged =
            Spent::reserve_within(budget, Weight::of(100), Duration::from_millis(20)).await;
        assert!(!uncharged.charged(), "the budget cannot pay for it");
        uncharged.returned();

        assert!(
            budget.available() <= 50 * SCALE,
            "a reservation that was never charged must not be returned: the drip of a \
             few milliseconds is not {}",
            budget.available()
        );
    }

    /// A reservation that was charged stays spent once the request it was for was served.
    #[tokio::test]
    async fn a_charged_served_reservation_stays_spent() {
        let budget = test_budget(60, 60);

        // Whatever the budget can pay for now, plus the weight about to be charged: the
        // charge is what this test states, and a balance it starts above is not.
        let before = budget.available();
        let charged_weight = 40 * SCALE;
        let mut charged = budget.reserve(Weight::of(40)).await;
        assert!(
            charged.charged(),
            "the allowance drips back within a second"
        );
        charged.served();
        charged.returned();

        assert!(
            budget.available() + charged_weight <= before + 60 * SCALE,
            "a served request's weight stays spent, but the drip of one second is not \
             what returned it"
        );
        // The same reservation, dropped without having served anything, is returned.
        let before = budget.available();
        drop(budget.reserve(Weight::of(20)).await);
        assert!(
            budget.available() >= before,
            "a reservation that was not served is returned"
        );
    }

    /// One budget a test states the arithmetic of, which outlives the test's own frame
    /// because a reservation borrows the budget it was charged by.
    fn test_budget(capacity: u32, per_second: u32) -> &'static Budget {
        Box::leak(Box::new(Budget::new(capacity, per_second)))
    }

    /// A venue that states how long to wait is obeyed within the wait this process
    /// is willing to take.
    #[test]
    fn a_retry_after_header_is_read_in_seconds_and_capped() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("2"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("99999"),
        );
        assert_eq!(retry_after(&headers), Some(LONGEST_WAIT));
    }
}
