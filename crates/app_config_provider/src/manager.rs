use std::{path::PathBuf, sync::Arc};

use app_config::settings::Config;

use crate::{
    events::{ConfigEvents, ConfigSubscription},
    store::{ConfigLoadError, ConfigPersistenceError, ConfigStore},
};

/// Owns the current settings snapshot and publishes its domain changes.
///
/// This is a plain value: the composition root holds it mutably and every write
/// goes through that borrow. There is no process-global installation step, so a
/// caller decides where the manager lives and on which thread it is written.
pub struct ConfigManager {
    store: ConfigStore,
    events: ConfigEvents,
}

impl ConfigManager {
    /// Wraps an already loaded store.
    pub fn new(store: ConfigStore) -> Self {
        Self {
            store,
            events: ConfigEvents::new(),
        }
    }

    /// Creates an in-memory manager for injected runtimes and tests.
    pub fn in_memory(config: Config) -> Self {
        Self::new(ConfigStore::in_memory(config))
    }

    /// Loads a file-backed manager from an injected path.
    ///
    /// A missing file starts with [`Config::default()`]; existing files that
    /// cannot be read or decoded are reported to the caller.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, ConfigLoadError> {
        Ok(Self::new(ConfigStore::from_path(path)?))
    }

    /// Returns a cheaply cloned immutable snapshot of the current settings.
    pub fn read(&self) -> Arc<Config> {
        self.store.read()
    }

    /// Stores a submitted snapshot and only then publishes it.
    ///
    /// An unchanged snapshot is neither stored nor published: that write returns
    /// `Ok(())` and publishes nothing. A write that stores a changed snapshot
    /// has published exactly one change set (possibly empty only for state that
    /// raises no domain event: the workspaces and the sidebar state).
    pub fn write(&mut self, config: Config) -> Result<(), ConfigPersistenceError> {
        if let Some(changes) = self.store.write(config)? {
            self.events.publish(changes);
        }

        Ok(())
    }

    /// Mutates a cloned candidate and publishes it, persisting it afterwards.
    ///
    /// A persistence failure is logged by the store, not returned; the in-memory
    /// snapshot is still published. Returns the closure's value.
    pub fn update<R>(&mut self, update: impl FnOnce(&mut Config) -> R) -> R {
        let (result, changes) = self.store.update(update);
        if let Some(changes) = changes {
            self.events.publish(changes);
        }

        result
    }

    /// Adds one subscriber to this manager's change channel.
    ///
    /// A subscription only receives events published after it was created; the
    /// current snapshot is always available through [`Self::read`].
    pub fn subscribe(&self) -> ConfigSubscription {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use app_config::{
        network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
        settings::{AccountConfig, AccountWalletKind, Appearance, Config, Language},
        time_zone::DataTimeZone,
        workspace::{WorkspaceConfig, WorkspaceLayout},
    };
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;

    use super::ConfigManager;
    use crate::{
        events::{ConfigEvent, ConfigSubscription},
        store::{ConfigPersistenceError, ConfigStore},
        test_support::test_temp,
    };

    // The former `a_subscriber_that_defers_its_write_should_not_reenter_the_emit`
    // test was deleted with the UI-toolkit migration: deferral was a context
    // feature of that toolkit. This design publishes to channels instead of
    // running callbacks, so a subscriber cannot re-enter a write at all.

    fn account() -> AccountConfig {
        AccountConfig {
            user_id: String::from("11111111-1111-4111-8111-111111111111"),
            wallet_address: String::from("0x0123456789abcdef"),
            wallet_kind: AccountWalletKind::Ethereum,
        }
    }

    fn proxy(port: u16) -> ProxyConfig {
        ProxyConfig {
            port,
            ..ProxyConfig::default()
        }
    }

    /// Stores `change` applied to the current snapshot, as a settings submission does.
    fn write_settings(
        manager: &mut ConfigManager,
        change: impl FnOnce(&mut Config),
    ) -> Result<(), ConfigPersistenceError> {
        let mut next = manager.read().as_ref().clone();
        change(&mut next);
        manager.write(next)
    }

    /// Takes every event published so far, in delivery order.
    fn drain(subscription: &mut ConfigSubscription) -> Vec<ConfigEvent> {
        std::iter::from_fn(|| subscription.try_recv()).collect()
    }

    #[test]
    fn a_write_should_emit_one_event_per_changed_item() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");

        write_settings(&mut manager, |config| {
            config.language = Language::En;
            config.data_time_zone = zone;
            config
                .proxies
                .insert(SmolStr::new_static("local"), proxy(1080));
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        })
        .expect("an in-memory manager has nothing to store");

        assert_eq!(
            drain(&mut events),
            [
                ConfigEvent::Language(Language::En),
                ConfigEvent::DataTimeZone(zone),
                ConfigEvent::ProxyCatalog(SmolStr::new_static("local")),
                ConfigEvent::Venue(VenueId::Binance),
            ]
        );
    }

    #[test]
    fn a_write_that_changes_nothing_should_emit_nothing() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();
        let before = manager.read();

        write_settings(&mut manager, |_| {}).expect("an unchanged snapshot is accepted");

        assert!(drain(&mut events).is_empty());
        let after = manager.read();
        assert!(
            Arc::ptr_eq(&before, &after),
            "an unchanged write keeps the snapshot the application reads"
        );
    }

    #[test]
    fn an_update_should_emit_the_change_set_of_its_diff() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();

        let result = manager.update(|config| {
            config.language = Language::En;
            7
        });

        assert_eq!(result, 7);
        assert_eq!(drain(&mut events), [ConfigEvent::Language(Language::En)]);
    }

    #[test]
    fn a_language_write_should_emit_only_language_changed() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();

        write_settings(&mut manager, |config| config.language = Language::En)
            .expect("an in-memory manager has nothing to store");

        assert_eq!(drain(&mut events), [ConfigEvent::Language(Language::En)]);
    }

    #[test]
    fn an_account_and_its_avatar_should_emit_one_account_changed() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();

        manager.update(|config| {
            config.account = Some(account());
            config.avatar = Some(String::from("<svg>avatar</svg>"));
        });

        assert_eq!(drain(&mut events), [ConfigEvent::Account]);
    }

    #[test]
    fn a_workspace_only_write_should_emit_nothing() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();

        write_settings(&mut manager, |config| {
            config.workspaces.entries.push(WorkspaceConfig {
                name: SmolStr::new_static("second"),
                layout: WorkspaceLayout::single_pane(),
            });
            config.workspaces.active = Some(SmolStr::new_static("second"));
        })
        .expect("an in-memory manager has nothing to store");

        assert!(drain(&mut events).is_empty());
        let stored = manager.read();
        assert_eq!(
            stored.workspaces.active,
            Some(SmolStr::new_static("second")),
            "the write reached the store even though it emits nothing"
        );
        assert!(
            stored
                .workspaces
                .entries
                .iter()
                .any(|workspace| workspace.name == "second"),
            "the new workspace entry is stored"
        );
    }

    #[test]
    fn a_workspace_only_write_should_survive_a_file_backed_manager() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join("config.json");
        let mut manager = ConfigManager::from_path(path.clone())
            .expect("a missing config file starts from defaults");
        let mut events = manager.subscribe();

        write_settings(&mut manager, |config| {
            config.workspaces.entries.push(WorkspaceConfig {
                name: SmolStr::new_static("second"),
                layout: WorkspaceLayout::single_pane(),
            });
            config.workspaces.active = Some(SmolStr::new_static("second"));
        })
        .expect("the file-backed manager should store the workspace");

        assert!(
            drain(&mut events).is_empty(),
            "a workspace-only write emits no domain event"
        );
        let stored = ConfigStore::from_path(path)
            .expect("the stored config should load")
            .read();
        assert_eq!(
            stored.workspaces.active,
            Some(SmolStr::new_static("second")),
            "the active workspace reached the file"
        );
        assert!(
            stored
                .workspaces
                .entries
                .iter()
                .any(|workspace| workspace.name == "second"),
            "the named workspace entry reached the file"
        );
    }

    #[test]
    fn a_failed_write_should_emit_nothing() {
        let blocking_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut manager = ConfigManager::from_path(blocking_file.join("config.json"))
            .expect("a missing config file starts from defaults");
        let mut events = manager.subscribe();
        let before = manager.read();

        let result = write_settings(&mut manager, |config| config.language = Language::En);

        assert!(
            result.is_err(),
            "a directory below a file cannot be created"
        );
        assert!(drain(&mut events).is_empty());
        let after = manager.read();
        assert!(
            Arc::ptr_eq(&before, &after),
            "a submission that was not stored leaves the snapshot alone"
        );
    }

    #[test]
    fn events_should_arrive_in_the_documented_order() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        let service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };

        write_settings(&mut manager, |config| {
            config.appearance = Appearance::Dark;
            config.language = Language::En;
            config.data_time_zone = zone;
            config.account = Some(account());
            config
                .proxies
                .insert(SmolStr::new_static("local"), proxy(1080));
            config.proxy_default = ProxyRoute::System;
            config
                .proxy_applications
                .insert(service.clone(), ProxyRoute::Direct);
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        })
        .expect("an in-memory manager has nothing to store");

        assert_eq!(
            drain(&mut events),
            [
                ConfigEvent::Appearance(Appearance::Dark),
                ConfigEvent::Language(Language::En),
                ConfigEvent::DataTimeZone(zone),
                ConfigEvent::Account,
                ConfigEvent::ProxyCatalog(SmolStr::new_static("local")),
                ConfigEvent::DefaultRoute,
                ConfigEvent::ServiceRoute(service),
                ConfigEvent::Venue(VenueId::Binance),
            ]
        );
    }

    /// Two service routes and two venues are published in the maps' order, at
    /// their documented positions, whatever order the mutation applied them in.
    #[test]
    fn events_should_follow_the_map_order_of_the_change_set() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        let account_service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        };
        let news_service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };

        write_settings(&mut manager, |config| {
            config.appearance = Appearance::Dark;
            config.language = Language::En;
            config.data_time_zone = zone;
            config.account = Some(account());
            config
                .proxies
                .insert(SmolStr::new_static("local"), proxy(1080));
            config.proxy_default = ProxyRoute::System;
            // Applied in reverse key order, so the assertion proves the emission
            // follows the maps rather than the order this closure applied.
            config
                .proxy_applications
                .insert(news_service.clone(), ProxyRoute::System);
            config
                .proxy_applications
                .insert(account_service.clone(), ProxyRoute::Direct);
            config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::Direct));
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        })
        .expect("an in-memory manager has nothing to store");

        assert_eq!(
            drain(&mut events),
            [
                ConfigEvent::Appearance(Appearance::Dark),
                ConfigEvent::Language(Language::En),
                ConfigEvent::DataTimeZone(zone),
                ConfigEvent::Account,
                ConfigEvent::ProxyCatalog(SmolStr::new_static("local")),
                ConfigEvent::DefaultRoute,
                ConfigEvent::ServiceRoute(account_service),
                ConfigEvent::ServiceRoute(news_service),
                ConfigEvent::Venue(VenueId::Binance),
                ConfigEvent::Venue(VenueId::Okx),
            ]
        );
    }

    #[test]
    fn dropping_a_subscription_should_stop_delivery() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut subscription = manager.subscribe();
        manager.update(|config| config.language = Language::En);
        assert_eq!(
            drain(&mut subscription),
            [ConfigEvent::Language(Language::En)]
        );

        drop(subscription);
        manager.update(|config| config.language = Language::ZhCn);

        let mut fresh = manager.subscribe();
        manager.update(|config| config.language = Language::En);
        assert_eq!(
            drain(&mut fresh),
            [ConfigEvent::Language(Language::En)],
            "a subscription only receives what is published after it was created"
        );
    }

    #[test]
    fn a_proxy_catalog_change_should_emit_one_event_per_name() {
        let mut configured = Config::default();
        configured
            .proxies
            .insert(SmolStr::new_static("zulu"), proxy(0));
        configured
            .proxies
            .insert(SmolStr::new_static("mike"), proxy(0));
        let mut manager = ConfigManager::in_memory(configured);
        let mut events = manager.subscribe();

        write_settings(&mut manager, |config| {
            config
                .proxies
                .insert(SmolStr::new_static("alpha"), proxy(0));
            config
                .proxies
                .insert(SmolStr::new_static("zulu"), proxy(1080));
            config.proxies.remove("mike");
        })
        .expect("an in-memory manager has nothing to store");

        assert_eq!(
            drain(&mut events),
            [
                ConfigEvent::ProxyCatalog(SmolStr::new_static("alpha")),
                ConfigEvent::ProxyCatalog(SmolStr::new_static("zulu")),
                ConfigEvent::ProxyCatalog(SmolStr::new_static("mike")),
            ],
            "the added and edited names come in map order, then the removed name"
        );
    }

    #[test]
    fn a_venue_change_should_carry_the_venue_key() {
        let mut manager = ConfigManager::in_memory(Config::default());
        let mut events = manager.subscribe();

        manager.update(|config| {
            config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::System));
        });

        assert_eq!(drain(&mut events), [ConfigEvent::Venue(VenueId::Okx)]);
    }
}
