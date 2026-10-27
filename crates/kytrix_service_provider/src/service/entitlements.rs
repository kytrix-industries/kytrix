//! The entitlement feature behind each Kytrix service capability.

use app_config::network::KytrixServiceCapability;

use super::engine::ServiceTopic;

/// The entitlement feature a subscription topic is gated by.
pub fn topic_feature_key(topic: &ServiceTopic) -> Option<&'static str> {
    match topic {
        ServiceTopic::News => Some("news"),
        ServiceTopic::Index(_) => Some("indices"),
        ServiceTopic::X => Some("x"),
        ServiceTopic::Wallet => Some("wallet"),
        ServiceTopic::Ais(_) => Some("ais"),
    }
}

/// The entitlement feature an outbound capability is gated by.
///
/// `Account`, `XMedia` and the two updater capabilities are not user
/// entitlements and therefore have no feature.
pub fn capability_feature_key(capability: KytrixServiceCapability) -> Option<&'static str> {
    match capability {
        KytrixServiceCapability::News => Some("news"),
        KytrixServiceCapability::Indices => Some("indices"),
        KytrixServiceCapability::X => Some("x"),
        KytrixServiceCapability::Wallet => Some("wallet"),
        KytrixServiceCapability::Ais => Some("ais"),
        KytrixServiceCapability::CryptocurrencyEtf => Some("cryptocurrency_etf"),
        KytrixServiceCapability::Cmc => Some("cmc_fear_greed"),
        KytrixServiceCapability::FinancialCalendar => Some("financial_calendar"),
        KytrixServiceCapability::PentagonPizzaIndex => Some("pentagon_pizza_index"),
        KytrixServiceCapability::Account
        | KytrixServiceCapability::XMedia
        | KytrixServiceCapability::UpdaterMetadata
        | KytrixServiceCapability::UpdaterDownload => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{
        ais::AisArea,
        engine::{IndexTopic, ServiceTopic},
    };

    #[test]
    fn every_subscription_topic_should_map_to_its_feature() {
        let topics = [
            (ServiceTopic::News, "news"),
            (
                ServiceTopic::Index(IndexTopic::from_code("SPX").expect("valid index code")),
                "indices",
            ),
            (ServiceTopic::X, "x"),
            (ServiceTopic::Wallet, "wallet"),
            (ServiceTopic::Ais(AisArea::Hormuz), "ais"),
        ];

        for (topic, key) in topics {
            assert_eq!(topic_feature_key(&topic), Some(key));
        }
    }

    #[test]
    fn every_outbound_capability_should_map_to_its_feature_or_none() {
        let capabilities = [
            (KytrixServiceCapability::Account, None),
            (
                KytrixServiceCapability::CryptocurrencyEtf,
                Some("cryptocurrency_etf"),
            ),
            (KytrixServiceCapability::News, Some("news")),
            (KytrixServiceCapability::Indices, Some("indices")),
            (KytrixServiceCapability::X, Some("x")),
            (KytrixServiceCapability::Wallet, Some("wallet")),
            (KytrixServiceCapability::XMedia, None),
            (KytrixServiceCapability::Cmc, Some("cmc_fear_greed")),
            (
                KytrixServiceCapability::FinancialCalendar,
                Some("financial_calendar"),
            ),
            (
                KytrixServiceCapability::PentagonPizzaIndex,
                Some("pentagon_pizza_index"),
            ),
            (KytrixServiceCapability::UpdaterMetadata, None),
            (KytrixServiceCapability::UpdaterDownload, None),
            (KytrixServiceCapability::Ais, Some("ais")),
        ];

        for (capability, key) in capabilities {
            assert_eq!(capability_feature_key(capability), key);
        }
    }
}
