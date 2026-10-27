//! The entitlement feature behind each Kytrix service capability.

use app_config::network::KytrixServiceCapability;

use super::engine::ServiceTopic;

/// The entitlement feature a subscription topic is gated by.
pub fn topic_feature_key(topic: &ServiceTopic) -> Option<&'static str> {
    match topic {
        ServiceTopic::News => Some("news"),
        ServiceTopic::FinancialIndex(_) => Some("financial_indices"),
        ServiceTopic::XAccountMonitoring => Some("x_account_monitoring"),
        ServiceTopic::CryptocurrencyWalletMonitoring => Some("cryptocurrency_wallet_monitoring"),
        ServiceTopic::StrategicMaritimeChokepointsTracking(_) => {
            Some("strategic_maritime_chokepoints_tracking")
        }
    }
}

/// The entitlement feature an outbound capability is gated by.
///
/// `Account`, `XAccountMonitoringMedia` and the two updater capabilities are not user
/// entitlements and therefore have no feature.
pub fn capability_feature_key(capability: KytrixServiceCapability) -> Option<&'static str> {
    match capability {
        KytrixServiceCapability::News => Some("news"),
        KytrixServiceCapability::FinancialIndices => Some("financial_indices"),
        KytrixServiceCapability::XAccountMonitoring => Some("x_account_monitoring"),
        KytrixServiceCapability::CryptocurrencyWalletMonitoring => {
            Some("cryptocurrency_wallet_monitoring")
        }
        KytrixServiceCapability::StrategicMaritimeChokepointsTracking => {
            Some("strategic_maritime_chokepoints_tracking")
        }
        KytrixServiceCapability::CryptocurrencyEtf => Some("cryptocurrency_etf"),
        KytrixServiceCapability::Cmc => Some("cmc_fear_greed"),
        KytrixServiceCapability::FinancialCalendar => Some("financial_calendar"),
        KytrixServiceCapability::PentagonPizzaIndex => Some("pentagon_pizza_index"),
        KytrixServiceCapability::Account
        | KytrixServiceCapability::XAccountMonitoringMedia
        | KytrixServiceCapability::UpdaterMetadata
        | KytrixServiceCapability::UpdaterDownload => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{
        engine::{FinancialIndexTopic, ServiceTopic},
        strategic_maritime_chokepoints_tracking::StrategicMaritimeChokepointsTrackingArea,
    };

    #[test]
    fn every_subscription_topic_should_map_to_its_feature() {
        let topics = [
            (ServiceTopic::News, "news"),
            (
                ServiceTopic::FinancialIndex(
                    FinancialIndexTopic::from_code("SPX").expect("valid index code"),
                ),
                "financial_indices",
            ),
            (ServiceTopic::XAccountMonitoring, "x_account_monitoring"),
            (
                ServiceTopic::CryptocurrencyWalletMonitoring,
                "cryptocurrency_wallet_monitoring",
            ),
            (
                ServiceTopic::StrategicMaritimeChokepointsTracking(
                    StrategicMaritimeChokepointsTrackingArea::Hormuz,
                ),
                "strategic_maritime_chokepoints_tracking",
            ),
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
            (
                KytrixServiceCapability::FinancialIndices,
                Some("financial_indices"),
            ),
            (
                KytrixServiceCapability::XAccountMonitoring,
                Some("x_account_monitoring"),
            ),
            (
                KytrixServiceCapability::CryptocurrencyWalletMonitoring,
                Some("cryptocurrency_wallet_monitoring"),
            ),
            (KytrixServiceCapability::XAccountMonitoringMedia, None),
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
            (
                KytrixServiceCapability::StrategicMaritimeChokepointsTracking,
                Some("strategic_maritime_chokepoints_tracking"),
            ),
        ];

        for (capability, key) in capabilities {
            assert_eq!(capability_feature_key(capability), key);
        }
    }
}
