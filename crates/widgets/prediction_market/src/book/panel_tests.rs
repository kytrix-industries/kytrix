//! Unit tests of the book panel: row mapping, anchoring, depth easing and the
//! splitter's arithmetic.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{App, ScrollStrategy, TestAppContext, px};
use market_participants_model::{
    decimal::Fraction,
    outcome::OutcomeLegRef,
    prediction::{PredictionBookLevel, PredictionOrderBook},
};

use super::{
    GroupedLevel, MAX_BOOK_RATIO, MIN_BOOK_RATIO, PanelRow, PredictionBookPanel, row_at,
    split_ratio,
};
use crate::test_support::{charted_market, event_ref, level, order_book};

/// How fast the panel's bars ease, mirrored here so a test can state the ratio it
/// expects at a given time without reading the panel's own constant.
const EASING_PER_SECOND: f32 = 12.0;

fn leg(index: usize) -> OutcomeLegRef {
    charted_market(&event_ref(), "charted").1[index].clone()
}

fn levels(pairs: &[(&str, &str)]) -> Vec<PredictionBookLevel> {
    pairs
        .iter()
        .map(|&(price, size)| level(price, size))
        .collect()
}

/// One fixture book of the charted market's first side.
fn book(tick: &str, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> Arc<PredictionOrderBook> {
    Arc::new(order_book(leg(0), tick, levels(bids), levels(asks)))
}

/// The depth key of one fixture price.
fn key(is_ask: bool, price: &str) -> (bool, u128) {
    let price: Fraction = price.parse().expect("the fixture price is a fraction");
    (is_ask, price.ratio().inner())
}

/// The depth key of one grouped level.
fn level_key(is_ask: bool, level: &GroupedLevel) -> (bool, u128) {
    (is_ask, level.price.ratio().inner())
}

/// The application state a rendering test needs.
fn install(cx: &mut App) {
    base_ui::style::theme::install(cx);
    i18n::global::embedded::install(cx).expect("the embedded locales should load");
}

#[test]
fn the_ladder_should_put_the_best_ask_directly_above_the_spread_row() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(
        book(
            "0.01",
            &[("0.30", "10"), ("0.25", "20")],
            &[("0.36", "10"), ("0.40", "20")],
        ),
        1,
    );

    let grouped = panel.grouped().expect("the book is grouped");
    assert_eq!(row_at(grouped, 0), Some(PanelRow::Ask(&grouped.asks[1])));
    assert_eq!(row_at(grouped, 1), Some(PanelRow::Ask(&grouped.asks[0])));
    assert_eq!(
        row_at(grouped, 2),
        Some(PanelRow::Spread),
        "the spread sits directly below the best ask"
    );
    assert_eq!(row_at(grouped, 3), Some(PanelRow::Bid(&grouped.bids[0])));
    assert_eq!(row_at(grouped, 4), Some(PanelRow::Bid(&grouped.bids[1])));
    assert_eq!(
        row_at(grouped, 5),
        None,
        "the ladder is exactly as long as its rows"
    );
}

#[gpui_kit::test]
fn the_first_book_should_jump_and_anchor_the_spread_row(cx: &mut TestAppContext) {
    let mut panel = PredictionBookPanel::new();
    assert!(panel.is_empty());

    panel.set_book(
        book(
            "0.01",
            &[("0.30", "10")],
            &[
                ("0.36", "10"),
                ("0.37", "20"),
                ("0.38", "30"),
                ("0.39", "40"),
            ],
        ),
        1,
    );

    assert!(!panel.is_empty());
    assert!(
        !panel.animation.active,
        "a first book does not move: it lands"
    );
    assert_eq!(panel.animation.eased, 1.0);
    assert_eq!(
        panel.animation.from.as_ref(),
        panel.animation.to.as_ref(),
        "there is no previous frame to move away from"
    );
    assert!(panel.anchor_pending());

    cx.update(|cx| {
        install(cx);
        let _ = panel.render(cx);
    });

    assert!(
        !panel.anchor_pending(),
        "the frame that drew the ladder consumed the anchor"
    );
    let deferred = panel
        .scroll()
        .0
        .borrow()
        .deferred_scroll_to_item
        .expect("the spread row was asked for");
    assert_eq!(
        deferred.item_index,
        panel.grouped().expect("the book is grouped").asks.len(),
        "the spread row is the anchored item"
    );
    assert_eq!(deferred.strategy, ScrollStrategy::Center);
}

#[test]
fn the_first_book_should_place_every_bar_without_animating() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(
        book("0.01", &[("0.30", "10")], &[("0.36", "10"), ("0.40", "30")]),
        1,
    );

    let grouped = panel.grouped().expect("the book is grouped");
    assert!(!panel.animation.active);
    assert_eq!(panel.animation.span, 0.0);
    assert_eq!(panel.animation.eased, 1.0);
    for level in &grouped.asks {
        assert_eq!(
            panel.animation.from.get(&level_key(true, level)).copied(),
            Some(level.depth),
            "an ask bar is drawn at the depth the first book asks for"
        );
    }
    for level in &grouped.bids {
        assert_eq!(
            panel.animation.from.get(&level_key(false, level)).copied(),
            Some(level.depth),
            "a bid bar is drawn at the depth the first book asks for"
        );
    }
}

#[test]
fn a_live_update_should_retarget_the_movement_from_the_painted_ratios() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    let painted = panel.animation.to.clone();

    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);

    assert!(panel.animation.active, "a live update moves the bars");
    assert_eq!(panel.animation.eased, 0.0, "the movement starts now");
    let before = painted
        .get(&key(true, "0.40"))
        .copied()
        .expect("the level was on screen");
    let from = panel
        .animation
        .from
        .get(&key(true, "0.40"))
        .copied()
        .expect("the level is in the new book");
    let to = panel
        .animation
        .to
        .get(&key(true, "0.40"))
        .copied()
        .expect("the level is in the new book");
    assert!(
        (from - before).abs() < 1e-6,
        "the movement starts from where the last frame painted: {from} vs {before}"
    );
    assert!((to - 0.25).abs() < 1e-6);
    assert!(
        (panel.animation.span - (before - to).abs()).abs() < 1e-6,
        "the span is the largest distance any bar has to cover"
    );
}

#[test]
fn the_bars_should_ease_partially_and_then_settle() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);

    let started = Instant::now();
    panel.animation.started = started - Duration::from_millis(30);
    assert!(
        panel.advance(started, false),
        "30 ms in, the bars are moving"
    );
    let expected = 1.0 - (-EASING_PER_SECOND * 0.03).exp();
    assert!(
        (panel.animation.eased - expected).abs() < 1e-5,
        "the movement is read from the elapsed time: {} vs {expected}",
        panel.animation.eased
    );

    panel.animation.started = started - Duration::from_secs(1);
    assert!(
        !panel.advance(started, false),
        "one second in, what is left is smaller than anyone can see"
    );
    assert_eq!(panel.animation.eased, 1.0);
    assert!(!panel.animation.active);
}

#[test]
fn a_level_that_appears_should_grow_from_zero() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.01", &[], &[("0.40", "10")]), 1);

    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);

    let added = key(true, "0.50");
    assert_eq!(
        panel.animation.from.get(&added).copied(),
        Some(0.0),
        "a level the previous book did not have starts empty"
    );
    assert!(
        panel
            .animation
            .to
            .get(&added)
            .copied()
            .is_some_and(|ratio| ratio > 0.0),
        "and grows to the depth the new book asks for"
    );
}

#[test]
fn reduce_motion_should_put_the_bars_where_they_belong() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);
    assert!(panel.animation.active);

    assert!(
        !panel.advance(Instant::now(), true),
        "a reader who asked for less movement is shown none, and no more frames are asked for"
    );
    assert_eq!(panel.animation.eased, 1.0);
    assert!(!panel.animation.active);
}

#[test]
fn another_side_or_another_grouping_should_jump_instead_of_moving() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.001", &[], &[("0.341", "10"), ("0.342", "10")]), 1);
    let initial = panel.animation.to.clone();

    // Another grouping is another set of rows, not a movement of the old ones.
    panel.set_multiplier(10);
    assert_eq!(panel.multiplier(), 10);
    assert!(!panel.animation.active);
    assert!(panel.anchor_pending());
    assert_eq!(
        panel.grouped().expect("the book is regrouped").asks.len(),
        1,
        "both ticks fall into the same ten-tick bucket"
    );
    assert_ne!(
        initial.as_ref(),
        panel.animation.to.as_ref(),
        "the grouped depths are rebuilt at the new step"
    );

    // Another side is another ladder as well.
    panel.clear();
    assert!(panel.is_empty());
    assert!(!panel.anchor_pending());
    panel.set_book(
        Arc::new(order_book(
            leg(1),
            "0.001",
            Vec::new(),
            levels(&[("0.341", "10")]),
        )),
        1,
    );
    assert!(!panel.animation.active);
    assert!(panel.anchor_pending());
}

#[test]
fn split_ratio_should_follow_the_pointer_and_stay_inside_the_area() {
    assert_eq!(split_ratio(0.5, px(0.0), px(500.0)), 0.5);
    let shrunk = split_ratio(0.5, px(50.0), px(500.0));
    assert!(
        (shrunk - 0.4).abs() < 1e-6,
        "dragging down gives the book less: {shrunk}"
    );
    let grown = split_ratio(0.5, px(-50.0), px(500.0));
    assert!(
        (grown - 0.6).abs() < 1e-6,
        "dragging up gives it more: {grown}"
    );
    assert_eq!(
        split_ratio(0.25, px(1_000.0), px(500.0)),
        MIN_BOOK_RATIO,
        "a drag past the smallest share stops at it"
    );
    assert_eq!(
        split_ratio(0.75, px(-1_000.0), px(500.0)),
        MAX_BOOK_RATIO,
        "a drag past the largest share stops at it"
    );
    assert_eq!(
        split_ratio(0.5, px(50.0), px(0.0)),
        0.5,
        "an area that has not been laid out leaves the notch alone"
    );
}
