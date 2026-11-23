use kytrix_service_provider::auth::model::{
    AuthError, LocalSessionStatus, SessionAvailability, SessionChange,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSession {
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

    #[must_use]
    pub fn from_status(status: LocalSessionStatus) -> Self {
        Self {
            availability: status.availability(),
            observed_change: status.observed_change(),
        }
    }
}

pub(crate) struct SessionWatch {
    changes: watch::Receiver<SessionChange>,
}

impl SessionWatch {
    pub(crate) fn new(changes: watch::Receiver<SessionChange>) -> Self {
        Self { changes }
    }

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

    pub(crate) async fn changed(&mut self) -> Option<bool> {
        self.changes.changed().await.ok()?;
        let availability = self.changes.borrow_and_update().availability();

        Some(clears_account(availability))
    }
}

fn clears_account(availability: SessionAvailability) -> bool {
    availability == SessionAvailability::Unavailable
}

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
