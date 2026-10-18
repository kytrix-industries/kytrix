//! The connection configuration one load runs on, and the way it is cancelled.
//!
//! A load is a conversation with one venue: the venue is asked what it publishes
//! and what that costs, and the answer decides what a commit publishes. Every
//! request of that conversation is dialed through **the venue's own connection
//! configuration**, never through a route the registry already holds: while
//! a load is staging, the venue is not loaded yet, which is exactly what the
//! transaction is about to change.
//!
//! The value carries the one thing a venue conversation needs beyond the venue
//! identity: the handle the transaction built from the venue's own configured
//! origin. The configuration's proxies, credential and origin pairs feed the
//! handle's internals, not this boundary.
//!
//! Cancellation travels beside the configuration: a load is cancelled by the
//! action that started it — the market menu's own row — and a loader observes it
//! between requests, never inside one.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::future::BoxFuture;
use market_participants_model::identifier::VenueId;
use tokio::sync::Notify;

use crate::{error::MarketParticipantError, venue::metadata::VenueReading};

/// The connection configuration of one venue, as one load sees it.
pub struct VenueConfig {
    venue: VenueId,
    conversation: Arc<dyn VenueConversation>,
}

impl VenueConfig {
    /// Builds the configuration of one load.
    ///
    /// The transaction is the one constructor: the venue identity and the
    /// conversation both come from it, and a loader can reach nothing else.
    pub(crate) fn new(venue: VenueId, conversation: Arc<dyn VenueConversation>) -> Self {
        Self {
            venue,
            conversation,
        }
    }

    /// Returns the venue this configuration belongs to.
    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// Reads what the venue publishes, in one reading of its own tables.
    ///
    /// This is the one conversation a venue that publishes scopes of its own has to
    /// have before it knows what it publishes; a venue that publishes only
    /// statically known scopes never asks, and the answer is never derived from
    /// anything but the venue's own reply. The names it deploys and the metadata of
    /// every scope it serves come back together, because the position of a deployed
    /// scope in the venue's own deployment table is what its listings' identities
    /// are built from.
    pub(crate) fn read_venue(
        &self,
    ) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        self.conversation.read_venue()
    }
}

/// The one conversation a loader has with its own venue.
///
/// The transaction builds the implementation, so a loader dials through the
/// venue's configured origin and crosses the same protocol validation as every
/// other market-participants call. A loader sees the verb, never the gateway behind it.
pub(crate) trait VenueConversation: Send + Sync {
    /// Reads what the venue publishes, in one reading of its own tables.
    fn read_venue(&self) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>>;
}

/// The cancellation of one load.
///
/// A load is cancelled by whoever started it, and a load that is cancelled
/// mid-stage commits nothing: the stage observes the flag between its steps and
/// answers [`crate::connector::VenueLoadError::Interrupted`]. The value is shared
/// with the loader, so a conversation that is already under way is not abandoned
/// — it finishes its own request and the load stops before the next one.
#[derive(Clone, Debug, Default)]
pub struct LoadCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl LoadCancellation {
    /// Creates a cancellation nothing has cancelled yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the load this value belongs to.
    ///
    /// Cancelling twice is not an error: a row that was left while its load was
    /// already stopping is the same state as one that was left once.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Returns whether the load this value belongs to was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Waits until the load this value belongs to is cancelled.
    ///
    /// A caller that cancelled before this wait started is answered at once, so
    /// the wait never misses a cancellation that already happened.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Registering the waiter before the flag is read again is what closes
            // the window between that read and the wait: a cancellation that
            // happens in between wakes this waiter instead of being missed.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}
