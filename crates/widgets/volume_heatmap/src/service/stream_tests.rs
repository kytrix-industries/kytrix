use std::{str::FromStr, sync::Arc, time::Duration};

use market_participants_connector::catalog::{
    generation::CatalogGeneration, registry::asset::AssetTerms, store::Catalog, test_support,
};
use market_participants_model::{
    asset::{
        AssetId, ContractAddress, CryptoTokenId, CryptoTokenOrigin, CryptoTokenTerms,
        TokenReference,
    },
    decimal::{Price, Quantity},
    identifier::{ChainId, IdentifierValue},
    listing::{
        route::{VenueListingKey, VenueRoute},
        venue::PriceBasis,
    },
    market::{
        price::MarketPrice,
        selection::ListingTicker,
        units::{MarketQuantity, QuantityUnit},
    },
};
use tokio::time::{Instant, advance};
use widget::{
    rpc::RpcCallError,
    runtime::WidgetRuntime,
    stream::{LatestStream, ReliableStream},
    util::deadline_millis,
};

use super::{
    stream::*,
    test_support::{FakeSource, FetchPlan, binance, okx, perp, repeated, routes, spot, ticker},
};
use crate::model::{
    HeatMapModelError, RouteListing, RouteTickerAnswer, TILE_LIMIT, combine_market_tickers,
};

#[allow(
    clippy::disallowed_methods,
    reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
)]
fn current_test_handle() -> &'static tokio::runtime::Handle {
    Box::leak(Box::new(tokio::runtime::Handle::current()))
}

fn test_service(source: FakeSource) -> VolumeHeatMapService<FakeSource> {
    VolumeHeatMapService::with_timing(source, REFRESH_INTERVAL, ATTEMPT_TIMEOUT)
}

async fn receive_status(
    reliable: &mut ReliableStream<VolumeHeatMapProtocol>,
) -> VolumeHeatMapStatus {
    let envelope = reliable
        .recv()
        .await
        .expect("the status stream should remain open");
    let VolumeHeatMapStreamMessage::Status(status) = envelope.message().as_ref() else {
        panic!("the reliable channel should only carry statuses");
    };
    status.clone()
}

async fn receive_snapshot(
    latest: &mut LatestStream<VolumeHeatMapProtocol>,
) -> Arc<VolumeHeatMapSnapshot> {
    latest.changed().await.expect("a map should be published");
    let envelope = latest
        .next_unseen()
        .expect("the published map should be unseen");
    let VolumeHeatMapStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
        panic!("the latest channel should only carry maps");
    };
    snapshot.clone()
}

fn only_notional(snapshot: &VolumeHeatMapSnapshot) -> Quantity {
    assert_eq!(snapshot.entries.len(), 1, "the fixtures build one tile");
    snapshot.entries[0].notional
}

async fn load_and_receive_first_map(
    runtime: &WidgetRuntime<VolumeHeatMapProtocol>,
    reliable: &mut ReliableStream<VolumeHeatMapProtocol>,
    latest: &mut LatestStream<VolumeHeatMapProtocol>,
) -> VolumeHeatMapSnapshot {
    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: binance(),
            routes: routes(),
        })
        .await
        .expect("the load should be accepted");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::StreamStarted);

    assert!(matches!(
        receive_status(reliable).await,
        VolumeHeatMapStatus::Connecting { .. }
    ));
    let refreshing = receive_status(reliable).await;
    let VolumeHeatMapStatus::Refreshing {
        next_refresh_at, ..
    } = refreshing
    else {
        panic!("a round should be reported before it starts");
    };
    let snapshot = receive_snapshot(latest).await;
    match receive_status(reliable).await {
        VolumeHeatMapStatus::Ready {
            revision,
            next_refresh_at: ready_at,
            ..
        } => {
            assert_eq!(revision, 1, "the first map is revision one");
            assert_eq!(
                ready_at, next_refresh_at,
                "readiness carries the same deadline the round announced"
            );
        }
        other => panic!("expected readiness, found {other:?}"),
    }
    snapshot.as_ref().clone()
}

#[tokio::test]
async fn fetch_snapshot_should_ask_every_scope_of_one_venue_at_once() {
    let spot_tickers = (0..=TILE_LIMIT)
        .rev()
        .map(|index| ticker(&format!("ASSET{index:02}"), "USDT", (1_000 - index) as u64))
        .collect::<Vec<_>>();
    let perp_tickers = (1..TILE_LIMIT)
        .rev()
        .map(|index| ticker(&format!("ASSET{index:02}"), "USDT", 1))
        .collect::<Vec<_>>();
    let source = FakeSource::new([
        FetchPlan::Tickers(spot_tickers),
        FetchPlan::Tickers(perp_tickers),
    ]);

    let (entries, changed) = fetch_snapshot(&source, &routes(), None)
        .await
        .expect("the venue answers should combine");

    assert_eq!(
        source.routes(),
        [spot("binance"), perp("binance")],
        "every scope of the venue the map is drawn from is asked"
    );
    assert!(
        source.routes_of(okx()).is_empty(),
        "a map of one venue never reads another: {:?}",
        source.routes()
    );
    assert!(
        source.maximum_in_flight() == 2,
        "the scopes are asked at the same time, not one after another"
    );
    assert!(changed);
    assert_eq!(entries.len(), TILE_LIMIT);
    assert!(
        entries
            .iter()
            .any(|entry| entry.base_asset_label.as_str() == "ASSET00")
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.base_asset_label.as_str() == "ASSET20")
    );
}

#[tokio::test]
async fn fetch_snapshot_should_accept_a_venue_answer_at_the_hard_limit() {
    let source = FakeSource::new([
        FetchPlan::Tickers(repeated("BTC", "USDT", 10, 10_000)),
        FetchPlan::Tickers(repeated("BTC", "USDT", 20, 10_000)),
    ]);

    let (entries, changed) = fetch_snapshot(&source, &routes(), None)
        .await
        .expect("answers at the hard limit should be accepted");

    assert!(changed);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.notional)
            .collect::<Vec<_>>(),
        [Quantity::from(30_u64)],
        "one instrument read on two scopes of one venue is one number"
    );
}

#[tokio::test]
async fn an_oversized_venue_answer_should_fail_the_round() {
    let spot_oversized = FakeSource::new([
        FetchPlan::Tickers(repeated("BTC", "USDT", 20, 10_001)),
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 10)]),
    ]);
    let error = fetch_snapshot(&spot_oversized, &routes(), None)
        .await
        .expect_err("an oversized answer should be refused");
    assert_eq!(
        error,
        VolumeHeatMapFetchError::TooManyTickers {
            route: spot("binance"),
            count: 10_001,
            limit: 10_000,
        }
    );

    let perp_oversized = FakeSource::new([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 10)]),
        FetchPlan::Tickers(repeated("BTC", "USDT", 20, 10_001)),
    ]);
    let error = fetch_snapshot(&perp_oversized, &routes(), None)
        .await
        .expect_err("an oversized answer should be refused");
    assert_eq!(
        error,
        VolumeHeatMapFetchError::TooManyTickers {
            route: perp("binance"),
            count: 10_001,
            limit: 10_000,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_load_should_fetch_at_once_and_publish_the_map_then_readiness() {
    let source = FakeSource::new([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 20)]),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    let snapshot = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;

    assert_eq!(only_notional(&snapshot), Quantity::from(120_u64));
    assert_eq!(source.fetch_count(), 2);
    assert_eq!(
        source.starts().len(),
        2,
        "a round asks each scope exactly once"
    );
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_load_of_another_venue_should_replace_the_generation() {
    let source = FakeSource::new([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
        FetchPlan::Tickers(vec![ticker("ETH", "USDT", 5)]),
        FetchPlan::Tickers(vec![]),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    let _ = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;

    let okx_routes: Arc<[VenueRoute]> = Arc::from([spot("okx")]);
    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: okx(),
            routes: okx_routes,
        })
        .await
        .expect("the other venue should load");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::StreamStarted);

    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Connecting { venue } if venue == okx()
    ));
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { venue, .. } if venue == okx()
    ));
    let snapshot = receive_snapshot(&mut latest).await;
    assert_eq!(
        snapshot.entries.len(),
        1,
        "the new generation publishes its own map"
    );
    assert_eq!(snapshot.entries[0].base_asset_label.as_str(), "ETH");
    assert_eq!(
        source.routes_of(okx()),
        [spot("okx")],
        "the venue the user picked is the one that is read"
    );
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_manual_refresh_should_run_a_round_without_waiting_for_the_interval() {
    let source = FakeSource::new(
        [
            FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
            FetchPlan::Tickers(vec![]),
        ]
        .into_iter()
        .chain([
            FetchPlan::Tickers(vec![ticker("BTC", "USDT", 200)]),
            FetchPlan::Tickers(vec![]),
        ]),
    );
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    let first = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;
    assert_eq!(only_notional(&first), Quantity::from(100_u64));

    advance(Duration::from_secs(20)).await;
    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Refresh { venue: binance() })
        .await
        .expect("a refresh should be queued");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::RefreshQueued);

    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    let refreshed = receive_snapshot(&mut latest).await;
    assert_eq!(only_notional(&refreshed), Quantity::from(200_u64));
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Ready { revision: 2, .. }
    ));
    assert_eq!(source.fetch_count(), 4);
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_of_another_venue_should_be_refused() {
    let source = FakeSource::new([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    let _ = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;
    let fetches = source.fetch_count();

    let error = runtime
        .client()
        .call(VolumeHeatMapRequest::Refresh { venue: okx() })
        .await
        .expect_err("a refresh of a venue nothing is reading must be refused");

    assert!(
        matches!(
            error,
            RpcCallError::Service {
                source: VolumeHeatMapServiceError::StreamClosed,
                ..
            }
        ),
        "found {error:?}"
    );
    assert_eq!(
        source.fetch_count(),
        fetches,
        "a refused refresh reads nothing"
    );
    assert!(source.routes_of(okx()).is_empty());
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_unchanged_map_should_advance_readiness_without_publishing_again() {
    let plans = [
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
    ]
    .into_iter()
    .chain([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
    ]);
    let source = FakeSource::new(plans);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    let _ = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;

    advance(REFRESH_INTERVAL).await;
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    match receive_status(&mut reliable).await {
        VolumeHeatMapStatus::Ready { revision, .. } => {
            assert_eq!(revision, 1, "a map that did not move is not a new revision");
        }
        other => panic!("expected readiness, found {other:?}"),
    }
    assert!(
        latest.next_unseen().is_none(),
        "the same map is not published twice"
    );
    assert_eq!(source.fetch_count(), 4);
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_should_be_refused_while_a_round_is_in_flight() {
    let source = FakeSource::new([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
        FetchPlan::Pending,
        FetchPlan::Pending,
    ]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();
    let _ = load_and_receive_first_map(&runtime, &mut reliable, &mut latest).await;

    runtime
        .client()
        .call(VolumeHeatMapRequest::Refresh { venue: binance() })
        .await
        .expect("the first refresh should be queued");
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    let error = runtime
        .client()
        .call(VolumeHeatMapRequest::Refresh { venue: binance() })
        .await
        .expect_err("a second refresh must be refused while the first is in flight");

    assert!(
        matches!(
            error,
            RpcCallError::Service {
                source: VolumeHeatMapServiceError::StreamBusy,
                ..
            }
        ),
        "found {error:?}"
    );
    assert_eq!(source.maximum_in_flight(), 2);
    runtime.shutdown().await;
    assert_eq!(source.in_flight(), 0);
    assert_eq!(
        source.cancelled_fetches(),
        2,
        "the round in flight when the scope ended is dropped, not answered"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_round_should_be_reported_and_take_the_loop_off_the_clock() {
    let source = FakeSource::new([FetchPlan::Failure, FetchPlan::Failure].into_iter().chain([
        FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
        FetchPlan::Tickers(vec![]),
    ]));
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: binance(),
            routes: routes(),
        })
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    match receive_status(&mut reliable).await {
        VolumeHeatMapStatus::Failed { error, .. } => assert!(matches!(
            error,
            VolumeHeatMapFetchError::MarketParticipant(_) | VolumeHeatMapFetchError::Model(_)
        )),
        other => panic!("expected a failure, found {other:?}"),
    }
    assert!(
        latest.next_unseen().is_none(),
        "a failed round publishes no map"
    );

    for interval in 1..=3 {
        advance(REFRESH_INTERVAL).await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the loop does not retry by itself (after {interval} intervals)"
        );
    }

    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Refresh { venue: binance() })
        .await
        .expect("the waiting loop should still accept a refresh");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::RefreshQueued);
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    let recovered = receive_snapshot(&mut latest).await;
    assert_eq!(only_notional(&recovered), Quantity::from(100_u64));
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Ready { .. }
    ));
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_round_that_does_not_answer_should_time_out_and_stop_the_schedule() {
    let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Pending]);
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: binance(),
            routes: routes(),
        })
        .await
        .expect("the load should be accepted");
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    advance(ATTEMPT_TIMEOUT).await;

    match receive_status(&mut reliable).await {
        VolumeHeatMapStatus::Failed { error, .. } => {
            assert_eq!(error, VolumeHeatMapFetchError::Timeout(ATTEMPT_TIMEOUT));
        }
        other => panic!("expected a timeout, found {other:?}"),
    }
    assert!(
        latest.next_unseen().is_none(),
        "a timed-out round publishes no map"
    );

    advance(REFRESH_INTERVAL).await;
    assert_eq!(
        source.fetch_count(),
        2,
        "a timed-out round is not retried by the schedule"
    );
    runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_explicit_load_should_replace_a_generation_that_was_off_the_clock() {
    let source = FakeSource::new(
        [
            FetchPlan::Failure,
            FetchPlan::Tickers(vec![]),
            FetchPlan::Tickers(vec![ticker("BTC", "USDT", 100)]),
            FetchPlan::Tickers(vec![]),
        ]
        .into_iter(),
    );
    let mut runtime = WidgetRuntime::spawn(current_test_handle(), test_service(source.clone()));
    let mut reliable = runtime
        .take_reliable_stream()
        .expect("the reliable stream should be available");
    let mut latest = runtime.latest_stream();

    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: binance(),
            routes: routes(),
        })
        .await
        .expect("the load should be accepted");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::StreamStarted);
    let _ = receive_status(&mut reliable).await;
    let _ = receive_status(&mut reliable).await;
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Failed { .. }
    ));

    let response = runtime
        .client()
        .call(VolumeHeatMapRequest::Load {
            venue: binance(),
            routes: routes(),
        })
        .await
        .expect("the recovery should be accepted");
    assert_eq!(response.into_value(), VolumeHeatMapResponse::StreamStarted);
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Connecting { .. }
    ));
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    let recovered = receive_snapshot(&mut latest).await;
    assert_eq!(only_notional(&recovered), Quantity::from(100_u64));
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Ready { .. }
    ));

    let before = source.fetch_count();
    advance(REFRESH_INTERVAL).await;
    assert!(matches!(
        receive_status(&mut reliable).await,
        VolumeHeatMapStatus::Refreshing { .. }
    ));
    assert!(
        source.fetch_count() > before,
        "a replaced generation is back on the clock"
    );
    runtime.shutdown().await;
}

#[test]
fn the_published_deadline_should_be_a_wall_clock_instant() {
    let now = deadline_millis(Instant::now());
    let later = deadline_millis(Instant::now() + Duration::from_secs(60));

    assert!(
        later - now >= 59_000,
        "a deadline is a wall-clock millisecond, not a tick count"
    );
}

const CONTRACT_ADDRESS: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

fn contract_token_terms() -> CryptoTokenTerms {
    CryptoTokenTerms {
        origin: CryptoTokenOrigin::Chain {
            chain_id: ChainId::new("1").expect("the fixture chain id is canonical"),
            token: TokenReference::Contract {
                address: ContractAddress::new(CONTRACT_ADDRESS)
                    .expect("the fixture address is an EVM address"),
            },
        },
    }
}

fn chain_native_token_terms(chain: &str, code: &str) -> CryptoTokenTerms {
    CryptoTokenTerms {
        origin: CryptoTokenOrigin::Chain {
            chain_id: ChainId::new(chain).expect("the fixture chain id is canonical"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("the fixture code is canonical"),
            },
        },
    }
}

fn generation_of(terms: &[AssetTerms]) -> Arc<CatalogGeneration> {
    test_support::install_asset_terms(&Catalog::new(), terms)
}

fn price_of(value: &str, quote: AssetId) -> MarketPrice {
    MarketPrice {
        value: Price::from_str(value).expect("the fixture price should parse"),
        quote_asset: quote,
        basis: PriceBasis::PerAssetUnit,
    }
}

fn ticker_of(listing: VenueListingKey, quote: AssetId) -> ListingTicker {
    ListingTicker {
        listing,
        monotonic_id: Some(1),
        open: price_of("10", quote.clone()),
        high: Some(price_of("12", quote.clone())),
        low: Some(price_of("9", quote.clone())),
        close: price_of("11", quote.clone()),
        traded_quantity: None,
        traded_notional: Some(
            MarketQuantity::amount(
                Quantity::from(10_u8),
                QuantityUnit::Notional { asset: quote },
            )
            .expect("a fixture notional is money"),
        ),
    }
}

fn contract_market(base: AssetId, quote: AssetId) -> (Vec<RouteListing>, Vec<ListingTicker>) {
    let listing = test_support::listing_key(&spot("binance"), "USDCUSDT");
    let listings = vec![RouteListing {
        listing: listing.clone(),
        instrument: test_support::spot(base, quote.clone()),
    }];
    (listings, vec![ticker_of(listing, quote)])
}

#[test]
fn a_tile_should_carry_the_code_the_display_catalog_published() {
    let base_terms = chain_native_token_terms("solana", "SOL");
    let base = AssetId::CryptoToken(CryptoTokenId::of_terms(&base_terms));
    let other = AssetId::CryptoToken(CryptoTokenId::of_terms(&chain_native_token_terms(
        "1", "SOL",
    )));
    let quote = test_support::token("fixture", "USDT");
    let generation = generation_of(&[
        AssetTerms::CryptoToken(base_terms),
        AssetTerms::CryptoToken(chain_native_token_terms("1", "SOL")),
        AssetTerms::CryptoToken(test_support::token_terms("fixture", "USDT")),
    ]);
    let catalog = generation.display_catalog();
    assert_eq!(
        catalog.asset_code(&base).map(|code| code.as_str()),
        Some("SOL"),
        "the generation publishes the native code of the token"
    );
    assert_eq!(
        catalog.asset_code(&base),
        catalog.asset_code(&other),
        "another chain's token of the same code publishes the same text"
    );
    let (listings, tickers) = contract_market(base, quote);

    let (asset_labels, asset_terms) = route_assets(&generation, &listings, &tickers);
    let (entries, _) = combine_market_tickers(
        vec![RouteTickerAnswer {
            listings,
            asset_labels,
            asset_terms,
            tickers,
        }],
        None,
    )
    .expect("a market whose base asset the catalog names is a tile");

    assert_eq!(entries.len(), 1, "the fixture builds one tile");
    assert_eq!(
        entries[0].base_asset_label.as_str(),
        "SOL",
        "the tile carries the code the catalog published"
    );
}

#[test]
fn a_market_whose_base_asset_has_no_published_code_should_be_refused() {
    let base_terms = contract_token_terms();
    let base = AssetId::CryptoToken(CryptoTokenId::of_terms(&base_terms));
    let quote = test_support::token("fixture", "USDT");
    let generation = generation_of(&[
        AssetTerms::CryptoToken(base_terms),
        AssetTerms::CryptoToken(test_support::token_terms("fixture", "USDT")),
    ]);
    assert_eq!(
        generation
            .display_catalog()
            .asset_code(&base)
            .map(|code| code.as_str()),
        None,
        "a contract token has no code unless a route stages one"
    );
    assert!(
        generation.assets().terms(&base).is_some(),
        "the registry of the generation really carries the token"
    );
    let (listings, tickers) = contract_market(base.clone(), quote);

    let (asset_labels, asset_terms) = route_assets(&generation, &listings, &tickers);
    assert!(
        !asset_labels.contains_key(&base),
        "an asset the catalog publishes no code for has no label"
    );

    let refused = combine_market_tickers(
        vec![RouteTickerAnswer {
            listings,
            asset_labels,
            asset_terms,
            tickers,
        }],
        None,
    )
    .expect_err("a reading whose base has no published code is refused");

    assert!(
        matches!(
            &refused,
            HeatMapModelError::AssetNotNamed { asset } if asset.as_ref() == &base
        ),
        "the refusal carries the asset it could not name, got {refused:?}"
    );
}

#[test]
fn a_market_whose_base_asset_the_registry_does_not_carry_should_be_left_out() {
    let base = AssetId::CryptoToken(CryptoTokenId::of_terms(&contract_token_terms()));
    let quote = test_support::token("fixture", "USDT");
    let generation = generation_of(&[AssetTerms::CryptoToken(test_support::token_terms(
        "fixture", "USDT",
    ))]);
    let (listings, tickers) = contract_market(base.clone(), quote);

    let (asset_labels, asset_terms) = route_assets(&generation, &listings, &tickers);
    assert!(
        !asset_labels.contains_key(&base) && !asset_terms.contains_key(&base),
        "an asset the registry does not carry has neither a code nor terms"
    );

    let (entries, _) = combine_market_tickers(
        vec![RouteTickerAnswer {
            listings,
            asset_labels,
            asset_terms,
            tickers,
        }],
        None,
    )
    .expect("a market outside the registry's scope is not a failure of the map");

    assert!(
        entries.is_empty(),
        "a reading the map cannot classify contributes no tile"
    );
}
