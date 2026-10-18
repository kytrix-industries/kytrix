//! The poll loop, driven through the framework's own runtime.
//!
//! A test here owns a real [`WidgetRuntime`] and a real Tokio clock, because what
//! is being tested is the loop's *timing*: that the first fetch does not wait out
//! an interval, that a manual refresh resets the schedule rather than racing it,
//! and that a failure stops the schedule instead of retrying it. A fake source is
//! the only thing that is replaced.

use std::{sync::Arc, time::Duration};

use market_participants_connector::market::timeframe::StatisticsTimeFrame;
use market_participants_model::listing::route::VenueListingKey;
use tokio::time::advance;
use widget::{
    rpc::RpcCallError,
    runtime::WidgetRuntime,
    stream::{LatestStream, ReliableStream},
};

use super::{
    stream::*,
    test_support::{FakeSource, FetchPlan, perpetual_listing},
};
use crate::model::{MetricKind, StatisticsPoints, StatisticsSnapshot};

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

/// The listing every service test follows.
fn listing() -> VenueListingKey {
    perpetual_listing("BTCUSDT")
}

/// A second listing of the very same canonical contract on another scope.
fn other_listing() -> VenueListingKey {
    market_participants_connector::catalog::test_support::listing_key(
        &market_participants_connector::catalog::test_support::perp_route(),
        "BTCUSDT",
    )
}

fn load_request(metric: MetricKind) -> StatisticsRequest {
    StatisticsRequest::from_scope(metric, listing(), StatisticsTimeFrame::Minute5, false)
}

fn test_service(source: FakeSource) -> StatisticsService<FakeSource> {
    StatisticsService::with_timing(source, Duration::from_secs(60), Duration::from_secs(5))
}

async fn receive_status(reliable: &mut ReliableStream<StatisticsProtocol>) -> StatisticsStatus {
    let envelope = reliable
        .recv()
        .await
        .expect("the status stream should remain open");
    let StatisticsStreamMessage::Status(status) = envelope.message().as_ref() else {
        panic!("the reliable channel should only carry statuses");
    };
    status.clone()
}

async fn receive_snapshot(
    latest: &mut LatestStream<StatisticsProtocol>,
) -> Arc<StatisticsSnapshot> {
    latest
        .changed()
        .await
        .expect("a snapshot should be published");
    let envelope = latest
        .next_unseen()
        .expect("the published snapshot should be unseen");
    let StatisticsStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
        panic!("the latest channel should only carry windows");
    };

    snapshot.clone()
}

/// Every metric the service knows, so one test can cover the whole routing table.
const ALL_METRICS: [MetricKind; 5] = [
    MetricKind::OpenInterest,
    MetricKind::TopTraderAccountRatio,
    MetricKind::TopTraderPositionRatio,
    MetricKind::LongShortAccountRatio,
    MetricKind::TakerVolume,
];

#[tokio::test]
async fn every_metric_should_reach_its_own_endpoint() {
    let source = FakeSource::new(ALL_METRICS.map(|_| FetchPlan::Success(1)));

    for metric in ALL_METRICS {
        let request = load_request(metric);
        let mut metadata = None;
        let (points, changed) = fetch_snapshot(&source, &stream_key(&request), &mut metadata, None)
            .await
            .expect("the fake fetch should succeed");

        assert_eq!(
            stream_key(&request).metric,
            metric,
            "the key of a load names the metric it was asked for"
        );
        assert!(changed, "a first window is a change");
        assert!(
            matches!(
                (metric, &points),
                (MetricKind::OpenInterest, StatisticsPoints::OpenInterest(_))
                    | (
                        MetricKind::TopTraderAccountRatio
                            | MetricKind::TopTraderPositionRatio
                            | MetricKind::LongShortAccountRatio,
                        StatisticsPoints::LongShortRatio(_)
                    )
                    | (MetricKind::TakerVolume, StatisticsPoints::TakerVolume(_))
            ),
            "{metric:?} should be answered with its own series"
        );
    }

    assert_eq!(source.metrics(), ALL_METRICS.to_vec());
    assert_eq!(source.queries().len(), ALL_METRICS.len());
}

fn stream_key(request: &StatisticsRequest) -> StatisticsStreamKey {
    match request {
        StatisticsRequest::Load {
            metric,
            listing,
            timeframe,
        }
        | StatisticsRequest::Retry {
            metric,
            listing,
            timeframe,
        } => StatisticsStreamKey::new(*metric, listing.clone(), *timeframe),
        StatisticsRequest::Refresh => panic!("a refresh has no scope of its own"),
    }
}

/// Two listings of the very same canonical contract are two scopes: what a pane
/// follows is the venue's own listing, so the key has to tell them apart.
#[tokio::test]
async fn two_listings_of_one_contract_should_be_two_scopes() {
    let first = StatisticsStreamKey::new(
        MetricKind::OpenInterest,
        listing(),
        StatisticsTimeFrame::Minute5,
    );
    let second = StatisticsStreamKey::new(
        MetricKind::OpenInterest,
        other_listing(),
        StatisticsTimeFrame::Minute5,
    );

    assert_ne!(
        first, second,
        "listings of one contract on two routes are different scopes"
    );
    assert_ne!(first.query().listing, second.query().listing);
    assert_eq!(first.query().limit, u16::from(crate::model::POINT_LIMIT));

    let source = FakeSource::new([FetchPlan::Success(1), FetchPlan::Success(2)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");

    runtime
        .client()
        .call(StatisticsRequest::from_scope(
            MetricKind::OpenInterest,
            listing(),
            StatisticsTimeFrame::Minute5,
            false,
        ))
        .await
        .expect("the first listing should start a stream");
    runtime
        .client()
        .call(StatisticsRequest::from_scope(
            MetricKind::OpenInterest,
            other_listing(),
            StatisticsTimeFrame::Minute5,
            false,
        ))
        .await
        .expect("the second listing should replace it");

    let _ = receive_status(&mut reliable).await;
    assert_eq!(
        source.queries().len(),
        2,
        "each stream fetched its own listing"
    );
    assert_ne!(
        source.queries()[0].listing,
        source.queries()[1].listing,
        "the two streams asked about the listings they were started for"
    );

    runtime.shutdown().await;
}

/// The fetch a snapshot of the fake source produces: one point at `timestamp`.
fn points_timestamp(snapshot: &StatisticsSnapshot) -> Option<i64> {
    match &snapshot.points {
        StatisticsPoints::OpenInterest(points) => points.last().map(|point| point.timestamp),
        StatisticsPoints::LongShortRatio(points) => points.last().map(|point| point.timestamp),
        StatisticsPoints::TakerVolume(points) => points.last().map(|point| point.timestamp),
    }
}

#[tokio::test(start_paused = true)]
async fn the_listing_precision_should_be_fetched_once_and_only_for_the_charts_that_need_it() {
    let source = FakeSource::new([FetchPlan::Success(1), FetchPlan::Success(2)]);
    for metric in [MetricKind::OpenInterest, MetricKind::LongShortAccountRatio] {
        let request = load_request(metric);
        let mut metadata = None;
        fetch_snapshot(&source, &stream_key(&request), &mut metadata, None)
            .await
            .expect("the fake fetch should succeed");
        if metric.needs_instrument_precision() {
            assert!(metadata.is_some(), "{metric:?} labels its values at a tick");
        } else {
            assert!(metadata.is_none(), "{metric:?} has no listing behind it");
        }
    }

    assert_eq!(
        source.metadata_fetches(),
        1,
        "only the series that label at a tick ask for one"
    );
}

#[tokio::test(start_paused = true)]
async fn a_load_should_fetch_at_once_and_publish_the_window_then_readiness() {
    let source = FakeSource::new([FetchPlan::Success(7)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    let response = runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    assert_eq!(response.into_value(), StatisticsResponse::StreamStarted);

    assert!(matches!(
        receive_status(&mut reliable).await,
        StatisticsStatus::Connecting
    ));
    let refreshing = receive_status(&mut reliable).await;
    let StatisticsStatus::Refreshing { next_refresh_at } = refreshing else {
        panic!("a fetch should be reported before it starts");
    };
    let snapshot = receive_snapshot(&mut latest).await;
    assert_eq!(points_timestamp(snapshot.as_ref()), Some(7));
    match receive_status(&mut reliable).await {
        StatisticsStatus::Ready {
            revision,
            next_refresh_at: ready_at,
        } => {
            assert_eq!(revision, 1, "the first window is revision one");
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
async fn an_unchanged_window_should_not_be_published_again() {
    let source = FakeSource::new([
        FetchPlan::Success(7),
        FetchPlan::Success(7),
        FetchPlan::Success(7),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    let _ = receive_snapshot(&mut latest).await;
    let _ = receive_status(&mut reliable).await;

    // The same window again: the venue answered, but nothing about the chart
    // changed, so there is nothing to draw and nothing to publish.
    advance(Duration::from_secs(60)).await;
    let _ = receive_status(&mut reliable).await;
    match receive_status(&mut reliable).await {
        StatisticsStatus::Ready { revision, .. } => {
            assert_eq!(revision, 1, "an unchanged window is not a new revision");
        }
        other => panic!("expected readiness, found {other:?}"),
    }
    assert_eq!(source.fetch_count(), 2);

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_manual_refresh_should_fetch_again_even_when_the_window_did_not_move() {
    let source = FakeSource::new([
        FetchPlan::Success(7),
        FetchPlan::Success(7),
        FetchPlan::Success(7),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    let _ = receive_snapshot(&mut latest).await;
    let _ = receive_status(&mut reliable).await;

    let response = runtime
        .client()
        .call(StatisticsRequest::Refresh)
        .await
        .expect("a refresh should be queued");
    assert_eq!(response.into_value(), StatisticsResponse::RefreshQueued);
    let _ = receive_status(&mut reliable).await;
    match receive_status(&mut reliable).await {
        StatisticsStatus::Ready { revision, .. } => assert_eq!(revision, 1),
        other => panic!("expected readiness, found {other:?}"),
    }

    assert_eq!(source.fetch_count(), 2);
    runtime.shutdown().await;
}

/// A failed fetch leaves the loop alive, off the clock, and recoverable.
///
/// Which of the fake's answers lands after the recovery is not asserted on: an
/// automatic tick armed before the failure can also fire while the refresh is in
/// flight, so *which* window arrives is a detail of the fake rather than of the
/// loop. What the loop promises is that the failure is reported, that nothing
/// happens on its own while it is off the clock, and that a refresh starts it
/// again without a whole new scope.
#[tokio::test(start_paused = true)]
async fn a_failed_fetch_should_be_reported_and_stop_the_schedule() {
    let source = FakeSource::new([
        FetchPlan::Failure,
        FetchPlan::Success(9),
        FetchPlan::Success(10),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    match receive_status(&mut reliable).await {
        StatisticsStatus::Failed { error } => {
            assert!(matches!(error, StatisticsFetchError::MarketParticipant(_)));
        }
        other => panic!("expected a failure, found {other:?}"),
    }
    assert_eq!(source.fetch_count(), 1);
    assert!(
        latest.next_unseen().is_none(),
        "a failed fetch publishes no window"
    );

    // The failure took the loop off the clock: it is alive and waiting, so three
    // intervals go by without a single fetch.
    //
    // The intervals are advanced one at a time on purpose. A single long advance
    // does not fire the timers a cycle arms while it runs, so a loop that put a
    // refused fetch straight back on the schedule would still look idle to a test
    // that skipped three intervals in one step.
    for interval in 1..=3 {
        advance(Duration::from_secs(60)).await;
        assert_eq!(
            source.fetch_count(),
            1,
            "the loop does not retry by itself (after {interval} intervals)"
        );
    }

    // The recovery is a refresh on the same scope rather than a new one, and it
    // fetches again.
    let response = runtime
        .client()
        .call(StatisticsRequest::Refresh)
        .await
        .expect("the waiting loop should still accept a refresh");
    assert_eq!(response.into_value(), StatisticsResponse::RefreshQueued);
    assert!(
        source.fetch_count() >= 2,
        "a queued refresh is what fetches again"
    );
    assert!(matches!(
        receive_status(&mut reliable).await,
        StatisticsStatus::Refreshing { .. }
    ));
    let recovered = receive_snapshot(&mut latest).await;
    assert!(
        points_timestamp(recovered.as_ref()).is_some(),
        "the recovery publishes a window"
    );
    assert!(matches!(
        receive_status(&mut reliable).await,
        StatisticsStatus::Ready { .. }
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
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;

    advance(Duration::from_secs(5)).await;
    match receive_status(&mut reliable).await {
        StatisticsStatus::Failed { error } => {
            assert_eq!(
                error,
                StatisticsFetchError::Timeout(Duration::from_secs(5)),
                "the failure names the timeout it hit"
            );
        }
        other => panic!("expected a timeout, found {other:?}"),
    }

    for interval in 1..=3 {
        advance(Duration::from_secs(60)).await;
        assert_eq!(
            source.fetch_count(),
            1,
            "a stuck loop does not keep asking (after {interval} intervals)"
        );
    }

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_second_metric_should_replace_the_first() {
    let source = FakeSource::new([FetchPlan::Success(1), FetchPlan::Success(2)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the first load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    let _ = receive_snapshot(&mut latest).await;
    let _ = receive_status(&mut reliable).await;

    let response = runtime
        .client()
        .call(load_request(MetricKind::TakerVolume))
        .await
        .expect("a second scope should replace the first");
    assert_eq!(response.into_value(), StatisticsResponse::StreamStarted);

    // The replacement announces itself from the beginning, and its window is the
    // new series'.
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    let snapshot = receive_snapshot(&mut latest).await;
    assert!(matches!(snapshot.points, StatisticsPoints::TakerVolume(_)));
    assert_eq!(
        source.metrics(),
        vec![MetricKind::OpenInterest, MetricKind::TakerVolume]
    );

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_while_one_is_in_flight_should_be_refused() {
    let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Success(1)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;

    let response = runtime
        .client()
        .call(StatisticsRequest::Refresh)
        .await
        .expect("a refresh is accepted while a fetch is in flight");
    assert_eq!(response.into_value(), StatisticsResponse::RefreshQueued);

    // The one that was queued is the fetch in flight, so the gate refuses a
    // second click rather than piling it up behind the first.
    let error = runtime
        .client()
        .call(StatisticsRequest::Refresh)
        .await
        .expect_err("a second refresh should be refused");
    assert!(matches!(
        error,
        RpcCallError::Service {
            source: StatisticsServiceError::StreamBusy,
            ..
        }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_without_a_scope_should_be_refused() {
    let source = FakeSource::new([FetchPlan::Success(1)]);
    let runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));

    let error = runtime
        .client()
        .call(StatisticsRequest::Refresh)
        .await
        .expect_err("there is nothing to refresh yet");
    assert!(matches!(
        error,
        RpcCallError::Service {
            source: StatisticsServiceError::StreamClosed,
            ..
        }
    ));

    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_scope_should_stop_fetching() {
    let source = FakeSource::new([FetchPlan::Success(1), FetchPlan::Success(2)]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(load_request(MetricKind::OpenInterest))
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    let _ = receive_snapshot(&mut latest).await;
    let _ = receive_status(&mut reliable).await;

    runtime.shutdown().await;
    let fetches = source.fetch_count();
    advance(Duration::from_secs(300)).await;

    assert_eq!(
        source.fetch_count(),
        fetches,
        "a shut-down service fetches nothing"
    );
}

#[test]
fn a_status_should_name_the_deadline_it_scheduled() {
    assert_eq!(StatisticsStatus::Connecting.next_refresh_at(), None);
    assert_eq!(
        StatisticsStatus::Refreshing {
            next_refresh_at: 1_000
        }
        .next_refresh_at(),
        Some(1_000)
    );
    assert_eq!(
        StatisticsStatus::Ready {
            revision: 3,
            next_refresh_at: 2_000
        }
        .next_refresh_at(),
        Some(2_000)
    );
}

#[test]
fn only_the_series_the_venue_labels_should_ask_for_its_precision() {
    assert!(MetricKind::OpenInterest.needs_instrument_precision());
    assert!(MetricKind::TakerVolume.needs_instrument_precision());
    assert!(!MetricKind::TopTraderAccountRatio.needs_instrument_precision());
    assert!(!MetricKind::TopTraderPositionRatio.needs_instrument_precision());
    assert!(!MetricKind::LongShortAccountRatio.needs_instrument_precision());
}

/// Each series is served by its own capability, which is what a route has to
/// declare for a pane of that series to be pointed at it.
#[test]
fn every_series_should_name_the_capability_that_serves_it() {
    use market_participants_model::routing::CapabilityId;

    assert_eq!(
        MetricKind::OpenInterest.capability(),
        CapabilityId::OpenInterest
    );
    assert_eq!(
        MetricKind::LongShortAccountRatio.capability(),
        CapabilityId::LongShortAccountRatio
    );
    assert_eq!(
        MetricKind::TopTraderAccountRatio.capability(),
        CapabilityId::TopTraderAccountRatio
    );
    assert_eq!(
        MetricKind::TopTraderPositionRatio.capability(),
        CapabilityId::TopTraderPositionRatio
    );
    assert_eq!(
        MetricKind::TakerVolume.capability(),
        CapabilityId::TakerVolume
    );
}
