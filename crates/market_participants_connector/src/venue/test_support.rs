//! The fixture root the venue tests drive a load through.
//!
//! The catalogue fixture answers the built-in Binance and OKX catalogues over
//! plain HTTP, and the info fixture answers Hyperliquid's own conversation, so a
//! test loads a real venue through the real gateway, registry, commit and request
//! path. Nothing here speaks for a venue: the fixtures answer with the venue's own
//! protocol, and a load commits exactly what they said.
//!
//! Each fixture venue is configured as a venue: its own published addresses resolve
//! at the fixture that serves it, so every scope it publishes — and every scope it
//! deploys, which is reached through its primary scope — is dialed there. The
//! addresses themselves stay the ones the venues publish, which is what
//! `app_config`'s test-support seam moves and nothing else.
//!

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

/// One venue conversation is bounded so a test cannot hang on a fixture that
/// never answers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A root whose Binance and OKX catalogues address `catalogs` and whose
/// Hyperliquid conversation addresses `info`, with the directory wiring attached.
///
/// The fixture servers speak plain HTTP on a loopback socket, so the transport is
/// built with the plaintext policy a test owns; every other transport decision is
/// the production one. The configuration the root is built over is what carries the
/// fixture addresses, so nothing has to be held beside the root for it to keep
/// dialling them.
///
/// Longbridge is pointed at the catalogue fixture as well, although its directory is
/// published by the market participant rather than by the venue: its route still resolves an
/// origin in this process, so a load of it can never reach the venue's real hosts. The
/// directory it reads is the one a test installs through
/// [`crate::test_support::SymbolDirectoryFixture`].
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

/// One built-in venue identity.
fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

/// Loads one venue through the load action, bounded by the request timeout.
pub(crate) async fn load(
    connector: &MarketParticipantsConnector,
    venue: &VenueId,
) -> Result<(), VenueLoadError> {
    timeout(REQUEST_TIMEOUT, connector.load_venue(*venue))
        .await
        .unwrap_or_else(|_| panic!("loading {venue} should not time out"))
}
