//! What one settings session holds: the changes the user has made but not confirmed.
//!
//! Nothing here reaches the settings. A control records a [`SettingsEdit`] and the draft keeps
//! both the edit and the settings it produces, which is what the pages are drawn from.
//!
//! The edits are kept rather than only their result, because the settings are not the draft's
//! to own: background work writes them while the dialog is open — an avatar is generated, a
//! workspace layout is persisted — and confirming must not undo it. Replaying the edits onto
//! the settings of that moment keeps every field the user did not touch at its latest value.

use std::sync::Arc;

use app_config::{network::ProxyConfig, settings::Config};
use gpui_kit::Context;
use smol_str::SmolStr;

use crate::{access::FieldValue, catalog::FieldAccess};

/// One change the user made, replayed onto the settings at the moment they are confirmed.
pub(crate) enum SettingsEdit {
    /// One field of the catalog, written the way its own access writes it.
    Field {
        access: FieldAccess,
        value: FieldValue,
    },
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

impl SettingsEdit {
    /// Makes this change in `config`.
    pub(crate) fn apply(&self, config: &mut Config) {
        match self {
            Self::Field { access, value } => access.write(config, value),
            Self::AddProxy { name, proxy } => {
                config.proxies.insert(name.clone(), proxy.clone());
            }
            Self::ReplaceProxy {
                original,
                name,
                proxy,
            } => {
                // The form rejects a name that is taken, so a replacement that cannot be made
                // means the catalog moved under the draft; the edit is dropped rather than
                // written as an insert, which would leave the routes bound to the old name.
                if let Err(error) = config.replace_proxy(original, name.clone(), proxy.clone()) {
                    log::error!("failed to replace proxy {original}: {error}");
                    // Neither half of what the replacement is replayed on is another writer's:
                    // the catalog a replay lands on is the one the form was started from plus
                    // the changes of this session, and the form is the only writer of either.
                    // So the entry is still there to be renamed, and its new name is still
                    // free, which is what the form checked before recording this edit.
                    debug_assert!(
                        config.proxies.contains_key(original.as_str())
                            && !config.proxies.contains_key(name.as_str()),
                        "a replay found {original} gone or {name} taken, which no writer of the settings does"
                    );
                }
            }
            Self::RemoveProxy { name } => {
                config.remove_proxy(name);
            }
        }
    }
}

/// The settings one session has changed, and the settings those changes produce.
pub(crate) struct SettingsDraft {
    /// What the settings look like with every change of this session made, which is what the
    /// pages are drawn from.
    config: Arc<Config>,
    /// What the user changed, in the order they changed it.
    edits: Vec<SettingsEdit>,
}

impl SettingsDraft {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            // A session that changes anything at all changes a few things; the capacity keeps
            // the edits of one without reallocating them.
            edits: Vec::with_capacity(8),
        }
    }

    /// The settings as this session has left them.
    pub(crate) fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    /// Records one change, and asks what draws this draft to draw it again.
    pub(crate) fn write(&mut self, edit: SettingsEdit, cx: &mut Context<Self>) {
        edit.apply(Arc::make_mut(&mut self.config));
        self.edits.push(edit);
        cx.notify();
    }

    /// What `current` becomes once every change of this session is made in it.
    pub(crate) fn applied_to(&self, current: &Config) -> Config {
        let mut config = current.clone();
        for edit in &self.edits {
            edit.apply(&mut config);
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use app_config::{
        network::{
            KytrixServiceCapability, ProxyAuthorizationConfig, ProxyConfig, ProxyRoute,
            ServiceTarget,
        },
        settings::{AccountConfig, AccountWalletKind, Appearance, Config, Language},
    };
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;

    use super::{SettingsDraft, SettingsEdit};
    use crate::access::{FieldValue, PROXY_INHERIT};

    /// One venue this build speaks, as the catalog of the fixture build names it.
    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue is canonical")
    }

    /// The access of a field of the catalog, so an edit is written the way a control writes it.
    fn field_access(page: &str, field: &str) -> crate::catalog::FieldAccess {
        crate::catalog::catalog(&crate::schema::tests::venues())
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

    fn x_media() -> ServiceTarget {
        ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XAccountMonitoringMedia,
        }
    }

    fn local_proxy(port: u16) -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port,
            authorization: ProxyAuthorizationConfig::None,
            resolve_dns_at_remote: true,
        }
    }

    /// The settings background work wrote while the dialog was open: what a replay must leave
    /// exactly as it found it.
    fn written_while_open() -> Config {
        let mut config = Config::default();
        config.language = Language::En;
        config.avatar = Some("<svg>generated</svg>".to_owned());
        config.account = Some(AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".into(),
            wallet_address: "0x0123456789abcdef".into(),
            wallet_kind: AccountWalletKind::Ethereum,
        });

        config
    }

    /// A draft holds the settings it was built from, and nothing else: settings that were
    /// never changed are the settings the draft reports.
    #[test]
    fn a_new_draft_should_be_the_settings_it_was_built_from() {
        let config = Config::default();
        let draft = SettingsDraft::new(config.clone());

        assert_eq!(draft.config().as_ref(), &config);

        let current = written_while_open();

        assert_eq!(draft.applied_to(&current), current);
    }

    /// An edit changes its own field and nothing else, which is what keeps background work
    /// from being rolled back by a confirmation.
    #[test]
    fn an_edit_should_change_only_its_own_field() {
        let access = field_access("appearance", "language");
        let value = FieldValue::new_static("zh-CN");
        let edit = SettingsEdit::Field { access, value };

        let current = written_while_open();
        let mut applied = current.clone();
        edit.apply(&mut applied);

        assert_eq!(current.language, Language::En);
        assert_eq!(applied.language, Language::ZhCn);
        assert_eq!(applied.avatar, current.avatar);
        assert_eq!(applied.account, current.account);
    }

    /// The same draft lands the same way on whatever the settings are at the moment it is
    /// confirmed, which is what makes it a log of the user's intent rather than a snapshot.
    #[test]
    fn the_same_edits_should_be_replayable_on_another_settings_value() {
        let mut draft = SettingsDraft::new(Config::default());
        draft.edits.push(SettingsEdit::Field {
            access: field_access("appearance", "language"),
            value: FieldValue::new_static("zh-CN"),
        });

        let mut first = Config::default();
        first.appearance = Appearance::Dark;
        let second = written_while_open();

        let applied_first = draft.applied_to(&first);
        let applied_second = draft.applied_to(&second);

        assert_eq!(first.language, Language::ZhCn);
        assert_eq!(applied_first.language, Language::ZhCn);
        assert_eq!(applied_first.appearance, first.appearance);
        assert_eq!(applied_second.language, Language::ZhCn);
        assert_eq!(applied_second.avatar, second.avatar);
        assert_eq!(applied_second.account, second.account);
    }

    /// A route bound to a name is what makes a rename more than an insert and a delete, and a
    /// replay has to move both.
    #[test]
    fn a_replayed_rename_should_move_the_routes_bound_to_the_name() {
        let mut config = Config::default();
        config
            .proxies
            .insert(SmolStr::new_static("hk"), local_proxy(1080));
        config
            .proxy_applications
            .insert(x_media(), ProxyRoute::named("hk"));
        let mut draft = SettingsDraft::new(config.clone());
        draft.edits.push(SettingsEdit::ReplaceProxy {
            original: SmolStr::new_static("hk"),
            name: SmolStr::new_static("tokyo"),
            proxy: local_proxy(2080),
        });

        let applied = draft.applied_to(&config);

        assert!(!applied.proxies.contains_key("hk"));
        assert_eq!(applied.proxies["tokyo"].port, 2080);
        assert_eq!(
            applied.service_proxy_route_for(&x_media()),
            ProxyRoute::named("tokyo")
        );
    }

    /// An added entry is in the replay, and a removed one is out of it together with the
    /// routes that named it — a venue's own line included, which is read back off the row
    /// that writes it.
    #[test]
    fn a_replayed_add_and_remove_should_reach_the_catalog_and_its_routes() {
        let mut config = Config::default();
        config
            .proxy_applications
            .insert(x_media(), ProxyRoute::named("hk"));
        let venue_row = field_access(
            "proxy-applications",
            &format!("proxy-applications.{}", venue("binance").as_str()),
        );
        venue_row.write(&mut config, &FieldValue::new_static("hk"));
        let mut draft = SettingsDraft::new(config.clone());
        draft.edits.push(SettingsEdit::AddProxy {
            name: SmolStr::new_static("hk"),
            proxy: local_proxy(1080),
        });
        draft.edits.push(SettingsEdit::AddProxy {
            name: SmolStr::new_static("osaka"),
            proxy: local_proxy(1081),
        });
        draft.edits.push(SettingsEdit::RemoveProxy {
            name: SmolStr::new_static("hk"),
        });

        let applied = draft.applied_to(&config);

        assert!(!applied.proxies.contains_key("hk"));
        assert!(applied.proxies.contains_key("osaka"));
        assert_eq!(
            applied.service_proxy_route_for(&x_media()),
            ProxyRoute::Direct
        );
        assert_eq!(
            venue_row.current(&applied),
            FieldValue::new_static(PROXY_INHERIT),
            "the venue whose line was removed follows the default line again"
        );
    }
}
