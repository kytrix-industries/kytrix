//! The venue's per-command pace.
//!
//! The market-data profile states, per command, how many requests an account may
//! make and how large a burst it may spend at once, and this module is that
//! statement as one bucket per command. A command the profile published no pace for
//! has no bucket and is not throttled, which is what the official client does with
//! the same table: the venue's own refusal is what answers a request that is over an
//! allowance nothing was published for.

use std::time::Duration;

use ahash::AHashMap;
use longbridge_proto::quote::RateLimit;
use parking_lot::Mutex;
use tokio::time::Instant;

/// One thousandth of a token, which is the unit a quota counts in.
///
/// A quota drips at the rate its own limit states, which is a fraction of a token
/// per millisecond as often as not: counting whole tokens would round a first
/// request away and let a second one through before the venue's own pace allowed it.
const MILLI: u64 = 1_000;

/// One command's allowance, as the venue's profile published it.
///
/// The bucket is a token bucket rather than a fixed window: `burst` tokens are
/// available at once and `limit` of them drip back every second, so a command that
/// bursts is paced over the seconds it needs instead of being cut off at a window
/// boundary. A request waits for its token rather than being refused, which is what
/// the official client does with the same numbers; a request the venue refuses anyway
/// is the venue's own answer.
///
/// The wait is computed from the bucket's own clock rather than slept off in fixed
/// steps: each refill adds exactly the whole milliseconds that passed, so a long wait
/// is one sleep and the sub-millisecond remainder stays in the clock instead of being
/// rounded away at every request.
#[derive(Debug)]
pub(super) struct LongbridgeQuota {
    state: Mutex<QuotaState>,
}

/// The tokens one quota holds, in thousandths, and when they last dripped.
#[derive(Debug)]
struct QuotaState {
    /// What may be spent right now, in thousandths of a token.
    available: u64,
    /// The most the bucket ever holds, in thousandths of a token.
    capacity: u64,
    /// How many thousandths of a token drip back every second.
    per_second: u64,
    /// When the drip was last accounted for.
    ///
    /// The clock is the runtime's rather than the wall's, so the bucket is measured
    /// by the same clock its own waits are: a test that moves the runtime's time
    /// moves the bucket with it instead of waiting out the venue's real pace.
    last_refill: Instant,
}

impl QuotaState {
    /// Adds the tokens that dripped back since the last refill.
    ///
    /// Only whole milliseconds are consumed, so the part of a millisecond that has
    /// not passed yet is accounted for by the next refill rather than lost: a bucket
    /// asked once per request would otherwise drip slightly slower than the venue's
    /// own rate, by up to a millisecond per request.
    fn refill(&mut self, now: Instant) {
        let millis = u64::try_from(now.saturating_duration_since(self.last_refill).as_millis())
            .unwrap_or(u64::MAX);
        if millis == 0 {
            return;
        }
        let drip = millis.saturating_mul(self.per_second);
        self.available = self.available.saturating_add(drip).min(self.capacity);
        self.last_refill += Duration::from_millis(millis);
    }

    /// How long one token takes to drip back, when none is available.
    fn wait_for_one(&self) -> Duration {
        let missing = MILLI.saturating_sub(self.available);
        Duration::from_millis(missing.saturating_mul(MILLI).div_ceil(self.per_second))
    }
}

impl LongbridgeQuota {
    /// Opens the bucket one command's published pace describes.
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

    /// Takes one token, waiting for as long as the bucket's own rate needs.
    ///
    /// A bucket only ever exists for a pace the venue published as positive, so every
    /// wait here ends: one token drips back within a second at the slowest rate a
    /// quota can state.
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

/// The per-command quotas one published rate-limit table describes.
///
/// An entry this build cannot honour — a command outside the byte the packet's own
/// field carries, or a pace the venue published as zero — is dropped with a warning
/// rather than installed as a bucket that could never grant a token: a command with
/// no bucket is sent without waiting, and the venue's own refusal is what answers it.
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

    /// One `(command, limit, burst)` row of the venue's rate-limit table.
    fn rate_limit(command: u8, limit: i32, burst: i32) -> RateLimit {
        RateLimit {
            command: i32::from(command),
            limit,
            burst,
        }
    }

    /// A bucket drips one token a second and hands out its burst at once, so a second
    /// request waits for the token its burst did not cover.
    ///
    /// The clock is the test's own, so what is asserted is the bucket's arithmetic
    /// rather than how fast the machine is.
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

    /// A bucket hands out its whole burst before it waits, and the tokens that dripped
    /// while nothing asked are still there: a pace is a rate rather than a window.
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

    /// The venue's table becomes one bucket per command code, and an entry this build
    /// cannot honour is dropped rather than installed as a bucket that never grants.
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
