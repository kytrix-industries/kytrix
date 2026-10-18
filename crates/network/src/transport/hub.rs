//! Shared transport hub, transport policy, and lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::transport::{error::TransportError, http::HttpClientPool, websocket::RawWebSocket};

/// Process-wide transport settings that are not part of a resolved route.
///
/// Plaintext traffic is denied unless a caller opts in, and extra trust anchors
/// exist so a deployment behind a private certificate authority can be reached
/// without weakening the default trust store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransportPolicy {
    pub allow_plaintext: bool,
    pub extra_root_certificates: Vec<Bytes>,
}

/// Shared owner of the network transports used by every connector facade.
#[derive(Clone)]
pub struct TransportHub {
    http: Arc<HttpClientPool>,
    lifecycle: Arc<LifecycleBarrier>,
    policy: TransportPolicy,
}

impl TransportHub {
    pub fn new(policy: TransportPolicy) -> Self {
        Self {
            http: Arc::new(HttpClientPool::new(policy.clone())),
            lifecycle: Arc::new(LifecycleBarrier::new()),
            policy,
        }
    }

    pub fn http(&self) -> &HttpClientPool {
        &self.http
    }

    pub fn policy(&self) -> &TransportPolicy {
        &self.policy
    }

    /// Establishes a WebSocket using this hub's policy and lifecycle.
    ///
    /// Callers should pass a child token of [`Self::cancellation`] when they
    /// need an additional local cancellation boundary.
    pub async fn connect_websocket(
        &self,
        url: &Url,
        route: &ResolvedProxy,
        headers: HeaderMap,
        cancellation: &CancellationToken,
    ) -> Result<RawWebSocket, TransportError> {
        if self.cancellation().is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        RawWebSocket::connect(url, route, headers, cancellation, &self.policy).await
    }

    pub fn cancellation(&self) -> &CancellationToken {
        self.lifecycle.cancellation()
    }

    pub async fn shutdown(&self) {
        self.lifecycle.shutdown().await;
    }

    pub fn scope(&self) -> TransportScope {
        TransportScope {
            global: Arc::clone(&self.lifecycle),
            local: Arc::new(LifecycleBarrier::child_of(&self.lifecycle)),
            policy: self.policy.clone(),
        }
    }

    pub fn begin_operation(&self) -> Result<TransportPermit, TransportError> {
        TransportPermit::global(Arc::clone(&self.lifecycle))
    }
}

pub(crate) struct LifecycleBarrier {
    cancellation: CancellationToken,
    active: AtomicUsize,
    idle: Notify,
}

impl LifecycleBarrier {
    pub(crate) fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            active: AtomicUsize::new(0),
            idle: Notify::new(),
        }
    }

    fn child_of(parent: &Self) -> Self {
        Self {
            cancellation: parent.cancellation.child_token(),
            active: AtomicUsize::new(0),
            idle: Notify::new(),
        }
    }

    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    fn acquire(barrier: Arc<Self>) -> Result<BarrierPermit, TransportError> {
        if barrier.cancellation.is_cancelled() {
            return Err(TransportError::Closed);
        }
        barrier.active.fetch_add(1, Ordering::AcqRel);
        if barrier.cancellation.is_cancelled() {
            barrier.release();
            return Err(TransportError::Closed);
        }
        Ok(BarrierPermit(barrier))
    }

    fn release(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.idle.notify_waiters();
        }
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn shutdown(&self) {
        self.cancel();
        self.wait_idle().await;
    }
}

struct BarrierPermit(Arc<LifecycleBarrier>);

impl Drop for BarrierPermit {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Keeps one operation visible to a lifecycle barrier until it finishes.
pub struct TransportPermit {
    _global: BarrierPermit,
    _local: Option<BarrierPermit>,
}

impl TransportPermit {
    fn global(global: Arc<LifecycleBarrier>) -> Result<Self, TransportError> {
        Ok(Self {
            _global: LifecycleBarrier::acquire(global)?,
            _local: None,
        })
    }
}

/// A cancellable child lifecycle of one [`TransportHub`].
#[derive(Clone)]
pub struct TransportScope {
    global: Arc<LifecycleBarrier>,
    local: Arc<LifecycleBarrier>,
    policy: TransportPolicy,
}

impl TransportScope {
    pub fn cancellation(&self) -> &CancellationToken {
        self.local.cancellation()
    }

    pub fn policy(&self) -> &TransportPolicy {
        &self.policy
    }

    pub fn begin_operation(&self) -> Result<TransportPermit, TransportError> {
        let global = LifecycleBarrier::acquire(Arc::clone(&self.global))?;
        let local = match LifecycleBarrier::acquire(Arc::clone(&self.local)) {
            Ok(local) => local,
            Err(error) => {
                drop(global);
                return Err(error);
            }
        };
        Ok(TransportPermit {
            _global: global,
            _local: Some(local),
        })
    }

    pub fn cancel(&self) {
        self.local.cancel();
    }

    pub async fn shutdown(&self) {
        self.local.shutdown().await;
    }
}
