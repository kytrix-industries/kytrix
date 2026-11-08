//! The application wiring the root-window tests share.
//!
//! A test that mounts the real root window mounts the real views it hands the entities to,
//! and those views read the globals the composition root installs: the sidebar's account
//! menu needs the service connector, so a harness that mounts the window installs it the
//! way the composition root does. What the harness does not install is the real account
//! session: the connector is built over the provider's signed-out in-memory test session,
//! whose credential store is empty and whose authority is a fake. These tests never sign
//! in, so installing it reads no keyring entry, performs no startup refresh, and sends no
//! account request.

use std::sync::Arc;

use app_config_provider::{manager::ConfigManager, notification::ConfigNotification};
use gpui_kit::App;
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use network::transport::hub::{TransportHub, TransportPolicy};

/// Installs the service connector the account menu reads from the globals.
///
/// The connector is the test construction, not the production one: its auth session
/// starts signed out over an empty in-memory credential store and a fake authority, so
/// the account menu's session query is answered locally — no keyring, no startup
/// refresh, no account request. Everything else it installs is the production wiring
/// the root window's views read.
pub(crate) fn install_service_connector(cx: &mut App) {
    let runtime = widget_tokio_runtime::global::handle(cx);
    let network = Arc::new(cx.global::<ConfigManager>().read().network_config());
    let notification = ConfigNotification::new(network);
    let transport = Arc::new(TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        ..TransportPolicy::default()
    }));
    let connector = KytrixServiceConnector::install_for_test(cx, &runtime, notification, transport)
        .expect("the test service connector should install");
    cx.set_global(connector);
}
