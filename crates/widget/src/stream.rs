use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use ahash::{HashMap, HashMapExt};
use thiserror::Error;
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use super::protocol::{StreamEnvelope, StreamGeneration, WidgetProtocol};

/// Bounded lossless receiver that rejects messages from retired generations.
pub struct ReliableStream<P: WidgetProtocol> {
    receiver: mpsc::Receiver<ReliableItem<P>>,
    generations: DeliveryRegistry<P>,
}

type LatestState<P> = HashMap<
    <P as WidgetProtocol>::StreamKey,
    LatestEntry<<P as WidgetProtocol>::StreamKey, <P as WidgetProtocol>::StreamMessage>,
>;

type LatestStaged<P> = (
    <P as WidgetProtocol>::StreamKey,
    StreamEnvelope<<P as WidgetProtocol>::StreamKey, <P as WidgetProtocol>::StreamMessage>,
    (StreamGeneration, u64),
);

type DeliveryRegistry<P> =
    Arc<std::sync::RwLock<HashMap<<P as WidgetProtocol>::StreamKey, DeliveryGeneration>>>;

/// Receiver that yields each key's newest unseen snapshot without queue growth.
pub struct LatestStream<P: WidgetProtocol> {
    receiver: watch::Receiver<LatestState<P>>,
    seen: HashMap<P::StreamKey, (StreamGeneration, u64)>,
    pending: VecDeque<StreamEnvelope<P::StreamKey, P::StreamMessage>>,
    // Reuse the staging allocation across watch updates. Latest streams may carry many
    // independent keys (for example a watch list), but only the newest envelope per key is
    // retained. Keeping this scratch storage avoids a heap allocation on every changed key.
    staging: Vec<LatestStaged<P>>,
}

pub struct StreamChannels<P: WidgetProtocol> {
    reliable_sender: mpsc::Sender<ReliableItem<P>>,
    latest_sender: watch::Sender<LatestState<P>>,
    generations: DeliveryRegistry<P>,
    key_capacity: usize,
}

impl<P: WidgetProtocol> Clone for StreamChannels<P> {
    fn clone(&self) -> Self {
        Self {
            reliable_sender: self.reliable_sender.clone(),
            latest_sender: self.latest_sender.clone(),
            generations: self.generations.clone(),
            key_capacity: self.key_capacity,
        }
    }
}

pub fn channels<P: WidgetProtocol>(
    reliable_capacity: NonZeroUsize,
) -> (StreamChannels<P>, ReliableStream<P>, LatestStream<P>) {
    let reliable_capacity = reliable_capacity.get();
    let (reliable_sender, reliable_receiver) = mpsc::channel(reliable_capacity);
    let (latest_sender, latest_receiver) =
        watch::channel(HashMap::with_capacity(reliable_capacity));
    let generations = Arc::new(std::sync::RwLock::new(HashMap::with_capacity(
        reliable_capacity,
    )));

    (
        StreamChannels {
            reliable_sender,
            latest_sender,
            generations: generations.clone(),
            key_capacity: reliable_capacity,
        },
        ReliableStream {
            receiver: reliable_receiver,
            generations,
        },
        LatestStream {
            receiver: latest_receiver,
            seen: HashMap::with_capacity(reliable_capacity),
            pending: VecDeque::with_capacity(reliable_capacity),
            staging: Vec::with_capacity(reliable_capacity),
        },
    )
}

impl<P: WidgetProtocol> ReliableStream<P> {
    pub async fn recv(&mut self) -> Option<StreamEnvelope<P::StreamKey, P::StreamMessage>> {
        loop {
            let item = self.receiver.recv().await?;
            let generations = match self.generations.read() {
                Ok(generations) => generations,
                Err(poisoned) => poisoned.into_inner(),
            };
            let is_current = generations.get(&item.key).is_some_and(|current| {
                current.generation == item.generation
                    && Arc::ptr_eq(&current.delivery, &item.delivery)
                    && !current.delivery.retired.load(Ordering::Acquire)
            });
            drop(generations);
            if is_current {
                return item.into_envelope();
            }
        }
    }
}

impl<P: WidgetProtocol> LatestStream<P> {
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        loop {
            if !self.pending.is_empty() || self.has_unseen() {
                return Ok(());
            }
            self.receiver.changed().await?;
        }
    }

    /// Returns one unseen snapshot, preferring the oldest generation and sequence.
    pub fn next_unseen(&mut self) -> Option<StreamEnvelope<P::StreamKey, P::StreamMessage>> {
        while let Some(envelope) = self.pending.pop_front() {
            let is_current = self
                .receiver
                .borrow()
                .get(envelope.key())
                .is_some_and(|entry| {
                    !entry.cancellation.is_cancelled()
                        && entry.generation == envelope.generation()
                        && entry
                            .envelope
                            .as_ref()
                            .is_some_and(|current| current.sequence() == envelope.sequence())
                });
            if is_current {
                return Some(envelope);
            }
        }

        self.staging.clear();
        let state = self.receiver.borrow_and_update();
        self.seen.retain(|key, _| {
            state
                .get(key)
                .is_some_and(|entry| !entry.cancellation.is_cancelled())
        });
        for (key, entry) in state.iter() {
            if entry.cancellation.is_cancelled() {
                continue;
            }
            let Some(envelope) = entry.envelope.as_ref() else {
                continue;
            };
            let version = (envelope.generation(), envelope.sequence());
            if self.seen.get(key).copied() != Some(version) {
                self.staging.push((key.clone(), envelope.clone(), version));
            }
        }
        drop(state);

        if self.staging.len() > 1 {
            self.staging
                .sort_unstable_by_key(|(_, _, version)| *version);
        }
        for (key, envelope, version) in self.staging.drain(..) {
            self.seen.insert(key, version);
            self.pending.push_back(envelope);
        }
        self.pending.pop_front()
    }

    fn has_unseen(&self) -> bool {
        self.receiver.borrow().iter().any(|(key, entry)| {
            !entry.cancellation.is_cancelled()
                && entry.envelope.as_ref().is_some_and(|envelope| {
                    self.seen.get(key).copied()
                        != Some((envelope.generation(), envelope.sequence()))
                })
        })
    }
}

impl<P: WidgetProtocol> Clone for LatestStream<P> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
            seen: self.seen.clone(),
            pending: self.pending.clone(),
            staging: Vec::with_capacity(self.staging.capacity()),
        }
    }
}

/// Publisher scoped to one stream key and immutable generation.
pub struct StreamPublisher<P: WidgetProtocol> {
    key: P::StreamKey,
    generation: StreamGeneration,
    cancellation: CancellationToken,
    channels: StreamChannels<P>,
    reliable_delivery: Arc<DeliveryState>,
    reliable_sequence: Arc<AtomicU64>,
    latest_sequence: Arc<AtomicU64>,
}

impl<P: WidgetProtocol> StreamPublisher<P> {
    pub const fn generation(&self) -> StreamGeneration {
        self.generation
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// Waits for cancellation by value, so a borrow of the publisher is not held
    /// across the wait and the publisher stays usable inside the same `select!`.
    pub async fn cancelled_owned(self) {
        self.cancellation.cancelled().await;
    }

    /// Publishes a message without loss, waiting for bounded-channel capacity.
    pub async fn publish_reliable(
        &self,
        message: P::StreamMessage,
    ) -> Result<(), StreamPublishError> {
        self.publish_reliable_arc(Arc::new(message)).await
    }

    pub async fn publish_reliable_arc(
        &self,
        message: Arc<P::StreamMessage>,
    ) -> Result<(), StreamPublishError> {
        if self.cancellation.is_cancelled() {
            return Err(StreamPublishError::Cancelled);
        }

        let sequence = self.reliable_sequence.fetch_add(1, Ordering::Relaxed);
        let envelope = StreamEnvelope::new(self.key.clone(), self.generation, sequence, message);
        let item = ReliableItem::new(
            envelope,
            self.reliable_delivery.clone(),
            self.channels.generations.clone(),
        );

        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(StreamPublishError::Cancelled),
            result = self.channels.reliable_sender.send(item) => {
                result.map_err(|_| StreamPublishError::ReceiverClosed)
            }
        }
    }

    /// Replaces the current latest-value snapshot without queue growth.
    pub fn publish_latest(&self, message: P::StreamMessage) -> Result<(), StreamPublishError> {
        self.publish_latest_arc(Arc::new(message))
    }

    pub fn publish_latest_arc(
        &self,
        message: Arc<P::StreamMessage>,
    ) -> Result<(), StreamPublishError> {
        let sequence = self.latest_sequence.fetch_add(1, Ordering::Relaxed);
        let mut published = false;
        let envelope = StreamEnvelope::new(self.key.clone(), self.generation, sequence, message);
        self.channels.latest_sender.send_if_modified(|state| {
            let Some(entry) = state.get_mut(&self.key) else {
                return false;
            };
            if entry.generation != self.generation
                || entry.cancellation.is_cancelled()
                || self.cancellation.is_cancelled()
            {
                return false;
            }
            entry.envelope = Some(envelope);
            published = true;
            true
        });
        published.then_some(()).ok_or(StreamPublishError::Cancelled)
    }
}

impl<P: WidgetProtocol> Clone for StreamPublisher<P> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            generation: self.generation,
            cancellation: self.cancellation.clone(),
            channels: self.channels.clone(),
            reliable_delivery: self.reliable_delivery.clone(),
            reliable_sequence: self.reliable_sequence.clone(),
            latest_sequence: self.latest_sequence.clone(),
        }
    }
}

impl<P: WidgetProtocol> fmt::Debug for StreamPublisher<P>
where
    P::StreamKey: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamPublisher")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum StreamPublishError {
    #[error("stream generation was cancelled")]
    Cancelled,
    #[error("widget stopped receiving reliable stream messages")]
    ReceiverClosed,
}

pub struct StreamSupervisor<P: WidgetProtocol> {
    channels: StreamChannels<P>,
    root_cancellation: CancellationToken,
    next_generation: u64,
    active: HashMap<P::StreamKey, ActiveStream>,
    tasks: JoinSet<(P::StreamKey, StreamGeneration, bool)>,
    drain: TaskDrain,
}

impl<P: WidgetProtocol> StreamSupervisor<P> {
    pub fn new(
        channels: StreamChannels<P>,
        root_cancellation: CancellationToken,
        drain: TaskDrain,
    ) -> Self {
        let key_capacity = channels.key_capacity;
        Self {
            channels,
            root_cancellation,
            next_generation: 1,
            active: HashMap::with_capacity(key_capacity),
            tasks: JoinSet::new(),
            drain,
        }
    }

    pub fn replace<F, Fut>(&mut self, key: P::StreamKey, task: F) -> StreamGeneration
    where
        F: FnOnce(StreamPublisher<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.reap_finished();
        self.cancel(&key);

        self.start(key, task)
    }

    pub fn start<F, Fut>(&mut self, key: P::StreamKey, task: F) -> StreamGeneration
    where
        F: FnOnce(StreamPublisher<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.reap_finished();
        if let Some(active) = self.active.get(&key) {
            return active.generation;
        }

        let generation = StreamGeneration::new(self.next_generation);
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let cancellation = self.root_cancellation.child_token();
        let reliable_delivery = Arc::new(DeliveryState::default());
        let mut generations = match self.channels.generations.write() {
            Ok(generations) => generations,
            Err(poisoned) => poisoned.into_inner(),
        };
        generations.insert(
            key.clone(),
            DeliveryGeneration {
                generation,
                delivery: reliable_delivery.clone(),
            },
        );
        drop(generations);
        self.active.insert(
            key.clone(),
            ActiveStream {
                generation,
                cancellation: cancellation.clone(),
            },
        );
        self.channels.latest_sender.send_if_modified(|state| {
            state.insert(
                key.clone(),
                LatestEntry {
                    generation,
                    cancellation: cancellation.clone(),
                    envelope: None,
                },
            );
            true
        });

        let task_key = key.clone();
        let latest_sender = self.channels.latest_sender.clone();
        let publisher = StreamPublisher {
            key,
            generation,
            cancellation: cancellation.clone(),
            channels: self.channels.clone(),
            reliable_delivery: reliable_delivery.clone(),
            reliable_sequence: Arc::new(AtomicU64::new(1)),
            latest_sequence: Arc::new(AtomicU64::new(1)),
        };
        let task_guard = self.drain.track();
        let user_task_guard = self.drain.track();
        let generations = self.channels.generations.clone();
        self.tasks.spawn(async move {
            let _task_guard = task_guard;
            let mut user_task = tokio::spawn(async move {
                let _user_task_guard = user_task_guard;
                task(publisher).await;
            });
            let _abort_on_drop = AbortTaskOnDrop(user_task.abort_handle());
            let completed_naturally = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    user_task.abort();
                    let _ = user_task.await;
                    false
                }
                result = &mut user_task => {
                    if let Err(error) = result {
                        log::error!("widget stream task failed: {error}");
                    }
                    true
                }
            };
            cancellation.cancel();
            if completed_naturally {
                reliable_delivery.completed.store(true, Ordering::Release);
                cleanup_completed_delivery::<P>(
                    &generations,
                    &task_key,
                    generation,
                    &reliable_delivery,
                );
            } else {
                retire_delivery::<P>(
                    &generations,
                    &task_key,
                    Some(generation),
                    Some(&reliable_delivery),
                );
            }
            latest_sender.send_if_modified(|state| {
                let should_remove = state
                    .get(&task_key)
                    .is_some_and(|entry| entry.generation == generation);
                should_remove && state.remove(&task_key).is_some()
            });
            (task_key, generation, completed_naturally)
        });

        generation
    }

    pub fn cancel(&mut self, key: &P::StreamKey) -> bool {
        let generation = self.active.remove(key).map(|active| {
            active.cancellation.cancel();
            active.generation
        });
        let removed_snapshot = self.channels.latest_sender.send_if_modified(|state| {
            let should_remove = generation.map_or_else(
                || state.contains_key(key),
                |generation| {
                    state
                        .get(key)
                        .is_some_and(|entry| entry.generation == generation)
                },
            );
            should_remove && state.remove(key).is_some()
        });
        self.retire_generation(key, generation);
        generation.is_some() || removed_snapshot
    }

    pub fn generation(&self, key: &P::StreamKey) -> Option<StreamGeneration> {
        self.active
            .get(key)
            .filter(|active| !active.cancellation.is_cancelled())
            .map(|active| active.generation)
    }

    pub fn cancel_all(&mut self) {
        for (_, active) in self.active.drain() {
            active.cancellation.cancel();
        }
        self.channels.latest_sender.send_if_modified(|state| {
            !state.is_empty() && {
                state.clear();
                true
            }
        });
        let mut generations = match self.channels.generations.write() {
            Ok(generations) => generations,
            Err(poisoned) => poisoned.into_inner(),
        };
        for generation in generations.values() {
            generation.delivery.retired.store(true, Ordering::Release);
        }
        generations.clear();
    }

    fn reap_finished(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            match result {
                Ok((key, generation, completed_naturally))
                    if self
                        .active
                        .get(&key)
                        .is_some_and(|active| active.generation == generation) =>
                {
                    self.active.remove(&key);
                    if !completed_naturally {
                        self.retire_generation(&key, Some(generation));
                    }
                }
                Ok(_) => {}
                Err(error) => log::error!("widget stream task failed: {error}"),
            }
        }
    }

    fn retire_generation(&self, key: &P::StreamKey, generation: Option<StreamGeneration>) {
        let mut generations = match self.channels.generations.write() {
            Ok(generations) => generations,
            Err(poisoned) => poisoned.into_inner(),
        };
        let should_remove = generation.map_or_else(
            || generations.contains_key(key),
            |generation| {
                generations
                    .get(key)
                    .is_some_and(|delivery| delivery.generation == generation)
            },
        );
        if should_remove && let Some(delivery) = generations.remove(key) {
            delivery.delivery.retired.store(true, Ordering::Release);
        }
    }
}

impl<P: WidgetProtocol> Drop for StreamSupervisor<P> {
    fn drop(&mut self) {
        self.cancel_all();
        self.tasks.abort_all();
    }
}

struct ActiveStream {
    generation: StreamGeneration,
    cancellation: CancellationToken,
}

struct DeliveryGeneration {
    generation: StreamGeneration,
    delivery: Arc<DeliveryState>,
}

#[derive(Default)]
struct DeliveryState {
    retired: AtomicBool,
    completed: AtomicBool,
    queued: std::sync::atomic::AtomicUsize,
}

struct ReliableItem<P: WidgetProtocol> {
    envelope: Option<StreamEnvelope<P::StreamKey, P::StreamMessage>>,
    key: P::StreamKey,
    generation: StreamGeneration,
    delivery: Arc<DeliveryState>,
    generations: DeliveryRegistry<P>,
}

impl<P: WidgetProtocol> ReliableItem<P> {
    fn new(
        envelope: StreamEnvelope<P::StreamKey, P::StreamMessage>,
        delivery: Arc<DeliveryState>,
        generations: DeliveryRegistry<P>,
    ) -> Self {
        delivery.queued.fetch_add(1, Ordering::AcqRel);
        Self {
            key: envelope.key().clone(),
            generation: envelope.generation(),
            envelope: Some(envelope),
            delivery,
            generations,
        }
    }

    fn into_envelope(mut self) -> Option<StreamEnvelope<P::StreamKey, P::StreamMessage>> {
        self.envelope.take()
    }
}

impl<P: WidgetProtocol> Drop for ReliableItem<P> {
    fn drop(&mut self) {
        if self.delivery.queued.fetch_sub(1, Ordering::AcqRel) == 1
            && self.delivery.completed.load(Ordering::Acquire)
        {
            cleanup_completed_delivery::<P>(
                &self.generations,
                &self.key,
                self.generation,
                &self.delivery,
            );
        }
    }
}

fn cleanup_completed_delivery<P: WidgetProtocol>(
    generations: &DeliveryRegistry<P>,
    key: &P::StreamKey,
    generation: StreamGeneration,
    delivery: &Arc<DeliveryState>,
) {
    if !delivery.completed.load(Ordering::Acquire) || delivery.queued.load(Ordering::Acquire) != 0 {
        return;
    }

    let mut generations = match generations.write() {
        Ok(generations) => generations,
        Err(poisoned) => poisoned.into_inner(),
    };
    let should_remove = generations.get(key).is_some_and(|current| {
        current.generation == generation && Arc::ptr_eq(&current.delivery, delivery)
    });
    if should_remove {
        generations.remove(key);
    }
}

fn retire_delivery<P: WidgetProtocol>(
    generations: &DeliveryRegistry<P>,
    key: &P::StreamKey,
    generation: Option<StreamGeneration>,
    expected: Option<&Arc<DeliveryState>>,
) {
    let mut generations = match generations.write() {
        Ok(generations) => generations,
        Err(poisoned) => poisoned.into_inner(),
    };
    let should_remove = generations.get(key).is_some_and(|current| {
        generation.is_none_or(|generation| current.generation == generation)
            && expected.is_none_or(|delivery| Arc::ptr_eq(&current.delivery, delivery))
    });
    if should_remove && let Some(current) = generations.remove(key) {
        current.delivery.retired.store(true, Ordering::Release);
    }
}

#[derive(Clone, Default)]
pub struct TaskDrain {
    inner: Arc<TaskDrainInner>,
}

impl TaskDrain {
    fn track(&self) -> TaskGuard {
        self.inner.active.fetch_add(1, Ordering::AcqRel);
        TaskGuard(self.clone())
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.inner.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct TaskDrainInner {
    active: std::sync::atomic::AtomicUsize,
    notify: Notify,
}

struct TaskGuard(TaskDrain);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.0.inner.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.inner.notify.notify_waiters();
        }
    }
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct LatestEntry<K, M> {
    generation: StreamGeneration,
    cancellation: CancellationToken,
    envelope: Option<StreamEnvelope<K, M>>,
}

impl<K: Clone, M> Clone for LatestEntry<K, M> {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            cancellation: self.cancellation.clone(),
            envelope: self.envelope.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, time::Duration};

    use tokio::{sync::oneshot, time::timeout};

    use super::*;

    struct TestProtocol;

    impl WidgetProtocol for TestProtocol {
        type Request = ();
        type Response = ();
        type StreamKey = &'static str;
        type StreamMessage = u64;
        type Error = Infallible;
    }

    #[tokio::test]
    async fn latest_stream_should_preserve_one_snapshot_per_key() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (alpha_published, alpha_done) = oneshot::channel();
        let (beta_published, beta_done) = oneshot::channel();

        supervisor.replace("alpha", |publisher| async move {
            let _ = publisher.publish_latest(10);
            let _ = alpha_published.send(());
            publisher.cancelled().await;
        });
        supervisor.replace("beta", |publisher| async move {
            let _ = publisher.publish_latest(20);
            let _ = beta_published.send(());
            publisher.cancelled().await;
        });
        alpha_done.await.expect("alpha stream should publish");
        beta_done.await.expect("beta stream should publish");
        latest.changed().await.expect("latest state should change");

        let mut snapshots = HashMap::with_capacity(2);
        while let Some(envelope) = latest.next_unseen() {
            snapshots.insert(*envelope.key(), *envelope.message().as_ref());
        }

        let expected: HashMap<_, _> = [("alpha", 10), ("beta", 20)].into_iter().collect();
        assert_eq!(snapshots, expected);
    }

    #[tokio::test]
    async fn changed_should_return_while_a_staged_key_remains_unread() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());

        supervisor.replace("alpha", |publisher| async move {
            let _ = publisher.publish_latest(10);
            publisher.cancelled().await;
        });
        supervisor.replace("beta", |publisher| async move {
            let _ = publisher.publish_latest(20);
            publisher.cancelled().await;
        });
        latest.changed().await.expect("latest state should change");
        let first = latest.next_unseen().expect("first key should be staged");

        timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("second staged key should not wait for a new watch version")
            .expect("latest state should remain open");
        let second = latest.next_unseen().expect("second key should be staged");

        assert_ne!(first.key(), second.key());
    }

    #[tokio::test]
    async fn latest_stream_should_reuse_staging_capacity_between_batches() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (alpha_sender, alpha_receiver) = oneshot::channel();
        let (beta_sender, beta_receiver) = oneshot::channel();

        supervisor.replace("alpha", |publisher| async move {
            let _ = alpha_sender.send(publisher.clone());
            publisher.cancelled().await;
        });
        supervisor.replace("beta", |publisher| async move {
            let _ = beta_sender.send(publisher.clone());
            publisher.cancelled().await;
        });
        let alpha = alpha_receiver.await.expect("alpha publisher should start");
        let beta = beta_receiver.await.expect("beta publisher should start");

        alpha
            .publish_latest(1)
            .expect("alpha snapshot should publish");
        beta.publish_latest(2)
            .expect("beta snapshot should publish");
        latest.changed().await.expect("first batch should arrive");
        while latest.next_unseen().is_some() {}
        let capacity = latest.staging.capacity();
        assert!(capacity >= 2);

        alpha
            .publish_latest(3)
            .expect("alpha update should publish");
        beta.publish_latest(4).expect("beta update should publish");
        latest.changed().await.expect("second batch should arrive");
        while latest.next_unseen().is_some() {}

        assert_eq!(latest.staging.capacity(), capacity);
    }

    #[tokio::test]
    async fn latest_stream_should_prune_seen_keys_after_stream_retirement() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (publisher_sender, publisher_receiver) = oneshot::channel();

        supervisor.replace("alpha", |publisher| async move {
            let _ = publisher_sender.send(publisher.clone());
            publisher.cancelled().await;
        });
        let publisher = timeout(Duration::from_secs(1), publisher_receiver)
            .await
            .expect("alpha publisher should start promptly")
            .expect("alpha publisher channel should stay open");
        publisher
            .publish_latest(1)
            .expect("alpha snapshot should publish");
        timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("initial snapshot should arrive promptly")
            .expect("latest state should stay open");
        assert!(latest.next_unseen().is_some());
        assert_eq!(latest.seen.len(), 1);

        assert!(supervisor.cancel(&"alpha"));
        assert!(latest.next_unseen().is_none());
        assert!(latest.seen.is_empty());
    }

    #[tokio::test]
    async fn replaced_generation_should_reject_an_escaped_old_publisher() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (old_publisher, old_receiver) = oneshot::channel();

        supervisor.replace("prices", |publisher| async move {
            let _ = old_publisher.send(publisher.clone());
            publisher.cancelled().await;
        });
        let old_publisher = old_receiver.await.expect("old publisher should start");
        let (new_publisher, new_receiver) = oneshot::channel();
        supervisor.replace("prices", |publisher| async move {
            let _ = new_publisher.send(publisher.clone());
            publisher.cancelled().await;
        });
        let new_publisher = new_receiver.await.expect("new publisher should start");

        assert_eq!(
            old_publisher.publish_latest(10),
            Err(StreamPublishError::Cancelled)
        );
        new_publisher
            .publish_latest(20)
            .expect("active generation should publish");
        latest.changed().await.expect("latest state should change");
        let envelope = latest.next_unseen().expect("new snapshot should exist");

        assert_eq!(*envelope.message().as_ref(), 20);
        assert_eq!(envelope.generation(), new_publisher.generation());
    }

    #[tokio::test]
    async fn naturally_completed_stream_should_retire_publisher_and_latest_snapshot() {
        let (channels, mut reliable, mut latest) =
            channels::<TestProtocol>(NonZeroUsize::new(2).expect("test capacity is non-zero"));
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (publisher_sender, publisher_receiver) = oneshot::channel();
        let (finish_sender, finish_receiver) = oneshot::channel();

        supervisor.replace("prices", |publisher| async move {
            publisher
                .publish_latest(10)
                .expect("active generation should publish");
            publisher
                .publish_reliable(30)
                .await
                .expect("active generation should publish a final event");
            let _ = publisher_sender.send(publisher.clone());
            let _ = finish_receiver.await;
        });
        let publisher = timeout(Duration::from_secs(1), publisher_receiver)
            .await
            .expect("stream publisher should escape promptly")
            .expect("stream publisher should escape");
        assert!(
            latest
                .receiver
                .borrow()
                .get("prices")
                .is_some_and(|entry| entry.envelope.is_some())
        );

        finish_sender
            .send(())
            .expect("stream task should still be running");
        timeout(Duration::from_secs(1), async {
            loop {
                if publisher.is_cancelled() && !latest.receiver.borrow().contains_key("prices") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed stream should retire its generation");

        assert_eq!(supervisor.generation(&"prices"), None);
        let final_event = timeout(Duration::from_secs(1), reliable.recv())
            .await
            .expect("queued final event should remain readable promptly")
            .expect("queued final event should survive natural completion");
        assert_eq!(final_event.message().as_ref(), &30);
        assert_eq!(
            publisher.publish_latest(20),
            Err(StreamPublishError::Cancelled)
        );
        assert_eq!(
            publisher.publish_reliable(20).await,
            Err(StreamPublishError::Cancelled)
        );
        assert!(latest.next_unseen().is_none());
    }

    #[tokio::test]
    async fn panicked_stream_should_retire_publisher_and_latest_snapshot() {
        let (channels, _reliable, mut latest) = channels::<TestProtocol>(NonZeroUsize::MIN);
        let cancellation = CancellationToken::new();
        let mut supervisor = StreamSupervisor::new(channels, cancellation, TaskDrain::default());
        let (publisher_sender, publisher_receiver) = oneshot::channel();

        supervisor.replace("prices", |publisher| async move {
            publisher
                .publish_latest(10)
                .expect("active generation should publish");
            let _ = publisher_sender.send(publisher.clone());
            panic!("test stream panic");
        });
        let publisher = publisher_receiver
            .await
            .expect("stream publisher should escape");

        timeout(Duration::from_secs(1), async {
            loop {
                supervisor.reap_finished();
                if publisher.is_cancelled() && !latest.receiver.borrow().contains_key("prices") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicked stream should retire its generation");

        assert_eq!(supervisor.generation(&"prices"), None);
        assert_eq!(
            publisher.publish_latest(20),
            Err(StreamPublishError::Cancelled)
        );
        assert!(latest.next_unseen().is_none());
    }
}
