//! The one executor bridge: background services in, widget state out.
//!
//! A widget's protocol is driven on the tokio runtime the composition root injected;
//! its results have to reach the interface thread. The toolkit this replaces bridged that
//! with `Context::spawn` + `Context::notify` and an entity update per message. The
//! engine has neither: its spawn takes `Send` futures and its only way back is
//! [`AppHandle::send`](ui_core::app::AppHandle::send), which requires a `Send`
//! closure. The widget's services are `!Send` (`Translate`, `ThemeHandle`), so the
//! bridge cannot call a widget handler from a background task at all — and calling
//! one was only ever a way to mutate the view, which in the engine is signal state.
//!
//! So the bridge delivers **messages**, not calls:
//!
//! 1. [`WidgetBinding::spawn`] starts the widget's service on the injected runtime
//!    and forwards its reliable stream, its latest-value stream and its RPC replies
//!    into a [`WidgetInbox`];
//! 2. each delivery requests a frame on the interface thread (the wake the old
//!    `context.notify()` performed);
//! 3. the widget drains the inbox in [`WidgetView::sync`](crate::view::WidgetView::sync)
//!    — where it has a [`WidgetCx`] again — and applies the messages to its own
//!    signals.
//!
//! That inversion is what makes the bridge `Send`: the inbox is an
//! `Arc<Mutex<VecDeque<…>>>` of protocol messages (every associated type of
//! [`WidgetProtocol`] is `Send + 'static`), and the wake closure captures only the
//! application handle and the window id.
//!
//! # The tokio side does not move
//!
//! [`WidgetRuntime`], [`RpcClient`], the stream supervisors and their tests are
//! untouched by the migration: this module changes only how their output reaches the
//! interface. Task lifetimes are the binding's job — aborting is explicit on
//! [`WidgetBinding::cancel`] and on drop, because a bare `JoinHandle` drop detaches.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
};

use tokio::task::JoinHandle;
use ui_core::{app::AppHandle, window::WindowId};

use crate::{
    cx::WidgetCx,
    protocol::{RequestId, WidgetProtocol},
    rpc::RpcCallError,
    runtime::{RuntimeExit, WidgetRuntime, WidgetRuntimeConfig, WidgetService},
};

/// One RPC outcome, as the widget receives it.
pub type WidgetRpcResult<P> =
    Result<<P as WidgetProtocol>::Response, RpcCallError<<P as WidgetProtocol>::Error>>;

/// One message a binding delivered from its service to the widget, in arrival order.
pub enum Inbound<P: WidgetProtocol> {
    /// A message of the reliable stream: delivered once, in order.
    Reliable {
        key: <P as WidgetProtocol>::StreamKey,
        message: Arc<<P as WidgetProtocol>::StreamMessage>,
    },
    /// The newest value for a keyed latest-value stream.
    Latest {
        key: <P as WidgetProtocol>::StreamKey,
        message: Arc<<P as WidgetProtocol>::StreamMessage>,
    },
    /// The reply to one RPC call the widget issued.
    Rpc {
        request_id: RequestId,
        result: WidgetRpcResult<P>,
    },
}

impl<P: WidgetProtocol> fmt::Debug for Inbound<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reliable { .. } => formatter.write_str("Inbound::Reliable"),
            Self::Latest { .. } => formatter.write_str("Inbound::Latest"),
            Self::Rpc { request_id, .. } => formatter
                .debug_struct("Inbound::Rpc")
                .field("request_id", request_id)
                .finish_non_exhaustive(),
        }
    }
}

/// The messages one widget binding delivered and the widget has not consumed yet.
///
/// It is `Clone` (it is an `Arc`), so the background tasks and the widget hold the
/// same queue. Draining is the only operation that removes messages; the widget
/// drains it in `sync`.
pub struct WidgetInbox<P: WidgetProtocol> {
    messages: Arc<Mutex<VecDeque<Inbound<P>>>>,
}

impl<P: WidgetProtocol> Clone for WidgetInbox<P> {
    fn clone(&self) -> Self {
        Self {
            messages: Arc::clone(&self.messages),
        }
    }
}

impl<P: WidgetProtocol> Default for WidgetInbox<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: WidgetProtocol> WidgetInbox<P> {
    /// Creates an empty inbox. A binding creates one; a widget never does.
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Takes every message delivered so far, in arrival order.
    #[must_use]
    pub fn drain(&self) -> Vec<Inbound<P>> {
        let mut messages = self
            .messages
            .lock()
            .expect("the widget inbox is not poisoned");
        messages.drain(..).collect()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages
            .lock()
            .expect("the widget inbox is not poisoned")
            .is_empty()
    }

    /// How many messages are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages
            .lock()
            .expect("the widget inbox is not poisoned")
            .len()
    }

    fn push(&self, message: Inbound<P>) {
        self.messages
            .lock()
            .expect("the widget inbox is not poisoned")
            .push_back(message);
    }
}

impl<P: WidgetProtocol> fmt::Debug for WidgetInbox<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WidgetInbox")
            .field("pending", &self.len())
            .finish_non_exhaustive()
    }
}

/// The widget-side owner of one service and the tasks forwarding its messages.
///
/// Concrete widgets define their protocol, service and business handlers; the
/// binding owns executor bridging, stream generation filtering, cancellation, and
/// task lifetimes, and the widget owns the inbox consumption in `sync`.
pub struct WidgetBinding<P: WidgetProtocol> {
    runtime: Option<WidgetRuntime<P>>,
    inbox: WidgetInbox<P>,
    reliable_task: Option<JoinHandle<()>>,
    latest_task: Option<JoinHandle<()>>,
    rpc_task: Option<JoinHandle<()>>,
}

impl<P: WidgetProtocol> WidgetBinding<P> {
    /// Starts one widget service with the default runtime configuration.
    pub fn spawn<S: WidgetService<P>>(cx: &mut WidgetCx<'_>, service: S) -> Self {
        Self::spawn_with_config(cx, service, WidgetRuntimeConfig::default())
    }

    /// Starts one widget service with an explicit runtime configuration.
    pub fn spawn_with_config<S: WidgetService<P>>(
        cx: &mut WidgetCx<'_>,
        service: S,
        config: WidgetRuntimeConfig,
    ) -> Self {
        let mut runtime = WidgetRuntime::spawn_with_config(cx.runtime(), service, config);
        let reliable = runtime.take_reliable_stream();
        let mut latest = runtime.latest_stream();
        let inbox = WidgetInbox::new();
        let window = cx.window();
        let handle = cx.handle();

        let reliable_task = reliable.map(|mut reliable| {
            let inbox = inbox.clone();
            let handle = handle.clone();
            cx.runtime().spawn(async move {
                while let Some(envelope) = reliable.recv().await {
                    let key = envelope.key().clone();
                    inbox.push(Inbound::Reliable {
                        key,
                        message: envelope.into_message(),
                    });
                    request_frame(&handle, window);
                }
            })
        });

        let latest_task = {
            let inbox = inbox.clone();
            Some(cx.runtime().spawn(async move {
                while latest.changed().await.is_ok() {
                    while let Some(envelope) = latest.next_unseen() {
                        let key = envelope.key().clone();
                        inbox.push(Inbound::Latest {
                            key,
                            message: envelope.into_message(),
                        });
                        request_frame(&handle, window);
                    }
                }
            }))
        };

        Self {
            runtime: Some(runtime),
            inbox,
            reliable_task,
            latest_task,
            rpc_task: None,
        }
    }

    /// The queue the widget drains in its `sync`.
    #[must_use]
    pub fn inbox(&self) -> &WidgetInbox<P> {
        &self.inbox
    }

    /// Takes every message delivered so far.
    #[must_use]
    pub fn take(&self) -> Vec<Inbound<P>> {
        self.inbox.drain()
    }

    /// The runtime's RPC client, for a caller that drives its own call loop.
    #[must_use]
    pub fn client(&self) -> crate::rpc::RpcClient<P> {
        self.runtime
            .as_ref()
            .expect("the binding's runtime is alive")
            .client()
    }

    /// Starts one RPC call, replacing (and aborting) the previous one.
    ///
    /// The reply arrives in the inbox as [`Inbound::Rpc`] with the framework request
    /// identity, so a widget can reject a late response after it replaced the call —
    /// the engine's version of the old "stale update" check.
    pub fn call(&mut self, cx: &mut WidgetCx<'_>, request: P::Request) {
        if let Some(task) = self.rpc_task.take() {
            task.abort();
        }

        let client = self
            .runtime
            .as_ref()
            .expect("the binding's runtime is alive")
            .client();
        let inbox = self.inbox.clone();
        let handle = cx.handle();
        let window = cx.window();

        self.rpc_task = Some(cx.runtime().spawn(async move {
            let response = client.call(request).await;
            let request_id = response
                .as_ref()
                .map_or_else(|error| error.request_id(), |response| response.request_id());
            let result = response.map(|response| response.into_value());
            inbox.push(Inbound::Rpc { request_id, result });
            request_frame(&handle, window);
        }));
    }

    /// Stops the service and aborts the forwarding tasks.
    pub fn cancel(&mut self) {
        self.abort_tasks();
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.cancel();
        }
    }

    /// Stops the service and waits for it to finish.
    pub async fn shutdown(mut self) -> RuntimeExit {
        self.abort_tasks();
        let Some(runtime) = self.runtime.take() else {
            return RuntimeExit::Cancelled;
        };
        runtime.shutdown().await
    }

    fn abort_tasks(&mut self) {
        for task in [
            self.reliable_task.take(),
            self.latest_task.take(),
            self.rpc_task.take(),
        ]
        .into_iter()
        .flatten()
        {
            task.abort();
        }
    }
}

impl<P: WidgetProtocol> Drop for WidgetBinding<P> {
    fn drop(&mut self) {
        self.abort_tasks();
    }
}

impl<P: WidgetProtocol> fmt::Debug for WidgetBinding<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WidgetBinding")
            .field("runtime", &self.runtime)
            .field("pending", &self.inbox.len())
            .finish_non_exhaustive()
    }
}

/// Asks the interface thread for the frame that will drain the inbox.
///
/// The old bridge called `context.notify()`; the engine's equivalent is a repaint
/// request on the widget's window. It is made from the application handle because the
/// caller is a background task.
fn request_frame(handle: &AppHandle, window: WindowId) {
    handle.send(move |app| {
        if let Err(error) = app.request_repaint(window) {
            log::error!("a widget binding could not request a frame: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        rc::Rc,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use ui_core::{
        app::App,
        node::signal::SignalScope,
        window::{WindowId, WindowOptions},
    };

    use super::{Inbound, WidgetBinding, WidgetInbox};
    use crate::{
        cx::WidgetCx,
        frame::FrameRouter,
        protocol::{RequestId, WidgetProtocol},
        runtime::{RuntimeExit, ServiceContext, WidgetService},
        services::WidgetServices,
    };

    struct TestProtocol;

    impl WidgetProtocol for TestProtocol {
        type Request = u64;
        type Response = u64;
        type StreamKey = &'static str;
        type StreamMessage = u64;
        type Error = Infallible;
    }

    /// Answers every RPC with the value it was given.
    struct EchoService;

    impl WidgetService<TestProtocol> for EchoService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            while let Some(request) = context.next_rpc().await {
                let (reply, value) = request.into_parts();
                let _ = reply.respond(Ok(value));
            }
        }
    }

    /// Never answers and never ends.
    struct PendingService;

    impl WidgetService<TestProtocol> for PendingService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            while context.next_rpc().await.is_some() {}
        }
    }

    /// Collects two RPC requests, then answers both — and records whether the answer
    /// had a live receiver on the widget side.
    ///
    /// The first request is answered only after the second one arrived, so the test
    /// controls when the call the binding already replaced gets its reply: with the
    /// abort in place its receiver is gone and `respond` fails, without it the reply
    /// reaches the inbox.
    struct RecordingService {
        received: Arc<AtomicUsize>,
        answered: Arc<Mutex<Vec<bool>>>,
    }

    impl WidgetService<TestProtocol> for RecordingService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            let Some(first) = context.next_rpc().await else {
                return;
            };
            self.received.fetch_add(1, Ordering::SeqCst);
            let Some(second) = context.next_rpc().await else {
                return;
            };
            self.received.fetch_add(1, Ordering::SeqCst);

            for request in [first, second] {
                let (reply, value) = request.into_parts();
                let delivered = reply.respond(Ok(value)).is_ok();
                self.answered
                    .lock()
                    .expect("the recording is not poisoned")
                    .push(delivered);
            }
            while context.next_rpc().await.is_some() {}
        }
    }

    /// A headless engine app, the services aggregate, and a runtime owned by the test.
    struct Fixture {
        app: App,
        runtime: tokio::runtime::Runtime,
        services: Rc<WidgetServices>,
        window: WindowId,
        scope: SignalScope,
    }

    impl Fixture {
        fn new() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("the test runtime builds");
            let mut app = App::new(runtime.handle().clone());
            let window = app.create_window(WindowOptions::default());
            let theme = base_ui::theme::handle::ThemeHandle::new(
                &mut app,
                base_ui::theme::appearance::Appearance::Light,
            );
            let layer = Rc::new(base_ui::kit::layer::Layer::new());
            let animation_router = Rc::new(base_ui::kit::overlay::AnimationRouter::new());
            let router =
                Rc::new(FrameRouter::new(&mut app).expect("the fixture owns the frame hook slot"));
            let services = Rc::new(
                WidgetServices::builder(
                    &mut app,
                    runtime.handle().clone(),
                    crate::test_support::fixture_translate(i18n::language::codes::LanguageCode::En),
                    theme,
                    layer,
                    router,
                    animation_router,
                )
                .build(),
            );
            let scope = app.signal_scope();

            Self {
                app,
                runtime,
                services,
                window,
                scope,
            }
        }

        /// Runs `body` with a context over the fixture and drops the borrow.
        fn with_cx<R>(&mut self, body: impl FnOnce(&mut WidgetCx<'_>) -> R) -> R {
            let mut cx = WidgetCx::new(&mut self.app, self.window, &self.services, self.scope);
            body(&mut cx)
        }

        /// Waits until the binding delivered one message, then takes everything.
        fn wait_for_one(
            &self,
            binding: &WidgetBinding<TestProtocol>,
        ) -> Vec<Inbound<TestProtocol>> {
            for _ in 0..2_000 {
                let messages = binding.take();
                if !messages.is_empty() {
                    return messages;
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            panic!("the binding delivered nothing within two seconds");
        }

        /// Spins until `condition` holds (the runtime's worker threads drive it).
        fn wait_until(&self, condition: impl Fn() -> bool, what: &str) {
            for _ in 0..2_000 {
                if condition() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            panic!("timed out waiting for {what}");
        }
    }

    /// The queue itself: empty, ordered, and draining only what arrived.
    #[test]
    fn the_inbox_starts_empty_and_drains_once() {
        let inbox: WidgetInbox<TestProtocol> = WidgetInbox::new();

        assert!(inbox.is_empty());
        assert_eq!(inbox.len(), 0);
        assert!(inbox.drain().is_empty());
    }

    /// The bridge's whole purpose: an RPC's reply reaches the widget's inbox, with the
    /// framework request identity.
    #[test]
    fn an_rpc_reply_reaches_the_inbox() {
        let mut fixture = Fixture::new();
        let mut binding = fixture.with_cx(|cx| WidgetBinding::spawn(cx, EchoService));

        fixture.with_cx(|cx| binding.call(cx, 42));
        let messages = fixture.wait_for_one(&binding);

        let (request_id, result) = messages
            .into_iter()
            .find_map(|message| match message {
                Inbound::Rpc { request_id, result } => Some((request_id, result)),
                _ => None,
            })
            .expect("the RPC reply arrived");
        assert_eq!(request_id, RequestId::new(1));
        assert_eq!(result.ok(), Some(42));
    }

    /// Replacing a call aborts the previous one: the widget cannot be handed two
    /// replies for one command slot, and the replaced future really is dropped.
    ///
    /// The service answers the replaced request *after* the replacement happened, so
    /// the two behaviours are distinguishable: with the abort the reply has no
    /// receiver and never reaches the inbox; without it both replies arrive.
    #[test]
    fn replacing_a_call_cancels_the_previous_one() {
        let mut fixture = Fixture::new();
        let received = Arc::new(AtomicUsize::new(0));
        let answered = Arc::new(Mutex::new(Vec::new()));
        let service = RecordingService {
            received: Arc::clone(&received),
            answered: Arc::clone(&answered),
        };
        let mut binding = fixture.with_cx(|cx| WidgetBinding::spawn(cx, service));

        fixture.with_cx(|cx| binding.call(cx, 1));
        fixture.wait_until(|| received.load(Ordering::SeqCst) >= 1, "the first request");
        fixture.with_cx(|cx| binding.call(cx, 2));
        fixture.wait_until(
            || {
                answered
                    .lock()
                    .expect("the recording is not poisoned")
                    .len()
                    == 2
            },
            "both requests to be answered",
        );

        let messages = fixture.wait_for_one(&binding);
        assert_eq!(
            messages.len(),
            1,
            "the replaced call's reply never reaches the inbox: {messages:?}"
        );
        let request_id = messages
            .into_iter()
            .find_map(|message| match message {
                Inbound::Rpc { request_id, .. } => Some(request_id),
                _ => None,
            })
            .expect("the surviving call's reply arrived");
        assert_eq!(request_id, RequestId::new(2));
        assert_eq!(
            answered
                .lock()
                .expect("the recording is not poisoned")
                .as_slice(),
            [false, true],
            "the replaced call's receiver was dropped; the replacing call's was alive"
        );

        let exit = fixture.runtime.block_on(binding.shutdown());
        assert!(
            !matches!(exit, RuntimeExit::Panicked(_)),
            "the service stopped without panicking: {exit:?}"
        );
    }

    /// A cancelled binding stops its service and delivers nothing more.
    #[test]
    fn cancel_stops_the_service() {
        let mut fixture = Fixture::new();
        let mut binding = fixture.with_cx(|cx| WidgetBinding::spawn(cx, PendingService));

        binding.cancel();
        assert!(binding.inbox().is_empty());

        let exit = fixture.runtime.block_on(binding.shutdown());
        assert!(
            !matches!(exit, RuntimeExit::Panicked(_)),
            "the service stopped without panicking: {exit:?}"
        );
    }
}
