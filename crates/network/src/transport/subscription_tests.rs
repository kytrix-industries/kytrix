use tokio::time::{Duration, timeout};

use crate::transport::{
    error::TransportError,
    subscription::{
        SubscriptionSendError, SubscriptionStatus, subscription_channel,
        subscription_channel_with_wake,
    },
};

#[tokio::test]
async fn slow_subscriber_should_fail_without_waiting_for_capacity() {
    let (publisher, mut subscription) = subscription_channel(1).unwrap();
    publisher.activate();
    publisher.try_send(1).unwrap();

    assert_eq!(
        publisher.try_send(2),
        Err(SubscriptionSendError::SlowSubscriber(2))
    );
    assert_eq!(
        subscription.recv().await,
        Err(TransportError::SlowSubscriber)
    );
    assert_eq!(subscription.recv().await, Err(TransportError::Closed));
}

#[tokio::test]
async fn slow_subscriber_should_not_affect_another_subscription() {
    let (slow_publisher, mut slow) = subscription_channel(1).unwrap();
    let (fast_publisher, mut fast) = subscription_channel(1).unwrap();
    slow_publisher.activate();
    fast_publisher.activate();
    slow_publisher.try_send(1).unwrap();

    assert!(matches!(
        slow_publisher.try_send(2),
        Err(SubscriptionSendError::SlowSubscriber(2))
    ));
    fast_publisher.try_send(3).unwrap();
    assert_eq!(fast.recv().await, Ok(3));
    assert_eq!(slow.recv().await, Err(TransportError::SlowSubscriber));
}

#[tokio::test]
async fn status_observer_should_report_lifecycle_transitions() {
    let (publisher, subscription) = subscription_channel::<usize>(1).unwrap();
    let mut status = subscription.observe_status();

    assert_eq!(*status.borrow(), SubscriptionStatus::Connecting);
    publisher.activate();
    status.changed().await.unwrap();
    assert_eq!(*status.borrow(), SubscriptionStatus::Active);
    publisher.reconnecting();
    status.changed().await.unwrap();
    assert_eq!(*status.borrow(), SubscriptionStatus::Reconnecting);
    publisher.fail(TransportError::RouteChanged);
    status.changed().await.unwrap();
    assert_eq!(
        *status.borrow(),
        SubscriptionStatus::Failed(TransportError::RouteChanged)
    );
}

#[tokio::test]
async fn close_should_wait_for_actor_cleanup() {
    let (publisher, subscription) = subscription_channel::<usize>(1).unwrap();
    publisher.activate();
    let actor = tokio::spawn(async move {
        publisher.cancelled().await;
        publisher.finish();
    });

    timeout(Duration::from_secs(1), subscription.close())
        .await
        .unwrap()
        .unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn dropping_subscription_should_cancel_its_publisher() {
    let (publisher, subscription) = subscription_channel::<usize>(1).unwrap();

    drop(subscription);

    timeout(Duration::from_secs(1), publisher.cancelled())
        .await
        .unwrap();
}

#[tokio::test]
async fn clean_finish_should_drain_buffered_values() {
    let (publisher, mut subscription) = subscription_channel(2).unwrap();
    publisher.activate();
    publisher.try_send(1).unwrap();
    publisher.try_send(2).unwrap();
    publisher.finish();
    drop(publisher);

    assert_eq!(subscription.recv().await, Ok(1));
    assert_eq!(subscription.recv().await, Ok(2));
    assert_eq!(subscription.recv().await, Err(TransportError::Closed));
}

#[test]
fn zero_subscription_capacity_should_be_rejected() {
    assert!(matches!(
        subscription_channel::<usize>(0),
        Err(TransportError::InvalidSubscriptionCapacity)
    ));
}

#[tokio::test]
async fn retiring_a_subscription_notifies_the_cleanup_wake() {
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let (publisher, _subscription) =
        subscription_channel_with_wake::<usize>(1, wake.clone()).unwrap();

    publisher.fail(TransportError::RouteChanged);

    assert!(
        timeout(Duration::from_secs(1), wake.notified())
            .await
            .is_ok()
    );
    assert!(publisher.is_cancelled());
}
