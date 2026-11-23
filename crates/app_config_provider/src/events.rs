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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigEvent {
    Appearance(Appearance),
    Language(Language),
    DataTimeZone(DataTimeZone),
    Account,
    ProxyCatalog(SmolStr),
    DefaultRoute,
    ServiceRoute(ServiceTarget),
    Venue(VenueId),
}

#[derive(Default)]
pub struct ConfigEvents {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<ConfigEvent>>>,
}

impl ConfigEvents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> ConfigSubscription {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.subscribers.lock().push(sender);

        ConfigSubscription { receiver }
    }

    pub(crate) fn publish(&self, changes: ConfigChangeSet) {
        let mut subscribers = self.subscribers.lock();
        for event in changed_items(changes) {
            subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
    }
}

pub struct ConfigSubscription {
    receiver: mpsc::UnboundedReceiver<ConfigEvent>,
}

impl ConfigSubscription {
    pub async fn recv(&mut self) -> Option<ConfigEvent> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Option<ConfigEvent> {
        self.receiver.try_recv().ok()
    }
}

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
