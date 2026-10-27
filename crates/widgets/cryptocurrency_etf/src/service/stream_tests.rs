//! The poll loop, driven through the framework's own runtime.
//!
//! A test here owns a real [`WidgetRuntime`] and a real Tokio clock, because what
//! is being tested is the loop's *timing*: that the first fetch does not wait out
//! an interval, that a manual refresh resets the schedule rather than racing it,
//! and that a failure stops the schedule instead of retrying it. A fake source is
//! the only thing that is replaced.

use std::{sync::Arc, time::Duration};

use smol_str::SmolStr;
use tokio::time::advance;
use widget::{
    rpc::RpcCallError,
    runtime::WidgetRuntime,
    stream::{LatestStream, ReliableStream},
};

use super::{
    stream::*,
    test_support::{FakeSource, FetchPlan},
};
use crate::model::EtfSnapshot;

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

fn stream_key(account_id: &str) -> EtfStreamKey {
    EtfStreamKey {
        account_id: SmolStr::new(account_id),
    }
}

fn account() -> EtfStreamKey {
    stream_key("test-account")
}

fn test_service(source: FakeSource) -> EtfService<FakeSource> {
    EtfService::with_timing(source, REFRESH_INTERVAL, ATTEMPT_TIMEOUT)
}

async fn receive_status(reliable: &mut ReliableStream<EtfProtocol>) -> EtfStatus {
    receive_status_for(reliable, &account()).await
}

/// Reads one status, asserting it belongs to `key`.
async fn receive_status_for(
    reliable: &mut ReliableStream<EtfProtocol>,
    key: &EtfStreamKey,
) -> EtfStatus {
    let envelope = reliable
        .recv()
        .await
        .expect("the status stream should remain open");
    assert_eq!(
        envelope.key(),
        key,
        "a status names the account it is about"
    );
    let EtfStreamMessage::Status(status) = envelope.message().as_ref() else {
        panic!("the reliable channel should only carry statuses");
    };
    status.clone()
}

async fn receive_snapshot(latest: &mut LatestStream<EtfProtocol>) -> Arc<EtfSnapshot> {
    latest
        .changed()
        .await
        .expect("a snapshot should be published");
    let envelope = latest
        .next_unseen()
        .expect("the published snapshot should be unseen");
    let EtfStreamMessage::Snapshot { snapshot, .. } = envelope.message().as_ref() else {
        panic!("the latest channel should only carry snapshots");
    };
    snapshot.clone()
}

/// The flow of the one day the fake's snapshot carries, which is how two answers
/// are told apart.
fn flow(snapshot: &EtfSnapshot) -> f64 {
    snapshot
        .dataset(crate::model::EtfAsset::Btc)
        .days
        .first()
        .expect("the fake answers with one day")
        .total_flow
}

/// Starts a runtime watching `account()` and consumes the four messages one
/// successful load publishes.
async fn load_and_settle(
    runtime: &WidgetRuntime<EtfProtocol>,
    reliable: &mut ReliableStream<EtfProtocol>,
    latest: &mut LatestStream<EtfProtocol>,
) {
    let response = runtime
        .client()
        .call(EtfRequest::Load(account()))
        .await
        .expect("the load should be accepted");
    assert_eq!(response.into_value(), EtfResponse::StreamStarted);
    assert!(matches!(
        receive_status(reliable).await,
        EtfStatus::Connecting
    ));
    assert!(matches!(
        receive_status(reliable).await,
        EtfStatus::Refreshing { .. }
    ));
    let _ = receive_snapshot(latest).await;
    assert!(matches!(
        receive_status(reliable).await,
        EtfStatus::Ready { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn fetch_changed_snapshot_should_issue_one_cancellable_source_call() {
    let source = FakeSource::new([FetchPlan::Pending]);
    let task_source = source.clone();
    let task = tokio::spawn(async move { fetch_changed_snapshot(&task_source, None).await });

    for _ in 0..100 {
        if source.fetch_count() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(source.maximum_in_flight(), 1);

    task.abort();
    let _ = task.await;

    assert_eq!(source.in_flight(), 0);
    assert_eq!(source.cancelled_fetches(), 1);
}

#[tokio::test]
async fn fetch_changed_snapshot_should_reuse_an_unchanged_previous_snapshot() {
    let previous = super::test_support::snapshot(1.0);
    let source = FakeSource::new([FetchPlan::Snapshot(1.0)]);

    let (actual, changed) = fetch_changed_snapshot(&source, Some(&previous))
        .await
        .expect("an unchanged answer should be accepted");

    assert!(!changed, "an answer that moved nothing is not a change");
    assert!(
        Arc::ptr_eq(&actual, &previous),
        "the allocation of an unchanged answer is the one that is kept"
    );
}

#[tokio::test(start_paused = true)]
async fn a_load_should_fetch_at_once_and_publish_the_snapshot_then_readiness() {
    let source = FakeSource::new([FetchPlan::Snapshot(7.0)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    let response = runtime
        .client()
        .call(EtfRequest::Load(account()))
        .await
        .expect("the load should be accepted");
    assert_eq!(response.into_value(), EtfResponse::StreamStarted);

    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Connecting
    ));
    let EtfStatus::Refreshing { next_refresh_at } = receive_status(&mut reliable).await else {
        panic!("a fetch should be reported before it starts");
    };
    let snapshot = receive_snapshot(&mut latest).await;
    assert_eq!(flow(snapshot.as_ref()), 7.0);
    match receive_status(&mut reliable).await {
        EtfStatus::Ready {
            revision,
            next_refresh_at: ready_at,
        } => {
            assert_eq!(revision, 1, "the first snapshot is revision one");
            assert_eq!(
                ready_at, next_refresh_at,
                "readiness carries the same deadline the fetch announced"
            );
        }
        other => panic!("expected readiness, found {other:?}"),
    }

    assert_eq!(source.fetch_count(), 1);
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_unchanged_answer_should_not_bump_the_revision() {
    let source = FakeSource::new([
        FetchPlan::Snapshot(7.0),
        FetchPlan::Snapshot(7.0),
        FetchPlan::Snapshot(7.0),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    load_and_settle(&runtime, &mut reliable, &mut latest).await;

    // The same answer again: the service answered, but nothing about the data
    // changed, so there is nothing to draw and nothing to publish.
    advance(REFRESH_INTERVAL).await;
    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Refreshing { .. }
    ));
    match receive_status(&mut reliable).await {
        EtfStatus::Ready { revision, .. } => {
            assert_eq!(revision, 1, "an unchanged answer is not a new revision");
        }
        other => panic!("expected readiness, found {other:?}"),
    }
    assert_eq!(source.fetch_count(), 2);

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_manual_refresh_should_fetch_again_and_reset_the_schedule() {
    let source = FakeSource::new([FetchPlan::Snapshot(7.0), FetchPlan::Snapshot(9.0)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    load_and_settle(&runtime, &mut reliable, &mut latest).await;

    // A manual refresh brings the next automatic fetch forward: it fetches now
    // and schedules the interval from now, rather than from where the old
    // schedule had got to.
    advance(Duration::from_secs(60)).await;
    let response = runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect("a refresh should be queued");
    assert_eq!(response.into_value(), EtfResponse::RefreshQueued);
    let EtfStatus::Refreshing { next_refresh_at } = receive_status(&mut reliable).await else {
        panic!("a manual refresh reports its fetch like any other");
    };
    let snapshot = receive_snapshot(&mut latest).await;
    assert_eq!(flow(snapshot.as_ref()), 9.0);
    match receive_status(&mut reliable).await {
        EtfStatus::Ready {
            revision,
            next_refresh_at: ready_at,
        } => {
            assert_eq!(revision, 2, "a changed answer is a new revision");
            assert_eq!(ready_at, next_refresh_at);
        }
        other => panic!("expected readiness, found {other:?}"),
    }
    assert_eq!(source.fetch_count(), 2);

    // Nothing is due until a whole interval after the refresh.
    advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.fetch_count(), 2);

    runtime.shutdown().await;
}

/// A failed fetch leaves the loop alive, off the clock, and recoverable.
///
/// It also leaves the snapshot the pane is still reading where it was: a failed
/// fetch publishes no snapshot, and it does not retract the last good one.
#[tokio::test(start_paused = true)]
async fn a_failed_fetch_should_be_reported_and_stop_the_schedule() {
    let source = FakeSource::new([
        FetchPlan::Snapshot(7.0),
        FetchPlan::Failure,
        FetchPlan::Snapshot(9.0),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    load_and_settle(&runtime, &mut reliable, &mut latest).await;

    advance(REFRESH_INTERVAL).await;
    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Refreshing { .. }
    ));
    let EtfStatus::Failed { error } = receive_status(&mut reliable).await else {
        panic!("a refused fetch is reported");
    };
    assert!(matches!(error, EtfFetchError::Service(_)));
    assert!(
        latest.next_unseen().is_none(),
        "a failed fetch publishes no snapshot"
    );
    let mut retained = runtime.latest_stream();
    let envelope = retained
        .next_unseen()
        .expect("the last good snapshot stays available");
    let EtfStreamMessage::Snapshot { snapshot, .. } = envelope.message().as_ref() else {
        panic!("the latest channel should only carry snapshots");
    };
    assert_eq!(
        flow(snapshot),
        7.0,
        "a failure does not retract what is shown"
    );

    // The failure took the loop off the clock: it is alive and waiting, so two
    // intervals go by without a single fetch.
    //
    // The intervals are advanced one at a time on purpose. A single long advance
    // does not fire the timers a cycle arms while it runs, so a loop that put a
    // refused fetch straight back on the schedule would still look idle to a test
    // that skipped three intervals in one step.
    for _ in 0..2 {
        advance(REFRESH_INTERVAL).await;
        assert_eq!(source.fetch_count(), 2, "the loop does not retry by itself");
    }

    // The recovery is a refresh on the same account rather than a new one.
    let response = runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect("the waiting loop should still accept a refresh");
    assert_eq!(response.into_value(), EtfResponse::RefreshQueued);
    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Refreshing { .. }
    ));
    let recovered = receive_snapshot(&mut latest).await;
    assert_eq!(flow(recovered.as_ref()), 9.0);
    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Ready { revision: 2, .. }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_fetch_that_does_not_answer_should_time_out_and_stop_the_schedule() {
    let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Pending]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");

    runtime
        .client()
        .call(EtfRequest::Load(account()))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;

    advance(ATTEMPT_TIMEOUT).await;
    match receive_status(&mut reliable).await {
        EtfStatus::Failed { error } => {
            assert_eq!(
                error,
                EtfFetchError::Timeout(ATTEMPT_TIMEOUT),
                "the failure names the timeout it hit"
            );
            assert_eq!(source.cancelled_fetches(), 1, "the attempt was dropped");
        }
        other => panic!("expected a timeout, found {other:?}"),
    }

    advance(REFRESH_INTERVAL * 2).await;
    assert_eq!(source.fetch_count(), 1, "a stuck loop does not keep asking");

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn another_account_should_replace_the_stream() {
    let source = FakeSource::new([FetchPlan::Snapshot(1.0), FetchPlan::Snapshot(2.0)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    load_and_settle(&runtime, &mut reliable, &mut latest).await;

    let second = stream_key("second-account");
    let response = runtime
        .client()
        .call(EtfRequest::Load(second.clone()))
        .await
        .expect("another account should replace the stream");
    assert_eq!(response.into_value(), EtfResponse::StreamStarted);

    // The replacement announces itself from the beginning, and every message it
    // publishes says which account it is about.
    assert!(matches!(
        receive_status_for(&mut reliable, &second).await,
        EtfStatus::Connecting
    ));
    assert!(matches!(
        receive_status_for(&mut reliable, &second).await,
        EtfStatus::Refreshing { .. }
    ));
    let snapshot = receive_snapshot(&mut latest).await;
    assert_eq!(flow(snapshot.as_ref()), 2.0);
    assert!(matches!(
        receive_status_for(&mut reliable, &second).await,
        EtfStatus::Ready { revision: 1, .. }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_without_a_stream_should_be_refused() {
    let source = FakeSource::new([FetchPlan::Snapshot(1.0)]);
    let runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));

    let error = runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect_err("there is nothing to refresh yet");
    assert!(matches!(
        error,
        RpcCallError::Service {
            source: EtfServiceError::StreamClosed,
            ..
        }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_while_one_is_in_flight_should_be_refused() {
    let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(1.0)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");

    runtime
        .client()
        .call(EtfRequest::Load(account()))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;

    let response = runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect("a refresh is accepted while a fetch is in flight");
    assert_eq!(response.into_value(), EtfResponse::RefreshQueued);

    // The one that was queued is the fetch in flight, so the gate refuses a second
    // click rather than piling it up behind the first.
    let error = runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect_err("a second refresh should be refused");
    assert!(matches!(
        error,
        RpcCallError::Service {
            source: EtfServiceError::StreamBusy,
            ..
        }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_shut_down_service_should_fetch_nothing_and_drop_what_is_in_flight() {
    let source = FakeSource::new([FetchPlan::Snapshot(1.0), FetchPlan::Pending]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    load_and_settle(&runtime, &mut reliable, &mut latest).await;

    runtime
        .client()
        .call(EtfRequest::Refresh)
        .await
        .expect("a refresh should be queued");
    assert!(matches!(
        receive_status(&mut reliable).await,
        EtfStatus::Refreshing { .. }
    ));

    runtime.shutdown().await;
    let fetches = source.fetch_count();
    advance(REFRESH_INTERVAL * 2).await;

    assert_eq!(
        source.fetch_count(),
        fetches,
        "a shut-down service fetches nothing"
    );
    assert_eq!(
        source.cancelled_fetches(),
        1,
        "the fetch in flight was dropped"
    );
    assert_eq!(source.in_flight(), 0);
}

#[test]
fn a_status_should_name_the_deadline_it_scheduled() {
    assert_eq!(EtfStatus::Connecting.next_refresh_at(), None);
    assert_eq!(
        EtfStatus::Refreshing {
            next_refresh_at: 1_000
        }
        .next_refresh_at(),
        Some(1_000)
    );
    assert_eq!(
        EtfStatus::Ready {
            revision: 3,
            next_refresh_at: 2_000
        }
        .next_refresh_at(),
        Some(2_000)
    );
    assert_eq!(
        EtfStatus::Failed {
            error: EtfFetchError::Timeout(Duration::from_secs(1))
        }
        .next_refresh_at(),
        None,
        "a failure is off the clock"
    );
}
