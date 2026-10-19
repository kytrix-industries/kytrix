//! The owner-local handoff from the interface thread to a background consumer.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::Notify;

/// The latest complete projection an owner's background consumer works from.
///
/// - `shared` always holds the latest projection; the request path and the
///   background consumer both read it.
/// - `notify` only wakes the consumer; it carries no value.
///
/// There is no pending slot: the value is in `shared`, and a consumer that is
/// woken reads it. Exactly one background consumer may
/// [`ConfigNotification::wait`]. `notify_one` wakes at most one waiter; when no
/// waiter is registered it leaves one permit (which is what makes a
/// publish-before-wait not lost).
pub struct ConfigNotification<P> {
    shared: Arc<RwLock<Arc<P>>>,
    notify: Arc<Notify>,
}

impl<P> Clone for ConfigNotification<P> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            notify: Arc::clone(&self.notify),
        }
    }
}

impl<P: Send + Sync + 'static> ConfigNotification<P> {
    /// Creates the notification over the projection the owner starts with.
    pub fn new(initial: Arc<P>) -> Self {
        Self {
            shared: Arc::new(RwLock::new(initial)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Replaces the shared projection and wakes the consumer.
    ///
    /// Crate-visible: the production caller is
    /// [`crate::events::ConfigEventBridge`]. A cross-crate test fake that drives
    /// an owner's state uses [`Self::publish_for_test`], which only exists with
    /// the `test-support` feature.
    pub(crate) fn publish(&self, projection: Arc<P>) {
        // The write must precede the wake: a woken consumer reads `current()`.
        *self.shared.write() = projection;
        self.notify.notify_one();
    }

    /// Test-only publish for fakes that drive an owner's state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn publish_for_test(&self, projection: Arc<P>) {
        self.publish(projection);
    }

    /// Returns the latest projection without waiting.
    pub fn current(&self) -> Arc<P> {
        self.shared.read().clone()
    }

    /// Waits for one publish, then returns the latest projection.
    ///
    /// Exactly one background consumer may call this. `notify_one` wakes at most
    /// one waiter; when no waiter is registered it leaves one permit (which is
    /// what makes a publish-before-wait not lost).
    ///
    /// A publish that happens between two `current` reads can only leave the
    /// shared value newer; the wake is idempotent.
    pub async fn wait(&self) -> Arc<P> {
        self.notify.notified().await;

        self.current()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::{
        sync::{mpsc, oneshot},
        time::timeout,
    };

    use super::ConfigNotification;

    #[derive(Debug, PartialEq, Eq)]
    struct Projection(u32);

    /// A bound long enough not to race the wake, short enough that a lost wake
    /// fails the test instead of hanging the suite.
    const WAIT_BOUND: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn wait_should_return_the_projection_that_woke_it() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));
        let (waiting_tx, waiting_rx) = oneshot::channel();
        let waiter = tokio::spawn({
            let notification = notification.clone();
            async move {
                waiting_tx
                    .send(())
                    .expect("the test should still be waiting for the waiter");
                notification.wait().await
            }
        });
        waiting_rx
            .await
            .expect("the waiter should say it is about to wait");

        notification.publish(Arc::new(Projection(2)));

        let projection = timeout(WAIT_BOUND, waiter)
            .await
            .expect("a publish should wake the waiter")
            .expect("the waiter task should finish");
        assert_eq!(projection, Arc::new(Projection(2)));
    }

    /// A wait that no publish answers must keep waiting: the notification has no
    /// answer of its own to hand back.
    #[tokio::test]
    async fn a_wait_should_not_return_without_a_publish() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));

        // This negative wait only has to outlive scheduling jitter; waiting
        // longer cannot strengthen the assertion because the wait must never
        // return at all.
        assert!(
            timeout(Duration::from_millis(75), notification.wait())
                .await
                .is_err(),
            "a wait must not return the current projection without a publish"
        );
    }

    #[tokio::test]
    async fn a_publish_before_a_wait_should_not_be_lost() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));

        notification.publish(Arc::new(Projection(2)));

        let projection = timeout(WAIT_BOUND, notification.wait())
            .await
            .expect("a publish before a wait should leave a permit for the wait");
        assert_eq!(projection, Arc::new(Projection(2)));
    }

    #[tokio::test]
    async fn publish_should_keep_only_the_latest_projection() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));
        let latest = Arc::new(Projection(3));

        notification.publish(Arc::new(Projection(2)));
        notification.publish(latest.clone());

        let projection = timeout(WAIT_BOUND, notification.wait())
            .await
            .expect("the publishes should leave a permit for the wait");
        assert_eq!(projection, latest);
        assert!(
            Arc::ptr_eq(&notification.current(), &latest),
            "the shared projection is the latest one, not an intermediate value"
        );
    }

    #[tokio::test]
    async fn current_should_always_answer_the_latest_projection() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));
        assert_eq!(notification.current(), Arc::new(Projection(1)));

        notification.publish(Arc::new(Projection(2)));
        assert_eq!(notification.current(), Arc::new(Projection(2)));

        let latest = Arc::new(Projection(3));
        notification.publish(latest.clone());
        assert!(
            Arc::ptr_eq(&notification.current(), &latest),
            "current answers the Arc that was published"
        );
    }

    #[tokio::test]
    async fn a_clone_should_observe_the_same_projection() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));
        let clone = notification.clone();
        assert!(Arc::ptr_eq(&notification.current(), &clone.current()));

        notification.publish(Arc::new(Projection(2)));

        assert_eq!(clone.current(), Arc::new(Projection(2)));
        assert_eq!(notification.current(), Arc::new(Projection(2)));
        assert!(
            Arc::ptr_eq(&notification.current(), &clone.current()),
            "the clone shares the same shared slot"
        );
    }

    /// A race test that fails a notify-before-store inversion with high
    /// probability, complementing (not replacing) the sequential-semantics tests
    /// above. On a multi-threaded runtime, moving `notify_one` before the shared
    /// write is caught with high probability because the woken consumer may read
    /// the slot before the store lands. The waiter is not provably registered
    /// when `publish` runs, so the guard is probabilistic: an independent
    /// reviewer measured 98/100 failing runs with 4 workers and 1000 rounds;
    /// 2/100 runs still passed.
    ///
    /// The correct implementation cannot fail it: the publisher waits for each
    /// round's acknowledgement before publishing the next value, so a consumer
    /// woken by a publish can only find that round's store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_woken_consumer_should_read_the_projection_that_woke_it() {
        const ROUNDS: u32 = 1000;

        let notification = ConfigNotification::new(Arc::new(Projection(0)));
        let (ready_tx, mut ready_rx) = mpsc::channel(1);
        let (observed_tx, mut observed_rx) = mpsc::channel(1);
        let waiter = tokio::spawn({
            let notification = notification.clone();
            async move {
                for _ in 0..ROUNDS {
                    ready_tx
                        .send(())
                        .await
                        .expect("the test should still be receiving readiness");
                    let projection = notification.wait().await;
                    observed_tx
                        .send(projection)
                        .await
                        .expect("the test should still be receiving observations");
                }
            }
        });

        for round in 1..=ROUNDS {
            ready_rx
                .recv()
                .await
                .expect("the waiter should be about to wait");
            notification.publish(Arc::new(Projection(round)));
            let observed = timeout(WAIT_BOUND, observed_rx.recv())
                .await
                .expect("the woken waiter should observe the publish")
                .expect("the waiter should report what it read");
            assert_eq!(
                observed,
                Arc::new(Projection(round)),
                "the waiter woken by round {round} must read that round's projection"
            );
        }

        timeout(WAIT_BOUND, waiter)
            .await
            .expect("the waiter should finish")
            .expect("the waiter task should not panic");
    }
}
