//! The download manager, as the updater asks for a payload.
//!
//! The process-wide download manager is still a toolkit entity (P5 removes the
//! toolkit), so the updater cannot hold it: an engine application has no toolkit
//! `App` to update it with. Everything the updater needs from it is named here as a
//! seam the composition root implements:
//!
//! - the operations a transfer takes ([`add`](DownloadService::add) /
//!   [`pause`](DownloadService::pause) / [`resume`](DownloadService::resume) /
//!   [`delete`](DownloadService::delete)) and the one read
//!   ([`task`](DownloadService::task));
//! - one event subscription: the manager publishes what became of a task as
//!   [`DownloadEvent`]s, and the sink hands them to the updater's inbox.
//!
//! The methods are called on the interface thread only, which is why the trait is
//! `!Send`: the manager's own state lives behind the toolkit's main-thread entities,
//! and nothing here runs on a worker. The event sink is `Send + Sync` so an
//! implementation may forward a worker's progress report through it; the updater's
//! sink only pushes into a mutex-guarded inbox and wakes the engine, both of which are
//! thread safe.
//!
//! A test supplies its own implementation ([`FakeDownloads`](crate::test_support));
//! production supplies the adapter the composition root builds over the process
//! manager ([`crate::Updater`] is installed with it, P4-5). The payload data types are
//! the manager's own, so the two sides cannot drift.

use std::{fmt, sync::Arc};

use download_provider::{
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadTask},
};

/// What the updater asks of the application's download manager.
///
/// One implementation lives for the application's lifetime (the composition root
/// builds it once); the updater never creates a manager of its own.
pub trait DownloadService {
    /// Adds a transfer and returns the task that names it.
    fn add(&self, request: DownloadRequest) -> DownloadId;

    /// Asks a transfer to stop at its partial file; `false` when the manager does not
    /// hold an active task with this identity.
    fn pause(&self, id: DownloadId) -> bool;

    /// Asks a stopped transfer to continue from its partial file; `false` when the
    /// manager does not hold a stopped task with this identity.
    fn resume(&self, id: DownloadId) -> bool;

    /// Removes a task and its files; `false` when the manager no longer held it.
    fn delete(&self, id: DownloadId) -> bool;

    /// The manager's current view of one task, when it holds it.
    fn task(&self, id: DownloadId) -> Option<DownloadTask>;

    /// Registers the one sink every task event is delivered to.
    ///
    /// The returned handle keeps the subscription alive; dropping it stops the
    /// delivery. An implementation replaces the previous sink rather than adding a
    /// second one, because the updater holds exactly one subscription.
    fn subscribe(&self, sink: Arc<dyn Fn(DownloadEvent) + Send + Sync>) -> DownloadSubscription;
}

/// Keeps a [`DownloadService::subscribe`] registration alive.
///
/// Dropping it unsubscribes: the updater releases its subscription when it is itself
/// released, so a closed window stops receiving download events.
pub struct DownloadSubscription {
    unsubscribe: Option<Box<dyn FnOnce()>>,
}

impl DownloadSubscription {
    /// A handle that unsubscribes by running `unsubscribe` on drop.
    pub fn new(unsubscribe: impl FnOnce() + 'static) -> Self {
        Self {
            unsubscribe: Some(Box::new(unsubscribe)),
        }
    }

    /// A handle for a service that has no subscription to withdraw.
    #[must_use]
    pub fn none() -> Self {
        Self { unsubscribe: None }
    }
}

impl Drop for DownloadSubscription {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

impl fmt::Debug for DownloadSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DownloadSubscription")
            .field("active", &self.unsubscribe.is_some())
            .finish()
    }
}
