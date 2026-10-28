//! The AIS protocol, its background service, and the seams a test drives it through.
//!
//! # One stream, keyed by area
//!
//! A pane follows one AIS area at a time, and the area is the stream's key: what the
//! server pushes is a complete picture of an area (`keyframe`) followed by increments
//! that only apply on top of the picture the pane already holds. Switching areas is
//! therefore switching streams rather than adding one, and a pane reading an area is
//! reading exactly one.
//!
//! # The request never waits
//!
//! Taking the account's session and opening the subscription are both slow operations,
//! each bounded by the service's own operation timeout, while a widget RPC carries a
//! shorter deadline. Both therefore happen inside the stream task: `Load` starts the
//! stream and answers at once, and everything that can fail is reported on the stream
//! rather than through the reply. A `Load` whose caller already went away starts
//! nothing, because nobody would read the stream it asked for.
//!
//! # A failure is reported once, and then held
//!
//! A stream that can no longer deliver publishes its failure on the reliable channel
//! and waits for the replacement the pane asks for when the user recovers. The task may
//! not end on its own: a task that ended retires its key's latest entry, and the pane
//! would have to have read the failure before that happened. Nothing here retries or
//! backs off — a terminal failure stays terminal until the user acts.

use std::{future::Future, sync::Arc};

use gpui_kit::{App, SharedString};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{ErrorCode, KytrixAccountSession, KytrixServiceError},
    global_shipping_chokepoint::{GlobalShippingChokepointArea, GlobalShippingChokepointUpdate},
};
use network::transport::{error::TransportError, subscription::Subscription};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::Instant;
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use super::model::MAX_VESSELS;

/// The account whose AIS picture a stream carries, and the area it covers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct GlobalShippingChokepointScope {
    pub account: SmolStr,
    pub area: GlobalShippingChokepointArea,
}

impl GlobalShippingChokepointScope {
    /// The scope an account follows. A pane reads the one area the map draws.
    pub(super) fn new(account: SmolStr) -> Self {
        Self {
            account,
            area: GlobalShippingChokepointArea::Hormuz,
        }
    }
}

/// What a pane asks its service to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GlobalShippingChokepointRequest {
    /// Follows the scope's area, replacing whatever stream is running.
    Load {
        scope: GlobalShippingChokepointScope,
    },
    /// Follows nothing, because nobody is signed in.
    Stop,
}

/// What the service answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GlobalShippingChokepointResponse {
    /// The area's stream is running.
    StreamStarted,
    /// Nothing is followed.
    Stopped,
}

/// Why a stream stopped, or why it never started.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum AisStreamError {
    /// The account's session could not be taken, or the subscription was refused.
    #[error(transparent)]
    Service(#[from] KytrixServiceError),
    /// The subscription closed, or what it delivered could not be read.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl LocalizedError for AisStreamError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            // An account without AIS access is a refusal about the subscription, not a
            // connection failure that asking again would fix, so it is named as itself.
            Self::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired)) => {
                i18n::t!(cx, "widget.global_shipping_chokepoint.entitlement_required")
            }
            Self::Service(error) => error.localized_message(cx),
            Self::Transport(error) => error.localized_message(cx),
        }
    }
}

/// One message of an area's stream.
#[derive(Clone, Debug)]
pub(super) enum AisStreamMessage {
    /// One push from the server: a keyframe, or an increment continuing the picture.
    Update(GlobalShippingChokepointUpdate),
    /// The stream is over; this travels reliably so the pane is told before the entry
    /// that would have carried it is retired.
    ///
    /// The error is shared rather than cloned into the view's failure state: the view is
    /// handed the message behind an `Arc`, and what it reports to the framework is also
    /// an `Arc`, so one failure is one allocation from the stream to the banner.
    Failed(Arc<AisStreamError>),
}

pub(super) struct GlobalShippingChokepointProtocol;

impl WidgetProtocol for GlobalShippingChokepointProtocol {
    type Request = GlobalShippingChokepointRequest;
    type Response = GlobalShippingChokepointResponse;
    type StreamKey = GlobalShippingChokepointArea;
    type StreamMessage = AisStreamMessage;
    type Error = AisStreamError;
}

/// The Kytrix service an AIS picture is read through.
///
/// The service is written against this seam rather than against the connector, so a test
/// can drive every state a session and a subscription can be in without one.
pub(super) trait GlobalShippingChokepointSource: Clone + Send + Sync + 'static {
    type Session: GlobalShippingChokepointSession;

    /// Takes the account's session, which the area's subscription is opened through.
    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

/// One account's session.
pub(super) trait GlobalShippingChokepointSession: Clone + Send + Sync + 'static {
    /// The subscription one area is followed through.
    ///
    /// It is an associated type rather than the concrete `Subscription<GlobalShippingChokepointUpdate>` so a
    /// test can hand in a subscription it drives and counts the drops of: releasing a
    /// replaced stream's subscription is what keeps the pane off an account it has left.
    type Subscription: GlobalShippingChokepointEventSubscription;

    fn subscribe_global_shipping_chokepoint(
        &self,
        area: GlobalShippingChokepointArea,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;
}

/// One live AIS subscription.
pub(super) trait GlobalShippingChokepointEventSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Result<GlobalShippingChokepointUpdate, TransportError>> + Send;
}

impl GlobalShippingChokepointSource for KytrixServiceConnector {
    type Session = KytrixAccountSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.acquire_account_session(account_id).await
    }
}

impl GlobalShippingChokepointSession for KytrixAccountSession {
    type Subscription = Subscription<GlobalShippingChokepointUpdate>;

    async fn subscribe_global_shipping_chokepoint(
        &self,
        area: GlobalShippingChokepointArea,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixAccountSession::subscribe_global_shipping_chokepoint(self, area).await
    }
}

impl GlobalShippingChokepointEventSubscription for Subscription<GlobalShippingChokepointUpdate> {
    async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
        Subscription::recv(self).await
    }
}

/// The service that follows one pane's AIS picture.
pub(super) struct GlobalShippingChokepointService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> GlobalShippingChokepointService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<GlobalShippingChokepointProtocol> for GlobalShippingChokepointService<S>
where
    S: GlobalShippingChokepointSource,
{
    async fn run(self, mut context: ServiceContext<GlobalShippingChokepointProtocol>) {
        let mut active: Option<GlobalShippingChokepointArea> = None;
        let source = self.source.clone();

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                GlobalShippingChokepointRequest::Stop => {
                    if let Some(area) = active.take() {
                        context.cancel_stream(&area);
                    }
                    let _ = reply.respond(Ok(GlobalShippingChokepointResponse::Stopped));
                }
                GlobalShippingChokepointRequest::Load { scope } => {
                    // The key is the area, so it is taken before the scope is moved into
                    // the stream task, which needs the account as well.
                    let key = scope.area;
                    if let Some(previous) = active.replace(key) {
                        context.cancel_stream(&previous);
                    }
                    // `next_rpc` already dropped expired requests; re-check before
                    // starting anything, so a caller that went away cannot leave a
                    // stream behind.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        let _ = reply.respond(Ok(GlobalShippingChokepointResponse::StreamStarted));
                        continue;
                    }
                    let source = source.clone();
                    // No `await` in this branch: the session and the subscription are
                    // taken inside the stream task, so the RPC deadline never races the
                    // operation timeout of either.
                    context.start_stream(key, move |publisher| async move {
                        run_stream(source, scope, publisher).await;
                    });
                    let _ = reply.respond(Ok(GlobalShippingChokepointResponse::StreamStarted));
                }
            }
        }
    }
}

/// Runs one area's subscription until it fails or is replaced.
async fn run_stream<S>(
    source: S,
    scope: GlobalShippingChokepointScope,
    publisher: StreamPublisher<GlobalShippingChokepointProtocol>,
) where
    S: GlobalShippingChokepointSource,
{
    // Slow operations all live here, each one interruptible by a replacement or a stop,
    // and each failure reported on the stream that asked for it.
    let session = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = source.acquire_session(&scope.account) => result,
    };
    let session = match session {
        Ok(session) => session,
        Err(error) => return publish_failure(&publisher, error.into()).await,
    };

    let subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = session.subscribe_global_shipping_chokepoint(scope.area) => result,
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => return publish_failure(&publisher, error.into()).await,
    };

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => break,
            received = subscription.recv() => match received {
                Ok(update) => {
                    let update = clamp_to_capacity(update);
                    if publisher
                        .publish_latest(AisStreamMessage::Update(update))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    publish_failure(&publisher, error.into()).await;
                    break;
                }
            }
        }
    }
}

/// Trims a push to the number of vessels the map may hold.
///
/// The server already selects at most [`MAX_VESSELS`] vessels by distance, so this is
/// the defence against a malformed push rather than the way the set is chosen. Only the
/// positions are trimmed; the ships are left alone, because they describe vessels the
/// picture may still hold. Which vessels the picture holds is never decided here: deltas
/// are passed through rather than evicted against a running set the client would then
/// have picked itself.
fn clamp_to_capacity(mut update: GlobalShippingChokepointUpdate) -> GlobalShippingChokepointUpdate {
    if update.positions.len() > MAX_VESSELS {
        update.positions.truncate(MAX_VESSELS);
    }
    update
}

/// Publishes the failure a stream ended with, and holds the stream until it is replaced.
///
/// A terminal failure is not retried: the pane asks for a new stream when the user takes
/// the recovery. Until then the task stays where it is rather than returning, because a
/// task that ended retires the key's latest entry before the pane could read it.
async fn publish_failure(
    publisher: &StreamPublisher<GlobalShippingChokepointProtocol>,
    error: AisStreamError,
) {
    let _ = publisher
        .publish_reliable(AisStreamMessage::Failed(Arc::new(error)))
        .await;
    publisher.cancelled().await;
}

#[cfg(test)]
mod tests {

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one: the
    /// streams and RPCs it spawns have to be polled by the same executor the test is
    /// awaiting on.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the tasks it awaits are the ones it spawned"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex, MutexGuard,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use kytrix_service_provider::service::global_shipping_chokepoint::GlobalShippingChokepointPosition;
    use tokio::sync::{Notify, mpsc};
    use widget::{
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;

    /// The account every fixture scope belongs to.
    const ACCOUNT: &str = "test-account";

    fn scope() -> GlobalShippingChokepointScope {
        GlobalShippingChokepointScope::new(SmolStr::new_static(ACCOUNT))
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<GlobalShippingChokepointProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            GlobalShippingChokepointService::new(source.clone()),
        )
    }

    async fn load(
        runtime: &WidgetRuntime<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointResponse {
        runtime
            .client()
            .call(GlobalShippingChokepointRequest::Load { scope: scope() })
            .await
            .expect("the stream should start")
            .into_value()
    }

    async fn stop(
        runtime: &WidgetRuntime<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointResponse {
        runtime
            .client()
            .call(GlobalShippingChokepointRequest::Stop)
            .await
            .expect("stopping should be answered")
            .into_value()
    }

    fn position(mmsi: u32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude: 0,
            longitude: 0,
            sog: 0,
            cog: 0,
            heading: 0,
            nav_status: 0,
            last_update: 0,
        }
    }

    fn update(
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch: 7,
            keyframe: false,
            sequence,
            base: sequence.saturating_sub(1),
            ships: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1,
        }
    }

    /// Lets everything else on this runtime run until `ready` holds.
    async fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let settled = tokio::time::timeout(Duration::from_secs(2), async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(settled.is_ok(), "{what} should happen");
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<GlobalShippingChokepointProtocol>,
        predicate: impl Fn(&AisStreamMessage) -> bool,
    ) -> AisStreamMessage {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = reliable
                    .recv()
                    .await
                    .expect("the reliable stream should stay open");
                if predicate(envelope.message().as_ref()) {
                    return envelope.message().as_ref().clone();
                }
            }
        })
        .await
        .expect("the expected reliable message should arrive")
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<GlobalShippingChokepointProtocol>,
    ) -> AisStreamError {
        let message = next_reliable(reliable, |message| {
            matches!(message, AisStreamMessage::Failed(_))
        })
        .await;
        let AisStreamMessage::Failed(error) = message else {
            panic!("the message should be a failure");
        };
        (*error).clone()
    }

    async fn next_update(
        latest: &mut LatestStream<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let AisStreamMessage::Update(update) = envelope.message().as_ref() {
                        return update.clone();
                    }
                }
            }
        })
        .await
        .expect("an update should be published")
    }

    #[tokio::test]
    async fn a_load_should_start_the_area_stream_and_answer_stream_started() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );

        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.positions, [position(1)]);

        assert_eq!(source.requested(), [SmolStr::new_static(ACCOUNT)]);
        assert_eq!(
            session.state().areas.as_slice(),
            [GlobalShippingChokepointArea::Hormuz]
        );
        assert_eq!(session.state().subscribes, 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_repeated_load_should_replace_the_stream_and_let_the_old_subscription_go() {
        let source = FakeSource::default();
        let first = source.session(ACCOUNT);
        let first_control = first.subscribe();
        let second = source.session(ACCOUNT);
        let second_control = second.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        first_control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the first subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.sequence, 1);

        load(&runtime).await;
        wait_for("the replaced stream should let its subscription go", || {
            first_control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        second_control
            .events
            .send(Ok(update(2, vec![position(2)])))
            .await
            .expect("the second subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.sequence, 2);

        assert_eq!(source.sessions(), 2, "a load takes the account again");
        assert_eq!(
            second_control.drops.load(Ordering::SeqCst),
            0,
            "the stream the pane is reading keeps its subscription"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_stop_should_cancel_the_stream_and_answer_stopped() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        let _ = next_update(&mut latest).await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        wait_for("the stopped stream should let its subscription go", || {
            control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped,
            "stopping nothing is still an answer"
        );
        assert!(
            latest.next_unseen().is_none(),
            "a cancelled stream publishes nothing more"
        );
        runtime.shutdown().await;
    }

    /// The whole point of the request not waiting: the answer arrives while the session
    /// is still being taken, and cancellation is the only way that acquisition ends.
    #[tokio::test]
    async fn a_load_should_answer_before_the_session_is_taken() {
        let source = FakeSource::default();
        let block = source.block();
        let runtime = runtime(&source);

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        block.wait_started().await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        block.wait_dropped().await;
        runtime.shutdown().await;
    }

    /// A subscription that never opens is cancelled the same way an acquisition is.
    #[tokio::test]
    async fn a_subscription_that_never_opens_should_be_cancelled_with_its_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let block = session.block_subscription();
        let runtime = runtime(&source);

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        block.wait_started().await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        block.wait_dropped().await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_session_that_could_not_be_taken_should_fail_the_stream_rather_than_the_load() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Server(ErrorCode::EntitlementRequired));
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        );
        assert!(
            latest.next_unseen().is_none(),
            "a stream that never started publishes no update"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_that_could_not_be_opened_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.refuse_subscription(KytrixServiceError::Closed);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Closed)
        );
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    /// A failed stream is replaced rather than retried by itself: the pane asks again,
    /// and the load is what lets the parked failure task go.
    #[tokio::test]
    async fn a_load_after_a_failure_should_replace_the_failed_stream() {
        let source = FakeSource::default();
        let failed = source.session(ACCOUNT);
        failed.refuse_subscription(KytrixServiceError::Closed);
        let recovered = source.session(ACCOUNT);
        let control = recovered.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Closed)
        );

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the recovered subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.positions, [position(1)]);
        assert_eq!(source.sessions(), 2, "a recovery takes the account again");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_broken_subscription_should_fail_the_stream_with_its_transport_error() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        control
            .events
            .send(Err(TransportError::Closed))
            .await
            .expect("the subscription should break");
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Transport(TransportError::Closed)
        );
        assert!(
            latest.next_unseen().is_none(),
            "a broken stream publishes no update of its own"
        );
        // A stream that returned once its failure was published would have retired the
        // key's latest entry with it, which is the reason the failure is held instead.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while control.drops.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "a failed stream holds its subscription until the pane replaces it"
        );

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        wait_for("the replaced stream should let its subscription go", || {
            control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        runtime.shutdown().await;
    }

    #[test]
    fn an_entitlement_refusal_should_stay_classifiable_as_the_service_error_it_is() {
        let error =
            AisStreamError::from(KytrixServiceError::Server(ErrorCode::EntitlementRequired));
        assert!(matches!(
            error,
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        ));

        let error = AisStreamError::from(KytrixServiceError::Closed);
        assert!(matches!(
            error,
            AisStreamError::Service(KytrixServiceError::Closed)
        ));
    }

    #[test]
    fn a_push_over_capacity_should_be_clipped_to_the_vessel_limit() {
        let over = clamp_to_capacity(update(1, (0..=MAX_VESSELS as u32).map(position).collect()));
        assert_eq!(over.positions.len(), MAX_VESSELS);

        let at_limit =
            clamp_to_capacity(update(2, (0..MAX_VESSELS as u32).map(position).collect()));
        assert_eq!(
            at_limit.positions.len(),
            MAX_VESSELS,
            "a push that fits is passed through untouched"
        );
    }

    #[tokio::test]
    async fn an_empty_increment_should_still_be_published_as_an_update() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, Vec::new())))
            .await
            .expect("a heartbeat is a push like any other");
        let published = next_update(&mut latest).await;

        assert_eq!(published.sequence, 1);
        assert!(
            published.positions.is_empty(),
            "a tick that changed nothing is still the tick the pane draws"
        );
        assert!(!published.keyframe);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutting_down_should_let_the_subscription_go() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        let _ = next_update(&mut latest).await;

        runtime.shutdown().await;
        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "shutdown should let the subscription go rather than leak it"
        );
    }

    #[derive(Clone, Default)]
    struct FakeSource {
        state: Arc<Mutex<FakeSourceState>>,
    }

    #[derive(Default)]
    struct FakeSourceState {
        sessions: VecDeque<FakeSetup>,
        acquired: Vec<SmolStr>,
    }

    /// What the next `acquire_session` call finds.
    enum FakeSetup {
        /// A session for the account that was asked for.
        Ready(FakeSession),
        /// A refusal, the way an account whose credentials are gone answers.
        Failed(KytrixServiceError),
        /// An acquisition that never answers, so cancellation is the only way out.
        Blocked(Arc<OperationBlock>),
    }

    impl FakeSource {
        /// Queues a session for `account`, which is what the next acquisition finds.
        fn session(&self, account: &str) -> FakeSession {
            let session = FakeSession::new(account);
            self.state()
                .sessions
                .push_back(FakeSetup::Ready(session.clone()));
            session
        }

        /// Queues a refusal in place of a session.
        fn refuse(&self, error: KytrixServiceError) {
            self.state().sessions.push_back(FakeSetup::Failed(error));
        }

        /// Queues an acquisition that never answers.
        fn block(&self) -> Arc<OperationBlock> {
            let block = Arc::new(OperationBlock::default());
            self.state()
                .sessions
                .push_back(FakeSetup::Blocked(block.clone()));
            block
        }

        /// The accounts the service has asked for a session of, in the order it asked.
        fn requested(&self) -> Vec<SmolStr> {
            self.state().acquired.clone()
        }

        /// How many sessions the service has asked for.
        fn sessions(&self) -> usize {
            self.state().acquired.len()
        }

        fn state(&self) -> MutexGuard<'_, FakeSourceState> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl GlobalShippingChokepointSource for FakeSource {
        type Session = FakeSession;

        async fn acquire_session(
            &self,
            account_id: &str,
        ) -> Result<Self::Session, KytrixServiceError> {
            let setup = {
                let mut state = self.state();
                state.acquired.push(SmolStr::new(account_id));
                state
                    .sessions
                    .pop_front()
                    .unwrap_or(FakeSetup::Failed(KytrixServiceError::Closed))
            };
            match setup {
                FakeSetup::Ready(session) => Ok(session),
                FakeSetup::Failed(error) => Err(error),
                FakeSetup::Blocked(block) => {
                    let _guard = OperationGuard {
                        block: block.clone(),
                    };
                    block.started.notify_one();
                    std::future::pending().await
                }
            }
        }
    }

    /// One account's session, as a test drives it.
    #[derive(Clone)]
    struct FakeSession {
        state: Arc<Mutex<FakeSessionState>>,
    }

    #[derive(Default)]
    struct FakeSessionState {
        subscriptions: VecDeque<FakeSubscribe>,
        subscribes: usize,
        areas: Vec<GlobalShippingChokepointArea>,
    }

    /// What the next `subscribe_global_shipping_chokepoint` call finds.
    enum FakeSubscribe {
        /// A subscription the test drives.
        Ready(FakeSubscription),
        /// A subscription the account is refused.
        Failed(KytrixServiceError),
        /// A subscription that never opens, so cancellation is the only way out.
        Blocked(Arc<OperationBlock>),
    }

    impl FakeSession {
        fn new(_account: &str) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeSessionState::default())),
            }
        }

        /// Queues one live subscription and hands back the knobs that drive it.
        fn subscribe(&self) -> FakeSubscriptionControl {
            let (events, receiver) = mpsc::channel(8);
            let drops = Arc::new(AtomicUsize::new(0));
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Ready(FakeSubscription {
                    events: receiver,
                    drops: drops.clone(),
                }));
            FakeSubscriptionControl { events, drops }
        }

        /// Queues a subscription the account is refused.
        fn refuse_subscription(&self, error: KytrixServiceError) {
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Failed(error));
        }

        /// Queues a subscription that never opens.
        fn block_subscription(&self) -> Arc<OperationBlock> {
            let block = Arc::new(OperationBlock::default());
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Blocked(block.clone()));
            block
        }

        fn state(&self) -> MutexGuard<'_, FakeSessionState> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl GlobalShippingChokepointSession for FakeSession {
        type Subscription = FakeSubscription;

        async fn subscribe_global_shipping_chokepoint(
            &self,
            area: GlobalShippingChokepointArea,
        ) -> Result<Self::Subscription, KytrixServiceError> {
            let next = {
                let mut state = self.state();
                state.subscribes += 1;
                state.areas.push(area);
                state
                    .subscriptions
                    .pop_front()
                    .unwrap_or(FakeSubscribe::Failed(KytrixServiceError::Closed))
            };
            match next {
                FakeSubscribe::Ready(subscription) => Ok(subscription),
                FakeSubscribe::Failed(error) => Err(error),
                FakeSubscribe::Blocked(block) => {
                    let _guard = OperationGuard {
                        block: block.clone(),
                    };
                    block.started.notify_one();
                    std::future::pending().await
                }
            }
        }
    }

    /// One live subscription inside the fake session.
    struct FakeSubscription {
        events: mpsc::Receiver<Result<GlobalShippingChokepointUpdate, TransportError>>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl GlobalShippingChokepointEventSubscription for FakeSubscription {
        async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
            self.events
                .recv()
                .await
                .unwrap_or(Err(TransportError::Closed))
        }
    }

    /// One live subscription, as a test drives it.
    struct FakeSubscriptionControl {
        events: mpsc::Sender<Result<GlobalShippingChokepointUpdate, TransportError>>,
        /// How many times this subscription has been dropped, which is how a test sees
        /// the service let go of a stream it no longer follows.
        drops: Arc<AtomicUsize>,
    }

    /// One operation a fake can be held inside.
    ///
    /// Holding one is how a test pins the window an operation is open in: a session or
    /// subscription that is held has not answered yet, so what the service does while it
    /// is in flight is what the test is looking at rather than a race against the
    /// answer.
    #[derive(Default)]
    struct OperationBlock {
        started: Notify,
        dropped: AtomicBool,
    }

    struct OperationGuard {
        block: Arc<OperationBlock>,
    }

    impl Drop for OperationGuard {
        fn drop(&mut self) {
            self.block.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl OperationBlock {
        async fn wait_started(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.started.notified())
                .await
                .expect("the blocked operation should start");
        }

        async fn wait_dropped(&self) {
            wait_for("the blocked operation to be dropped", || {
                self.dropped.load(Ordering::SeqCst)
            })
            .await;
        }
    }
}
