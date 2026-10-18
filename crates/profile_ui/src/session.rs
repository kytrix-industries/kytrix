//! Whether the sign-in behind the stored account is still good.
//!
//! The account in the settings says who signed in; it does not say whether the
//! credential that proves it is still valid. That answer lives in the account session,
//! and it can arrive two ways: as a **change** published while the application runs (a
//! rejected refresh, a sign-out from elsewhere), or as the **state already on disk**
//! when the application starts.
//!
//! Both are needed and they can disagree, because neither has seen the other's news. A
//! query that started before a change arrived would report a session the change has
//! already invalidated; a change published before the query ran is older than what the
//! query read. So the query result is only trusted when the change it observed is still
//! the current one — otherwise the newer published state wins.

use kytrix_service_provider::auth::model::{
    AuthError, LocalSessionStatus, SessionAvailability, SessionChange,
};
use tokio::sync::watch;

/// What one reading of the credential on disk found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalSession {
    availability: SessionAvailability,
    observed_change: SessionChange,
}

impl LocalSession {
    #[cfg(test)]
    pub(crate) const fn new(
        availability: SessionAvailability,
        observed_change: SessionChange,
    ) -> Self {
        Self {
            availability,
            observed_change,
        }
    }

    /// Reads the same information out of the account session's own report.
    pub(crate) fn from_status(status: LocalSessionStatus) -> Self {
        Self {
            availability: status.availability(),
            observed_change: status.observed_change(),
        }
    }
}

/// Watches the local credential for the moment it stops being valid.
pub(crate) struct SessionWatch {
    changes: watch::Receiver<SessionChange>,
}

impl SessionWatch {
    pub(crate) fn new(changes: watch::Receiver<SessionChange>) -> Self {
        Self { changes }
    }

    /// Reconciles the state read from disk with the changes seen so far.
    ///
    /// Returns whether the stored account has to be forgotten. A query that fails is
    /// not a sign-out: the application keeps believing what the session last
    /// published, which is what it would have believed had the query never run.
    pub(crate) fn reconcile(&mut self, queried: Result<LocalSession, AuthError>) -> bool {
        let current = *self.changes.borrow_and_update();
        let availability = match queried {
            Ok(session) => reconcile_availability(
                session.availability,
                current.availability(),
                session.observed_change == current,
            ),
            Err(error) => {
                log::warn!("failed to inspect the local OAuth session: {error}");
                current.availability()
            }
        };

        clears_account(availability)
    }

    /// Waits for the next published change.
    ///
    /// `None` means the account session is gone and nothing more will be published, so
    /// the caller stops watching rather than waiting forever.
    pub(crate) async fn changed(&mut self) -> Option<bool> {
        self.changes.changed().await.ok()?;
        let availability = self.changes.borrow_and_update().availability();

        Some(clears_account(availability))
    }
}

/// Whether an availability invalidates the stored account.
///
/// Only "unavailable" does. "Unknown" is what a session publishes before it has
/// decided anything — the first value every watcher sees — and treating it as a
/// sign-out would erase the account of every user on every start.
fn clears_account(availability: SessionAvailability) -> bool {
    availability == SessionAvailability::Unavailable
}

/// Which availability a query and a change together imply.
///
/// The query is only trusted when the change it observed is still the current one: a
/// change published after the query read the credential is newer than the query.
fn reconcile_availability(
    queried: SessionAvailability,
    current: SessionAvailability,
    query_observed_current_change: bool,
) -> SessionAvailability {
    if query_observed_current_change {
        queried
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use kytrix_service_provider::auth::model::{AuthError, SessionAvailability, SessionChange};
    use tokio::sync::watch;

    use super::{LocalSession, SessionWatch, clears_account, reconcile_availability};

    #[test]
    fn only_an_unavailable_session_should_clear_the_account() {
        assert!(clears_account(SessionAvailability::Unavailable));
        assert!(!clears_account(SessionAvailability::Available));
        assert!(!clears_account(SessionAvailability::Unknown));
    }

    #[test]
    fn a_newer_published_change_should_override_a_stale_query() {
        use SessionAvailability::{Available, Unavailable};

        assert_eq!(
            reconcile_availability(Unavailable, Available, false),
            Available
        );
        assert_eq!(
            reconcile_availability(Available, Unavailable, false),
            Unavailable
        );
    }

    #[test]
    fn a_query_that_observed_the_current_change_should_supply_the_verdict() {
        use SessionAvailability::{Available, Unavailable, Unknown};

        assert_eq!(
            reconcile_availability(Unavailable, Unknown, true),
            Unavailable
        );
        assert_eq!(reconcile_availability(Unknown, Available, true), Unknown);
    }

    #[test]
    fn a_credential_that_is_no_longer_valid_should_clear_the_account() {
        let (_changes, receiver) = watch::channel(SessionChange::default());
        let mut watch = SessionWatch::new(receiver);
        let session = LocalSession::new(SessionAvailability::Unavailable, SessionChange::default());

        assert!(watch.reconcile(Ok(session)));
    }

    #[test]
    fn a_credential_that_is_still_valid_should_keep_the_account() {
        let (_changes, receiver) = watch::channel(SessionChange::default());
        let mut watch = SessionWatch::new(receiver);
        let session = LocalSession::new(SessionAvailability::Available, SessionChange::default());

        assert!(!watch.reconcile(Ok(session)));
    }

    #[test]
    fn reconciling_a_failed_query_should_keep_the_published_state() {
        let (_changes, receiver) = watch::channel(SessionChange::default());
        let mut watch = SessionWatch::new(receiver);

        assert!(!watch.reconcile(Err(AuthError::MissingCredential)));
    }

    #[tokio::test]
    async fn a_published_change_should_be_reported_and_a_closed_session_should_end_the_watch() {
        let (changes, receiver) = watch::channel(SessionChange::default());
        let mut watch = SessionWatch::new(receiver);

        tokio::spawn(async move {
            let _ = changes.send(SessionChange::default());
            drop(changes);
        });

        assert_eq!(watch.changed().await, Some(false));
        assert_eq!(watch.changed().await, None);
    }
}
