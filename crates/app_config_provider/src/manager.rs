use std::{path::PathBuf, sync::Arc};

use app_config::{change::ConfigChangeSet, settings::Config};
use gpui_kit::{App, AppContext as _, Entity, Global};

use crate::{
    events::{
        AccountChanged, AppearanceChanged, ConfigEvents, DataTimeZoneChanged, DefaultRouteChanged,
        LanguageChanged, ProxyCatalogChanged, ServiceRouteChanged, VenueChanged,
    },
    store::{ConfigLoadError, ConfigPersistenceError, ConfigRecovery, ConfigStore},
};

/// Owns the current settings snapshot and the entity its changes are emitted from.
pub struct ConfigManager {
    store: ConfigStore,
    events: Entity<ConfigEvents>,
}

impl Global for ConfigManager {}

/// Loads the stored settings, creates the event entity and installs both.
///
/// This stays a module-level function: the composition root already calls
/// `config_manager::install(cx)`; only the construction helpers are associated.
pub fn install(cx: &mut App) -> Option<ConfigRecovery> {
    let (store, recovery) = ConfigStore::load_default();
    let events = cx.new(|_| ConfigEvents);
    cx.set_global(ConfigManager { store, events });

    recovery
}

impl ConfigManager {
    /// Creates an in-memory manager for injected runtimes and tests.
    pub fn in_memory(config: Config, cx: &mut App) -> Self {
        Self {
            store: ConfigStore::in_memory(config),
            events: cx.new(|_| ConfigEvents),
        }
    }

    /// Loads an in-memory manager from an injected path, for tests that need a file.
    pub fn from_path(path: impl Into<PathBuf>, cx: &mut App) -> Result<Self, ConfigLoadError> {
        Ok(Self {
            store: ConfigStore::from_path(path)?,
            events: cx.new(|_| ConfigEvents),
        })
    }

    /// Returns a cheaply cloned immutable snapshot of the current settings.
    pub fn read(&self) -> Arc<Config> {
        self.store.read()
    }

    /// Returns the entity every configuration event is emitted from.
    pub fn events(&self) -> &Entity<ConfigEvents> {
        &self.events
    }

    /// Stores a submitted snapshot and only then publishes it.
    ///
    /// An unchanged snapshot is neither stored nor published. The emitter is
    /// part of this manager, so a write that returns `Ok` has published exactly
    /// one change set (possibly empty only for workspaces).
    ///
    /// This takes `&mut App` so a caller cannot reach it through
    /// `cx.global_mut::<ConfigManager>()`: the borrow that yields the manager
    /// and the borrow that yields the application would overlap. Configuration
    /// writes therefore always go through `cx.update_global`.
    pub fn write(&mut self, config: Config, cx: &mut App) -> Result<(), ConfigPersistenceError> {
        if let Some(changes) = self.store.write(config)? {
            self.emit(changes, cx);
        }

        Ok(())
    }

    /// Mutates a cloned candidate and publishes it, persisting it afterwards.
    ///
    /// A persistence failure is logged, not returned; the in-memory snapshot is
    /// still published. Returns the closure's value.
    pub fn update<R>(&mut self, update: impl FnOnce(&mut Config) -> R, cx: &mut App) -> R {
        let (result, changes) = self.store.update(update);
        if let Some(changes) = changes {
            self.emit(changes, cx);
        }

        result
    }

    /// Emits one event per changed item, in a fixed domain order.
    ///
    /// The order is appearance, language, display time zone, account, proxy
    /// catalog (map order), default route, service routes (map order), venues
    /// (map order). Tests pin it; consumers must not depend on it for
    /// correctness, only the notification layer does not reorder.
    fn emit(&mut self, changes: ConfigChangeSet, cx: &mut App) {
        let events = self.events.clone();
        events.update(cx, |_, cx| {
            if let Some(appearance) = changes.appearance {
                cx.emit(AppearanceChanged(appearance));
            }
            if let Some(language) = changes.language {
                cx.emit(LanguageChanged(language));
            }
            if let Some(data_time_zone) = changes.data_time_zone {
                cx.emit(DataTimeZoneChanged(data_time_zone));
            }
            if changes.account_changed {
                cx.emit(AccountChanged);
            }
            for name in changes.proxies {
                cx.emit(ProxyCatalogChanged { name });
            }
            if changes.proxy_default_changed {
                cx.emit(DefaultRouteChanged);
            }
            for target in changes.proxy_applications {
                cx.emit(ServiceRouteChanged { target });
            }
            for venue in changes.venues {
                cx.emit(VenueChanged { venue });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc};

    use app_config::{
        network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
        settings::{AccountConfig, AccountWalletKind, Appearance, Config, Language},
        time_zone::DataTimeZone,
        workspace::{WorkspaceConfig, WorkspaceLayout},
    };
    use gpui_kit::{
        App, BorrowAppContext as _, Entity, EventEmitter, Subscription, TestAppContext,
    };
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;

    use super::ConfigManager;
    use crate::{
        events::{
            AccountChanged, AppearanceChanged, ConfigEvents, DataTimeZoneChanged,
            DefaultRouteChanged, LanguageChanged, ProxyCatalogChanged, ServiceRouteChanged,
            VenueChanged,
        },
        store::{ConfigPersistenceError, ConfigStore},
        test_support::test_temp,
    };

    /// One delivered event, kept so a test can assert both what and in which
    /// order the manager emitted.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Recorded {
        Appearance(Appearance),
        Language(Language),
        DataTimeZone(DataTimeZone),
        Account,
        Proxy(SmolStr),
        DefaultRoute,
        ServiceRoute(ServiceTarget),
        Venue(VenueId),
    }

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

    /// Installs an in-memory manager over `config`, as the composition root does.
    fn install_manager_with(cx: &mut TestAppContext, config: Config) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
        });
    }

    fn install_manager(cx: &mut TestAppContext) {
        install_manager_with(cx, Config::default());
    }

    fn events(cx: &mut TestAppContext) -> Entity<ConfigEvents> {
        cx.update(|cx| cx.global::<ConfigManager>().events().clone())
    }

    /// Stores `change` applied to the current snapshot, as a settings submission does.
    fn write_settings(
        cx: &mut TestAppContext,
        change: impl FnOnce(&mut Config),
    ) -> Result<(), ConfigPersistenceError> {
        cx.update(|cx| {
            let mut next = cx.global::<ConfigManager>().read().as_ref().clone();
            change(&mut next);
            cx.update_global::<ConfigManager, _>(|manager, cx| manager.write(next, cx))
        })
    }

    /// Subscribes `on_event` to one event type of the manager's event entity.
    fn subscribe<E: 'static>(
        cx: &mut App,
        events: &Entity<ConfigEvents>,
        mut on_event: impl FnMut(&E) + 'static,
    ) -> Subscription
    where
        ConfigEvents: EventEmitter<E>,
    {
        cx.subscribe(events, move |_, event: &E, _| on_event(event))
    }

    /// Subscribes to every configuration event and records the delivery order.
    fn record_every_event(
        cx: &mut App,
        events: &Entity<ConfigEvents>,
    ) -> (Rc<RefCell<Vec<Recorded>>>, Vec<Subscription>) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let mut subscriptions = Vec::with_capacity(8);
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &AppearanceChanged| {
                sink.borrow_mut().push(Recorded::Appearance(event.0));
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &LanguageChanged| {
                sink.borrow_mut().push(Recorded::Language(event.0));
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &DataTimeZoneChanged| {
                sink.borrow_mut().push(Recorded::DataTimeZone(event.0));
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |_: &AccountChanged| {
                sink.borrow_mut().push(Recorded::Account);
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &ProxyCatalogChanged| {
                sink.borrow_mut().push(Recorded::Proxy(event.name.clone()));
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |_: &DefaultRouteChanged| {
                sink.borrow_mut().push(Recorded::DefaultRoute);
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &ServiceRouteChanged| {
                sink.borrow_mut()
                    .push(Recorded::ServiceRoute(event.target.clone()));
            }));
        }
        {
            let sink = Rc::clone(&recorded);
            subscriptions.push(subscribe(cx, events, move |event: &VenueChanged| {
                sink.borrow_mut().push(Recorded::Venue(event.venue));
            }));
        }

        (recorded, subscriptions)
    }

    #[gpui_kit::test]
    fn a_write_should_emit_one_event_per_changed_item(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let languages = Rc::new(RefCell::new(Vec::new()));
        let zones = Rc::new(RefCell::new(Vec::new()));
        let proxies = Rc::new(RefCell::new(Vec::new()));
        let venues = Rc::new(RefCell::new(Vec::new()));
        let (_language, _zone, _proxy, _venue) = cx.update(|cx| {
            let language = subscribe(cx, &events, {
                let sink = Rc::clone(&languages);
                move |event: &LanguageChanged| sink.borrow_mut().push(event.0)
            });
            let zone = subscribe(cx, &events, {
                let sink = Rc::clone(&zones);
                move |event: &DataTimeZoneChanged| sink.borrow_mut().push(event.0)
            });
            let proxy = subscribe(cx, &events, {
                let sink = Rc::clone(&proxies);
                move |event: &ProxyCatalogChanged| sink.borrow_mut().push(event.name.clone())
            });
            let venue = subscribe(cx, &events, {
                let sink = Rc::clone(&venues);
                move |event: &VenueChanged| sink.borrow_mut().push(event.venue)
            });

            (language, zone, proxy, venue)
        });
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");

        write_settings(cx, |config| {
            config.language = Language::En;
            config.data_time_zone = zone;
            config
                .proxies
                .insert(SmolStr::new_static("local"), proxy(1080));
            config.set_venue_proxy(&VenueId::Binance, Some(ProxyRoute::System));
        })
        .expect("an in-memory manager has nothing to store");

        assert_eq!(*languages.borrow(), [Language::En]);
        assert_eq!(*zones.borrow(), [zone]);
        assert_eq!(*proxies.borrow(), [SmolStr::new_static("local")]);
        assert_eq!(*venues.borrow(), [VenueId::Binance]);
    }

    #[gpui_kit::test]
    fn a_write_that_changes_nothing_should_emit_nothing(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));
        let before = cx.update(|cx| cx.global::<ConfigManager>().read());

        write_settings(cx, |_| {}).expect("an unchanged snapshot is accepted");

        assert!(recorded.borrow().is_empty());
        let after = cx.update(|cx| cx.global::<ConfigManager>().read());
        assert!(
            Arc::ptr_eq(&before, &after),
            "an unchanged write keeps the snapshot the application reads"
        );
    }

    #[gpui_kit::test]
    fn an_update_should_emit_the_change_set_of_its_diff(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let languages = Rc::new(RefCell::new(Vec::new()));
        let _language = cx.update(|cx| {
            let sink = Rc::clone(&languages);
            subscribe(cx, &events, move |event: &LanguageChanged| {
                sink.borrow_mut().push(event.0);
            })
        });

        let result = cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(
                |config| {
                    config.language = Language::En;
                    7
                },
                cx,
            )
        });

        assert_eq!(result, 7);
        assert_eq!(*languages.borrow(), [Language::En]);
    }

    #[gpui_kit::test]
    fn a_language_write_should_emit_only_language_changed(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));

        write_settings(cx, |config| config.language = Language::En)
            .expect("an in-memory manager has nothing to store");

        assert_eq!(*recorded.borrow(), [Recorded::Language(Language::En)]);
    }

    #[gpui_kit::test]
    fn an_account_and_its_avatar_should_emit_one_account_changed(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let account_changes = Rc::new(RefCell::new(0_u32));
        let _account = cx.update(|cx| {
            let sink = Rc::clone(&account_changes);
            subscribe(cx, &events, move |_: &AccountChanged| {
                *sink.borrow_mut() += 1;
            })
        });

        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(
                |config| {
                    config.account = Some(account());
                    config.avatar = Some(String::from("<svg>avatar</svg>"));
                },
                cx,
            );
        });

        assert_eq!(*account_changes.borrow(), 1);
    }

    #[gpui_kit::test]
    fn a_workspace_only_write_should_emit_nothing(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));

        write_settings(cx, |config| {
            config.workspaces.entries.push(WorkspaceConfig {
                name: SmolStr::new_static("second"),
                layout: WorkspaceLayout::single_pane(),
            });
            config.workspaces.active = Some(SmolStr::new_static("second"));
        })
        .expect("an in-memory manager has nothing to store");

        assert!(recorded.borrow().is_empty());
        let stored = cx.update(|cx| cx.global::<ConfigManager>().read());
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

    #[gpui_kit::test]
    fn a_workspace_only_write_should_survive_a_file_backed_manager(cx: &mut TestAppContext) {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join("config.json");
        let manager = cx
            .update(|cx| ConfigManager::from_path(path.clone(), cx))
            .expect("a missing config file starts from defaults");
        cx.update(|cx| cx.set_global(manager));
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));

        write_settings(cx, |config| {
            config.workspaces.entries.push(WorkspaceConfig {
                name: SmolStr::new_static("second"),
                layout: WorkspaceLayout::single_pane(),
            });
            config.workspaces.active = Some(SmolStr::new_static("second"));
        })
        .expect("the file-backed manager should store the workspace");

        assert!(
            recorded.borrow().is_empty(),
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

    #[gpui_kit::test]
    fn a_failed_write_should_emit_nothing(cx: &mut TestAppContext) {
        let blocking_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manager = cx
            .update(|cx| ConfigManager::from_path(blocking_file.join("config.json"), cx))
            .expect("a missing config file starts from defaults");
        cx.update(|cx| cx.set_global(manager));
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));
        let before = cx.update(|cx| cx.global::<ConfigManager>().read());

        let result = write_settings(cx, |config| config.language = Language::En);

        assert!(
            result.is_err(),
            "a directory below a file cannot be created"
        );
        assert!(recorded.borrow().is_empty());
        let after = cx.update(|cx| cx.global::<ConfigManager>().read());
        assert!(
            Arc::ptr_eq(&before, &after),
            "a submission that was not stored leaves the snapshot alone"
        );
    }

    #[gpui_kit::test]
    fn events_should_arrive_in_the_documented_order(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        let service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };

        write_settings(cx, |config| {
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
            *recorded.borrow(),
            [
                Recorded::Appearance(Appearance::Dark),
                Recorded::Language(Language::En),
                Recorded::DataTimeZone(zone),
                Recorded::Account,
                Recorded::Proxy(SmolStr::new_static("local")),
                Recorded::DefaultRoute,
                Recorded::ServiceRoute(service),
                Recorded::Venue(VenueId::Binance),
            ]
        );
    }

    /// Two service routes and two venues are emitted in the maps' order, at their
    /// documented positions, whatever order the mutation applied them in.
    #[gpui_kit::test]
    fn events_should_follow_the_map_order_of_the_change_set(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let (recorded, _subscriptions) = cx.update(|cx| record_every_event(cx, &events));
        let zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        let account_service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        };
        let news_service = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };

        write_settings(cx, |config| {
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
            *recorded.borrow(),
            [
                Recorded::Appearance(Appearance::Dark),
                Recorded::Language(Language::En),
                Recorded::DataTimeZone(zone),
                Recorded::Account,
                Recorded::Proxy(SmolStr::new_static("local")),
                Recorded::DefaultRoute,
                Recorded::ServiceRoute(account_service),
                Recorded::ServiceRoute(news_service),
                Recorded::Venue(VenueId::Binance),
                Recorded::Venue(VenueId::Okx),
            ]
        );
    }

    #[gpui_kit::test]
    fn dropping_a_subscription_should_stop_delivery(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let languages = Rc::new(RefCell::new(Vec::new()));
        let subscription = cx.update(|cx| {
            let sink = Rc::clone(&languages);
            subscribe(cx, &events, move |event: &LanguageChanged| {
                sink.borrow_mut().push(event.0);
            })
        });

        drop(subscription);
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.language = Language::En, cx);
        });

        assert!(languages.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn a_proxy_catalog_change_should_emit_one_event_per_name(cx: &mut TestAppContext) {
        let mut configured = Config::default();
        configured
            .proxies
            .insert(SmolStr::new_static("zulu"), proxy(0));
        configured
            .proxies
            .insert(SmolStr::new_static("mike"), proxy(0));
        install_manager_with(cx, configured);
        let events = events(cx);
        let names = Rc::new(RefCell::new(Vec::new()));
        let _proxy = cx.update(|cx| {
            let sink = Rc::clone(&names);
            subscribe(cx, &events, move |event: &ProxyCatalogChanged| {
                sink.borrow_mut().push(event.name.clone());
            })
        });

        write_settings(cx, |config| {
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
            *names.borrow(),
            [
                SmolStr::new_static("alpha"),
                SmolStr::new_static("zulu"),
                SmolStr::new_static("mike"),
            ],
            "the added and edited names come in map order, then the removed name"
        );
    }

    #[gpui_kit::test]
    fn a_venue_change_should_carry_the_venue_key(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let venues = Rc::new(RefCell::new(Vec::new()));
        let _venue = cx.update(|cx| {
            let sink = Rc::clone(&venues);
            subscribe(cx, &events, move |event: &VenueChanged| {
                sink.borrow_mut().push(event.venue);
            })
        });

        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(
                |config| {
                    config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::System));
                },
                cx,
            );
        });

        assert_eq!(*venues.borrow(), [VenueId::Okx]);
    }

    #[gpui_kit::test]
    fn a_subscriber_that_defers_its_write_should_not_reenter_the_emit(cx: &mut TestAppContext) {
        install_manager(cx);
        let events = events(cx);
        let appearances = Rc::new(RefCell::new(Vec::new()));
        let observed_by_the_next_listener = Rc::new(RefCell::new(Vec::new()));
        let before_update = cx.update(|cx| cx.global::<ConfigManager>().read().appearance);
        // The deferred write must actually change the appearance, whichever value the
        // defaults give it, or there would be no event to observe.
        let target = if before_update == Appearance::Dark {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        let (_appearance, _language, _next_listener) = cx.update(|cx| {
            let appearance = subscribe(cx, &events, {
                let sink = Rc::clone(&appearances);
                move |event: &AppearanceChanged| sink.borrow_mut().push(event.0)
            });
            let language = cx.subscribe(
                &events,
                move |_: Entity<ConfigEvents>, _: &LanguageChanged, cx| {
                    cx.defer(move |cx| {
                        cx.update_global::<ConfigManager, _>(|manager, cx| {
                            manager.update(|config| config.appearance = target, cx);
                        });
                    });
                },
            );
            let next_listener = cx.subscribe(&events, {
                let sink = Rc::clone(&observed_by_the_next_listener);
                move |_: Entity<ConfigEvents>, _: &LanguageChanged, cx| {
                    sink.borrow_mut()
                        .push(cx.global::<ConfigManager>().read().appearance);
                }
            });

            (appearance, language, next_listener)
        });

        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.language = Language::En, cx);
        });

        assert_eq!(
            *observed_by_the_next_listener.borrow(),
            [before_update],
            "a listener registered after the writer runs before the deferred write"
        );
        assert_eq!(*appearances.borrow(), [target]);
        assert_eq!(
            cx.update(|cx| cx.global::<ConfigManager>().read().appearance),
            target
        );
    }
}
