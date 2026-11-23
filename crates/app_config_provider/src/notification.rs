use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::Notify;

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
    pub fn new(initial: Arc<P>) -> Self {
        Self {
            shared: Arc::new(RwLock::new(initial)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn publish(&self, projection: Arc<P>) {
        *self.shared.write() = projection;
        self.notify.notify_one();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn publish_for_test(&self, projection: Arc<P>) {
        self.publish(projection);
    }

    pub fn current(&self) -> Arc<P> {
        self.shared.read().clone()
    }

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

    #[tokio::test]
    async fn a_wait_should_not_return_without_a_publish() {
        let notification = ConfigNotification::new(Arc::new(Projection(1)));

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
