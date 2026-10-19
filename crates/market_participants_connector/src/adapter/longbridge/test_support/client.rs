//! The clients that read the fixture: the configuration that makes the venue reachable,
//! the credentials it resolves, the route a read is addressed to and the listing a read
//! is about.
//!
//! A test builds those the way production does — the credentials go through the venue's
//! own settings row and the listing through the venue's own catalogue — so what it
//! drives is the adapter's real path rather than a second one written for the test.

use std::sync::Arc;

use app_config::{
    market_participants::{credentials::LongbridgeCredentials, settings::VenueSettings},
    network::{NetworkRouteGeneration, ResolvedProxy},
    settings::Config,
};
use app_config_provider::notification::ConfigNotification;
use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
    routing::CapabilityId,
};
use network::transport::{
    http::HttpClientClass,
    hub::{TransportHub, TransportPolicy},
};
use smol_str::SmolStr;

use super::venue::LongbridgeFixture;
use crate::{
    adapter::{ExecutionContext, longbridge::SymbolRow},
    connection::ConnectionPool,
    protocol::RequestIdentity,
    routing::LONGBRIDGE_US_SCOPE,
    runtime::{LongbridgeCredentialValues, MarketParticipantNetworkSnapshot},
};

/// One execution context that reads the fixture's sockets.
///
/// The configuration is what makes the venue reachable and what its credentials are
/// resolved from: the three values are stored in the venue's own settings row and the
/// snapshot is what a request's runtime receiver carries, so the adapter resolves its
/// credentials exactly the way it does in production.
pub(crate) fn execution(
    fixture: &LongbridgeFixture,
    credentials: LongbridgeCredentials,
    capability: CapabilityId,
) -> ExecutionContext {
    let mut config = Config::default();
    let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
    else {
        panic!("the built-in table configures longbridge as Longbridge");
    };
    settings.credentials = credentials;
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config));
    let notification = ConfigNotification::new(Arc::clone(&snapshot));

    let transport = Arc::new(TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }));
    let client = transport
        .http()
        .client(&ResolvedProxy::NoProxy, HttpClientClass::Public)
        .expect("the fixture HTTP client builds");
    let route = route();
    let identity = RequestIdentity::new(route.clone(), capability);
    ExecutionContext::new(
        identity,
        fixture.origins(),
        client,
        Arc::new(ConnectionPool::with_transport(transport)),
    )
    .with_proxy(ResolvedProxy::NoProxy, NetworkRouteGeneration::default())
    .with_credential_generation(snapshot.credential_generation(&VenueId::Longbridge, &route.scope))
    .with_runtime(notification)
}

/// One complete Longbridge credential set.
pub(crate) fn credentials() -> LongbridgeCredentials {
    LongbridgeCredentials {
        app_key: SmolStr::new_static(APP_KEY),
        app_secret: SmolStr::new_static(APP_SECRET),
        access_token: SmolStr::new_static(ACCESS_TOKEN),
    }
}

/// The same set as the resolved values an adapter dials with.
pub(crate) fn credential_values() -> LongbridgeCredentialValues {
    LongbridgeCredentialValues {
        app_key: SmolStr::new_static(APP_KEY),
        app_secret: SmolStr::new_static(APP_SECRET),
        access_token: SmolStr::new_static(ACCESS_TOKEN),
    }
}

/// The three values the fixture stores, in one place because the stored settings and
/// the resolved values have to be the same credential.
pub(crate) const APP_KEY: &str = "longbridge-app-key";
pub(crate) const APP_SECRET: &str = "longbridge-app-secret";
pub(crate) const ACCESS_TOKEN: &str = "longbridge-access-token";

/// The route every fixture reads the venue on.
pub(crate) fn route() -> VenueRoute {
    VenueRoute::new(
        VenueId::Longbridge,
        VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the built-in scope name is canonical"),
    )
}

/// The one listing these fixtures are about, as a published catalogue resolves it.
///
/// An adapter receives a resolved listing rather than a key: the venue's own symbol and
/// the typed identifiers are what a request is encoded from, and both come out of the
/// catalogue the row was published into. This is that resolution, through the venue's own
/// adapter entry, so a test drives the wire with what production hands it.
pub(crate) async fn resolved_listing() -> crate::catalog::resolved::ResolvedListingRef {
    let entry = crate::adapter::longbridge::catalog::catalog_entry(
        &route(),
        SymbolRow {
            code: SmolStr::new_static("AAPL"),
            name: SmolStr::new_static("Apple Inc. Common Stock"),
            mic: market_participants_model::identifier::Mic::new("XNAS")
                .expect("the fixture market is canonical"),
        },
    )
    .expect("the fixture row is publishable");
    let fixture = crate::catalog::test_support::Fixture::with_source(
        route(),
        "longbridge-us-catalog",
        Arc::new(LongbridgeCatalog::of(entry)),
    );
    fixture
        .ensure()
        .await
        .expect("the fixture route publishes its catalogue");
    let key = fixture
        .catalog()
        .cached_listings(&crate::adapter::longbridge::test_support::route())
        .expect("the published route holds its listings")[0]
        .listing
        .key
        .clone();
    fixture
        .catalog()
        .resolve_listing_ref(&key)
        .expect("the published catalogue resolves its own listing")
}

/// The venue's own catalogue, as the market-participant source publishes it.
///
/// The venue's adapter answers with entries that name the display codes their terms do
/// not state — a security's code is the directory's own ticker — so a fixture that held
/// only the listings would be publishing an entry the naming gate refuses. The batch is
/// built by the same function the market-participant source calls, so a fixture publishes exactly
/// what a load publishes.
struct LongbridgeCatalog {
    batch: crate::catalog::batch::CatalogBatch,
}

impl LongbridgeCatalog {
    /// Builds the source of one adapter entry.
    fn of(entry: crate::adapter::NativeCatalogEntry) -> Self {
        Self {
            batch: crate::catalog::market_participants::markets_batch(vec![entry])
                .expect("the fixture entry is publishable"),
        }
    }
}

impl crate::catalog::source::CatalogSource for LongbridgeCatalog {
    fn fetch_catalog(
        &self,
        _context: crate::catalog::source::CatalogFetchContext,
    ) -> futures::future::BoxFuture<
        'static,
        Result<crate::catalog::batch::CatalogBatch, crate::catalog::error::SourceError>,
    > {
        let batch = self.batch.clone();
        Box::pin(async move { Ok(batch) })
    }
}
