//! The configuration change events and the channels that publish them.
//!
//! The manager owns a [`ConfigEvents`] value next to its snapshot; a consumer
//! takes a [`ConfigSubscription`] and drains it. Nothing here is a process
//! global and nothing here is tied to a UI thread: a value plus an unbounded
//! channel is the whole mechanism.

use app_config::{
    change::ConfigChangeSet,
    network::ServiceTarget,
    settings::{Appearance, Language},
    time_zone::DataTimeZone,
};
use market_participants_model::identifier::VenueId;
use parking_lot::Mutex;
use smol_str::SmolStr;
use tokio::sync::mpsc;

/// One configuration change.
///
/// The manager publishes one of these per changed item, in a fixed domain order:
/// appearance, language, display time zone, account, proxy catalog (map order),
/// default route, service routes (map order), venues (map order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigEvent {
    /// The application appearance changed.
    Appearance(Appearance),
    /// The interface language changed.
    Language(Language),
    /// The display time zone for data timestamps changed.
    DataTimeZone(DataTimeZone),
    /// The account or the avatar of the account changed.
    Account,
    /// One named proxy was added, removed or edited.
    ProxyCatalog(SmolStr),
    /// The default outbound route changed.
    DefaultRoute,
    /// One Kytrix service application route was added, removed or edited.
    ServiceRoute(ServiceTarget),
    /// One venue's settings were added, removed or edited.
    Venue(VenueId),
}

/// The live change subscriptions of one configuration manager.
///
/// The manager owns this value for its whole life and hands each caller an
/// independent [`ConfigSubscription`]. It carries no state beyond the sender
/// list, so it can be created, subscribed to and published from any thread.
#[derive(Default)]
pub struct ConfigEvents {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<ConfigEvent>>>,
}

impl ConfigEvents {
    /// Creates a publisher with no subscribers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one subscriber and returns its receiving end.
    pub fn subscribe(&self) -> ConfigSubscription {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.subscribers.lock().push(sender);

        ConfigSubscription { receiver }
    }

    /// Publishes `changes` as one event per changed item, in the domain order
    /// [`ConfigEvent`] documents.
    ///
    /// Only channel sends happen here: no subscriber code runs, so publishing
    /// cannot re-enter the manager. A subscriber whose receiving end was dropped
    /// is removed the first time a send to it fails.
    pub(crate) fn publish(&self, changes: ConfigChangeSet) {
        let mut subscribers = self.subscribers.lock();
        for event in changed_items(changes) {
            subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
    }
}

/// The receiving end of one [`ConfigEvents::subscribe`] call.
///
/// Dropping it is the whole unsubscribe: the publisher notices the failed send
/// on its next publish and forgets the sender.
pub struct ConfigSubscription {
    receiver: mpsc::UnboundedReceiver<ConfigEvent>,
}

impl ConfigSubscription {
    /// Waits for the next event, or returns `None` once the manager is dropped.
    pub async fn recv(&mut self) -> Option<ConfigEvent> {
        self.receiver.recv().await
    }

    /// Returns the next event without waiting, for synchronous consumers.
    pub fn try_recv(&mut self) -> Option<ConfigEvent> {
        self.receiver.try_recv().ok()
    }
}

/// Expands a change set into the events the manager publishes, in domain order.
fn changed_items(changes: ConfigChangeSet) -> impl Iterator<Item = ConfigEvent> {
    changes
        .appearance
        .map(ConfigEvent::Appearance)
        .into_iter()
        .chain(changes.language.map(ConfigEvent::Language))
        .chain(changes.data_time_zone.map(ConfigEvent::DataTimeZone))
        .chain(changes.account_changed.then_some(ConfigEvent::Account))
        .chain(changes.proxies.into_iter().map(ConfigEvent::ProxyCatalog))
        .chain(
            changes
                .proxy_default_changed
                .then_some(ConfigEvent::DefaultRoute),
        )
        .chain(
            changes
                .proxy_applications
                .into_iter()
                .map(ConfigEvent::ServiceRoute),
        )
        .chain(changes.venues.into_iter().map(ConfigEvent::Venue))
}

#[cfg(test)]
mod tests {
    use app_config::{change::ConfigChangeSet, settings::Language};

    use super::{ConfigEvent, ConfigEvents};

    fn language_change(language: Language) -> ConfigChangeSet {
        ConfigChangeSet {
            language: Some(language),
            ..ConfigChangeSet::default()
        }
    }

    #[tokio::test]
    async fn a_subscriber_should_receive_each_published_event() {
        let events = ConfigEvents::new();
        let mut subscription = events.subscribe();

        events.publish(language_change(Language::En));

        assert_eq!(
            subscription.recv().await,
            Some(ConfigEvent::Language(Language::En))
        );
        assert_eq!(
            subscription.try_recv(),
            None,
            "one publish delivers exactly one event"
        );
    }

    #[test]
    fn publishing_should_outlive_a_dropped_subscriber() {
        let events = ConfigEvents::new();
        let mut kept = events.subscribe();
        drop(events.subscribe());

        events.publish(language_change(Language::En));

        assert_eq!(
            kept.try_recv(),
            Some(ConfigEvent::Language(Language::En)),
            "the remaining subscriber still receives the change"
        );
    }

    #[test]
    fn every_subscriber_should_receive_the_same_events() {
        let events = ConfigEvents::new();
        let mut first = events.subscribe();
        let mut second = events.subscribe();

        events.publish(ConfigChangeSet {
            language: Some(Language::En),
            ..ConfigChangeSet::default()
        });

        for subscription in [&mut first, &mut second] {
            assert_eq!(
                subscription.try_recv(),
                Some(ConfigEvent::Language(Language::En))
            );
            assert_eq!(subscription.try_recv(), None);
        }
    }
}
