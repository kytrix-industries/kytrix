use std::sync::Arc;

use app_config_provider::notification::ConfigNotification;
use market_participants_connector::{
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    market::timeframe::{Adjustment, StatisticsTimeFrame, TimeFrame},
    runtime::MarketParticipantNetworkSnapshot,
    test_support::test_handle,
};
use market_participants_model::{
    identifier::{IdentifierValue, VenueId, VenueScopeId},
    listing::route::VenueRoute,
    outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
    prediction::PredictionEventRef,
};
use network::transport::hub::{TransportHub, TransportPolicy};

#[test]
fn the_market_facade_exposes_the_candlestick_intervals() {
    let intervals = [
        (TimeFrame::Second1, "1s"),
        (TimeFrame::Minute1, "1m"),
        (TimeFrame::Minute2, "2m"),
        (TimeFrame::Minute10, "10m"),
        (TimeFrame::Minute15, "15m"),
        (TimeFrame::Minute20, "20m"),
        (TimeFrame::Minute45, "45m"),
        (TimeFrame::Hour3, "3h"),
        (TimeFrame::Hour4, "4h"),
        (TimeFrame::Day1, "1d"),
        (TimeFrame::Week1, "1w"),
        (TimeFrame::Month3, "3M"),
        (TimeFrame::Quarter1, "1Q"),
        (TimeFrame::Year1, "1Y"),
    ];

    for (interval, label) in intervals {
        assert_eq!(interval.label(), label);
        assert_eq!(
            serde_json::to_string(&interval).expect("the interval serializes"),
            format!("\"{label}\""),
            "the wall-clock label and the wire spelling are the same string"
        );
    }
}

#[test]
fn the_market_facade_exposes_the_statistics_intervals() {
    let intervals = [
        (StatisticsTimeFrame::Minute5, "5m"),
        (StatisticsTimeFrame::Minute15, "15m"),
        (StatisticsTimeFrame::Minute30, "30m"),
        (StatisticsTimeFrame::Hour1, "1h"),
        (StatisticsTimeFrame::Hour2, "2h"),
        (StatisticsTimeFrame::Hour4, "4h"),
        (StatisticsTimeFrame::Hour6, "6h"),
        (StatisticsTimeFrame::Hour12, "12h"),
        (StatisticsTimeFrame::Day1, "1d"),
    ];
    let mut labels: Vec<&str> = Vec::with_capacity(intervals.len());

    for (interval, label) in intervals {
        assert_eq!(interval.label(), label);
        assert_eq!(
            serde_json::to_string(&interval).expect("the interval serializes"),
            format!("\"{label}\"")
        );
        labels.push(label);
    }
    let distinct = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), distinct, "every interval has its own label");
}

#[test]
fn the_market_facade_exposes_the_adjustment_conventions() {
    let conventions = [(Adjustment::None, "none"), (Adjustment::Forward, "forward")];

    for (adjustment, label) in conventions {
        assert_eq!(adjustment.label(), label);
        assert_eq!(
            serde_json::to_string(&adjustment).expect("the adjustment serializes"),
            format!("\"{label}\""),
            "the label and the wire spelling are the same string"
        );
    }
    assert_eq!(
        Adjustment::default(),
        Adjustment::None,
        "the neutral convention is the default one"
    );
}

fn connector() -> MarketParticipantsConnector {
    let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
    let notification = ConfigNotification::new(snapshot);
    MarketParticipantsConnector::new(
        MarketParticipantsConnectorInput::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            notification,
        )
        .with_runtime(test_handle()),
    )
    .expect("MarketParticipantsConnector should build from typed runtime inputs")
}

#[test]
fn the_market_facade_takes_the_prediction_order_book_by_its_own_types() {
    let connector = connector();
    let capability = connector.market();
    let identity = |digits: &str| format!("v1/{}", digits.repeat(64));
    let leg = OutcomeLegRef {
        group: OutcomeGroupId::from_identity_value(&identity("1"))
            .expect("the group identity is valid"),
        member: OutcomeMemberId::from_identity_value(&identity("2"))
            .expect("the member identity is valid"),
        side: OutcomeSideId::from_identity_value(&identity("3"))
            .expect("the side identity is valid"),
    };
    let event = PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the prediction scope is canonical"),
        ),
        native_event: IdentifierValue::new("1262243").expect("the event id is valid"),
    };

    let _fetch = capability.fetch_prediction_order_book(event.clone(), Vec::new(), leg.clone());
    let _subscribe = capability.subscribe_prediction_order_book(event, Vec::new(), leg);
}
