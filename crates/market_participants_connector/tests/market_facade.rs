//! The public surface a widget builds a market request from.
//!
//! A candlestick or perpetual-statistics builder is chosen by its interval, so a
//! caller outside this crate has to be able to name the interval types. These
//! tests are the outside view: they compile only if the market facade really
//! exposes them, which is what a widget's own build depends on.

use market_participants_connector::market::timeframe::{
    Adjustment, StatisticsTimeFrame, TimeFrame,
};

/// The candlestick intervals a caller can name, with the wire spellings the
/// crate's own queries serialize them to.
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

/// The statistics intervals are nameable too, and their labels are the closed set
/// a statistics builder can be chosen by.
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

/// The adjustment conventions a candlestick request can carry, with the
/// spellings they serialize to.
///
/// A caller persists the convention it charted with, so the spelling is wire
/// schema just as an interval's is, and the default has to be the one convention
/// every route can honestly serve.
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
