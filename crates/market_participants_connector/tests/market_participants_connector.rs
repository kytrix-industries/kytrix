use std::sync::Arc;

use app_config_provider::notification::ConfigNotification;
use market_participants_connector::{
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    runtime::MarketParticipantNetworkSnapshot,
};
use market_participants_model::identifier::VenueId;
use network::transport::hub::{TransportHub, TransportPolicy};
use widget_tokio_runtime::runtime::Runtime;

fn connector() -> MarketParticipantsConnector {
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
    let notification = ConfigNotification::new(snapshot);
    MarketParticipantsConnector::new(
        MarketParticipantsConnectorInput::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            notification,
        )
        .with_runtime(Runtime::for_test()),
    )
    .expect("MarketParticipantsConnector should build from typed runtime inputs")
}

/// A fresh root is empty: building it registers no route, and only a load can
/// put a venue in the registry.
///
/// What loading a venue publishes — its market routes, its account routes and
/// the catalog source bindings behind them — is covered by the root's own unit
/// tests, which drive the load seam without a venue answering.
#[tokio::test]
async fn root_constructs_with_an_empty_registry() {
    let connector = connector();

    assert!(
        connector.venues().scopes().is_empty(),
        "a root that was just built holds no venue"
    );
    assert_eq!(
        connector.supported_venues().len(),
        4,
        "the four implemented venues are declared"
    );
    assert!(
        connector.catalog().route_set().is_none(),
        "and its catalogue holds no route"
    );
    let binance_spot = market_participants_model::listing::route::VenueRoute::new(
        VenueId::Binance,
        market_participants_model::identifier::VenueScopeId::new("spot")
            .expect("the scope id is canonical"),
    );
    assert!(
        connector.venues().scope(&binance_spot).is_none(),
        "a declared venue is not a loaded one"
    );

    // This root was built from typed runtime inputs rather than installed, so it owns no
    // configuration bridge and no invalidation task: nothing publishes into its snapshot.
    // With no venue loaded, its venue table is empty either way, and it stays empty across
    // the reads this test made.
    assert!(
        connector.venues().scopes().is_empty(),
        "a root with no loaded venue keeps its own venue table empty"
    );

    connector.shutdown().await;
}
