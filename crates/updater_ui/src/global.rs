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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{
        network::{ProxyAuthorizationConfig, ProxyConfig},
        settings::{Config, Language},
    };
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{BorrowAppContext as _, TestAppContext};
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::SmolStr;
    use widget_tokio_runtime::runtime::Runtime;

    use super::{UpdaterDeps, entity, install};

    #[gpui_kit::test]
    fn install_should_connect_the_updater_to_the_network_projection_of_the_settings(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
            install(
                cx,
                UpdaterDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                    runtime: Runtime::for_test(),
                },
            );
        });

        let updater = cx.update(|cx| entity(cx).expect("install puts the updater in a global"));
        let initial = cx.update(|cx| updater.read(cx).config_for_test().current());
        assert_eq!(
            initial.as_ref(),
            &cx.update(|cx| cx.global::<ConfigManager>().read().network_config()),
            "the updater starts from the network projection the manager holds"
        );

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.proxies.insert(
                            SmolStr::new_static("install-bridge"),
                            ProxyConfig {
                                addr: SmolStr::new_static("127.0.0.1"),
                                port: 9,
                                authorization: ProxyAuthorizationConfig::None,
                                resolve_dns_at_remote: false,
                            },
                        );
                    },
                    cx,
                );
            });
        });
        let after_network_write = cx.update(|cx| updater.read(cx).config_for_test().current());
        assert!(
            after_network_write
                .proxies
                .contains_key(&SmolStr::new_static("install-bridge")),
            "a network write should reach the projection the updater routes with"
        );

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.language = Language::En, cx);
            });
        });
        let after_language_write = cx.update(|cx| updater.read(cx).config_for_test().current());
        assert!(
            Arc::ptr_eq(&after_network_write, &after_language_write),
            "a write outside the network domain must not publish a network projection"
        );
    }
}
