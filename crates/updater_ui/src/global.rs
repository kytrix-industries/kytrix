//! Where the updater is installed, and where the views find it.

use std::sync::Arc;

use app_config::network::Config;
use gpui_kit::{App, AppContext as _, Entity, Global};
use network::transport::hub::TransportHub;
use tokio::sync::watch;
use widget_tokio_runtime::runtime::Runtime;

use crate::state::Updater;

/// What the updater needs from the composition root.
pub struct UpdaterDeps {
    /// The application's transport: the routes, the clients and the operation barrier.
    pub transport: Arc<TransportHub>,
    /// The network projection of the settings, read again for every route resolution.
    ///
    /// The field is the watch receiver itself rather than a name the settings
    /// manager gives the type: what a subscriber holds is this receiver, and the
    /// alias the manager reads it as is private to that crate.
    pub config: watch::Receiver<Arc<Config>>,
    /// The application runtime the requests run on.
    pub runtime: Runtime,
}

/// Installs the updater: the composition root's one call for this feature.
///
/// The first check is scheduled rather than run here, so it does not compete with
/// everything else a launch is already doing.
pub fn install(cx: &mut App, deps: UpdaterDeps) {
    let updater = cx.new(|_| Updater::new(deps));
    updater.update(cx, |updater, cx| updater.start(cx));
    cx.set_global(UpdaterGlobal(updater));
}

/// The updater the composition root installed, for the views that draw it.
pub(crate) fn entity(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<UpdaterGlobal>()
        .map(|installed| installed.0.clone())
}

/// The installed updater, in the application's global state.
struct UpdaterGlobal(Entity<Updater>);

impl Global for UpdaterGlobal {}
