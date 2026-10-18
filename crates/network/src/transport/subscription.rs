//! Bounded logical subscriptions with observable lifecycle state.

use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::transport::error::TransportError;

/// Observable lifecycle state for a logical subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Connecting,
    Active,
    Reconnecting,
    Closing,
    Closed,
    Failed(TransportError),
}

impl SubscriptionStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Failed(_))
    }
}

struct SubscriptionState {
    cancellation: CancellationToken,
    status: watch::Sender<SubscriptionStatus>,
    reconnect_epoch: watch::Sender<u64>,
    cleanup_wake: Option<Arc<Notify>>,
}

impl SubscriptionState {
    fn transition(&self, next: SubscriptionStatus) {
        self.status.send_if_modified(|current| {
            if current.is_terminal() || *current == next {
                return false;
            }
            *current = next.clone();
            true
        });
    }

    fn reconnecting(&self) {
        let epoch = self.reconnect_epoch.borrow().wrapping_add(1).max(1);
        self.reconnect_epoch.send_replace(epoch);
        self.transition(SubscriptionStatus::Reconnecting);
    }

    fn terminate(&self, next: SubscriptionStatus) {
        self.transition(next);
        self.cancellation.cancel();
        self.notify_cleanup();
    }

    fn cancel(&self) {
        self.cancellation.cancel();
        self.notify_cleanup();
    }

    fn notify_cleanup(&self) {
        if let Some(wake) = &self.cleanup_wake {
            wake.notify_one();
        }
    }
}

/// The actor-facing half of a bounded logical subscription.
pub struct SubscriptionPublisher<T> {
    sender: mpsc::Sender<T>,
    state: Arc<SubscriptionState>,
}

impl<T> SubscriptionPublisher<T> {
    pub fn activate(&self) {
        self.state.transition(SubscriptionStatus::Active);
    }

    pub fn reconnecting(&self) {
        self.state.reconnecting();
    }

    /// Publishes without waiting on a slow consumer.
    ///
    /// A full queue atomically retires this logical subscriber; it does not
    /// block or affect any other subscriber on the physical session.
    pub fn try_send(&self, value: T) -> Result<(), SubscriptionSendError<T>> {
        if self.state.cancellation.is_cancelled() {
            return Err(SubscriptionSendError::Closed(value));
        }
        match self.sender.try_send(value) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(value)) => {
                self.state
                    .terminate(SubscriptionStatus::Failed(TransportError::SlowSubscriber));
                Err(SubscriptionSendError::SlowSubscriber(value))
            }
            Err(mpsc::error::TrySendError::Closed(value)) => {
                self.state.terminate(SubscriptionStatus::Closed);
                Err(SubscriptionSendError::Closed(value))
            }
        }
    }

    pub fn fail(&self, error: TransportError) {
        self.state.terminate(SubscriptionStatus::Failed(error));
    }

    pub fn finish(&self) {
        self.state.terminate(SubscriptionStatus::Closed);
    }

    pub async fn cancelled(&self) {
        self.state.cancellation.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancellation.is_cancelled()
    }
}

impl<T> Drop for SubscriptionPublisher<T> {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The reason a non-blocking subscription publish failed.
#[derive(Debug, PartialEq, Eq)]
pub enum SubscriptionSendError<T> {
    SlowSubscriber(T),
    Closed(T),
}

/// A cancellation-aware, bounded stream of typed subscription values.
pub struct Subscription<T> {
    receiver: mpsc::Receiver<T>,
    state: Arc<SubscriptionState>,
    status: watch::Receiver<SubscriptionStatus>,
    terminal_delivered: bool,
}

/// Creates the actor and consumer halves of a bounded logical subscription.
pub fn subscription_channel<T>(
    capacity: usize,
) -> Result<(SubscriptionPublisher<T>, Subscription<T>), TransportError> {
    subscription_channel_inner(capacity, None)
}

/// Creates a bounded subscription whose consumer wakes a cleanup task when it
/// is retired.
pub fn subscription_channel_with_wake<T>(
    capacity: usize,
    cleanup_wake: Arc<Notify>,
) -> Result<(SubscriptionPublisher<T>, Subscription<T>), TransportError> {
    subscription_channel_inner(capacity, Some(cleanup_wake))
}

fn subscription_channel_inner<T>(
    capacity: usize,
    cleanup_wake: Option<Arc<Notify>>,
) -> Result<(SubscriptionPublisher<T>, Subscription<T>), TransportError> {
    if capacity == 0 {
        return Err(TransportError::InvalidSubscriptionCapacity);
    }
    let (sender, receiver) = mpsc::channel(capacity);
    let (status_sender, status) = watch::channel(SubscriptionStatus::Connecting);
    let (reconnect_epoch_sender, _) = watch::channel(0_u64);
    let state = Arc::new(SubscriptionState {
        cancellation: CancellationToken::new(),
        status: status_sender,
        reconnect_epoch: reconnect_epoch_sender,
        cleanup_wake,
    });
    Ok((
        SubscriptionPublisher {
            sender,
            state: Arc::clone(&state),
        },
        Subscription {
            receiver,
            state,
            status,
            terminal_delivered: false,
        },
    ))
}

impl<T> Subscription<T> {
    /// Receives the next value or the subscription's typed terminal error.
    pub async fn recv(&mut self) -> Result<T, TransportError> {
        loop {
            match self.status().clone() {
                SubscriptionStatus::Failed(error) => return Err(self.terminal_error(error)),
                SubscriptionStatus::Closed => {
                    return self
                        .receiver
                        .try_recv()
                        .map_err(|_| self.terminal_error(TransportError::Closed));
                }
                SubscriptionStatus::Connecting
                | SubscriptionStatus::Active
                | SubscriptionStatus::Reconnecting
                | SubscriptionStatus::Closing => {}
            }

            tokio::select! {
                biased;
                changed = self.status.changed() => {
                    if changed.is_err() {
                        return Err(self.terminal_error(TransportError::Closed));
                    }
                }
                value = self.receiver.recv() => {
                    if let Some(value) = value {
                        return Ok(value);
                    }
                    return Err(self.terminal_error(TransportError::Closed));
                }
            }
        }
    }

    /// Cancels the subscription and waits for its actor to finish cleanup.
    pub async fn close(mut self) -> Result<(), TransportError> {
        self.receiver.close();
        self.state.transition(SubscriptionStatus::Closing);
        self.state.cancel();
        loop {
            match self.status().clone() {
                SubscriptionStatus::Closed => return Ok(()),
                SubscriptionStatus::Failed(error) => return Err(error),
                SubscriptionStatus::Connecting
                | SubscriptionStatus::Active
                | SubscriptionStatus::Reconnecting
                | SubscriptionStatus::Closing => {}
            }
            if self.status.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    pub fn status(&self) -> SubscriptionStatus {
        self.status.borrow().clone()
    }

    pub fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.clone()
    }

    /// Observes the monotonically increasing physical-session reconnect epoch.
    pub fn observe_reconnect_epoch(&self) -> watch::Receiver<u64> {
        self.state.reconnect_epoch.subscribe()
    }

    /// Takes an already-published value without awaiting, for callers asserting
    /// synchronous publish and replay semantics.
    #[cfg(feature = "test-support")]
    pub fn try_recv_for_test(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    fn terminal_error(&mut self, error: TransportError) -> TransportError {
        if self.terminal_delivered {
            TransportError::Closed
        } else {
            self.terminal_delivered = true;
            error
        }
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.receiver.close();
        self.state.cancel();
    }
}
