use smol_str::SmolStr;

/// Authentication state for a physical connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Stateful {
        authenticated: bool,
        account_id: Option<SmolStr>,
        session_token: Option<SmolStr>,
    },
    Stateless,
}

impl ConnectionState {
    pub const fn stateful() -> Self {
        Self::Stateful {
            authenticated: false,
            account_id: None,
            session_token: None,
        }
    }

    pub const fn stateless() -> Self {
        Self::Stateless
    }

    pub const fn is_stateful(&self) -> bool {
        matches!(self, Self::Stateful { .. })
    }

    pub const fn is_authenticated(&self) -> bool {
        matches!(
            self,
            Self::Stateful {
                authenticated: true,
                ..
            }
        )
    }

    pub fn authenticate(
        &mut self,
        account_id: impl Into<SmolStr>,
        session_token: impl Into<SmolStr>,
    ) {
        if let Self::Stateful {
            authenticated,
            account_id: current_account,
            session_token: current_token,
        } = self
        {
            *authenticated = true;
            *current_account = Some(account_id.into());
            *current_token = Some(session_token.into());
        }
    }

    pub fn unauthenticate(&mut self) {
        if let Self::Stateful {
            authenticated,
            account_id,
            session_token,
        } = self
        {
            *authenticated = false;
            *account_id = None;
            *session_token = None;
        }
    }
}
