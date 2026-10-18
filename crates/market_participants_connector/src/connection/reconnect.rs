use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub multiplier: u32,
    pub max_delay: Duration,
    pub max_attempts: Option<usize>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            multiplier: 2,
            max_delay: Duration::from_secs(30),
            max_attempts: None,
        }
    }
}

impl ReconnectPolicy {
    pub fn delay_for(self, attempt: usize) -> Duration {
        let mut delay = self.initial_delay;
        let mut index = 0;
        while index < attempt {
            delay = match delay.checked_mul(self.multiplier) {
                Some(next) if next <= self.max_delay => next,
                _ => self.max_delay,
            };
            index += 1;
        }
        delay
    }

    pub fn should_attempt(self, attempt: usize) -> bool {
        match self.max_attempts {
            Some(maximum) => attempt < maximum,
            None => true,
        }
    }
}

pub struct ExponentialBackoff {
    policy: ReconnectPolicy,
    attempt: usize,
}

impl ExponentialBackoff {
    pub const fn new(policy: ReconnectPolicy) -> Self {
        Self { policy, attempt: 0 }
    }

    pub fn next_delay(&mut self) -> Option<Duration> {
        if !self.policy.should_attempt(self.attempt) {
            return None;
        }
        let delay = self.policy.delay_for(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        Some(delay)
    }

    pub const fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ExponentialBackoff, ReconnectPolicy};

    #[test]
    fn delays_increase_and_cap() {
        let policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(100),
            multiplier: 2,
            max_delay: Duration::from_millis(250),
            max_attempts: None,
        };
        let mut backoff = ExponentialBackoff::new(policy);
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(200)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(250)));
    }
}
