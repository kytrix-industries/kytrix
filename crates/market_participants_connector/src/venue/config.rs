use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::future::BoxFuture;
use market_participants_model::identifier::VenueId;
use tokio::sync::Notify;

use crate::{error::MarketParticipantError, venue::metadata::VenueReading};

pub struct VenueConfig {
    venue: VenueId,
    conversation: Arc<dyn VenueConversation>,
}

impl VenueConfig {
    pub(crate) fn new(venue: VenueId, conversation: Arc<dyn VenueConversation>) -> Self {
        Self {
            venue,
            conversation,
        }
    }

    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub(crate) fn read_venue(
        &self,
    ) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        self.conversation.read_venue()
    }
}

pub(crate) trait VenueConversation: Send + Sync {
    fn read_venue(&self) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>>;
}

#[derive(Clone, Debug, Default)]
pub struct LoadCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl LoadCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}
