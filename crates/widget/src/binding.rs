use std::sync::Arc;

use gpui_kit::{Context, Task};
use tokio::runtime::Handle;

use crate::{
    protocol::{RequestId, WidgetProtocol},
    rpc::RpcCallError,
    runtime::{RuntimeExit, WidgetRuntime, WidgetRuntimeConfig, WidgetService},
};

pub type WidgetRpcResult<P> =
    Result<<P as WidgetProtocol>::Response, RpcCallError<<P as WidgetProtocol>::Error>>;

type StreamHandler<V, P> = fn(
    &mut V,
    <P as WidgetProtocol>::StreamKey,
    Arc<<P as WidgetProtocol>::StreamMessage>,
    &mut Context<V>,
);

pub type ConditionalStreamHandler<V, P> = fn(
    &mut V,
    <P as WidgetProtocol>::StreamKey,
    Arc<<P as WidgetProtocol>::StreamMessage>,
    &mut Context<V>,
) -> bool;

enum StreamHandlerPolicy<V, P: WidgetProtocol> {
    Always(StreamHandler<V, P>),
    Conditional(ConditionalStreamHandler<V, P>),
}

impl<V, P: WidgetProtocol> StreamHandlerPolicy<V, P> {
    fn handle(
        &self,
        view: &mut V,
        key: P::StreamKey,
        message: Arc<P::StreamMessage>,
        context: &mut Context<V>,
    ) -> bool {
        match self {
            Self::Always(handler) => {
                handler(view, key, message, context);
                true
            }
            Self::Conditional(handler) => handler(view, key, message, context),
        }
    }
}

type RpcHandler<V, P> = fn(&mut V, WidgetRpcResult<P>, &mut Context<V>);

/// GPUI-side owner for one widget service and its message forwarding tasks.
///
/// Concrete widgets define their protocol, service, and business handlers. The
/// binding owns executor bridging, stream generation filtering, cancellation,
/// and task lifetimes.
pub struct WidgetBinding<P: WidgetProtocol> {
    runtime: WidgetRuntime<P>,
    reliable_task: Task<()>,
    latest_task: Task<()>,
    rpc_task: Option<Task<()>>,
}

impl<P: WidgetProtocol> WidgetBinding<P> {
    pub fn cancel(&self) {
        self.runtime.cancel();
    }

    pub async fn shutdown(self) -> RuntimeExit {
        let Self {
            runtime,
            reliable_task,
            latest_task,
            rpc_task,
        } = self;
        drop((reliable_task, latest_task, rpc_task));
        runtime.shutdown().await
    }

    pub fn spawn<V, S>(
        handle: &Handle,
        service: S,
        context: &mut Context<V>,
        on_reliable: StreamHandler<V, P>,
        on_latest: StreamHandler<V, P>,
    ) -> Self
    where
        V: 'static,
        S: WidgetService<P>,
    {
        Self::spawn_with_config(
            handle,
            service,
            WidgetRuntimeConfig::default(),
            context,
            on_reliable,
            on_latest,
        )
    }

    pub fn spawn_conditionally<V, S>(
        handle: &Handle,
        service: S,
        context: &mut Context<V>,
        on_reliable: ConditionalStreamHandler<V, P>,
        on_latest: ConditionalStreamHandler<V, P>,
    ) -> Self
    where
        V: 'static,
        S: WidgetService<P>,
    {
        Self::spawn_internal(
            handle,
            service,
            WidgetRuntimeConfig::default(),
            context,
            StreamHandlerPolicy::Conditional(on_reliable),
            StreamHandlerPolicy::Conditional(on_latest),
        )
    }

    pub fn spawn_with_config<V, S>(
        handle: &Handle,
        service: S,
        config: WidgetRuntimeConfig,
        context: &mut Context<V>,
        on_reliable: StreamHandler<V, P>,
        on_latest: StreamHandler<V, P>,
    ) -> Self
    where
        V: 'static,
        S: WidgetService<P>,
    {
        Self::spawn_internal(
            handle,
            service,
            config,
            context,
            StreamHandlerPolicy::Always(on_reliable),
            StreamHandlerPolicy::Always(on_latest),
        )
    }

    fn spawn_internal<V, S>(
        handle: &Handle,
        service: S,
        config: WidgetRuntimeConfig,
        context: &mut Context<V>,
        on_reliable: StreamHandlerPolicy<V, P>,
        on_latest: StreamHandlerPolicy<V, P>,
    ) -> Self
    where
        V: 'static,
        S: WidgetService<P>,
    {
        let mut runtime = WidgetRuntime::spawn_with_config(handle, service, config);
        let reliable = runtime.take_reliable_stream();
        let mut latest = runtime.latest_stream();

        let reliable_task = context.spawn(async move |this, context| {
            let Some(mut reliable) = reliable else {
                return;
            };
            while let Some(envelope) = reliable.recv().await {
                let key = envelope.key().clone();
                let message = envelope.into_message();
                if this
                    .update(context, |this, context| {
                        if on_reliable.handle(this, key, message, context) {
                            context.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let latest_task = context.spawn(async move |this, context| {
            while latest.changed().await.is_ok() {
                while let Some(envelope) = latest.next_unseen() {
                    let key = envelope.key().clone();
                    let message = envelope.into_message();
                    if this
                        .update(context, |this, context| {
                            if on_latest.handle(this, key, message, context) {
                                context.notify();
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });

        Self {
            runtime,
            reliable_task,
            latest_task,
            rpc_task: None,
        }
    }

    /// Starts one UI command, cancelling the previous UI-owned call if pending.
    pub fn call<V>(
        &mut self,
        request: P::Request,
        context: &mut Context<V>,
        on_result: RpcHandler<V, P>,
    ) where
        V: 'static,
    {
        let client = self.runtime.client();
        self.rpc_task = Some(context.spawn(async move |this, context| {
            let result = client
                .call(request)
                .await
                .map(|response| response.into_value());
            let _ = this.update(context, |this, context| {
                on_result(this, result, context);
                context.notify();
            });
        }));
    }

    /// Starts one call and preserves the framework request identity for the
    /// owner that needs to reject late responses after replacement.
    pub fn call_with_id<V>(
        &mut self,
        request: P::Request,
        context: &mut Context<V>,
        on_result: impl FnOnce(&mut V, RequestId, WidgetRpcResult<P>, &mut Context<V>) + 'static,
    ) where
        V: 'static,
    {
        let client = self.runtime.client();
        self.rpc_task = Some(context.spawn(async move |this, context| {
            let response = client.call(request).await;
            let request_id = response
                .as_ref()
                .map_or_else(|error| error.request_id(), |response| response.request_id());
            let result = response.map(|response| response.into_value());
            let _ = this.update(context, |this, context| {
                on_result(this, request_id, result, context);
                context.notify();
            });
        }));
    }
}
