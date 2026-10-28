//! The per-domain configuration events and the owner bridge.

use std::sync::Arc;

use app_config::{
    network::ServiceTarget,
    settings::{Appearance, Config, Language},
    time_zone::DataTimeZone,
};
use gpui_kit::{App, Entity, EventEmitter, Subscription};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use crate::{manager::ConfigManager, notification::ConfigNotification};

/// The application appearance changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppearanceChanged(pub Appearance);

/// The interface language changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageChanged(pub Language);

/// The display time zone for data timestamps changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataTimeZoneChanged(pub DataTimeZone);

/// The account or the avatar of the account changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountChanged;

/// One named proxy was added, removed or edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyCatalogChanged {
    pub name: SmolStr,
}

/// The default outbound route changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultRouteChanged;

/// One Kytrix service application route was added, removed or edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRouteChanged {
    pub target: ServiceTarget,
}

/// One venue's settings were added, removed or edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VenueChanged {
    pub venue: VenueId,
}

/// The entity every configuration event is emitted from.
///
/// The manager holds it for the life of the application; a subscription to it
/// is a subscription to the events it emits. The entity carries no state.
pub struct ConfigEvents;

impl EventEmitter<AppearanceChanged> for ConfigEvents {}
impl EventEmitter<LanguageChanged> for ConfigEvents {}
impl EventEmitter<DataTimeZoneChanged> for ConfigEvents {}
impl EventEmitter<AccountChanged> for ConfigEvents {}
impl EventEmitter<ProxyCatalogChanged> for ConfigEvents {}
impl EventEmitter<DefaultRouteChanged> for ConfigEvents {}
impl EventEmitter<ServiceRouteChanged> for ConfigEvents {}
impl EventEmitter<VenueChanged> for ConfigEvents {}

/// The domains one owner subscribes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigTopics {
    network: bool,
    venues: bool,
}

impl ConfigTopics {
    /// The proxy catalog, the default route and the service application routes.
    pub const NETWORK: Self = Self {
        network: true,
        venues: false,
    };
    /// The venue settings.
    pub const VENUES: Self = Self {
        network: false,
        venues: true,
    };
    /// Both network and venue settings.
    pub const NETWORK_AND_VENUES: Self = Self {
        network: true,
        venues: true,
    };
}

/// Keeps one owner subscribed to the domains it names.
///
/// The bridge is created exactly once per owner, in that owner's
/// `install(cx, ...)`. Dropping it unsubscribes every domain it holds.
pub struct ConfigEventBridge {
    _subscriptions: Vec<Subscription>,
}

impl ConfigEventBridge {
    /// Subscribes `project` to every domain in `topics`.
    ///
    /// Each event reads the manager's current snapshot and the owner's previous
    /// projection, builds the next projection and publishes it. The callback
    /// runs on the interface thread, in event order.
    pub fn install<P>(
        cx: &mut App,
        topics: ConfigTopics,
        notification: ConfigNotification<P>,
        project: Arc<dyn Fn(&Config, &P) -> P>,
    ) -> Self
    where
        P: Send + Sync + 'static,
    {
        let events = cx.global::<ConfigManager>().events().clone();
        let mut subscriptions = Vec::with_capacity(4);
        if topics.network {
            subscriptions.push(subscribe::<P, ProxyCatalogChanged>(
                cx,
                &events,
                notification.clone(),
                project.clone(),
            ));
            subscriptions.push(subscribe::<P, DefaultRouteChanged>(
                cx,
                &events,
                notification.clone(),
                project.clone(),
            ));
            subscriptions.push(subscribe::<P, ServiceRouteChanged>(
                cx,
                &events,
                notification.clone(),
                project.clone(),
            ));
        }
        if topics.venues {
            subscriptions.push(subscribe::<P, VenueChanged>(
                cx,
                &events,
                notification,
                project,
            ));
        }
        Self {
            _subscriptions: subscriptions,
        }
    }
}

fn subscribe<P, E>(
    cx: &mut App,
    events: &Entity<ConfigEvents>,
    notification: ConfigNotification<P>,
    project: Arc<dyn Fn(&Config, &P) -> P>,
) -> Subscription
where
    P: Send + Sync + 'static,
    E: 'static,
    ConfigEvents: EventEmitter<E>,
{
    cx.subscribe(events, move |_, _event: &E, cx| {
        let config = cx.global::<ConfigManager>().read();
        let previous = notification.current();
        notification.publish(Arc::new(project(&config, &previous)));
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{
        network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
        settings::Config,
    };
    use gpui_kit::TestAppContext;
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;

    use super::{ConfigEventBridge, ConfigTopics};
    use crate::{manager::ConfigManager, notification::ConfigNotification};

    /// A projection small enough to compare and to cross threads.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Projection {
        publishes: u32,
        proxies: usize,
        venue_routes: usize,
        services: usize,
    }

    fn project(config: &Config, previous: &Projection) -> Projection {
        Projection {
            publishes: previous.publishes + 1,
            proxies: config.proxies.len(),
            venue_routes: config.network_config().venue_routes.len(),
            services: config.proxy_applications.len(),
        }
    }

    fn install_manager(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });
    }

    fn install_bridge(
        cx: &mut TestAppContext,
        topics: ConfigTopics,
    ) -> (ConfigEventBridge, ConfigNotification<Projection>) {
        cx.update(|cx| {
            let notification = ConfigNotification::new(Arc::new(Projection {
                publishes: 0,
                proxies: 0,
                venue_routes: 0,
                services: 0,
            }));
            let bridge =
                ConfigEventBridge::install(cx, topics, notification.clone(), Arc::new(project));

            (bridge, notification)
        })
    }

    fn change_settings(cx: &mut TestAppContext, change: impl FnOnce(&mut Config)) {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(change, cx);
        });
    }

    fn add_proxy(cx: &mut TestAppContext, name: &'static str) {
        change_settings(cx, |config| {
            config
                .proxies
                .insert(SmolStr::new_static(name), ProxyConfig::default());
        });
    }

    #[gpui_kit::test]
    fn a_bridged_topic_should_publish_the_projection_built_from_the_current_snapshot(
        cx: &mut TestAppContext,
    ) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);

        add_proxy(cx, "local");

        let current = notification.current();
        assert_eq!(current.publishes, 1);
        assert_eq!(current.proxies, 1);
    }

    #[gpui_kit::test]
    fn a_bridge_should_ignore_a_domain_it_did_not_subscribe_to(cx: &mut TestAppContext) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);
        let initial = notification.current();

        change_settings(cx, |config| {
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        });

        assert!(Arc::ptr_eq(&notification.current(), &initial));
    }

    #[gpui_kit::test]
    fn each_event_should_publish_once_even_when_two_events_arrive_in_one_update(
        cx: &mut TestAppContext,
    ) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);

        change_settings(cx, |config| {
            config
                .proxies
                .insert(SmolStr::new_static("local"), ProxyConfig::default());
            config.proxy_default = ProxyRoute::System;
        });

        let current = notification.current();
        assert_eq!(current.publishes, 2, "each delivered event publishes once");
        assert_eq!(current.proxies, 1);
    }

    #[gpui_kit::test]
    fn a_projection_should_be_built_from_the_previous_projection(cx: &mut TestAppContext) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);

        add_proxy(cx, "first");
        assert_eq!(notification.current().publishes, 1);

        add_proxy(cx, "second");
        assert_eq!(notification.current().publishes, 2);
    }

    #[gpui_kit::test]
    fn a_service_route_change_should_publish_from_a_network_bridge(cx: &mut TestAppContext) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };

        change_settings(cx, |config| {
            config
                .proxy_applications
                .insert(target.clone(), ProxyRoute::Direct);
        });

        let current = notification.current();
        assert_eq!(current.publishes, 1);
        assert_eq!(current.services, 1);
        assert_eq!(current.proxies, 0);
        assert_eq!(current.venue_routes, 0);
    }

    #[gpui_kit::test]
    fn a_venues_bridge_should_publish_on_a_venue_change_and_ignore_a_network_change(
        cx: &mut TestAppContext,
    ) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::VENUES);

        add_proxy(cx, "local");
        assert_eq!(
            notification.current().publishes,
            0,
            "a venues-only bridge ignores the proxy catalog"
        );

        change_settings(cx, |config| config.proxy_default = ProxyRoute::System);
        assert_eq!(
            notification.current().publishes,
            0,
            "a venues-only bridge ignores the default route"
        );

        change_settings(cx, |config| {
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        });

        let current = notification.current();
        assert_eq!(current.publishes, 1);
        assert_eq!(current.venue_routes, 1);
        assert_eq!(
            current.proxies, 1,
            "the projection reads the current snapshot"
        );
    }

    #[gpui_kit::test]
    fn a_bridge_over_both_topics_should_publish_for_both_domains(cx: &mut TestAppContext) {
        install_manager(cx);
        let (_bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK_AND_VENUES);
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::EthereumWallet,
        };

        add_proxy(cx, "local");
        assert_eq!(notification.current().publishes, 1);
        assert_eq!(notification.current().proxies, 1);

        change_settings(cx, |config| {
            config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::System));
            config
                .proxy_applications
                .insert(target.clone(), ProxyRoute::Direct);
        });

        let current = notification.current();
        assert_eq!(current.publishes, 3);
        assert_eq!(current.venue_routes, 1);
        assert_eq!(current.services, 1);
        assert_eq!(current.proxies, 1);
    }

    #[gpui_kit::test]
    fn dropping_a_bridge_should_stop_publishing(cx: &mut TestAppContext) {
        install_manager(cx);
        let (bridge, notification) = install_bridge(cx, ConfigTopics::NETWORK);
        add_proxy(cx, "local");
        assert_eq!(notification.current().publishes, 1);

        drop(bridge);
        add_proxy(cx, "second");

        let current = notification.current();
        assert_eq!(
            current.publishes, 1,
            "a dropped bridge holds no subscriptions"
        );
        assert_eq!(current.proxies, 1, "and publishes no later projection");
    }
}
