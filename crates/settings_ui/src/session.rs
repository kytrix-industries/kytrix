//! What one settings session writes: every change is stored as it is made.
//!
//! Nothing here holds the settings. They live in `app_config_provider`'s global, and a change is
//! applied to the snapshot of the moment it is written rather than to a copy taken when the page
//! opened: background work writes them while the page is open — an avatar is generated, a
//! workspace layout is persisted, a sidebar state is saved — and a write must not undo it.
//!
//! A change is stored before it is published: [`ConfigManager::write`] persists the candidate and
//! only then emits what changed, so a change that cannot be stored leaves the settings exactly as
//! they were. The reason is kept here for the page to report, and what the user typed is what they
//! can try again with.

use std::{error::Error, fmt, path::PathBuf, sync::Arc};

use app_config::{
    network::ProxyConfig,
    settings::Config,
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::{manager::ConfigManager, store::ConfigPersistenceError};
use gpui_kit::{App, BorrowAppContext as _, Context};
use smol_str::SmolStr;

use crate::{access::FieldValue, catalog::FieldAccess};

/// Why a change could not be stored.
///
/// The store's own error is shared rather than copied, because the page keeps the last one until
/// it has reported it and a [`ConfigPersistenceError`] is not cloneable.
#[derive(Clone, Debug)]
pub(crate) struct SettingsWriteError {
    cause: Arc<ConfigPersistenceError>,
}

impl SettingsWriteError {
    fn new(cause: ConfigPersistenceError) -> Self {
        Self {
            cause: Arc::new(cause),
        }
    }
}

impl fmt::Display for SettingsWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.cause)
    }
}

impl Error for SettingsWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// One change made to the settings, applied to the snapshot it is written on.
pub(crate) enum SettingsChange {
    /// One field of the catalog, written the way its own access writes it.
    Field {
        access: FieldAccess,
        value: FieldValue,
    },
    /// The directory the local translation model is loaded from.
    TranslationModelDirectory { directory: PathBuf },
    /// The model the local translation is loaded from, or none when no model is chosen.
    TranslationModelSelected { model: Option<TranslationModel> },
    /// The site the local translation model is downloaded from.
    TranslationDownloadSite { site: DownloadSite },
    /// One entry added to the proxy catalog.
    AddProxy { name: SmolStr, proxy: ProxyConfig },
    /// One entry renamed and rewritten, together with every route bound to its name.
    ReplaceProxy {
        original: SmolStr,
        name: SmolStr,
        proxy: ProxyConfig,
    },
    /// One entry removed, together with every route bound to its name.
    RemoveProxy { name: SmolStr },
}

impl SettingsChange {
    /// Makes this change in `config`.
    pub(crate) fn apply(&self, config: &mut Config) {
        match self {
            Self::Field { access, value } => access.write(config, value),
            Self::TranslationModelDirectory { directory } => {
                config.translation.model_directory = Some(directory.clone());
            }
            Self::TranslationModelSelected { model } => {
                config.translation.selected_model = *model;
            }
            Self::TranslationDownloadSite { site } => {
                config.translation.download_site = *site;
            }
            Self::AddProxy { name, proxy } => {
                config.proxies.insert(name.clone(), proxy.clone());
            }
            Self::ReplaceProxy {
                original,
                name,
                proxy,
            } => {
                // The form is modal and is the only interface that writes the catalog, so a
                // replacement that cannot be made means the settings were changed by hand
                // outside the application: the source entry is gone or its new name is taken.
                // The failure is logged and the change is abandoned rather than written as an
                // insert, which would leave every route bound to the old name.
                if let Err(error) = config.replace_proxy(original, name.clone(), proxy.clone()) {
                    log::error!("failed to replace proxy {original}: {error}");
                }
            }
            Self::RemoveProxy { name } => {
                config.remove_proxy(name);
            }
        }
    }
}

/// The session the settings page and its views write through.
///
/// The entity exists so what draws the settings can observe it, and so a write that could not be
/// stored reaches the page as one repaint that carries the reason.
pub(crate) struct SettingsSession {
    /// Why the last write could not be stored, waiting for the page to take it and report it.
    failure: Option<SettingsWriteError>,
}

impl SettingsSession {
    pub(crate) fn new() -> Self {
        Self { failure: None }
    }

    /// The settings as they are now.
    ///
    /// This is the snapshot of the moment rather than a copy of the session: a write applies its
    /// changes to a snapshot read again, so what is read here is only what to draw.
    pub(crate) fn config(&self, cx: &App) -> Arc<Config> {
        cx.global::<ConfigManager>().read()
    }

    /// Writes one change into the settings.
    pub(crate) fn write(
        &mut self,
        change: SettingsChange,
        cx: &mut Context<Self>,
    ) -> Result<(), SettingsWriteError> {
        self.write_all(std::iter::once(change), cx)
    }

    /// Writes a group of changes into the settings as one stored configuration.
    ///
    /// The order is the order they are applied in. A directory and the selection it invalidates
    /// go through here, so the two land in one write and one set of events: there is no moment
    /// where the stored directory moved but the selection still names the old one.
    pub(crate) fn write_all(
        &mut self,
        changes: impl IntoIterator<Item = SettingsChange>,
        cx: &mut Context<Self>,
    ) -> Result<(), SettingsWriteError> {
        let changes = changes.into_iter().collect::<Vec<_>>();
        if changes.is_empty() {
            return Ok(());
        }

        let current = self.config(cx);
        let mut candidate = current.as_ref().clone();
        for change in changes {
            change.apply(&mut candidate);
        }
        // A change that produces what is stored already is not a write: the settings keep the
        // value they have, and there is nothing for the user to be told.
        if &candidate == current.as_ref() {
            return Ok(());
        }

        let result = cx
            .update_global::<ConfigManager, _>(|manager, cx| manager.write(candidate, cx))
            .map_err(SettingsWriteError::new);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        // What draws the settings is told the settings moved; a failed write repaints too, because
        // the page is what takes the reason and reports it.
        cx.notify();
        result
    }

    /// Takes the last failure, so the page reports one reason once.
    pub(crate) fn take_failure(&mut self) -> Option<SettingsWriteError> {
        self.failure.take()
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, fs, sync::Arc};

    use app_config::{
        network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
        settings::{AccountConfig, AccountWalletKind, Config, Language},
        translation::{DownloadSite, TranslationModel},
    };
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext};
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;
    use tempfile::TempDir;

    use super::{SettingsChange, SettingsSession};
    use crate::{
        access::FieldValue,
        catalog::{FieldAccess, catalog},
        schema,
    };

    fn test_temp(name: &str) -> TempDir {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("this crate lives two directories below the workspace root");
        let root = workspace.join("target").join("test-temp").join("session");
        fs::create_dir_all(&root).expect("the test temporary directory should be created");
        tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&root)
            .expect("a temporary directory should be created")
    }

    /// Opens a session over an in-memory manager holding `config`.
    fn session_over(config: Config, cx: &mut TestAppContext) -> Entity<SettingsSession> {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
            cx.new(|_| SettingsSession::new())
        })
    }

    fn config_now(cx: &TestAppContext) -> Arc<Config> {
        cx.read(|cx| cx.global::<ConfigManager>().read())
    }

    fn access(page: &str, field: &str) -> FieldAccess {
        catalog(&schema::tests::venues())
            .page(page)
            .unwrap_or_else(|| panic!("{page} should be in the catalog"))
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|definition| definition.id == field)
            .unwrap_or_else(|| panic!("{page} should offer {field}"))
            .access
            .clone()
    }

    fn proxy(port: u16) -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port,
            ..ProxyConfig::default()
        }
    }

    fn service_target() -> ServiceTarget {
        ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        }
    }

    fn write(
        session: &Entity<SettingsSession>,
        change: SettingsChange,
        cx: &mut TestAppContext,
    ) -> Result<(), super::SettingsWriteError> {
        session.update(cx, |session, cx| session.write(change, cx))
    }

    /// The settings a session writes are stored at once, not held for a later confirmation.
    #[gpui_kit::test]
    fn a_change_should_reach_the_settings_at_once(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);

        write(
            &session,
            SettingsChange::Field {
                access: access("appearance", "language"),
                value: FieldValue::new_static("en"),
            },
            cx,
        )
        .expect("an in-memory manager stores every settings change");

        assert_eq!(
            config_now(cx).language,
            Language::En,
            "the change should be the settings of the moment it was written"
        );
    }

    /// A change is applied to the settings as they are when it is written, so what another
    /// subsystem wrote while the page was open survives it.
    #[gpui_kit::test]
    fn a_change_should_keep_what_was_written_while_the_page_was_open(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.avatar = Some("<svg>generated-while-open</svg>".to_owned());
                        config.account = Some(AccountConfig {
                            user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                            wallet_address: "0xfeed".to_owned(),
                            wallet_kind: AccountWalletKind::Ethereum,
                        });
                    },
                    cx,
                );
            });
        });

        write(
            &session,
            SettingsChange::Field {
                access: access("appearance", "appearance"),
                value: FieldValue::new_static("Dark"),
            },
            cx,
        )
        .expect("an in-memory manager stores every settings change");

        let stored = config_now(cx);
        assert_eq!(stored.appearance, app_config::settings::Appearance::Dark);
        assert_eq!(
            stored.avatar.as_deref(),
            Some("<svg>generated-while-open</svg>"),
            "a change must not roll back the avatar another subsystem wrote"
        );
        assert_eq!(
            stored
                .account
                .as_ref()
                .map(|account| account.user_id.as_str()),
            Some("22222222-2222-4222-8222-222222222222"),
            "a change must not roll back the account another subsystem signed in"
        );
    }

    /// Adding an entry is one commit: the settings hold it as soon as the change is written.
    #[gpui_kit::test]
    fn adding_a_proxy_should_store_it_at_once(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);

        write(
            &session,
            SettingsChange::AddProxy {
                name: SmolStr::new_static("hk"),
                proxy: proxy(1080),
            },
            cx,
        )
        .expect("an in-memory manager stores every settings change");

        assert_eq!(
            config_now(cx).proxies.get("hk").map(|proxy| proxy.port),
            Some(1080)
        );
    }

    /// Renaming an entry moves every named route with it, in the same commit.
    #[gpui_kit::test]
    fn replacing_a_proxys_name_should_move_the_routes_bound_to_it(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config
                            .proxies
                            .insert(SmolStr::new_static("hk"), proxy(1080));
                        config.proxy_default = ProxyRoute::named("hk");
                        config
                            .proxy_applications
                            .insert(service_target(), ProxyRoute::named("hk"));
                        config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::named("hk")));
                    },
                    cx,
                );
            });
        });

        write(
            &session,
            SettingsChange::ReplaceProxy {
                original: SmolStr::new_static("hk"),
                name: SmolStr::new_static("osaka"),
                proxy: proxy(1081),
            },
            cx,
        )
        .expect("an in-memory manager stores every settings change");

        let stored = config_now(cx);
        assert!(
            !stored.proxies.contains_key("hk") && stored.proxies.contains_key("osaka"),
            "the entry should have moved to its new name"
        );
        assert_eq!(stored.proxies["osaka"].port, 1081);
        assert_eq!(stored.proxy_default, ProxyRoute::named("osaka"));
        assert_eq!(
            stored.proxy_applications[&service_target()],
            ProxyRoute::named("osaka"),
            "a service route bound to the entry should follow the rename"
        );
        assert_eq!(
            stored.venue(&VenueId::Okx).and_then(|venue| venue.proxy()),
            Some(ProxyRoute::named("osaka")),
            "a venue bound to the entry should follow the rename"
        );
    }

    /// Removing an entry resets every route that named it, in the same commit.
    #[gpui_kit::test]
    fn removing_a_proxy_should_reset_the_routes_bound_to_it(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config
                            .proxies
                            .insert(SmolStr::new_static("hk"), proxy(1080));
                        config.proxy_default = ProxyRoute::named("hk");
                        config
                            .proxy_applications
                            .insert(service_target(), ProxyRoute::named("hk"));
                        config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::named("hk")));
                    },
                    cx,
                );
            });
        });

        write(
            &session,
            SettingsChange::RemoveProxy {
                name: SmolStr::new_static("hk"),
            },
            cx,
        )
        .expect("an in-memory manager stores every settings change");

        let stored = config_now(cx);
        assert!(stored.proxies.is_empty(), "the entry should be gone");
        assert_eq!(
            stored.proxy_default,
            ProxyRoute::Direct,
            "a default line naming the removed entry falls back to the direct line"
        );
        assert!(
            stored.proxy_applications.is_empty(),
            "every service route naming the removed entry should be reset"
        );
        assert!(
            stored
                .venue(&VenueId::Okx)
                .is_some_and(|venue| venue.proxy().is_none()),
            "a venue naming the removed entry should follow the default line again"
        );
    }

    /// A directory change and the selection it invalidates are applied to one candidate, so the
    /// snapshot the settings publish holds the new directory and no selection naming the old one.
    #[gpui_kit::test]
    fn a_directory_change_should_clear_the_selection_it_invalidates(cx: &mut TestAppContext) {
        let first = test_temp("first-directory");
        let second = test_temp("second-directory");
        let mut config = Config::default();
        config.translation.model_directory = Some(first.path().to_path_buf());
        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
        let session = session_over(config, cx);
        let before = config_now(cx);

        session
            .update(cx, |session, cx| {
                session.write_all(
                    [
                        SettingsChange::TranslationModelDirectory {
                            directory: second.path().to_path_buf(),
                        },
                        SettingsChange::TranslationModelSelected { model: None },
                    ],
                    cx,
                )
            })
            .expect("an in-memory manager stores every settings change");

        let after = config_now(cx);
        assert_eq!(
            after.translation.model_directory,
            Some(second.path().to_path_buf())
        );
        assert_eq!(after.translation.selected_model, None);
        assert_eq!(
            after.translation.download_site,
            DownloadSite::HuggingFace,
            "the rest of the translation settings should be left alone"
        );
        assert!(
            !Arc::ptr_eq(&before, &after),
            "the changes should be published as a new snapshot holding both of them"
        );
    }

    /// A write that cannot be stored leaves the settings alone and keeps its reason until the
    /// page takes it, once.
    #[gpui_kit::test]
    fn a_write_that_cannot_be_stored_should_be_kept_for_the_page_once(cx: &mut TestAppContext) {
        let directory = test_temp("unwritable");
        let blocking = directory.path().join("not-a-directory");
        fs::write(&blocking, b"block directory creation").expect("the blocking file is written");
        let manager = cx
            .update(|cx| ConfigManager::from_path(blocking.join("config.json"), cx))
            .expect("a config manager loads around a missing file");
        let session = cx.update(|cx| {
            cx.set_global(manager);
            cx.new(|_| SettingsSession::new())
        });
        let before = config_now(cx);

        let error = session
            .update(cx, |session, cx| {
                session.write(
                    SettingsChange::Field {
                        access: access("appearance", "appearance"),
                        value: FieldValue::new_static("Dark"),
                    },
                    cx,
                )
            })
            .expect_err("a settings file behind a file cannot be stored");

        assert!(
            Arc::ptr_eq(&before, &config_now(cx)),
            "a write that could not be stored must leave the snapshot alone"
        );
        let message = error.to_string();
        assert!(
            message.contains("failed to create config directory"),
            "the displayed reason should be the store's own: {message}"
        );
        assert_eq!(
            error
                .source()
                .expect("the write error should expose the store's failure")
                .to_string(),
            message,
            "the reason the page shows is the reason the store gave"
        );

        let taken = session.update(cx, |session, _| session.take_failure());
        assert!(
            taken.is_some(),
            "the page should be handed the reason of the failed write"
        );
        assert!(
            session
                .update(cx, |session, _| session.take_failure())
                .is_none(),
            "one failure should be reported once"
        );
    }

    /// A change that stores what is stored already is not a write and has no reason to report.
    #[gpui_kit::test]
    fn an_unchanged_change_should_be_accepted_without_a_report(cx: &mut TestAppContext) {
        let session = session_over(Config::default(), cx);
        let before = config_now(cx);
        let language = access("appearance", "language");
        let current = language.current(before.as_ref());

        write(
            &session,
            SettingsChange::Field {
                access: language,
                value: current,
            },
            cx,
        )
        .expect("a change that stores nothing is accepted");

        session
            .update(cx, |session, cx| session.write_all([], cx))
            .expect("a write of nothing is accepted");

        assert!(
            Arc::ptr_eq(&before, &config_now(cx)),
            "a change that stores what is stored already is not a write"
        );
        assert!(
            session
                .update(cx, |session, _| session.take_failure())
                .is_none(),
            "a change that stores nothing is nothing to report"
        );
    }

    /// A setter that refuses its value leaves the settings as they are; the user's choice is
    /// kept as the old value and there is nothing to report.
    #[gpui_kit::test]
    fn a_change_a_setter_refuses_should_be_accepted_without_a_report(cx: &mut TestAppContext) {
        let mut config = Config::default();
        // A venue this build does not hold a row for is the one shape whose setter writes
        // nothing: the settings have no place for the value the row stands for.
        config.venues.remove(&VenueId::Okx);
        let session = session_over(config, cx);
        let before = config_now(cx);

        write(
            &session,
            SettingsChange::Field {
                access: access("market_participants.okx", "market_participants.okx.api_key"),
                value: FieldValue::new_static("typed-key"),
            },
            cx,
        )
        .expect("a value the settings cannot hold is not a failure");

        assert!(
            Arc::ptr_eq(&before, &config_now(cx)),
            "a refused value leaves the settings exactly as they were"
        );
        assert!(
            session
                .update(cx, |session, _| session.take_failure())
                .is_none(),
            "a refused value is the user's choice, not a failure to report"
        );
    }
}
