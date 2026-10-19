//! Where the updater is installed, and where the views find it.

use std::sync::Arc;

use app_config::network::Config;
use app_config_provider::{
    events::{ConfigEventBridge, ConfigTopics},
    manager::ConfigManager,
    notification::ConfigNotification,
};
use gpui_kit::{App, AppContext as _, Entity, Global};
use network::transport::hub::TransportHub;
use widget_tokio_runtime::runtime::Runtime;

use crate::state::Updater;

/// What the updater needs from the composition root.
pub struct UpdaterDeps {
    /// The application's transport: the routes, the clients and the operation barrier.
    pub transport: Arc<TransportHub>,
    /// The application runtime the requests run on.
    pub runtime: Runtime,
}

/// Installs the updater: the composition root's one call for this feature.
///
/// The first check is scheduled rather than run here, so it does not compete with
/// everything else a launch is already doing.
///
/// # Panics
///
/// Panics when `ConfigManager` was not installed, because the notification the updater
/// publishes is read from the network settings the manager holds. The composition root and
/// the test harnesses install the manager before they call this.
pub fn install(cx: &mut App, deps: UpdaterDeps) {
    let notification = ConfigNotification::new(Arc::new(
        cx.global::<ConfigManager>().read().network_config(),
    ));
    let bridge = ConfigEventBridge::install(
        cx,
        ConfigTopics::NETWORK,
        notification.clone(),
        Arc::new(
            |config: &app_config::settings::Config, _previous: &Config| config.network_config(),
        ),
    );
    let updater = cx.new(|_| Updater::new(deps, notification));
    updater.update(cx, |updater, cx| updater.start(cx));
    cx.set_global(UpdaterGlobal {
        updater,
        _bridge: bridge,
    });
}

/// The updater the composition root installed, for the views that draw it.
pub(crate) fn entity(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<UpdaterGlobal>()
        .map(|installed| installed.updater.clone())
}

/// The installed updater, in the application's global state.
///
/// The bridge lives here rather than in the entity: the updater is constructed by
/// tests without an application, and a subscription needs one.
struct UpdaterGlobal {
    updater: Entity<Updater>,
    _bridge: ConfigEventBridge,
}

impl Global for UpdaterGlobal {}
