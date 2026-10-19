//! The account a widget's data is scoped to.

use app_config::settings::Config;
use smol_str::SmolStr;

/// Returns the account id the settings currently hold, as a widget scope.
pub fn account_of(config: &Config) -> Option<SmolStr> {
    config
        .account
        .as_ref()
        .map(|account| SmolStr::new(account.user_id.as_str()))
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind, Config};

    use super::account_of;

    /// Settings holding `user_id` as the signed-in account, or holding nobody when
    /// it is `None`.
    fn settings(user_id: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = user_id.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// The scope a widget reads is the user id of the account the settings hold.
    #[test]
    fn account_of_should_answer_the_configured_user_id() {
        assert_eq!(
            account_of(&settings(Some("11111111-1111-4111-8111-111111111111"))).as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    /// A signed-out installation scopes no widget to anyone.
    #[test]
    fn account_of_should_answer_nothing_without_an_account() {
        assert_eq!(account_of(&settings(None)), None);
    }
}
