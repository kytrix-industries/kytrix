use std::{fmt, future::Future, num::NonZeroUsize, time::Duration};

use tokio::{runtime::Handle, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    protocol::{StreamGeneration, WidgetProtocol},
    rpc::{self, RpcClient, RpcReceiver, RpcRequest},
    stream::{self, LatestStream, ReliableStream, StreamSupervisor, TaskDrain},
};

/// Background implementation owned by one widget instance.
pub trait WidgetService<P: WidgetProtocol>: Send + 'static {
    fn run(self, context: ServiceContext<P>) -> impl Future<Output = ()> + Send;
}

/// Capacity and deadline defaults for one widget runtime.
#[derive(Clone, Copy, Debug)]
pub struct WidgetRuntimeConfig {
    rpc_capacity: NonZeroUsize,
    reliable_stream_capacity: NonZeroUsize,
    default_rpc_timeout: Duration,
}

impl WidgetRuntimeConfig {
    pub const fn new(
        rpc_capacity: NonZeroUsize,
        reliable_stream_capacity: NonZeroUsize,
        default_rpc_timeout: Duration,
    ) -> Self {
        Self {
            rpc_capacity,
            reliable_stream_capacity,
            default_rpc_timeout,
        }
    }
}

impl Default for WidgetRuntimeConfig {
    fn default() -> Self {
        let rpc_capacity = NonZeroUsize::MIN.saturating_add(31);
        let reliable_stream_capacity = NonZeroUsize::MIN.saturating_add(255);
        Self::new(
            rpc_capacity,
            reliable_stream_capacity,
            Duration::from_secs(10),
        )
    }
}

/// UI-side handle for one service plus its reliable and latest-value outputs.
pub struct WidgetRuntime<P: WidgetProtocol> {
    client: RpcClient<P>,
    reliable_stream: Option<ReliableStream<P>>,
    latest_stream: LatestStream<P>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<RuntimeExit>>,
    stream_drain: TaskDrain,
}

impl<P: WidgetProtocol> WidgetRuntime<P> {
    pub fn spawn<S>(handle: &Handle, service: S) -> Self
    where
        S: WidgetService<P>,
    {
        Self::spawn_with_config(handle, service, WidgetRuntimeConfig::default())
    }

    pub fn spawn_with_config<S>(handle: &Handle, service: S, config: WidgetRuntimeConfig) -> Self
    where
        S: WidgetService<P>,
    {
        let (client, rpc_receiver) =
            rpc::channel(handle, config.rpc_capacity, config.default_rpc_timeout);
        let (stream_channels, reliable_stream, latest_stream) =
            stream::channels(config.reliable_stream_capacity);
        let cancellation = CancellationToken::new();
        let service_cancellation = cancellation.clone();
        let stream_drain = TaskDrain::default();
        let task_stream_drain = stream_drain.clone();
        let context = ServiceContext {
            rpc_receiver,
            streams: StreamSupervisor::new(
                stream_channels,
                service_cancellation.clone(),
                stream_drain.clone(),
            ),
            cancellation: service_cancellation.clone(),
        };
        let task = handle.spawn(async move {
            let exit = tokio::select! {
                biased;
                () = service_cancellation.cancelled() => RuntimeExit::Cancelled,
                () = service.run(context) => RuntimeExit::Completed,
            };
            service_cancellation.cancel();
            task_stream_drain.wait().await;
            exit
        });

        Self {
            client,
            reliable_stream: Some(reliable_stream),
            latest_stream,
            cancellation,
            task: Some(task),
            stream_drain,
        }
    }

    pub fn client(&self) -> RpcClient<P> {
        self.client.clone()
    }

    /// Takes the single reliable-event receiver used by the GPUI view.
    pub fn take_reliable_stream(&mut self) -> Option<ReliableStream<P>> {
        self.reliable_stream.take()
    }

    /// Creates a new latest-value receiver. Slow readers observe only the newest snapshot.
    pub fn latest_stream(&self) -> LatestStream<P> {
        self.latest_stream.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn shutdown(mut self) -> RuntimeExit {
        self.cancellation.cancel();
        let Some(task) = self.task.take() else {
            return RuntimeExit::Cancelled;
        };

        let result = match task.await {
            Ok(exit) => exit,
            Err(error) if error.is_cancelled() => RuntimeExit::Aborted,
            Err(error) => RuntimeExit::Panicked(error.to_string()),
        };
        self.stream_drain.wait().await;
        result
    }
}

impl<P: WidgetProtocol> fmt::Debug for WidgetRuntime<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WidgetRuntime")
            .field("cancelled", &self.is_cancelled())
            .field("reliable_stream_taken", &self.reliable_stream.is_none())
            .finish_non_exhaustive()
    }
}

impl<P: WidgetProtocol> Drop for WidgetRuntime<P> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeExit {
    Completed,
    Cancelled,
    Aborted,
    Panicked(String),
}

/// Service-side context that owns RPC input and all supervised stream tasks.
pub struct ServiceContext<P: WidgetProtocol> {
    rpc_receiver: RpcReceiver<P>,
    streams: StreamSupervisor<P>,
    cancellation: CancellationToken,
}

impl<P: WidgetProtocol> ServiceContext<P> {
    pub async fn next_rpc(&mut self) -> Option<RpcRequest<P>> {
        loop {
            let request = tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return None,
                request = self.rpc_receiver.recv() => request?,
            };

            if request.deadline() > Instant::now() && !request.is_cancelled() {
                return Some(request);
            }
        }
    }

    pub fn replace_stream<F, Fut>(&mut self, key: P::StreamKey, task: F) -> StreamGeneration
    where
        F: FnOnce(super::stream::StreamPublisher<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.streams.replace(key, task)
    }

    /// Starts a stream only when the key has no active generation.
    ///
    /// Repeated initialization requests are intentionally idempotent: an active
    /// publisher, snapshot, and generation remain untouched. Use
    /// [`Self::replace_stream`] only for an explicit identity change or retry.
    pub fn start_stream<F, Fut>(&mut self, key: P::StreamKey, task: F) -> StreamGeneration
    where
        F: FnOnce(super::stream::StreamPublisher<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.streams.start(key, task)
    }

    pub fn cancel_stream(&mut self, key: &P::StreamKey) -> bool {
        self.streams.cancel(key)
    }

    pub fn stream_generation(&self, key: &P::StreamKey) -> Option<StreamGeneration> {
        self.streams.generation(key)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
}

impl<P: WidgetProtocol> fmt::Debug for ServiceContext<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceContext")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one. The
    /// clock these tests read is the runtime's: `#[tokio::test(start_paused = true)]`
    /// pauses it and `tokio::time::advance` moves it, and both only affect the runtime
    /// whose clock was paused. Work started on another runtime keeps waiting on real
    /// time while the test advances a clock it never reads.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::{
        convert::Infallible,
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use tokio::sync::oneshot;

    use super::*;
    use crate::rpc::RpcCallError;

    struct TestProtocol;

    impl WidgetProtocol for TestProtocol {
        type Request = u64;
        type Response = u64;
        type StreamKey = &'static str;
        type StreamMessage = u64;
        type Error = Infallible;
    }

    struct EchoService;

    impl WidgetService<TestProtocol> for EchoService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            while let Some(request) = context.next_rpc().await {
                let (reply, value) = request.into_parts();
                let _ = reply.respond(Ok(value));
            }
        }
    }

    #[tokio::test]
    async fn rpc_assigns_monotonic_request_ids() {
        let runtime = WidgetRuntime::spawn(current_test_handle(), EchoService);
        let client = runtime.client();

        let first = client.call(10).await.expect("first RPC should succeed");
        let second = client.call(20).await.expect("second RPC should succeed");

        assert_eq!(
            (first.request_id().get(), second.request_id().get()),
            (1, 2)
        );
    }

    #[test]
    fn rpc_call_should_work_outside_a_tokio_executor() {
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Tokio runtime should start");
        let runtime = WidgetRuntime::spawn(tokio_runtime.handle(), EchoService);
        let client = runtime.client();
        let mut call = Box::pin(client.call(42));
        let mut context = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut context).is_pending());
        let response = (0..16)
            .find_map(|_| {
                tokio_runtime.block_on(tokio::task::yield_now());
                match call.as_mut().poll(&mut context) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                }
            })
            .expect("single-threaded Tokio runtime should drive the RPC")
            .expect("RPC should bridge from outside the Tokio executor");

        assert_eq!(response.into_value(), 42);
        tokio_runtime.block_on(runtime.shutdown());
    }

    struct PendingService {
        cancellation_seen: oneshot::Sender<()>,
    }

    impl WidgetService<TestProtocol> for PendingService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            let Some(request) = context.next_rpc().await else {
                return;
            };
            let (mut reply, _) = request.into_parts();
            reply.cancelled().await;
            let _ = self.cancellation_seen.send(());
        }
    }

    #[tokio::test]
    async fn rpc_timeout_cancels_the_service_request() {
        let (cancellation_seen, cancelled) = oneshot::channel();
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), PendingService { cancellation_seen });

        let error = runtime
            .client()
            .call_with_timeout(1, Duration::from_millis(10))
            .await
            .expect_err("pending RPC should time out");

        assert!(matches!(error, RpcCallError::DeadlineExceeded { .. }));
        cancelled
            .await
            .expect("service should observe cancellation");
    }

    struct StreamService;

    impl WidgetService<TestProtocol> for StreamService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            context.replace_stream("prices", |publisher| async move {
                publisher
                    .publish_reliable(1)
                    .await
                    .expect("reliable receiver should be open");
                publisher
                    .publish_reliable(2)
                    .await
                    .expect("reliable receiver should be open");
                publisher
                    .publish_latest(10)
                    .expect("latest receiver should be open");
                publisher
                    .publish_latest(20)
                    .expect("latest receiver should be open");
                publisher.cancelled().await;
            });
            context.cancelled().await;
        }
    }

    #[tokio::test]
    async fn streams_keep_reliable_events_and_coalesce_latest_snapshots() {
        let config = WidgetRuntimeConfig::new(
            NonZeroUsize::new(1).expect("non-zero"),
            NonZeroUsize::new(1).expect("non-zero"),
            Duration::from_secs(1),
        );
        let mut runtime =
            WidgetRuntime::spawn_with_config(current_test_handle(), StreamService, config);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable receiver should only be taken once");
        let mut latest = runtime.latest_stream();

        let first = reliable.recv().await.expect("first event should arrive");
        let second = reliable.recv().await.expect("second event should arrive");
        latest
            .changed()
            .await
            .expect("latest snapshot should be published");
        let latest_value = latest
            .next_unseen()
            .expect("latest value should exist")
            .message()
            .as_ref()
            .to_owned();

        assert_eq!(
            (*first.message().as_ref(), *second.message().as_ref()),
            (1, 2)
        );
        assert_eq!(latest_value, 20);
    }

    struct StartOnceService;

    impl WidgetService<TestProtocol> for StartOnceService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            context.start_stream("prices", |publisher| async move {
                publisher
                    .publish_latest(7)
                    .expect("initial snapshot should publish");
                publisher.cancelled().await;
            });
            let first = context.stream_generation(&"prices");
            let second = context.start_stream("prices", |_publisher| async move {});
            assert_eq!(first, Some(second));
            context.cancelled().await;
        }
    }

    #[tokio::test]
    async fn start_stream_is_idempotent_and_preserves_generation() {
        let runtime = WidgetRuntime::spawn(current_test_handle(), StartOnceService);
        runtime.shutdown().await;
    }

    struct DropProbe {
        dropped_on_tokio: Arc<AtomicBool>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.dropped_on_tokio
                .store(Handle::try_current().is_ok(), Ordering::Release);
        }
    }

    struct LifecycleService {
        probe: DropProbe,
    }

    impl WidgetService<TestProtocol> for LifecycleService {
        async fn run(self, context: ServiceContext<TestProtocol>) {
            let _probe = self.probe;
            context.cancelled().await;
        }
    }

    #[tokio::test]
    async fn shutdown_drops_service_resources_inside_tokio() {
        let dropped_on_tokio = Arc::new(AtomicBool::new(false));
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            LifecycleService {
                probe: DropProbe {
                    dropped_on_tokio: dropped_on_tokio.clone(),
                },
            },
        );

        let exit = runtime.shutdown().await;

        assert_eq!(exit, RuntimeExit::Cancelled);
        assert!(dropped_on_tokio.load(Ordering::Acquire));
    }

    struct ChildLifecycleService {
        dropped: oneshot::Sender<()>,
        started: oneshot::Sender<()>,
    }

    impl WidgetService<TestProtocol> for ChildLifecycleService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            context.replace_stream("child", move |publisher| async move {
                struct Signal(Option<oneshot::Sender<()>>);
                impl Drop for Signal {
                    fn drop(&mut self) {
                        if let Some(sender) = self.0.take() {
                            let _ = sender.send(());
                        }
                    }
                }

                let _signal = Signal(Some(self.dropped));
                let _ = self.started.send(());
                publisher.cancelled().await;
            });
            context.cancelled().await;
        }
    }

    #[tokio::test]
    async fn shutdown_should_wait_for_child_stream_resources() {
        let (dropped, child_dropped) = oneshot::channel();
        let (started, child_started) = oneshot::channel();
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ChildLifecycleService { dropped, started },
        );
        child_started
            .await
            .expect("child stream should start before shutdown");

        let exit = runtime.shutdown().await;

        assert_eq!(exit, RuntimeExit::Cancelled);
        child_dropped
            .await
            .expect("child stream should be dropped before shutdown returns");
    }
}
