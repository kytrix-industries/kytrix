use smol_str::SmolStr;

/// Account identity and credential generations bound to authenticated work.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AccountProfile {
    account_id: SmolStr,
    credential_revision: u64,
    credential_epoch: u64,
}

impl AccountProfile {
    pub(crate) fn new(
        account_id: impl Into<SmolStr>,
        credential_revision: u64,
        credential_epoch: u64,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            credential_revision,
            credential_epoch,
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn credential_revision(&self) -> u64 {
        self.credential_revision
    }

    pub(crate) const fn credential_epoch(&self) -> u64 {
        self.credential_epoch
    }
}
