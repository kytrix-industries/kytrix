use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    pub message: &'static str,
    pub answer: &'static str,
    pub interval: Duration,
    pub maximum_missed: u32,
    pub send_while_active: bool,
}

#[derive(Clone, Debug)]
pub struct Heartbeat {
    keepalive: Option<Keepalive>,
    silent_since: Instant,
    missed: u32,
}

impl Heartbeat {
    pub fn start(keepalive: Option<Keepalive>) -> Self {
        Self {
            keepalive,
            silent_since: Instant::now(),
            missed: 0,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.keepalive
            .map(|keepalive| self.silent_since + keepalive.interval)
    }

    pub fn message(&self) -> Option<&'static str> {
        self.keepalive.map(|keepalive| keepalive.message)
    }

    pub fn on_message_sent(&mut self) {
        self.silent_since = Instant::now();
        self.missed = self.missed.saturating_add(1);
    }

    pub fn on_inbound(&mut self, payload: &[u8]) -> bool {
        let answer = self
            .keepalive
            .is_some_and(|keepalive| payload == keepalive.answer.as_bytes());
        let fixed_cadence = self
            .keepalive
            .is_some_and(|keepalive| keepalive.send_while_active);
        if !fixed_cadence || answer {
            self.silent_since = Instant::now();
            self.missed = 0;
        }
        answer
    }

    pub fn is_healthy(&self) -> bool {
        self.keepalive
            .is_none_or(|keepalive| self.missed <= keepalive.maximum_missed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEPALIVE: Keepalive = Keepalive {
        message: "ping",
        answer: "pong",
        interval: Duration::from_secs(20),
        maximum_missed: 1,
        send_while_active: false,
    };

    #[test]
    fn a_venue_that_needs_no_keepalive_never_sends_one() {
        let mut heartbeat = Heartbeat::start(None);

        assert_eq!(heartbeat.deadline(), None);
        assert_eq!(heartbeat.message(), None);
        assert!(!heartbeat.on_inbound(b"pong"));
        assert!(heartbeat.is_healthy());
    }

    #[test]
    fn any_inbound_frame_restarts_the_silence_window() {
        let mut heartbeat = Heartbeat::start(Some(KEEPALIVE));
        let armed = heartbeat.deadline().expect("a keepalive is armed");
        assert_eq!(heartbeat.message(), Some("ping"));

        assert!(!heartbeat.on_inbound(br#"{"arg":{"channel":"books"}}"#));

        assert!(
            heartbeat.deadline().expect("a keepalive is armed") >= armed,
            "an inbound frame should push the deadline out"
        );
    }

    #[test]
    fn a_fixed_cadence_heartbeat_is_not_reset_by_subscription_data() {
        const FIXED: Keepalive = Keepalive {
            message: "PING",
            answer: "PONG",
            interval: Duration::from_secs(10),
            maximum_missed: 1,
            send_while_active: true,
        };
        let mut heartbeat = Heartbeat::start(Some(FIXED));
        let armed = heartbeat.deadline().expect("a keepalive is armed");

        heartbeat.on_inbound(br#"{"event_type":"book"}"#);

        assert_eq!(
            heartbeat.deadline(),
            Some(armed),
            "subscription data must not postpone a fixed-cadence heartbeat"
        );

        heartbeat.on_message_sent();
        heartbeat.on_message_sent();
        assert!(!heartbeat.is_healthy());
        heartbeat.on_inbound(br#"{"event_type":"book"}"#);
        assert!(
            !heartbeat.is_healthy(),
            "subscription data must not answer the heartbeat"
        );
        assert!(heartbeat.on_inbound(b"PONG"));
        assert!(heartbeat.is_healthy());
    }

    #[test]
    fn the_answer_is_recognized_and_missing_answers_make_a_connection_dead() {
        let mut heartbeat = Heartbeat::start(Some(KEEPALIVE));
        assert!(heartbeat.on_inbound(b"pong"));

        heartbeat.on_message_sent();
        assert!(heartbeat.is_healthy(), "one missing answer is tolerated");

        heartbeat.on_message_sent();
        assert!(!heartbeat.is_healthy());

        assert!(heartbeat.on_inbound(b"pong"));
        assert!(heartbeat.is_healthy(), "an answer revives the connection");
    }
}
