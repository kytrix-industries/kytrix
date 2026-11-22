//! The signed-in account as this feature stores it and as the menu shows it.
//!
//! The account is **not** owned here: it lives in the application settings, reached through
//! [`crate::host::AccountSettings`], and this module is where every value the menu draws is
//! derived from that one record — the shortened address, the wallet family a signed-in user
//! belongs to, the label an alert names the family with. Signing in and signing out write that
//! record, so the menu and the rest of the application cannot disagree about who is signed in.
//!
//! Everything in here is pure: a value in, a value out. The four write rules of the settings
//! seam are stated on [`crate::host::AccountSettings`]; this module states the read-side rule
//! ([`avatar_of`]) on the snapshot type it reads.

use app_config::settings::{AccountConfig, AccountWalletKind};
use kytrix_service_provider::auth::model::{
    AccountIdentity, AccountWalletKind as ServiceWalletKind,
};
use smol_str::{SmolStr, format_smolstr};

use crate::host::AccountSnapshot;

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
/// The shortened address is derived once, when the account is read, rather than on every frame
/// the menu is open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedInAccount {
    wallet_address: SmolStr,
    shortened_address: SmolStr,
    wallet_kind: AccountWalletKind,
}

impl SignedInAccount {
    /// Derives the drawn form of an address and its family.
    #[must_use]
    pub fn new(wallet_address: impl Into<SmolStr>, wallet_kind: AccountWalletKind) -> Self {
        let wallet_address = wallet_address.into();
        let shortened_address = shortened_wallet_address(wallet_address.as_str(), wallet_kind);
        Self {
            wallet_address,
            shortened_address,
            wallet_kind,
        }
    }

    /// The complete address, which is what "copy" puts on the clipboard.
    #[must_use]
    pub fn wallet_address(&self) -> &SmolStr {
        &self.wallet_address
    }

    /// The address as the menu draws it, shortened to fit one line.
    #[must_use]
    pub fn shortened_address(&self) -> &SmolStr {
        &self.shortened_address
    }

    /// The wallet family the account belongs to, which the menu draws as a mark.
    #[must_use]
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
/// `0x0123...4567` while really being eight characters long would misstate which account is
/// signed in, which is worse than an unshortened label. A cut that would land inside a
/// multi-byte character is refused for the same reason — the address is shown whole rather than
/// mangled.
#[must_use]
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
#[must_use]
pub(crate) fn account_from_identity(identity: &AccountIdentity) -> AccountConfig {
    AccountConfig {
        user_id: identity.user_id().to_owned(),
        wallet_address: identity.wallet_address().to_owned(),
        wallet_kind: configured_wallet_kind(identity.wallet_kind()),
    }
}

/// The configured spelling of a wallet family.
#[must_use]
pub(crate) const fn configured_wallet_kind(wallet_kind: ServiceWalletKind) -> AccountWalletKind {
    match wallet_kind {
        ServiceWalletKind::Ethereum => AccountWalletKind::Ethereum,
        ServiceWalletKind::Solana => AccountWalletKind::Solana,
    }
}

/// The wallet family as the authorization report writes it.
///
/// The report is one line of copy naming the account, and the family it belongs to is named the
/// way the address is: Ethereum or Solana, as the product spells them. It is not a catalogue key
/// because it is not a sentence — it is a proper name that travels inside one.
#[must_use]
pub(crate) const fn wallet_kind_label(wallet_kind: AccountWalletKind) -> &'static str {
    match wallet_kind {
        AccountWalletKind::Ethereum => "Ethereum",
        AccountWalletKind::Solana => "Solana",
    }
}

/// The generated avatar of the account `snapshot` names, if it has one.
///
/// An avatar without an account is not readable: it would be a face for nobody. A store that
/// kept one (a crash between the two writes, a hand-edited file) therefore cannot put it on
/// screen, and the menu draws the default face instead.
#[must_use]
pub(crate) fn avatar_of(snapshot: &AccountSnapshot) -> Option<&SmolStr> {
    snapshot.account.as_ref()?;
    snapshot.avatar.as_ref()
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind};
    use kytrix_service_provider::auth::model::AccountWalletKind as ServiceWalletKind;

    use super::{
        SignedInAccount, avatar_of, configured_wallet_kind, shortened_wallet_address,
        wallet_kind_label,
    };
    use crate::host::AccountSnapshot;

    const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";
    const SOLANA: &str = "7YttLkHDoNj9wyDur5VjZ9Pd7WZZvnGvWKbxzwfKQnDb";

    fn account(wallet_address: &str, wallet_kind: AccountWalletKind) -> AccountConfig {
        AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            wallet_address: wallet_address.to_owned(),
            wallet_kind,
        }
    }

    /// A snapshot holding `account`, with no avatar generated yet.
    fn configured(account: Option<AccountConfig>, avatar: Option<&str>) -> AccountSnapshot {
        AccountSnapshot {
            account,
            avatar: avatar.map(smol_str::SmolStr::new),
        }
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
        assert_eq!(wallet_kind_label(AccountWalletKind::Ethereum), "Ethereum");
        assert_eq!(wallet_kind_label(AccountWalletKind::Solana), "Solana");
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

    #[test]
    fn an_avatar_without_an_account_should_not_be_readable() {
        assert_eq!(
            avatar_of(&configured(None, Some("<svg>orphan</svg>"))),
            None,
            "a face for nobody is not a face the menu can draw"
        );
        assert_eq!(
            avatar_of(&configured(
                Some(account(ETHEREUM, AccountWalletKind::Ethereum)),
                Some("<svg>face</svg>"),
            )),
            Some(&smol_str::SmolStr::new_static("<svg>face</svg>")),
            "the avatar of the signed-in account is readable"
        );
    }
}
