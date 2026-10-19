//! The signed-in account as this feature stores it and as the menu shows it.
//!
//! The account is **not** owned here: it lives in the application settings, and this
//! module is the only place that writes it. Everything the profile menu reads back
//! out — the shortened address, the wallet family a signed-in user belongs to — is
//! derived from that one record, so signing in and signing out cannot leave the menu
//! and the rest of the application disagreeing about who is signed in.

use app_config::settings::{AccountConfig, AccountWalletKind, Config};
use app_config_provider::manager::ConfigManager;
use gpui_kit::{App, BorrowAppContext, SharedString};
use kytrix_service_provider::auth::model::{
    AccountIdentity, AccountWalletKind as ServiceWalletKind,
};
use smol_str::{SmolStr, format_smolstr};

/// Leading characters an Ethereum address keeps when it is shortened.
const ETHEREUM_ADDRESS_PREFIX: usize = 10;

/// Leading characters a Solana address keeps when it is shortened.
///
/// A Solana address has no `0x`, so the prefix counts address characters only.
const SOLANA_ADDRESS_PREFIX: usize = 8;

/// Trailing characters every shortened address keeps.
const ADDRESS_SUFFIX: usize = 8;

/// A signed-in account, ready to be drawn.
///
/// The shortened address is derived once, when the account is read, rather than on
/// every frame the menu is open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedInAccount {
    wallet_address: SharedString,
    shortened_address: SharedString,
    wallet_kind: AccountWalletKind,
}

impl SignedInAccount {
    pub fn new(wallet_address: impl Into<SharedString>, wallet_kind: AccountWalletKind) -> Self {
        let wallet_address = wallet_address.into();
        let shortened_address = SharedString::new(shortened_wallet_address(
            wallet_address.as_str(),
            wallet_kind,
        ));
        Self {
            wallet_address,
            shortened_address,
            wallet_kind,
        }
    }

    /// The complete address, which is what "copy" puts on the clipboard.
    pub fn wallet_address(&self) -> SharedString {
        self.wallet_address.clone()
    }

    /// The address as the menu draws it, shortened to fit one line.
    pub fn shortened_address(&self) -> SharedString {
        self.shortened_address.clone()
    }

    pub const fn wallet_kind(&self) -> AccountWalletKind {
        self.wallet_kind
    }
}

impl From<&AccountConfig> for SignedInAccount {
    fn from(account: &AccountConfig) -> Self {
        Self::new(account.wallet_address.as_str(), account.wallet_kind)
    }
}

/// Shortens a wallet address to `prefix...suffix`.
///
/// An address too short to shorten is returned whole: an address that reads as
/// `0x0123...4567` while really being eight characters long would misstate which
/// account is signed in, which is worse than an unshortened label.
pub fn shortened_wallet_address(wallet_address: &str, wallet_kind: AccountWalletKind) -> SmolStr {
    let prefix_length = match wallet_kind {
        AccountWalletKind::Ethereum => ETHEREUM_ADDRESS_PREFIX,
        AccountWalletKind::Solana => SOLANA_ADDRESS_PREFIX,
    };
    let Some(suffix_start) = wallet_address.len().checked_sub(ADDRESS_SUFFIX) else {
        return SmolStr::new(wallet_address);
    };
    if wallet_address.len() < prefix_length + ADDRESS_SUFFIX
        || !wallet_address.is_char_boundary(prefix_length)
        || !wallet_address.is_char_boundary(suffix_start)
    {
        return SmolStr::new(wallet_address);
    }

    format_smolstr!(
        "{}...{}",
        &wallet_address[..prefix_length],
        &wallet_address[suffix_start..]
    )
}

/// Reads a completed authorization as the account record the settings keep.
pub(crate) fn account_from_identity(identity: &AccountIdentity) -> AccountConfig {
    AccountConfig {
        user_id: identity.user_id().to_owned(),
        wallet_address: identity.wallet_address().to_owned(),
        wallet_kind: configured_wallet_kind(identity.wallet_kind()),
    }
}

const fn configured_wallet_kind(wallet_kind: ServiceWalletKind) -> AccountWalletKind {
    match wallet_kind {
        ServiceWalletKind::Ethereum => AccountWalletKind::Ethereum,
        ServiceWalletKind::Solana => AccountWalletKind::Solana,
    }
}

/// Stores a completed authorization as the application's account.
///
/// The previous avatar is dropped in the same write: it belongs to the account that
/// just went away, and a menu that showed the new address beside the old face for
/// even one frame would be showing two accounts at once.
pub(crate) fn store_account(account: AccountConfig, cx: &mut App) {
    cx.update_global::<ConfigManager, _>(|manager, cx| {
        manager.update(
            |config| {
                config.account = Some(account);
                config.avatar = None;
            },
            cx,
        );
    });
}

/// Forgets the current account and its avatar.
pub(crate) fn clear_account(cx: &mut App) {
    cx.update_global::<ConfigManager, _>(|manager, cx| {
        manager.update(
            |config| {
                config.account = None;
                config.avatar = None;
            },
            cx,
        );
    });
}

/// Stores a generated avatar, unless the account it was generated for is gone.
///
/// Generation is a background task, so it can finish after the user signed out or
/// after a different wallet signed in. Writing it then would put one account's face
/// on another account, so a result that no longer belongs to the current account is
/// dropped instead.
pub(crate) fn store_avatar_if_current(wallet_address: &str, svg: String, cx: &mut App) -> bool {
    let belongs_to_current_account = cx
        .global::<ConfigManager>()
        .read()
        .account
        .as_ref()
        .is_some_and(|account| account.wallet_address == wallet_address);
    if !belongs_to_current_account {
        return false;
    }

    cx.update_global::<ConfigManager, _>(|manager, cx| {
        manager.update(|config| config.avatar = Some(svg), cx);
    });
    true
}

/// The generated avatar of the account `config` names, if it has one.
///
/// An avatar without an account is not readable: it would be a face for nobody.
pub(crate) fn avatar_of(config: &Config) -> Option<&str> {
    config.account.as_ref()?;
    config.avatar.as_deref()
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::TestAppContext;
    use kytrix_service_provider::auth::model::AccountWalletKind as ServiceWalletKind;

    use super::{
        SignedInAccount, avatar_of, clear_account, configured_wallet_kind,
        shortened_wallet_address, store_account, store_avatar_if_current,
    };

    const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";
    const SOLANA: &str = "7YttLkHDoNj9wyDur5VjZ9Pd7WZZvnGvWKbxzwfKQnDb";

    fn account(wallet_address: &str, wallet_kind: AccountWalletKind) -> AccountConfig {
        AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            wallet_address: wallet_address.to_owned(),
            wallet_kind,
        }
    }

    fn install(cx: &mut TestAppContext, config: Config) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
        });
    }

    /// A settings snapshot with the two fields this feature owns.
    fn configured(account: Option<AccountConfig>, avatar: Option<String>) -> Config {
        let mut config = Config::default();
        config.account = account;
        config.avatar = avatar;
        config
    }

    #[test]
    fn every_wallet_family_should_have_a_configured_counterpart() {
        assert_eq!(
            (
                configured_wallet_kind(ServiceWalletKind::Ethereum),
                configured_wallet_kind(ServiceWalletKind::Solana),
            ),
            (AccountWalletKind::Ethereum, AccountWalletKind::Solana)
        );
    }

    #[test]
    fn ethereum_addresses_should_keep_the_0x_prefix_and_eight_trailing_characters() {
        assert_eq!(
            shortened_wallet_address(ETHEREUM, AccountWalletKind::Ethereum),
            "0x01234567...01234567"
        );
    }

    #[test]
    fn solana_addresses_should_keep_eight_characters_on_each_side() {
        assert_eq!(
            shortened_wallet_address(SOLANA, AccountWalletKind::Solana),
            "7YttLkHD...zwfKQnDb"
        );
    }

    #[test]
    fn an_address_too_short_to_shorten_should_be_shown_whole() {
        for (address, kind) in [
            ("0x0123", AccountWalletKind::Ethereum),
            ("short", AccountWalletKind::Solana),
            ("", AccountWalletKind::Ethereum),
        ] {
            assert_eq!(shortened_wallet_address(address, kind), address);
        }
    }

    #[test]
    fn shortening_should_not_split_a_character() {
        let address = "钱包地址钱包地址钱包地址钱包地址钱包地址钱包地址钱包地址钱包地址";

        assert_eq!(
            shortened_wallet_address(address, AccountWalletKind::Ethereum),
            address
        );
    }

    #[test]
    fn signed_in_account_should_expose_the_complete_and_shortened_addresses() {
        let account = SignedInAccount::new(ETHEREUM, AccountWalletKind::Ethereum);

        assert_eq!(account.wallet_address(), ETHEREUM);
        assert_eq!(account.shortened_address(), "0x01234567...01234567");
    }

    #[test]
    fn a_signed_in_account_should_derive_from_the_stored_account() {
        let account = SignedInAccount::from(&account(SOLANA, AccountWalletKind::Solana));

        assert_eq!(account.wallet_address(), SOLANA);
        assert_eq!(account.wallet_kind(), AccountWalletKind::Solana);
    }

    #[gpui_kit::test]
    fn storing_an_account_should_replace_the_identity_and_drop_the_old_avatar(
        cx: &mut TestAppContext,
    ) {
        install(
            cx,
            configured(
                Some(account(ETHEREUM, AccountWalletKind::Ethereum)),
                Some("<svg>old</svg>".to_owned()),
            ),
        );

        cx.update(|cx| store_account(account(SOLANA, AccountWalletKind::Solana), cx));

        let config = cx.update(|cx| cx.global::<ConfigManager>().read());
        let stored = config.account.as_ref().expect("the new account is stored");
        assert_eq!(stored.wallet_address, SOLANA);
        assert_eq!(stored.wallet_kind, AccountWalletKind::Solana);
        assert_eq!(config.avatar, None);
    }

    #[gpui_kit::test]
    fn clearing_an_account_should_remove_the_identity_and_the_avatar(cx: &mut TestAppContext) {
        install(
            cx,
            configured(
                Some(account(ETHEREUM, AccountWalletKind::Ethereum)),
                Some("<svg>old</svg>".to_owned()),
            ),
        );

        cx.update(clear_account);

        let config = cx.update(|cx| cx.global::<ConfigManager>().read());
        assert!(config.account.is_none());
        assert!(config.avatar.is_none());
    }

    #[gpui_kit::test]
    fn a_generated_avatar_should_only_be_stored_for_the_current_account(cx: &mut TestAppContext) {
        install(cx, Config::default());

        assert!(!cx.update(|cx| store_avatar_if_current(
            "wallet-a",
            "<svg>a</svg>".to_owned(),
            cx
        )));

        cx.update(|cx| store_account(account(ETHEREUM, AccountWalletKind::Ethereum), cx));
        assert!(!cx.update(|cx| store_avatar_if_current(
            "wallet-b",
            "<svg>b</svg>".to_owned(),
            cx
        )));
        assert!(cx.update(|cx| store_avatar_if_current(ETHEREUM, "<svg>a</svg>".to_owned(), cx)));

        let config = cx.update(|cx| cx.global::<ConfigManager>().read());
        assert_eq!(config.avatar.as_deref(), Some("<svg>a</svg>"));
        assert_eq!(avatar_of(&config), Some("<svg>a</svg>"));
    }

    #[gpui_kit::test]
    fn an_avatar_without_an_account_should_not_be_readable(cx: &mut TestAppContext) {
        install(cx, configured(None, Some("<svg>orphan</svg>".to_owned())));

        let config = cx.update(|cx| cx.global::<ConfigManager>().read());

        assert_eq!(avatar_of(&config), None);
    }
}
