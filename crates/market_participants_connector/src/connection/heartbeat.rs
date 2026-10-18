//! Keepalive for a venue that closes a connection which stops carrying data.

use std::time::{Duration, Instant};

/// How one venue keeps an otherwise silent connection open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    /// The message sent when the connection has been silent for `interval`.
    pub message: &'static str,
    /// What the venue answers the message with. The answer is liveness rather than
    /// subscription data, so it is never published as an event.
    pub answer: &'static str,
    /// How long the connection may stay silent before the message is sent. It has to be
    /// shorter than the venue's own silence window.
    pub interval: Duration,
    /// How many answers may go missing in a row before the connection is treated as dead.
    pub maximum_missed: u32,
}

/// One connection's keepalive timer.
///
/// The timer restarts on every inbound frame, so a connection carrying a live
/// subscription stays silent: the message is sent only once the venue's silence window is
/// the thing about to close it.
#[derive(Clone, Debug)]
pub struct Heartbeat {
    /// `None` for a venue that keeps a silent connection open on its own.
    keepalive: Option<Keepalive>,
    silent_since: Instant,
    missed: u32,
}

impl Heartbeat {
    /// Starts the timer of a connection that has just been established.
    pub fn start(keepalive: Option<Keepalive>) -> Self {
        Self {
            keepalive,
            silent_since: Instant::now(),
            missed: 0,
        }
    }

    /// When the next keepalive message is due, or `None` when the venue needs none.
    pub fn deadline(&self) -> Option<Instant> {
        self.keepalive
            .map(|keepalive| self.silent_since + keepalive.interval)
    }

    /// The message that keeps this connection open, if the venue needs one.
    pub fn message(&self) -> Option<&'static str> {
        self.keepalive.map(|keepalive| keepalive.message)
    }

    /// Records that the message was sent, which is what the venue has to answer.
    pub fn on_message_sent(&mut self) {
        self.silent_since = Instant::now();
        self.missed = self.missed.saturating_add(1);
    }

    /// Records an inbound frame and reports whether it was the venue's answer.
    ///
    /// Any frame restarts the timer, because any frame proves the connection is carrying
    /// traffic; only the answer says the venue replied to the keepalive message itself.
    pub fn on_inbound(&mut self, payload: &[u8]) -> bool {
        self.silent_since = Instant::now();
        self.missed = 0;
        self.keepalive
            .is_some_and(|keepalive| payload == keepalive.answer.as_bytes())
    }

    /// Whether the venue has answered recently enough to keep this connection.
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
