use std::{sync::Arc, time::Duration};

use app_config::market_participants::fixture::{self, FixtureAddress};
use app_config_provider::notification::ConfigNotification;
use market_participants_model::identifier::VenueId;
use tokio::time::timeout;
use url::Url;

use crate::{
    catalog::store::Catalog,
    connector::{MarketParticipantsConnector, VenueLoadError},
    routing::{BINANCE_VENUE, HYPERLIQUID_VENUE, LONGBRIDGE_VENUE, OKX_VENUE},
    runtime::MarketParticipantNetworkSnapshot,
    test_support::{connector_over, fixture_config, plaintext_policy},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) fn fixture_connector(
    catalogs: &str,
    info: &str,
) -> (MarketParticipantsConnector, Arc<Catalog>) {
    let catalogs: Url = catalogs.parse().expect("the fixture origin is a URL");
    let info: Url = info.parse().expect("the fixture origin is a URL");
    let fixture = fixture::venues([
        (venue(BINANCE_VENUE), FixtureAddress::at(&catalogs)),
        (venue(OKX_VENUE), FixtureAddress::at(&catalogs)),
        (venue(HYPERLIQUID_VENUE), FixtureAddress::at(&info)),
        (venue(LONGBRIDGE_VENUE), FixtureAddress::at(&catalogs)),
    ]);
    let notification = ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::new(
        fixture_config(&fixture),
    )));
    connector_over(notification, plaintext_policy())
}

fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

pub(crate) async fn load(
    connector: &MarketParticipantsConnector,
    venue: &VenueId,
) -> Result<(), VenueLoadError> {
    timeout(REQUEST_TIMEOUT, connector.load_venue(*venue))
        .await
        .unwrap_or_else(|_| panic!("loading {venue} should not time out"))
}
