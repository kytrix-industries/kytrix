use std::{
    fmt,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use thiserror::Error;
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc},
    task::AbortHandle,
    time::{Instant, sleep_until},
};

use super::protocol::{RequestId, WidgetProtocol};

/// Successful RPC response paired with the framework-generated request identifier.
#[derive(Debug)]
pub struct RpcResponse<T> {
    request_id: RequestId,
    value: T,
}

impl<T> RpcResponse<T> {
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub fn into_value(self) -> T {
        self.value
    }
}

/// Failure returned to the UI while submitting or awaiting an RPC call.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RpcCallError<E> {
    #[error("RPC request {request_id} exceeded its deadline")]
    DeadlineExceeded { request_id: RequestId },
    #[error("RPC service closed before request {request_id} could be submitted")]
    ServiceClosed { request_id: RequestId },
    #[error("RPC service dropped the response for request {request_id}")]
    ResponseDropped { request_id: RequestId },
    #[error("RPC request {request_id} failed: {source}")]
    Service {
        request_id: RequestId,
        #[source]
        source: E,
    },
}

impl<E> RpcCallError<E> {
    pub const fn request_id(&self) -> RequestId {
        match self {
            Self::DeadlineExceeded { request_id }
            | Self::ServiceClosed { request_id }
            | Self::ResponseDropped { request_id }
            | Self::Service { request_id, .. } => *request_id,
        }
    }
}

/// Cloneable, bounded RPC client used by a widget UI.
pub struct RpcClient<P: WidgetProtocol> {
    sender: mpsc::Sender<RpcRequest<P>>,
    lifecycle: Arc<ChannelLifecycle>,
    runtime: Handle,
    next_request_id: Arc<AtomicU64>,
    default_timeout: Duration,
}

impl<P: WidgetProtocol> RpcClient<P> {
    pub async fn call(
        &self,
        request: P::Request,
    ) -> Result<RpcResponse<P::Response>, RpcCallError<P::Error>> {
        self.call_with_timeout(request, self.default_timeout).await
    }

    pub async fn call_with_timeout(
        &self,
        request: P::Request,
        timeout: Duration,
    ) -> Result<RpcResponse<P::Response>, RpcCallError<P::Error>> {
        let request_id = RequestId::new(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let deadline = Instant::now() + timeout;
        let state = Arc::new(CallState::new(request_id, deadline));
        let task = self.runtime.spawn(execute_call::<P>(
            self.sender.clone(),
            self.lifecycle.clone(),
            request,
            state.clone(),
        ));
        let _abort_on_drop = AbortOnDrop(task.abort_handle());
        // Declared after the abort guard so caller cancellation wins synchronously on drop.
        let mut caller_on_drop = CallerOnDrop::new(state.clone());

        let result = match task.await {
            Ok(result) => result,
            Err(_) => {
                state.service_closed();
                wait_for_result(&state).await
            }
        };
        caller_on_drop.disarm();
        result
    }
}

async fn execute_call<P: WidgetProtocol>(
    sender: mpsc::Sender<RpcRequest<P>>,
    lifecycle: Arc<ChannelLifecycle>,
    request: P::Request,
    state: Arc<CallState<P>>,
) -> Result<RpcResponse<P::Response>, RpcCallError<P::Error>> {
    let permit = loop {
        if let Some(outcome) = state.take_outcome() {
            return finish_call(state.request_id, outcome);
        }

        let notified = state.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(outcome) = state.take_outcome() {
            return finish_call(state.request_id, outcome);
        }

        tokio::select! {
            biased;
            () = sleep_until(state.deadline) => {
                state.expire();
            }
            () = &mut notified => {}
            permit = sender.reserve() => {
                match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        state.service_closed();
                    }
                }
            }
        }
    };

    let envelope = RpcRequest {
        request,
        response: ResponseGuard::new(state.clone()),
    };
    if !state.enqueue(&lifecycle, permit, envelope) {
        return wait_for_result(&state).await;
    }

    wait_for_result(&state).await
}

async fn wait_for_result<P: WidgetProtocol>(
    state: &CallState<P>,
) -> Result<RpcResponse<P::Response>, RpcCallError<P::Error>> {
    loop {
        let notified = state.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(outcome) = state.take_outcome() {
            return finish_call(state.request_id, outcome);
        }

        tokio::select! {
            biased;
            () = sleep_until(state.deadline) => {
                state.expire();
            }
            () = &mut notified => {}
        }
    }
}

fn finish_call<P: WidgetProtocol>(
    request_id: RequestId,
    outcome: CallOutcome<P>,
) -> Result<RpcResponse<P::Response>, RpcCallError<P::Error>> {
    match outcome {
        CallOutcome::Response(Ok(value)) => Ok(RpcResponse { request_id, value }),
        CallOutcome::Response(Err(source)) => Err(RpcCallError::Service { request_id, source }),
        CallOutcome::Failure(CallFailure::DeadlineExceeded) => {
            Err(RpcCallError::DeadlineExceeded { request_id })
        }
        CallOutcome::Failure(CallFailure::ServiceClosed) => {
            Err(RpcCallError::ServiceClosed { request_id })
        }
        CallOutcome::Failure(CallFailure::CallerDropped | CallFailure::ResponseDropped) => {
            Err(RpcCallError::ResponseDropped { request_id })
        }
    }
}

impl<P: WidgetProtocol> Clone for RpcClient<P> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            lifecycle: self.lifecycle.clone(),
            runtime: self.runtime.clone(),
            next_request_id: self.next_request_id.clone(),
            default_timeout: self.default_timeout,
        }
    }
}

impl<P: WidgetProtocol> fmt::Debug for RpcClient<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcClient")
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

pub struct RpcReceiver<P: WidgetProtocol> {
    receiver: mpsc::Receiver<RpcRequest<P>>,
    lifecycle: Arc<ChannelLifecycle>,
}

impl<P: WidgetProtocol> RpcReceiver<P> {
    pub async fn recv(&mut self) -> Option<RpcRequest<P>> {
        self.receiver.recv().await
    }
}

impl<P: WidgetProtocol> Drop for RpcReceiver<P> {
    fn drop(&mut self) {
        self.lifecycle.close();
    }
}

/// RPC request delivered to a background widget service.
pub struct RpcRequest<P: WidgetProtocol> {
    request: P::Request,
    response: ResponseGuard<P>,
}

impl<P: WidgetProtocol> RpcRequest<P> {
    pub fn request_id(&self) -> RequestId {
        self.response.state.request_id
    }

    pub fn deadline(&self) -> Instant {
        self.response.state.deadline
    }

    pub fn is_cancelled(&self) -> bool {
        !self.response.state.is_pending()
    }

    pub fn into_parts(self) -> (RpcRequestContext<P>, P::Request) {
        let Self { request, response } = self;
        (RpcRequestContext { response }, request)
    }
}

impl<P: WidgetProtocol> fmt::Debug for RpcRequest<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcRequest")
            .field("request_id", &self.request_id())
            .field("deadline", &self.deadline())
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Reply handle and cancellation metadata for one in-flight RPC request.
pub struct RpcRequestContext<P: WidgetProtocol> {
    response: ResponseGuard<P>,
}

impl<P: WidgetProtocol> RpcRequestContext<P> {
    pub fn request_id(&self) -> RequestId {
        self.response.state.request_id
    }

    pub fn deadline(&self) -> Instant {
        self.response.state.deadline
    }

    pub fn is_cancelled(&self) -> bool {
        !self.response.state.is_pending()
    }

    pub async fn cancelled(&mut self) {
        self.response.state.cancelled().await;
    }

    /// Delivers a response only if neither caller cancellation nor the absolute deadline won.
    pub fn respond(self, result: Result<P::Response, P::Error>) -> Result<(), RpcResponseDropped> {
        let request_id = self.request_id();
        self.response
            .respond(result)
            .then_some(())
            .ok_or(RpcResponseDropped { request_id })
    }
}

impl<P: WidgetProtocol> fmt::Debug for RpcRequestContext<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcRequestContext")
            .field("request_id", &self.request_id())
            .field("deadline", &self.deadline())
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("RPC response for request {request_id} is no longer deliverable")]
pub struct RpcResponseDropped {
    request_id: RequestId,
}

impl RpcResponseDropped {
    pub const fn request_id(self) -> RequestId {
        self.request_id
    }
}

pub fn channel<P: WidgetProtocol>(
    runtime: &Handle,
    capacity: NonZeroUsize,
    default_timeout: Duration,
) -> (RpcClient<P>, RpcReceiver<P>) {
    let (sender, receiver) = mpsc::channel(capacity.get());
    let lifecycle = Arc::new(ChannelLifecycle::new());
    (
        RpcClient {
            sender,
            lifecycle: lifecycle.clone(),
            runtime: runtime.clone(),
            next_request_id: Arc::new(AtomicU64::new(1)),
            default_timeout,
        },
        RpcReceiver {
            receiver,
            lifecycle,
        },
    )
}

struct ChannelLifecycle {
    open: Mutex<bool>,
}

impl ChannelLifecycle {
    fn new() -> Self {
        Self {
            open: Mutex::new(true),
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        match self.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn close(&self) {
        *self.lock() = false;
    }
}

struct CallState<P: WidgetProtocol> {
    request_id: RequestId,
    deadline: Instant,
    status: Mutex<CallStatus<P>>,
    changed: Notify,
}

impl<P: WidgetProtocol> CallState<P> {
    fn new(request_id: RequestId, deadline: Instant) -> Self {
        Self {
            request_id,
            deadline,
            status: Mutex::new(CallStatus::Pending { submitted: false }),
            changed: Notify::new(),
        }
    }

    fn lock_status(&self) -> MutexGuard<'_, CallStatus<P>> {
        match self.status.lock() {
            Ok(status) => status,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn expire_locked(&self, status: &mut CallStatus<P>) -> bool {
        if matches!(status, CallStatus::Pending { .. }) && Instant::now() >= self.deadline {
            *status = CallStatus::DeadlineExceeded;
            true
        } else {
            false
        }
    }

    fn is_pending(&self) -> bool {
        let (pending, expired) = {
            let mut status = self.lock_status();
            let expired = self.expire_locked(&mut status);
            (matches!(*status, CallStatus::Pending { .. }), expired)
        };
        if expired {
            self.changed.notify_waiters();
        }
        pending
    }

    fn enqueue(
        &self,
        lifecycle: &ChannelLifecycle,
        permit: mpsc::Permit<'_, RpcRequest<P>>,
        request: RpcRequest<P>,
    ) -> bool {
        // Lock order is lifecycle then per-request status; this section has no await.
        let lifecycle = lifecycle.lock();
        if !*lifecycle {
            drop(permit);
            self.service_closed();
            return false;
        }

        let (submitted, expired) = {
            let mut status = self.lock_status();
            let expired = self.expire_locked(&mut status);
            let submitted = match &mut *status {
                CallStatus::Pending { submitted } if !*submitted => {
                    *submitted = true;
                    permit.send(request);
                    true
                }
                _ => false,
            };
            (submitted, expired)
        };
        if expired {
            self.changed.notify_waiters();
        }
        submitted
    }

    fn respond(&self, result: Result<P::Response, P::Error>) -> bool {
        let mut result = Some(result);
        let (accepted, changed) = {
            let mut status = self.lock_status();
            if self.expire_locked(&mut status) {
                (false, true)
            } else if matches!(*status, CallStatus::Pending { .. }) {
                *status = CallStatus::Responded(result.take());
                (true, true)
            } else {
                (false, false)
            }
        };
        if changed {
            self.changed.notify_waiters();
        }
        accepted
    }

    fn expire(&self) -> bool {
        let expired = {
            let mut status = self.lock_status();
            self.expire_locked(&mut status)
        };
        if expired {
            self.changed.notify_waiters();
        }
        expired
    }

    fn caller_dropped(&self) -> bool {
        self.fail(CallFailure::CallerDropped)
    }

    fn service_closed(&self) -> bool {
        self.fail(CallFailure::ServiceClosed)
    }

    fn response_dropped(&self) -> bool {
        self.fail(CallFailure::ResponseDropped)
    }

    fn fail(&self, failure: CallFailure) -> bool {
        let (won, changed) = {
            let mut status = self.lock_status();
            if self.expire_locked(&mut status) {
                (false, true)
            } else if matches!(*status, CallStatus::Pending { .. }) {
                *status = match failure {
                    CallFailure::DeadlineExceeded => CallStatus::DeadlineExceeded,
                    CallFailure::CallerDropped => CallStatus::CallerDropped,
                    CallFailure::ServiceClosed => CallStatus::ServiceClosed,
                    CallFailure::ResponseDropped => CallStatus::ResponseDropped,
                };
                (true, true)
            } else {
                (false, false)
            }
        };
        if changed {
            self.changed.notify_waiters();
        }
        won
    }

    fn take_outcome(&self) -> Option<CallOutcome<P>> {
        let (outcome, expired) = {
            let mut status = self.lock_status();
            let expired = self.expire_locked(&mut status);
            let outcome = match &mut *status {
                CallStatus::Pending { .. } | CallStatus::Responded(None) => None,
                CallStatus::Responded(result) => result.take().map(CallOutcome::Response),
                CallStatus::DeadlineExceeded => {
                    Some(CallOutcome::Failure(CallFailure::DeadlineExceeded))
                }
                CallStatus::CallerDropped => Some(CallOutcome::Failure(CallFailure::CallerDropped)),
                CallStatus::ServiceClosed => Some(CallOutcome::Failure(CallFailure::ServiceClosed)),
                CallStatus::ResponseDropped => {
                    Some(CallOutcome::Failure(CallFailure::ResponseDropped))
                }
            };
            (outcome, expired)
        };
        if expired {
            self.changed.notify_waiters();
        }
        outcome
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_pending() {
                return;
            }

            tokio::select! {
                biased;
                () = sleep_until(self.deadline) => {
                    self.expire();
                }
                () = &mut notified => {}
            }
        }
    }
}

enum CallStatus<P: WidgetProtocol> {
    Pending { submitted: bool },
    Responded(Option<Result<P::Response, P::Error>>),
    DeadlineExceeded,
    CallerDropped,
    ServiceClosed,
    ResponseDropped,
}

enum CallOutcome<P: WidgetProtocol> {
    Response(Result<P::Response, P::Error>),
    Failure(CallFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallFailure {
    DeadlineExceeded,
    CallerDropped,
    ServiceClosed,
    ResponseDropped,
}

struct ResponseGuard<P: WidgetProtocol> {
    state: Arc<CallState<P>>,
    armed: bool,
}

impl<P: WidgetProtocol> ResponseGuard<P> {
    fn new(state: Arc<CallState<P>>) -> Self {
        Self { state, armed: true }
    }

    fn respond(mut self, result: Result<P::Response, P::Error>) -> bool {
        self.armed = false;
        self.state.respond(result)
    }
}

impl<P: WidgetProtocol> Drop for ResponseGuard<P> {
    fn drop(&mut self) {
        if self.armed {
            self.state.response_dropped();
        }
    }
}

struct CallerOnDrop<P: WidgetProtocol> {
    state: Arc<CallState<P>>,
    armed: bool,
}

impl<P: WidgetProtocol> CallerOnDrop<P> {
    fn new(state: Arc<CallState<P>>) -> Self {
        Self { state, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<P: WidgetProtocol> Drop for CallerOnDrop<P> {
    fn drop(&mut self) {
        if self.armed {
            self.state.caller_dropped();
        }
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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
        pin::Pin,
        sync::Arc,
        task::{Context, Poll, Waker},
    };

    use super::*;

    struct TestProtocol;

    impl WidgetProtocol for TestProtocol {
        type Request = u64;
        type Response = u64;
        type StreamKey = ();
        type StreamMessage = ();
        type Error = Infallible;
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    fn test_lifecycle() -> Arc<ChannelLifecycle> {
        Arc::new(ChannelLifecycle::new())
    }

    fn test_state(value: u64, deadline: Instant) -> Arc<CallState<TestProtocol>> {
        Arc::new(CallState::new(RequestId::new(value), deadline))
    }

    fn test_request(value: u64, deadline: Instant) -> RpcRequest<TestProtocol> {
        let state = test_state(value, deadline);
        RpcRequest {
            request: value,
            response: ResponseGuard::new(state),
        }
    }

    fn test_context(state: Arc<CallState<TestProtocol>>) -> RpcRequestContext<TestProtocol> {
        RpcRequestContext {
            response: ResponseGuard::new(state),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn expired_call_is_not_enqueued_when_first_polled_after_deadline() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));

        tokio::time::advance(timeout + Duration::from_secs(1)).await;

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("expired call should finish on its first poll");
        };
        assert!(matches!(
            result.expect_err("call should exceed its deadline"),
            RpcCallError::DeadlineExceeded { .. }
        ));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn call_waiting_for_capacity_is_not_enqueued_after_deadline() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        sender
            .try_send(test_request(99, deadline + timeout))
            .expect("test request should fill the RPC queue");
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());

        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        assert_eq!(
            requests
                .try_recv()
                .expect("deadline should pass before queue capacity is released")
                .request,
            99
        );

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("capacity-blocked call should finish at its deadline");
        };
        assert!(matches!(
            result.expect_err("call should exceed its deadline"),
            RpcCallError::DeadlineExceeded { .. }
        ));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn caller_drop_after_capacity_reservation_prevents_enqueue() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let (sender, mut requests) = mpsc::channel(1);
        let permit = sender
            .reserve()
            .await
            .expect("test queue should remain open");
        let state = test_state(1, deadline);
        let request = RpcRequest {
            request: 7,
            response: ResponseGuard::new(state.clone()),
        };
        drop(CallerOnDrop::new(state.clone()));

        assert!(!state.enqueue(&test_lifecycle(), permit, request));
        assert!(requests.try_recv().is_err());
        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::CallerDropped))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_after_capacity_reservation_prevents_enqueue() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let permit = sender
            .reserve()
            .await
            .expect("test queue should remain open");
        let state = test_state(1, deadline);
        let request = RpcRequest {
            request: 7,
            response: ResponseGuard::new(state.clone()),
        };
        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        assert!(state.expire());

        assert!(!state.enqueue(&test_lifecycle(), permit, request));
        assert!(requests.try_recv().is_err());
        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::DeadlineExceeded))
        ));
    }

    #[tokio::test]
    async fn receiver_drop_after_reservation_rejects_enqueue() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let (client, receiver) = channel::<TestProtocol>(
            current_test_handle(),
            NonZeroUsize::MIN,
            Duration::from_secs(60),
        );
        let permit = client
            .sender
            .reserve()
            .await
            .expect("test queue should remain open");
        let state = test_state(1, deadline);
        let request = RpcRequest {
            request: 7,
            response: ResponseGuard::new(state.clone()),
        };

        drop(receiver);
        assert!(!state.enqueue(&client.lifecycle, permit, request));

        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::ServiceClosed))
        ));
        assert_eq!(Arc::strong_count(&state), 1);
    }

    #[tokio::test]
    async fn enqueue_before_receiver_drop_linearizes_submission() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let (client, receiver) = channel::<TestProtocol>(
            current_test_handle(),
            NonZeroUsize::MIN,
            Duration::from_secs(60),
        );
        let permit = client
            .sender
            .reserve()
            .await
            .expect("test queue should remain open");
        let state = test_state(1, deadline);
        let request = RpcRequest {
            request: 7,
            response: ResponseGuard::new(state.clone()),
        };

        assert!(state.enqueue(&client.lifecycle, permit, request));
        drop(receiver);

        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::ResponseDropped))
        ));
    }

    #[test]
    fn caller_drop_before_responder_rejects_response() {
        let state = test_state(1, Instant::now() + Duration::from_secs(60));
        let caller = CallerOnDrop::new(state.clone());
        let reply = test_context(state.clone());

        drop(caller);
        let response = reply.respond(Ok(7));

        assert!(response.is_err());
        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::CallerDropped))
        ));
    }

    #[test]
    fn responder_before_caller_drop_preserves_response() {
        let state = test_state(1, Instant::now() + Duration::from_secs(60));
        let caller = CallerOnDrop::new(state.clone());
        let reply = test_context(state.clone());

        reply.respond(Ok(7)).expect("response should win");
        drop(caller);

        let Some(CallOutcome::Response(Ok(value))) = state.take_outcome() else {
            panic!("accepted response should remain the winner");
        };
        assert_eq!(value, 7);
    }

    #[test]
    fn into_parts_moves_the_response_guard_without_dropping_it() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let state = test_state(1, deadline);
        let request = RpcRequest {
            request: 7,
            response: ResponseGuard::new(state.clone()),
        };

        let (reply, value) = request.into_parts();

        assert_eq!(value, 7);
        assert!(state.is_pending());
        reply
            .respond(Ok(value))
            .expect("moved response guard should remain deliverable");
    }

    #[tokio::test(start_paused = true)]
    async fn response_before_deadline_survives_pending_poll_and_late_caller_poll() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, value) = requests
            .try_recv()
            .expect("request should be delivered")
            .into_parts();
        assert!(poll_once(call.as_mut()).is_pending());

        reply
            .respond(Ok(value))
            .expect("response should win before the deadline");
        tokio::time::advance(timeout + Duration::from_secs(1)).await;

        let Poll::Ready(response) = poll_once(call.as_mut()) else {
            panic!("accepted response should wake the caller");
        };
        assert_eq!(
            response
                .expect("accepted response should survive the deadline")
                .into_value(),
            7
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_before_response_rejects_response() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, _) = requests
            .try_recv()
            .expect("request should be delivered")
            .into_parts();
        assert!(poll_once(call.as_mut()).is_pending());

        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("deadline should finish the call");
        };
        assert!(matches!(
            result.expect_err("deadline should win"),
            RpcCallError::DeadlineExceeded { .. }
        ));
        assert!(reply.respond(Ok(7)).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn late_response_claims_deadline_even_before_caller_is_repolled() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, _) = requests
            .try_recv()
            .expect("request should be delivered")
            .into_parts();

        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        assert!(reply.respond(Ok(7)).is_err());

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("late response should record the deadline winner");
        };
        assert!(matches!(
            result.expect_err("deadline should win"),
            RpcCallError::DeadlineExceeded { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn response_context_drop_before_deadline_remains_response_dropped_after_deadline() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, _) = requests
            .try_recv()
            .expect("request should be delivered")
            .into_parts();

        drop(reply);
        tokio::time::advance(timeout + Duration::from_secs(1)).await;

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("response drop should finish the call");
        };
        assert!(matches!(
            result.expect_err("response drop should remain the winner"),
            RpcCallError::ResponseDropped { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn response_context_drop_after_deadline_is_deadline_exceeded() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, _) = requests
            .try_recv()
            .expect("request should be delivered")
            .into_parts();

        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        drop(reply);

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("deadline should finish the call");
        };
        assert!(matches!(
            result.expect_err("deadline should beat a late response drop"),
            RpcCallError::DeadlineExceeded { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn queued_request_drop_before_deadline_is_response_dropped() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let (sender, mut requests) = mpsc::channel(1);
        let state = test_state(1, deadline);
        let mut call = Box::pin(execute_call::<TestProtocol>(
            sender,
            test_lifecycle(),
            7,
            state,
        ));
        assert!(poll_once(call.as_mut()).is_pending());

        drop(requests.try_recv().expect("request should be delivered"));

        let Poll::Ready(result) = poll_once(call.as_mut()) else {
            panic!("request drop should finish the call");
        };
        assert!(matches!(
            result.expect_err("request drop should be classified"),
            RpcCallError::ResponseDropped { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn caller_drop_rejects_response_synchronously() {
        let timeout = Duration::from_secs(5);
        let (client, mut requests) =
            channel::<TestProtocol>(current_test_handle(), NonZeroUsize::MIN, timeout);
        let mut call = Box::pin(client.call(7));
        assert!(poll_once(call.as_mut()).is_pending());
        let (reply, _) = requests
            .recv()
            .await
            .expect("request should be delivered")
            .into_parts();

        drop(call);

        assert!(reply.is_cancelled());
        assert!(reply.respond(Ok(7)).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_beats_caller_drop_after_deadline() {
        let timeout = Duration::from_secs(5);
        let deadline = Instant::now() + timeout;
        let state = test_state(1, deadline);
        let caller = CallerOnDrop::new(state.clone());

        tokio::time::advance(timeout + Duration::from_secs(1)).await;
        drop(caller);

        assert!(matches!(
            state.take_outcome(),
            Some(CallOutcome::Failure(CallFailure::DeadlineExceeded))
        ));
    }
}
