use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
};

use ahash::HashMapExt;
use tokio::task::JoinHandle;
use ui_core::{app::AppHandle, window::WindowId};

use crate::{
    cx::WidgetCx,
    protocol::{RequestId, WidgetProtocol},
    rpc::RpcCallError,
    runtime::{RuntimeExit, WidgetRuntime, WidgetRuntimeConfig, WidgetService},
};

pub type WidgetRpcResult<P> =
    Result<<P as WidgetProtocol>::Response, RpcCallError<<P as WidgetProtocol>::Error>>;

pub enum Inbound<P: WidgetProtocol> {
    Reliable {
        key: <P as WidgetProtocol>::StreamKey,
        message: Arc<<P as WidgetProtocol>::StreamMessage>,
    },
    Latest {
        key: <P as WidgetProtocol>::StreamKey,
        message: Arc<<P as WidgetProtocol>::StreamMessage>,
    },
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
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    #[must_use]
    pub fn drain(&self) -> Vec<Inbound<P>> {
        let mut messages = self
            .messages
            .lock()
            .expect("the widget inbox is not poisoned");
        messages.drain(..).collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages
            .lock()
            .expect("the widget inbox is not poisoned")
            .is_empty()
    }

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

pub struct Pending<C> {
    commands: ahash::HashMap<RequestId, C>,
    dropped: usize,
}

impl<C> Pending<C> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: ahash::HashMap::with_capacity(1),
            dropped: 0,
        }
    }

    pub fn track(&mut self, request_id: RequestId, command: C) {
        self.commands.clear();
        self.commands.insert(request_id, command);
    }

    pub fn take(&mut self, request_id: RequestId) -> Option<C> {
        match self.commands.remove(&request_id) {
            Some(command) => Some(command),
            None => {
                self.dropped += 1;
                None
            }
        }
    }

    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn clear(&mut self) {
        self.commands.clear();
    }
}

impl<C> Default for Pending<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> fmt::Debug for Pending<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pending")
            .field("waiting", &self.commands.len())
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

pub struct WidgetBinding<P: WidgetProtocol> {
    runtime: Option<WidgetRuntime<P>>,
    inbox: WidgetInbox<P>,
    reliable_task: Option<JoinHandle<()>>,
    latest_task: Option<JoinHandle<()>>,
    rpc_task: Option<JoinHandle<()>>,
}

impl<P: WidgetProtocol> WidgetBinding<P> {
    pub fn spawn<S: WidgetService<P>>(cx: &mut WidgetCx<'_>, service: S) -> Self {
        Self::spawn_with_config(cx, service, WidgetRuntimeConfig::default())
    }

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

    #[must_use]
    pub fn inbox(&self) -> &WidgetInbox<P> {
        &self.inbox
    }

    #[must_use]
    pub fn take(&self) -> Vec<Inbound<P>> {
        self.inbox.drain()
    }

    #[must_use]
    pub fn client(&self) -> crate::rpc::RpcClient<P> {
        self.runtime
            .as_ref()
            .expect("the binding's runtime is alive")
            .client()
    }

    pub fn call(&mut self, cx: &mut WidgetCx<'_>, request: P::Request) -> RequestId {
        if let Some(task) = self.rpc_task.take() {
            task.abort();
        }

        let client = self
            .runtime
            .as_ref()
            .expect("the binding's runtime is alive")
            .client();
        let request_id = client.reserve_request_id();
        let inbox = self.inbox.clone();
        let handle = cx.handle();
        let window = cx.window();

        self.rpc_task = Some(cx.runtime().spawn(async move {
            let result = client
                .call_with_id(request_id, request)
                .await
                .map(|response| response.into_value());
            inbox.push(Inbound::Rpc { request_id, result });
            request_frame(&handle, window);
        }));

        request_id
    }

    pub fn cancel(&mut self) {
        self.abort_tasks();
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.cancel();
        }
    }

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

    use super::{Inbound, Pending, WidgetBinding, WidgetInbox};
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

    struct EchoService;

    impl WidgetService<TestProtocol> for EchoService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            while let Some(request) = context.next_rpc().await {
                let (reply, value) = request.into_parts();
                let _ = reply.respond(Ok(value));
            }
        }
    }

    struct PendingService;

    impl WidgetService<TestProtocol> for PendingService {
        async fn run(self, mut context: ServiceContext<TestProtocol>) {
            while context.next_rpc().await.is_some() {}
        }
    }

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

        fn with_cx<R>(&mut self, body: impl FnOnce(&mut WidgetCx<'_>) -> R) -> R {
            let mut cx = WidgetCx::new(&mut self.app, self.window, &self.services, self.scope);
            body(&mut cx)
        }

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

    #[test]
    fn the_inbox_starts_empty_and_drains_once() {
        let inbox: WidgetInbox<TestProtocol> = WidgetInbox::new();

        assert!(inbox.is_empty());
        assert_eq!(inbox.len(), 0);
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn a_call_returns_the_identity_its_reply_carries() {
        let mut fixture = Fixture::new();
        let mut binding = fixture.with_cx(|cx| WidgetBinding::spawn(cx, EchoService));

        let expected = fixture.with_cx(|cx| binding.call(cx, 42));
        let messages = fixture.wait_for_one(&binding);

        let (request_id, result) = messages
            .into_iter()
            .find_map(|message| match message {
                Inbound::Rpc { request_id, result } => Some((request_id, result)),
                _ => None,
            })
            .expect("the RPC reply arrived");
        assert_eq!(request_id, expected, "the caller knew the reply's identity");
        assert_eq!(expected, RequestId::new(1), "identities count from one");
        assert_eq!(result.ok(), Some(42));
    }

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

        let first = fixture.with_cx(|cx| binding.call(cx, 1));
        fixture.wait_until(|| received.load(Ordering::SeqCst) >= 1, "the first request");
        let second = fixture.with_cx(|cx| binding.call(cx, 2));
        assert_eq!(
            (first, second),
            (RequestId::new(1), RequestId::new(2)),
            "each call takes the next identity, replaced or not"
        );
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

    #[test]
    fn a_pending_table_matches_identities_and_counts_late_answers() {
        let mut pending = Pending::new();
        pending.track(RequestId::new(1), "load");

        assert_eq!(pending.take(RequestId::new(1)), Some("load"));
        assert!(pending.is_empty());
        assert_eq!(pending.dropped(), 0);
        assert_eq!(
            pending.take(RequestId::new(1)),
            None,
            "an answer is taken once"
        );
        assert_eq!(pending.dropped(), 1, "the duplicate is counted");
        assert_eq!(
            pending.take(RequestId::new(2)),
            None,
            "an identity this table never tracked is not this binding's answer"
        );
        assert_eq!(pending.dropped(), 2);
    }

    #[test]
    fn tracking_a_replacement_makes_the_replaced_identity_unknown() {
        let mut pending = Pending::new();
        pending.track(RequestId::new(1), "load");
        pending.track(RequestId::new(2), "refresh");

        assert_eq!(pending.take(RequestId::new(1)), None, "the replaced call");
        assert_eq!(pending.dropped(), 1);
        assert_eq!(
            pending.take(RequestId::new(2)),
            Some("refresh"),
            "the command really in flight still answers"
        );
        assert_eq!(pending.dropped(), 1);
    }

    #[test]
    fn clearing_abandons_the_command_without_counting_it() {
        let mut pending = Pending::new();
        pending.track(RequestId::new(1), "load");
        pending.clear();

        assert!(pending.is_empty());
        assert_eq!(pending.dropped(), 0);
        assert_eq!(pending.take(RequestId::new(1)), None);
        assert_eq!(pending.dropped(), 1);
    }
}
