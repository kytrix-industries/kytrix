//! Where the download manager is installed, and where its views find it.

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

use crate::provider::DownloadProvider;

/// What the download manager needs from the composition root.
pub struct DownloadProviderDeps {
    /// The application's transport: the routes, the clients and the operation barrier.
    pub transport: Arc<TransportHub>,
    /// The application runtime every transfer runs on.
    pub runtime: Runtime,
}

/// Installs the download manager: the composition root's one call for this feature.
///
/// Returns the installed entity so the owners that add downloads — the updater and
/// the translation page — can be handed it directly rather than looking it up.
///
/// # Panics
///
/// Panics when `ConfigManager` was not installed, because the network projection the
/// provider routes with is read from the settings the manager holds. The composition
/// root and the test harnesses install the manager before they call this.
pub fn install(cx: &mut App, deps: DownloadProviderDeps) -> Entity<DownloadProvider> {
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
    let provider = cx.new(|_| DownloadProvider::new(deps, notification));
    cx.set_global(DownloadProviderGlobal {
        provider: provider.clone(),
        _bridge: bridge,
    });

    provider
}

/// The download manager the composition root installed, for the views that draw it.
pub fn entity(cx: &App) -> Option<Entity<DownloadProvider>> {
    cx.try_global::<DownloadProviderGlobal>()
        .map(|installed| installed.provider.clone())
}

/// The installed provider, in the application's global state.
///
/// The bridge lives here rather than in the entity: the entity is constructed by
/// tests without an application, and a subscription needs one.
struct DownloadProviderGlobal {
    provider: Entity<DownloadProvider>,
    _bridge: ConfigEventBridge,
}

impl Global for DownloadProviderGlobal {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{
        network::{ProxyAuthorizationConfig, ProxyConfig},
        settings::Config,
    };
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{BorrowAppContext as _, TestAppContext};
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::SmolStr;
    use widget_tokio_runtime::runtime::Runtime;

    use super::{DownloadProviderDeps, entity, install};

    fn deps() -> DownloadProviderDeps {
        DownloadProviderDeps {
            transport: Arc::new(TransportHub::new(TransportPolicy {
                allow_plaintext: true,
                extra_root_certificates: Vec::new(),
            })),
            runtime: Runtime::for_test(),
        }
    }

    #[gpui_kit::test]
    fn install_should_put_the_provider_in_a_global_and_follow_the_network_settings(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
            install(cx, deps());
        });

        let provider = cx.update(|cx| entity(cx).expect("install puts the provider in a global"));
        assert!(
            cx.update(|cx| entity(cx).is_some_and(|found| found == provider)),
            "the global answers the entity install returned"
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

        let network = cx.update(|cx| provider.read(cx).config_for_test().current());
        assert!(
            network
                .proxies
                .contains_key(&SmolStr::new_static("install-bridge")),
            "a network write should reach the projection the provider routes with"
        );
    }

    #[gpui_kit::test]
    fn entity_should_answer_none_before_the_provider_is_installed(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(
                entity(cx).is_none(),
                "an uninstalled manager has no entity to answer"
            );
        });
    }
}
