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
//! [`fixture_application`] adds what a test about the notification contract needs:
//! an application the root is installed in, so the venue event boundary exists and a
//! commit is announced to a subscriber rather than to nobody.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use app_config::market_participants::fixture::{self, FixtureAddress};
use gpui_kit::{App, TestAppContext};
use market_participants_model::identifier::VenueId;
use tokio::{sync::watch, time::timeout};
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use super::loaded::VenuesUpdated;
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
    let (_, receiver) = watch::channel(Arc::new(MarketParticipantNetworkSnapshot::new(
        fixture_config(&fixture),
    )));
    connector_over(receiver, plaintext_policy())
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

/// One fixture application: the installed root, the runtime its loads run on, and
/// every commit its subscribers have been told about.
pub(crate) struct FixtureApplication {
    connector: MarketParticipantsConnector,
    runtime: Runtime,
    announcements: Rc<RefCell<Vec<VenuesUpdated>>>,
}

impl FixtureApplication {
    /// The root every capability of this application is read through.
    pub(crate) fn connector(&self) -> &MarketParticipantsConnector {
        &self.connector
    }

    /// Loads one venue through the load action, on the root's own runtime.
    ///
    /// A load is the root's own work and this thread is not inside a runtime, so
    /// driving it here is what makes what it reported the test's to read.
    pub(crate) fn load(&self, venue: &VenueId) -> Result<(), VenueLoadError> {
        self.runtime.handle().block_on(load(&self.connector, venue))
    }

    /// The commits announced so far, in the order the interface received them.
    pub(crate) fn announcements(&self) -> Vec<VenuesUpdated> {
        self.announcements.borrow().clone()
    }

    /// Closes the fixture root and every connection it owns.
    pub(crate) fn shutdown(&self) {
        self.runtime.handle().block_on(self.connector.shutdown());
    }

    /// Waits until `expected` commits have reached the subscriber, and answers what
    /// it has seen.
    ///
    /// A commit reaches the interface as a message the application's own executor
    /// delivers, so waiting for one means handing the thread back until the answer
    /// that woke it arrives. The wait is bounded by the fixture timeout, so a commit
    /// that never reports fails the test here instead of hanging it.
    pub(crate) fn wait_for_announcements(
        &self,
        cx: &mut TestAppContext,
        expected: usize,
    ) -> Vec<VenuesUpdated> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while self.announcements.borrow().len() < expected {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} announcements"
            );
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.announcements()
    }
}

/// Installs one fixture application over `catalogs` and `info`.
///
/// The venue event boundary is the one [`MarketParticipantsConnector::install`] attaches: the
/// entity exists exactly when a root is installed in an application, and every commit
/// is announced through it, so counting its emissions counts what the menu, the
/// selector and every pane receive. The root is built from typed runtime inputs
/// rather than by that install path because the fixture servers speak plain HTTP on a
/// loopback socket while the install path takes the production transport policy, which
/// refuses it; the boundary and the subscriber are the same either way.
pub(crate) fn fixture_application(cx: &mut App, catalogs: &str, info: &str) -> FixtureApplication {
    let (connector, _catalog) = fixture_connector(catalogs, info);
    connector.attach_events(cx);
    let events = connector.venues().events();
    let announcements: Rc<RefCell<Vec<VenuesUpdated>>> = Rc::new(RefCell::new(Vec::new()));
    cx.subscribe(&events, {
        let announcements = Rc::clone(&announcements);
        move |_, update: &VenuesUpdated, _| announcements.borrow_mut().push(update.clone())
    })
    .detach();
    cx.set_global(connector.clone());
    FixtureApplication {
        connector,
        runtime: Runtime::for_test(),
        announcements,
    }
}
