use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::Notify;

#[cfg(not(test))]
const PROCESS_BUDGET: u32 = 1_000;

#[cfg(test)]
const PROCESS_BUDGET: u32 = 100_000;
const LONGEST_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Weight(u32);

impl Weight {
    pub(crate) const fn of(weight: u32) -> Self {
        Self(weight)
    }
}

pub(crate) struct Spent {
    weight: Weight,
    budget: &'static Budget,
    charged: bool,
    served: bool,
}

impl Spent {
    pub(crate) async fn reserve(weight: Weight) -> Self {
        info_budget().reserve(weight).await
    }

    #[cfg(test)]
    async fn reserve_within(
        budget: &'static Budget,
        weight: Weight,
        longest_wait: Duration,
    ) -> Self {
        budget.reserve_within(weight, longest_wait).await
    }

    #[cfg(test)]
    const fn charged(&self) -> bool {
        self.charged
    }

    #[cfg(test)]
    fn returned(self) {
        drop(self);
    }

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

pub(crate) struct Budget {
    state: Mutex<State>,
    refilled: Notify,
}

struct State {
    available: u64,
    capacity: u64,
    per_second: u64,
    last_refill: Instant,
}

const SCALE: u64 = 1_000;

impl State {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let dripped = millis.saturating_mul(self.per_second).saturating_div(SCALE);
        if dripped == 0 {
            return;
        }
        self.available = self.available.saturating_add(dripped).min(self.capacity);
        self.last_refill = now;
    }

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
    fn production() -> Self {
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

    async fn reserve(&'static self, weight: Weight) -> Spent {
        self.reserve_within(weight, LONGEST_WAIT).await
    }

    async fn reserve_within(&'static self, weight: Weight, longest_wait: Duration) -> Spent {
        Spent {
            weight,
            budget: self,
            charged: self.acquire_within(weight, longest_wait).await,
            served: false,
        }
    }

    #[cfg(test)]
    async fn acquire(&self, weight: Weight) -> bool {
        self.acquire_within(weight, LONGEST_WAIT).await
    }

    async fn acquire_within(&self, weight: Weight, longest_wait: Duration) -> bool {
        let needed = u64::from(weight.0).saturating_mul(SCALE);
        let deadline = Instant::now() + longest_wait;
        loop {
            let (wait, notified) = {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.refill(now);
                if state.available >= needed {
                    state.available -= needed;
                    return true;
                }
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

    #[cfg(test)]
    fn available(&self) -> u64 {
        let mut state = self.state.lock();
        state.refill(Instant::now());
        state.available
    }
}

fn info_budget() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(Budget::production)
}

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

    #[tokio::test]
    async fn a_request_waits_for_weight_it_cannot_pay_for_yet() {
        let budget = Budget::new(60, 60);

        assert!(
            budget.acquire(Weight::of(60)).await,
            "the allowance is paid"
        );
        let started = std::time::Instant::now();
        assert!(budget.acquire(Weight::of(20)).await);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(300),
            "a request over the budget waits for the weight to drip back, waited {waited:?}"
        );
        assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    }

    #[tokio::test]
    async fn a_request_the_budget_could_not_pay_for_is_not_charged() {
        let budget = Budget::new(60, 1);

        assert!(
            !budget
                .acquire_within(Weight::of(100), Duration::from_millis(50))
                .await,
            "a request past the wait is sent anyway and is not charged"
        );
    }

    #[tokio::test]
    async fn a_refused_requests_weight_is_returned_to_the_budget() {
        let budget = test_budget(60, 60);

        assert!(budget.acquire(Weight::of(60)).await);
        budget.refund(Weight::of(60));
        let started = std::time::Instant::now();
        assert!(budget.acquire(Weight::of(60)).await);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn an_uncharged_reservation_returns_nothing() {
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

    #[tokio::test]
    async fn a_charged_served_reservation_stays_spent() {
        let budget = test_budget(60, 60);

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
        let before = budget.available();
        drop(budget.reserve(Weight::of(20)).await);
        assert!(
            budget.available() >= before,
            "a reservation that was not served is returned"
        );
    }

    fn test_budget(capacity: u32, per_second: u32) -> &'static Budget {
        Box::leak(Box::new(Budget::new(capacity, per_second)))
    }

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
