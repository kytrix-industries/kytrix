use std::rc::Rc;

use app_config::settings::AccountConfig;
use profile_ui::host::{AccountSettings, AccountSnapshot};
use smol_str::SmolStr;
use ui_core::app::App;

use crate::composition::ConfigHub;

pub(crate) struct StoredAccounts {
    hub: Rc<ConfigHub>,
}

impl StoredAccounts {
    pub(crate) fn new(hub: Rc<ConfigHub>) -> Self {
        Self { hub }
    }
}

impl AccountSettings for StoredAccounts {
    fn read(&self, app: &App) -> AccountSnapshot {
        app.read_with(&self.hub.config_signal(), |config| AccountSnapshot {
            account: config.account.clone(),
            avatar: config.avatar.as_deref().map(SmolStr::new),
        })
    }

    fn store_account(&self, app: &mut App, account: AccountConfig) {
        let _ = self.hub.edit(app, |config| {
            config.account = Some(account);
            config.avatar = None;
        });
    }

    fn clear_account(&self, app: &mut App) {
        let _ = self.hub.edit(app, |config| {
            config.account = None;
            config.avatar = None;
        });
    }

    fn store_avatar_if_current(&self, app: &mut App, wallet: &str, svg: String) -> bool {
        let current = self.hub.read(|config| {
            config
                .account
                .as_ref()
                .is_some_and(|account| account.wallet_address == wallet)
        });
        if !current {
            return false;
        }
        self.hub
            .edit(app, |config| config.avatar = Some(svg))
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountWalletKind, Config};
    use app_config_provider::store::ConfigStore;

    use super::*;
    use crate::composition::ConfigHub;

    fn settings() -> (ui_core::test_support::app::TestApp, Rc<ConfigHub>) {
        let mut app = ui_core::test_support::app::TestApp::new();
        let hub = ConfigHub::for_test(app.app_mut(), ConfigStore::in_memory(Config::default()));
        (app, hub)
    }

    fn account(wallet: &str) -> AccountConfig {
        AccountConfig {
            user_id: format!("user-{wallet}"),
            wallet_address: wallet.to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        }
    }

    #[test]
    fn storing_an_account_should_drop_the_previous_face() {
        let (mut app, hub) = settings();
        let accounts = StoredAccounts::new(Rc::clone(&hub));
        let _ = hub.edit(app.app_mut(), |config| {
            config.avatar = Some("previous face".to_owned());
        });

        accounts.store_account(app.app_mut(), account("0xabc"));
        let snapshot = accounts.read(app.app());
        assert_eq!(snapshot.account, Some(account("0xabc")));
        assert_eq!(snapshot.avatar, None, "the face went with the old account");

        accounts.store_account(app.app_mut(), account("0xdef"));
        assert_eq!(accounts.read(app.app()).account, Some(account("0xdef")));
        accounts.clear_account(app.app_mut());
        assert_eq!(accounts.read(app.app()), AccountSnapshot::default());
    }

    #[test]
    fn an_avatar_should_only_be_stored_for_the_current_wallet() {
        let (mut app, hub) = settings();
        let accounts = StoredAccounts::new(hub);

        assert!(
            !accounts.store_avatar_if_current(app.app_mut(), "0xabc", "face".to_owned()),
            "nobody is signed in"
        );

        accounts.store_account(app.app_mut(), account("0xabc"));
        assert!(
            accounts.store_avatar_if_current(app.app_mut(), "0xabc", "face".to_owned()),
            "the account's own face is stored"
        );
        assert_eq!(accounts.read(app.app()).avatar.as_deref(), Some("face"));
        assert!(
            !accounts.store_avatar_if_current(app.app_mut(), "0xdef", "other".to_owned()),
            "a face generated for another wallet is dropped"
        );
    }
}
