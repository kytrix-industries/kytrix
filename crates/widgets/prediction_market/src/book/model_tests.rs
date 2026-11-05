//! Unit tests of the grouped order book: buckets, totals, depth and labels.

use market_participants_model::{
    decimal::{Fraction, Quantity, Tick},
    outcome::OutcomeLegRef,
    prediction::PredictionOrderBook,
};

use super::{
    DEFAULT_BOOK_RATIO, GROUPING_MULTIPLIERS, MAX_BOOK_RATIO, MIN_BOOK_RATIO, clamp_ratio,
    format_cents, format_notional, format_shares, group, grouping_steps, settle_grouping,
};
use crate::test_support::{charted_market, event_ref, level, order_book};

/// One side of the screenshot's ladder: price, size and the running notional the
/// pane reads off it.
fn screenshot_asks(leg: OutcomeLegRef) -> PredictionOrderBook {
    order_book(
        leg,
        "0.001",
        Vec::new(),
        vec![
            level("0.343", "300"),
            level("0.344", "602"),
            level("0.345", "3681"),
            level("0.346", "59948.32"),
        ],
    )
}

fn leg() -> OutcomeLegRef {
    charted_market(&event_ref(), "charted").1[0].clone()
}

fn price(value: &str) -> Fraction {
    value.parse().expect("the fixture price is a fraction")
}

fn size(value: &str) -> Quantity {
    value.parse().expect("the fixture size is a quantity")
}

#[test]
fn grouping_should_round_bids_down_and_asks_up() {
    let book = order_book(
        leg(),
        "0.01",
        // Neither level is on the tick, so its bucket is decided by the side.
        vec![level("0.349", "10"), level("0.301", "20")],
        vec![level("0.351", "10"), level("0.399", "20")],
    );

    let grouped = group(&book, 1);

    assert_eq!(
        grouped
            .bids
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.34"), price("0.3")],
        "a bid joins the cent below it, so the book never claims a better bid \
         than the venue published"
    );
    assert_eq!(
        grouped
            .asks
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.36"), price("0.4")],
        "an ask joins the cent above it, so the book never claims a cheaper ask"
    );
}

#[test]
fn grouping_should_sum_the_levels_inside_one_bucket() {
    let book = order_book(
        leg(),
        "0.01",
        vec![level("0.34", "10"), level("0.341", "5")],
        vec![level("0.349", "10"), level("0.35", "5")],
    );

    let grouped = group(&book, 1);

    assert_eq!(grouped.asks.len(), 1);
    assert_eq!(grouped.asks[0].price, price("0.35"));
    assert_eq!(grouped.asks[0].size, size("15"));
    assert_eq!(grouped.bids.len(), 1);
    assert_eq!(grouped.bids[0].price, price("0.34"));
    assert_eq!(grouped.bids[0].size, size("15"));
}

#[test]
fn a_coarser_grouping_should_merge_neighbouring_ticks() {
    let book = screenshot_asks(leg());

    let grouped = group(&book, 10);

    assert_eq!(grouped.tick, "0.001".parse::<Tick>().unwrap());
    assert_eq!(
        grouped
            .asks
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.35")],
        "every level of the ladder is inside the same ten-tick bucket"
    );
    assert_eq!(
        grouped.asks[0].size,
        size("64531.32"),
        "a bucket holds every share the levels inside it held"
    );
}

#[test]
fn the_cumulative_notional_should_match_the_published_ladder() {
    let book = screenshot_asks(leg());

    let grouped = group(&book, 1);

    assert_eq!(
        grouped
            .asks
            .iter()
            .map(|level| format_notional(level.total))
            .collect::<Vec<_>>(),
        vec!["$102.90", "$309.99", "$1,579.93", "$22,322.05"],
        "each row adds its own notional to the better rows above it"
    );
    assert_eq!(
        grouped
            .asks
            .iter()
            .map(|level| format_shares(level.size))
            .collect::<Vec<_>>(),
        vec!["300.00", "602.00", "3,681.00", "59,948.32"]
    );
}

#[test]
fn depth_should_climb_towards_the_far_end_of_a_side() {
    let book = screenshot_asks(leg());

    let grouped = group(&book, 1);
    let depths: Vec<f32> = grouped.asks.iter().map(|level| level.depth).collect();

    assert_eq!(depths.len(), 4);
    assert!(
        depths.windows(2).all(|pair| pair[1] > pair[0]),
        "each row holds at least what the rows above it held: {depths:?}"
    );
    assert!((depths[3] - 1.0).abs() < 1e-6, "the far end is all of it");
    let expected = 300.0 / 64_531.32;
    assert!(
        (depths[0] - expected).abs() < 1e-6,
        "the best row holds its own shares: {} vs {expected}",
        depths[0]
    );
}

#[test]
fn bids_should_run_from_the_best_price_downwards() {
    let book = order_book(
        leg(),
        "0.01",
        vec![level("0.3", "1"), level("0.34", "2"), level("0.32", "3")],
        vec![level("0.4", "1"), level("0.36", "2"), level("0.38", "3")],
    );

    let grouped = group(&book, 1);

    assert_eq!(
        grouped
            .bids
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.34"), price("0.32"), price("0.3")],
        "the bid side is drawn best-first, so it runs downwards"
    );
    assert_eq!(
        grouped
            .asks
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.36"), price("0.38"), price("0.4")],
        "the ask side is drawn best-first, so it runs upwards"
    );
    assert_eq!(grouped.best_bid, Some(price("0.34")));
    assert_eq!(grouped.best_ask, Some(price("0.36")));
}

#[test]
fn the_spread_should_be_the_best_ask_minus_the_best_bid() {
    let book = order_book(
        leg(),
        "0.001",
        vec![level("0.343", "10")],
        vec![level("0.351", "10")],
    );

    let grouped = group(&book, 1);

    assert_eq!(grouped.spread, Some(price("0.008")));
    assert_eq!(format_cents(price("0.008"), grouped.tick), "0.8¢");
}

#[test]
fn a_book_without_a_bid_has_no_spread() {
    let book = order_book(leg(), "0.01", Vec::new(), vec![level("0.35", "10")]);

    let grouped = group(&book, 1);

    assert_eq!(grouped.spread, None);
    assert_eq!(grouped.best_bid, None);
}

#[test]
fn a_crossed_book_has_no_spread_either() {
    let book = order_book(
        leg(),
        "0.01",
        vec![level("0.36", "10")],
        vec![level("0.35", "10")],
    );

    assert_eq!(group(&book, 1).spread, None);
}

#[test]
fn a_zero_size_level_should_be_dropped() {
    let book = order_book(
        leg(),
        "0.01",
        vec![level("0.34", "0"), level("0.33", "5")],
        vec![level("0.35", "0")],
    );

    let grouped = group(&book, 1);

    assert_eq!(
        grouped
            .bids
            .iter()
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![price("0.33")]
    );
    assert!(grouped.asks.is_empty());
    assert_eq!(grouped.best_ask, None);
}

#[test]
fn an_empty_book_should_hold_nothing() {
    let book = order_book(leg(), "0.01", Vec::new(), Vec::new());

    let grouped = group(&book, 1);

    assert!(grouped.is_empty());
    assert!(grouped.asks.is_empty());
    assert!(grouped.bids.is_empty());
    assert_eq!(grouped.spread, None);
    assert_eq!(grouped.last, None);
}

#[test]
fn the_last_trade_should_survive_grouping() {
    let mut book = screenshot_asks(leg());
    book.last_trade_price = Some(price("0.345"));

    assert_eq!(group(&book, 1).last, Some(price("0.345")));
}

#[test]
fn a_multiplier_the_tick_cannot_express_should_fall_back_to_the_finest() {
    // A whole-share tick leaves no finer grouping than the tick itself.
    let coarse = order_book(leg(), "1", vec![level("0", "10")], vec![level("1", "10")]);
    assert_eq!(settle_grouping(coarse.tick_size, 10), 1);

    // Unknown multipliers never reach the book at all.
    let book = screenshot_asks(leg());
    assert_eq!(settle_grouping(book.tick_size, 7), 1);

    // A step of a whole share is not a price grouping.
    let tenth = order_book(leg(), "0.1", Vec::new(), vec![level("0.3", "10")]);
    assert_eq!(
        settle_grouping(tenth.tick_size, 10),
        1,
        "ten tenths is not a step below one"
    );
}

#[test]
fn grouping_steps_should_name_the_steps_a_tick_can_express() {
    let fine: Vec<(u32, String)> = grouping_steps("0.001".parse::<Tick>().unwrap())
        .into_iter()
        .map(|(multiplier, label)| (multiplier, label.to_string()))
        .collect();
    assert_eq!(
        fine,
        vec![
            (1, "0.001".to_owned()),
            (10, "0.01".to_owned()),
            (100, "0.1".to_owned()),
        ]
    );

    let cent: Vec<(u32, String)> = grouping_steps("0.01".parse::<Tick>().unwrap())
        .into_iter()
        .map(|(multiplier, label)| (multiplier, label.to_string()))
        .collect();
    assert_eq!(
        cent,
        vec![(1, "0.01".to_owned()), (10, "0.1".to_owned())],
        "a hundred cents is one share, which is not a step"
    );

    assert!(
        grouping_steps("1".parse::<Tick>().unwrap()).is_empty(),
        "a whole-share tick has no grouping below one"
    );
    assert_eq!(GROUPING_MULTIPLIERS.len(), 3);
}

#[test]
fn a_price_should_be_quoted_in_cents_at_its_ticks_precision() {
    let thousandth = "0.001".parse::<Tick>().unwrap();
    let cent = "0.01".parse::<Tick>().unwrap();

    assert_eq!(format_cents(price("0.346"), thousandth), "34.6¢");
    assert_eq!(
        format_cents(price("0.34"), thousandth),
        "34.0¢",
        "a thousandth tick is a tenth of a cent, so the tenth is kept"
    );
    assert_eq!(format_cents(price("0.34"), cent), "34¢");
    assert_eq!(format_cents(price("1"), cent), "100¢");
    assert_eq!(format_cents(Fraction::ZERO, cent), "0¢");
}

#[test]
fn shares_and_notional_should_be_grouped_and_rounded() {
    let shares = size("59948.32");

    assert_eq!(format_shares(shares), "59,948.32");
    assert_eq!(format_shares(size("999")), "999.00");
    assert_eq!(format_notional(shares), "$59,948.32");
    assert_eq!(
        format_notional(size("22322.05172")),
        "$22,322.05",
        "a notional is rounded to the cent before it is drawn"
    );
    assert_eq!(
        format_notional(size("0.005")),
        "$0.01",
        "half a cent rounds up"
    );
}

#[test]
fn clamp_ratio_should_keep_the_notch_inside_the_body() {
    assert_eq!(clamp_ratio(0.5), 0.5);
    assert_eq!(clamp_ratio(MIN_BOOK_RATIO), MIN_BOOK_RATIO);
    assert_eq!(clamp_ratio(MAX_BOOK_RATIO), MAX_BOOK_RATIO);
    assert_eq!(clamp_ratio(0.0), MIN_BOOK_RATIO);
    assert_eq!(clamp_ratio(9.0), MAX_BOOK_RATIO);
    assert_eq!(clamp_ratio(f32::NAN), DEFAULT_BOOK_RATIO);
    assert_eq!(clamp_ratio(f32::INFINITY), DEFAULT_BOOK_RATIO);
    assert_eq!(
        (MIN_BOOK_RATIO, MAX_BOOK_RATIO, DEFAULT_BOOK_RATIO),
        (0.2, 0.8, 0.5)
    );
}
