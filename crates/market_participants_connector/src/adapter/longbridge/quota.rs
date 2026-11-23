use std::time::Duration;

use ahash::AHashMap;
use longbridge_proto::quote::RateLimit;
use parking_lot::Mutex;
use tokio::time::Instant;

const MILLI: u64 = 1_000;

#[derive(Debug)]
pub(super) struct LongbridgeQuota {
    state: Mutex<QuotaState>,
}

#[derive(Debug)]
struct QuotaState {
    available: u64,
    capacity: u64,
    per_second: u64,
    last_refill: Instant,
}

impl QuotaState {
    fn refill(&mut self, now: Instant) {
        let millis = u64::try_from(now.saturating_duration_since(self.last_refill).as_millis())
            .unwrap_or(u64::MAX);
        if millis == 0 {
            return;
        }
        let drip = millis.saturating_mul(self.per_second) / MILLI;
        self.available = self.available.saturating_add(drip).min(self.capacity);
        self.last_refill += Duration::from_millis(millis);
    }

    fn wait_for_one(&self) -> Duration {
        let missing = MILLI.saturating_sub(self.available);
        Duration::from_millis(missing.saturating_mul(MILLI).div_ceil(self.per_second))
    }
}

impl LongbridgeQuota {
    pub(super) fn new(burst: u32, limit: u32) -> Self {
        let capacity = u64::from(burst).saturating_mul(MILLI);
        Self {
            state: Mutex::new(QuotaState {
                available: capacity,
                capacity,
                per_second: u64::from(limit).saturating_mul(MILLI),
                last_refill: Instant::now(),
            }),
        }
    }

    pub(super) async fn acquire_one(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock();
                state.refill(Instant::now());
                if state.available >= MILLI {
                    state.available -= MILLI;
                    return;
                }
                state.wait_for_one()
            };
            tokio::time::sleep(wait.max(Duration::from_millis(1))).await;
        }
    }
}

pub(super) fn quotas_of(limits: &[RateLimit]) -> AHashMap<u8, LongbridgeQuota> {
    let mut quotas = AHashMap::with_capacity(limits.len());
    for limit in limits {
        let Ok(command) = u8::try_from(limit.command) else {
            log::warn!(
                "the Longbridge rate-limit table names command {}, which is not one this build can send",
                limit.command
            );
            continue;
        };
        if command == 0 || limit.limit < 1 || limit.burst < 1 {
            log::warn!(
                "the Longbridge rate-limit table states no usable pace for command {command}: \
                 limit {}, burst {}",
                limit.limit,
                limit.burst
            );
            continue;
        }
        quotas.insert(
            command,
            LongbridgeQuota::new(limit.burst as u32, limit.limit as u32),
        );
    }
    quotas
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use longbridge_proto::quote::RateLimit;

    use super::{LongbridgeQuota, quotas_of};

    fn rate_limit(command: u8, limit: i32, burst: i32) -> RateLimit {
        RateLimit {
            command: i32::from(command),
            limit,
            burst,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refill_drips_at_the_rate_the_table_states() {
        let quota = LongbridgeQuota::new(1, 1);

        quota.acquire_one().await;

        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(
            tokio::time::timeout(Duration::ZERO, quota.acquire_one())
                .await
                .is_err(),
            "a tenth of the rate is not a whole token"
        );

        let started = tokio::time::Instant::now();
        quota.acquire_one().await;
        assert_eq!(
            started.elapsed(),
            Duration::from_millis(900),
            "what the first tenth did not cover is nine tenths of the rate away"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waits_for_the_token_its_burst_did_not_cover() {
        let quota = LongbridgeQuota::new(1, 1);

        quota.acquire_one().await;
        let started = tokio::time::Instant::now();
        quota.acquire_one().await;

        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1),
            "the second token is one second of the bucket's own rate away"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_spent_before_the_rate_is_waited_for() {
        let quota = LongbridgeQuota::new(3, 1);

        for _ in 0..3 {
            quota.acquire_one().await;
        }
        let started = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        quota.acquire_one().await;
        quota.acquire_one().await;

        assert_eq!(
            started.elapsed(),
            Duration::from_secs(2),
            "the two tokens that dripped while nothing asked are spent without waiting"
        );
    }

    #[test]
    fn the_venues_table_becomes_one_bucket_per_command() {
        let quotas = quotas_of(&[
            rate_limit(19, 5, 10),
            rate_limit(27, 0, 10),
            rate_limit(6, 5, 0),
            rate_limit(0, 5, 5),
        ]);

        assert!(quotas.contains_key(&19));
        assert!(!quotas.contains_key(&27), "a rate of zero grants nothing");
        assert!(!quotas.contains_key(&6), "a burst of zero grants nothing");
        assert!(
            !quotas.contains_key(&0),
            "the unknown command is not one this build sends"
        );
        assert_eq!(quotas.len(), 1, "only the entry with a usable pace");
    }
}
