use std::{str::FromStr, sync::Arc};

use market_participants_model::{
    asset::{AssetId, Iso4217Code},
    decimal::{Quantity, Ratio, Tick},
    listing::{
        route::VenueListingKey,
        rules::{ListingPrecision, TradingRules},
    },
    market::{
        book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
        units::{MarketQuantity, QuantityUnit},
    },
};

use super::series::*;

fn quantity(value: &str) -> Quantity {
    Quantity::from_str(value).expect("test quantity should parse")
}

fn ratio(value: &str) -> Ratio {
    Ratio::from_str(value).expect("test ratio should parse")
}

/// One listing of the model's own fixtures. The model never reads it, but a
/// window is always the window of one listing.
fn listing() -> VenueListingKey {
    market_participants_connector::catalog::test_support::listing_key(
        &market_participants_connector::catalog::test_support::perp_route(),
        "BTCUSDT",
    )
}

fn traded(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::AssetUnits {
            asset: market_participants_connector::catalog::test_support::token("binance", "BTC"),
        },
    }
}

fn notional(value: Quantity) -> MarketQuantity {
    MarketQuantity {
        value,
        unit: QuantityUnit::Notional {
            asset: market_participants_connector::catalog::test_support::token("binance", "USDT"),
        },
    }
}

fn open_interest(timestamp: i64, amount: &str) -> ListingOpenInterest {
    ListingOpenInterest {
        listing: listing(),
        timestamp,
        quantity: traded(quantity(amount)),
        notional: Some(notional(quantity("1"))),
    }
}

fn ratio_point(timestamp: i64, value: &str) -> ListingLongShortRatio {
    ListingLongShortRatio {
        listing: listing(),
        timestamp,
        ratio: ratio(value),
    }
}

fn taker_volume(timestamp: i64, buy: &str, sell: &str) -> ListingTakerVolume {
    ListingTakerVolume {
        listing: listing(),
        timestamp,
        buy: traded(quantity(buy)),
        sell: traded(quantity(sell)),
    }
}

fn open_interest_timestamps(points: &StatisticsPoints) -> Vec<i64> {
    let StatisticsPoints::OpenInterest(points) = points else {
        panic!("expected open-interest points");
    };
    points.iter().map(|point| point.timestamp).collect()
}

#[test]
fn open_interest_should_sort_deduplicate_and_keep_the_newest_window() {
    // Oldest last, one timestamp repeated, one interval more than the window.
    let mut items = (1..=31)
        .rev()
        .map(|timestamp| open_interest(timestamp, &timestamp.to_string()))
        .collect::<Vec<_>>();
    items.push(open_interest(31, "99"));

    let (points, changed) =
        normalize_open_interest(items, None).expect("the fixture readings should be accepted");

    assert!(changed);
    assert_eq!(
        open_interest_timestamps(&points).len(),
        usize::from(POINT_LIMIT)
    );
    assert_eq!(open_interest_timestamps(&points).first(), Some(&2));
    let StatisticsPoints::OpenInterest(points) = &points else {
        panic!("expected open-interest points");
    };
    assert_eq!(points[29].base_asset_volume, quantity("99"));
}

/// The amount of an open-interest interval is a monetary amount and its size is a
/// traded quantity: a reading expressed as anything else is refused rather than
/// drawn as if it were one.
#[test]
fn open_interest_should_refuse_a_reading_that_is_not_expressed_in_its_own_units() {
    let mut amount_only = open_interest(1, "1");
    amount_only.quantity = notional(quantity("1"));
    let error = normalize_open_interest([amount_only], None)
        .expect_err("an amount is not the size of an open interest");
    assert_eq!(
        error,
        market_participants_model::error::ModelError::QuantityUnitMismatch {
            field: "open_interest.quantity",
            expected: market_participants_model::error::QuantityUnitExpectation::TradedQuantity,
        }
    );

    let mut traded_amount = open_interest(1, "1");
    traded_amount.notional = Some(traded(quantity("1")));
    let error = normalize_open_interest([traded_amount], None)
        .expect_err("a traded quantity is not a monetary amount");
    assert_eq!(
        error,
        market_participants_model::error::ModelError::QuantityUnitMismatch {
            field: "open_interest.notional",
            expected: market_participants_model::error::QuantityUnitExpectation::Notional,
        }
    );
}

/// A venue is not obliged to publish what the open interest was worth: the line is
/// then absent for that interval rather than drawn from a number nobody published.
#[test]
fn an_open_interest_interval_without_an_amount_should_keep_a_missing_value() {
    let mut item = open_interest(1, "2");
    item.notional = None;

    let (points, changed) =
        normalize_open_interest([item], None).expect("a missing amount is not a failure");

    assert!(changed);
    let StatisticsPoints::OpenInterest(points) = &points else {
        panic!("expected open-interest points");
    };
    assert_eq!(points[0].base_asset_volume, quantity("2"));
    assert_eq!(points[0].quote_asset_volume, None);
}

#[test]
fn a_window_should_not_depend_on_the_order_the_venue_sent_it_in() {
    let ascending = (1..=3)
        .map(|timestamp| open_interest(timestamp, &timestamp.to_string()))
        .collect::<Vec<_>>();
    let descending = ascending.iter().cloned().rev().collect::<Vec<_>>();

    let (ascending, _) =
        normalize_open_interest(ascending, None).expect("the fixture readings should be accepted");
    let (descending, _) =
        normalize_open_interest(descending, None).expect("the fixture readings should be accepted");

    assert_eq!(open_interest_timestamps(&ascending), vec![1, 2, 3]);
    assert_eq!(
        open_interest_timestamps(&ascending),
        open_interest_timestamps(&descending)
    );
}

#[test]
fn an_unchanged_window_should_reuse_its_allocation() {
    let items = (1..=31)
        .map(|timestamp| open_interest(timestamp, &timestamp.to_string()))
        .collect::<Vec<_>>();
    let (previous, _) =
        normalize_open_interest(items.clone(), None).expect("the fixture readings are accepted");

    let (next, changed) =
        normalize_open_interest(items, Some(&previous)).expect("the fixture readings are accepted");

    let (StatisticsPoints::OpenInterest(previous), StatisticsPoints::OpenInterest(next)) =
        (&previous, &next)
    else {
        panic!("expected open-interest points");
    };
    assert!(!changed, "the same intervals are not a change");
    assert!(Arc::ptr_eq(previous, next));
}

#[test]
fn a_moved_window_should_report_a_change() {
    let (previous, _) =
        normalize_open_interest([open_interest(1, "1"), open_interest(2, "2")], None)
            .expect("the fixture readings should be accepted");

    let (_, changed) = normalize_open_interest(
        [open_interest(2, "2"), open_interest(3, "3")],
        Some(&previous),
    )
    .expect("the fixture readings should be accepted");

    assert!(changed);
}

#[test]
fn a_window_from_another_series_should_not_be_reused() {
    let (previous, _) = normalize_open_interest([open_interest(1, "1")], None)
        .expect("the fixture readings should be accepted");

    let (points, changed) = normalize_long_short_ratio([ratio_point(1, "1")], Some(&previous))
        .expect("the fixture readings should be accepted");

    assert!(
        changed,
        "a window of another series is not a window of this one"
    );
    assert!(matches!(points, StatisticsPoints::LongShortRatio(_)));
}

#[test]
fn invalid_timestamps_should_be_discarded() {
    let (points, _) = normalize_taker_volume(
        [
            taker_volume(0, "9", "9"),
            taker_volume(-1, "9", "9"),
            taker_volume(2, "2", "20"),
        ],
        None,
    )
    .expect("the fixture readings should be accepted");

    assert_eq!(ratio_timestamps_of_taker(&points), vec![2]);
}

/// Both sides of a taker volume are traded quantities, so a volume expressed as a
/// monetary amount is refused rather than drawn as if it were traded.
#[test]
fn taker_volume_should_refuse_a_side_that_is_not_a_traded_quantity() {
    let error = normalize_taker_volume(
        [ListingTakerVolume {
            listing: listing(),
            timestamp: 1,
            buy: notional(quantity("1")),
            sell: traded(quantity("1")),
        }],
        None,
    )
    .expect_err("a buy volume expressed as an amount is not a traded volume");

    assert_eq!(
        error,
        market_participants_model::error::ModelError::QuantityUnitMismatch {
            field: "taker_volume.buy",
            expected: market_participants_model::error::QuantityUnitExpectation::TradedQuantity,
        }
    );
}

fn ratio_timestamps_of_taker(points: &StatisticsPoints) -> Vec<i64> {
    let StatisticsPoints::TakerVolume(points) = points else {
        panic!("expected taker-volume points");
    };
    points.iter().map(|point| point.timestamp).collect()
}

#[test]
fn ratio_points_should_keep_the_newest_value_of_a_repeated_interval() {
    let (points, _) = normalize_long_short_ratio(
        [
            ratio_point(1, "1"),
            ratio_point(2, "2"),
            ratio_point(1, "3"),
        ],
        None,
    )
    .expect("the fixture readings should be accepted");

    let StatisticsPoints::LongShortRatio(points) = &points else {
        panic!("expected long-short-ratio points");
    };
    assert_eq!(
        points.as_ref(),
        [
            LongShortRatioPoint {
                timestamp: 1,
                ratio: ratio("3"),
            },
            LongShortRatioPoint {
                timestamp: 2,
                ratio: ratio("2"),
            },
        ]
    );
}

#[test]
fn ratio_shares_should_split_a_ratio_into_its_two_sides() {
    let (long, short) = ratio_shares(ratio("1.5164")).expect("a positive ratio has shares");

    assert!((long - 0.602_6).abs() < 1e-4);
    assert!((long + short - 1.0).abs() < f64::EPSILON);
}

#[test]
fn ratio_shares_should_reject_a_ratio_that_is_not_a_reading() {
    assert_eq!(ratio_shares(Ratio::ZERO), None);
}

#[test]
fn a_ratio_should_be_readable_to_the_printed_precision_of_its_scale() {
    assert!((ratio_as_f64(ratio("1.25")) - 1.25).abs() < f64::EPSILON);
    assert!((quantity_as_f64(quantity("12.5")) - 12.5).abs() < f64::EPSILON);
}

#[test]
fn a_window_should_hold_the_series_it_was_normalized_from() {
    let (open_interest, _) = normalize_open_interest([open_interest(1, "1")], None)
        .expect("the fixture readings should be accepted");
    let (ratios, _) = normalize_long_short_ratio([ratio_point(1, "1")], None)
        .expect("the fixture readings should be accepted");
    let (taker, _) = normalize_taker_volume([taker_volume(1, "1", "1")], None)
        .expect("the fixture readings should be accepted");

    assert!(matches!(open_interest, StatisticsPoints::OpenInterest(_)));
    assert!(matches!(ratios, StatisticsPoints::LongShortRatio(_)));
    assert!(matches!(taker, StatisticsPoints::TakerVolume(_)));
}

/// One rule set of two fixture assets, which is what the metadata is read from.
fn fixture_rules() -> TradingRules {
    let currency = |code: &str| {
        AssetId::Currency(market_participants_model::asset::CurrencyId::new(
            Iso4217Code::new(code).expect("the fixture code is canonical"),
        ))
    };
    market_participants_connector::catalog::test_support::rules(currency("USD"), currency("EUR"))
}

#[test]
fn metadata_should_carry_the_listings_own_ticks() {
    assert_eq!(
        ListingPrecision::of(&fixture_rules()),
        ListingPrecision {
            price_tick: Tick::from_inner(1_000_000_000_000_000_000)
                .expect("a whole unit is a valid tick"),
            quantity_tick: Tick::from_inner(1_000_000_000_000_000_000)
                .expect("a whole unit is a valid tick"),
        }
    );
}

/// A rule set that carries no quantity step leaves the labels written at whole
/// units rather than refusing to draw the window.
#[test]
fn metadata_should_fall_back_to_whole_units_without_a_quantity_step() {
    let mut rules = fixture_rules();
    rules.common.quantity.step = Quantity::ZERO;

    assert_eq!(
        ListingPrecision::of(&rules).quantity_tick,
        Tick::from_inner(1_000_000_000_000_000_000).expect("a whole unit is a valid tick")
    );
}
