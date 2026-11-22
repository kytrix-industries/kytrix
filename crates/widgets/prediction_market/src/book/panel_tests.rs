//! Tests of the book panel: the pure ladder state (row mapping, anchoring,
//! grouping, depth easing and the splitter's arithmetic) and the engine-level
//! drawing contract (list rows, depth-bar quads, steady frames, the depth timer
//! and release).

use std::{rc::Rc, sync::Arc, time::Duration};

use base_ui::{
    kit::motion::DEPTH_ANIMATION_RESPONSE_PER_SECOND,
    style_builder::StyleBuilder,
    theme::{appearance::Appearance, role::Role},
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_model::{
    decimal::Fraction,
    outcome::OutcomeLegRef,
    prediction::{PredictionBookLevel, PredictionOrderBook},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    interaction::ItemAlign,
    node::{builder::NodeBuilder, id::NodeId},
    test_support::record::PrimitiveKind,
};
use widget::test_support::{CanvasPrimitive, WidgetTestApp};

use super::{
    model::{GroupedLevel, MAX_BOOK_RATIO, MIN_BOOK_RATIO},
    panel::{PanelRow, PredictionBookPanel, row_at, split_ratio},
};
use crate::test_support::{charted_market, event_ref, level, order_book};

/// The fixed page the fixture mounts the panel into.
const PAGE: (f32, f32) = (400.0, 400.0);

/// The height of one ladder row, as the depth-bar expectations use it.
const ROW_HEIGHT: f32 = 22.0;

/// The depth timer's tick.
const TICK: Duration = Duration::from_millis(16);

const HEADER_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_HEADER";
const EMPTY_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_EMPTY";

fn product_translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

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

/// A ladder deeper than the fixture page, so the list has to scroll.
fn deep_book(levels_per_side: usize) -> Arc<PredictionOrderBook> {
    let asks: Vec<PredictionBookLevel> = (0..levels_per_side)
        .map(|index| level(&format!("0.{:03}", 200 + index), "10"))
        .collect();
    let bids: Vec<PredictionBookLevel> = (0..levels_per_side)
        .map(|index| level(&format!("0.{:03}", 100 - index), "10"))
        .collect();
    Arc::new(order_book(leg(0), "0.001", bids, asks))
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

// ------------------------------------------------------------------ pure state

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

    let grouped = panel.grouped.as_deref().expect("the book is grouped");
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

#[test]
fn the_first_book_should_jump_and_place_every_bar_without_animating() {
    let mut panel = PredictionBookPanel::new();
    assert!(panel.is_empty());

    panel.set_book(
        book("0.01", &[("0.30", "10")], &[("0.36", "10"), ("0.40", "30")]),
        1,
    );

    assert!(!panel.is_empty());
    assert!(
        !panel.animation.active,
        "a first book does not move: it lands"
    );
    assert_eq!(panel.animation.eased, 1.0);
    assert_eq!(panel.animation.span, 0.0);
    assert_eq!(
        panel.animation.from.as_ref(),
        panel.animation.to.as_ref(),
        "there is no previous frame to move away from"
    );
    assert!(panel.anchor_pending);
    let grouped = panel.grouped.as_deref().expect("the book is grouped");
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

    assert!(panel.advance(false), "the first tick of the movement draws");
    let expected = 1.0 - (-DEPTH_ANIMATION_RESPONSE_PER_SECOND * 0.016).exp();
    assert!(
        (panel.animation.eased - expected).abs() < 1e-5,
        "one tick is 16 ms of the old elapsed-time easing: {} vs {expected}",
        panel.animation.eased
    );

    let mut ticks = 1;
    while panel.advance(false) {
        ticks += 1;
        assert!(ticks < 200, "the movement has to settle");
    }
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
        !panel.advance(true),
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
    assert_eq!(panel.multiplier, 10);
    assert!(!panel.animation.active);
    assert!(panel.anchor_pending);
    assert_eq!(
        panel
            .grouped
            .as_deref()
            .expect("the book is regrouped")
            .asks
            .len(),
        1,
        "both ticks fall into the same ten-tick bucket"
    );
    assert!(
        !Rc::ptr_eq(&initial, &panel.animation.to),
        "the grouped depths are rebuilt at the new step"
    );

    // Another side is another ladder as well.
    panel.clear();
    assert!(panel.is_empty());
    assert!(!panel.anchor_pending);
    assert_eq!(panel.animation.eased, 1.0);
    assert!(!panel.animation.active);
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
    assert!(panel.anchor_pending);
}

#[test]
fn the_depth_snapshot_should_interpolate_between_the_painted_and_target_ratios() {
    let mut panel = PredictionBookPanel::new();
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    panel.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);
    let price: Fraction = "0.40".parse().expect("the fixture price is a fraction");
    let from = panel
        .animation
        .from
        .get(&key(true, "0.40"))
        .copied()
        .expect("the level is in the movement");
    let to = panel
        .animation
        .to
        .get(&key(true, "0.40"))
        .copied()
        .expect("the level is in the movement");
    assert!(
        (panel.animation.snapshot().ratio(true, price) - from).abs() < 1e-6,
        "no tick means the frame still draws where the last one painted"
    );

    assert!(panel.advance(false));
    let eased = panel.animation.eased;
    let expected = from + (to - from) * eased;
    assert!(
        (panel.animation.snapshot().ratio(true, price) - expected).abs() < 1e-6,
        "a tick draws the interpolation of the same snapshot"
    );
    assert_eq!(panel.animation.snapshot().ratio(false, price), 0.0);
}

#[test]
fn the_grouping_choice_should_follow_the_book_and_fall_back_to_its_tick() {
    let mut panel = PredictionBookPanel::new();
    assert!(!panel.has_grouping_choice());
    assert_eq!(panel.grouping_label(), None);

    let tick = "0.001";
    panel.set_book(book(tick, &[("0.341", "10")], &[("0.342", "10")]), 1);
    assert!(
        panel.has_grouping_choice(),
        "three steps are offered at this tick"
    );
    assert_eq!(panel.grouping_label().as_deref(), Some("0.001"));
    panel.set_multiplier(10);
    assert_eq!(panel.grouping_label().as_deref(), Some("0.01"));

    panel.clear();
    assert!(
        !panel.has_grouping_choice(),
        "a cleared panel offers nothing"
    );
    assert_eq!(panel.grouping_label(), None);

    // A half-share tick offers a step of one share, which is not a price: the
    // ladder is drawn at the venue's own tick, which is also the fallback label.
    panel.set_book(book("0.5", &[("0.4", "10")], &[("0.6", "10")]), 1);
    assert!(!panel.has_grouping_choice());
    assert_eq!(
        panel.grouping_label(),
        Some(format_smolstr!(
            "{}",
            panel.book.as_ref().expect("book").tick_size
        ))
    );
}

#[test]
fn split_ratio_should_follow_the_pointer_and_stay_inside_the_area() {
    assert_eq!(split_ratio(0.5, 0.0, 500.0), 0.5);
    let shrunk = split_ratio(0.5, 50.0, 500.0);
    assert!(
        (shrunk - 0.4).abs() < 1e-6,
        "dragging down gives the book less: {shrunk}"
    );
    let grown = split_ratio(0.5, -50.0, 500.0);
    assert!(
        (grown - 0.6).abs() < 1e-6,
        "dragging up gives it more: {grown}"
    );
    assert_eq!(
        split_ratio(0.25, 1_000.0, 500.0),
        MIN_BOOK_RATIO,
        "a drag past the smallest share stops at it"
    );
    assert_eq!(
        split_ratio(0.75, -1_000.0, 500.0),
        MAX_BOOK_RATIO,
        "a drag past the largest share stops at it"
    );
    assert_eq!(
        split_ratio(0.5, 50.0, 0.0),
        0.5,
        "an area that has not been laid out leaves the notch alone"
    );
}

// -------------------------------------------------------------- engine drawing

/// The panel mounted into a fixed page over the framework's headless host.
struct Fixture {
    harness: WidgetTestApp,
    panel: PredictionBookPanel,
}

impl Fixture {
    fn new() -> Self {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let page = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_PREDICTION_MARKET_BOOK_PAGE")
                .style(
                    StyleBuilder::new()
                        .width(PAGE.0)
                        .height(PAGE.1)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the fixture page builds")
        });
        let mut panel = PredictionBookPanel::new();
        harness
            .with_cx(|cx| panel.build(cx, page))
            .expect("the panel builds");
        harness.drain();
        Self { harness, panel }
    }

    fn set_book(&mut self, book: Arc<PredictionOrderBook>, multiplier: u32) {
        self.panel.set_book(book, multiplier);
    }

    fn sync(&mut self) -> bool {
        let harness = &mut self.harness;
        let panel = &mut self.panel;
        harness
            .with_cx(|cx| panel.sync(cx))
            .expect("the panel syncs")
    }

    fn draw(&mut self) -> bool {
        let painted = self.sync();
        self.harness.drain();
        painted
    }

    fn reduce_motion(&mut self, reduce: bool) {
        let reduce_motion = self.harness.services().reduce_motion;
        self.harness.app_mut().write(&reduce_motion, reduce);
    }

    fn node(&self, selector: &str) -> NodeId {
        self.harness
            .find_by_debug_prefix(self.harness.root(), selector)
            .unwrap_or_else(|| panic!("`{selector}` is built"))
    }

    fn canvas(&self) -> NodeId {
        self.panel
            .built_canvas()
            .expect("the built panel draws into its canvas")
    }

    fn quads(&self) -> Vec<CanvasPrimitive> {
        self.harness
            .canvas_primitives(self.canvas())
            .into_iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .collect()
    }

    fn row(&self, index: usize) -> NodeId {
        self.panel
            .mounted_rows(self.harness.app())
            .into_iter()
            .find_map(|(mounted, node)| (mounted == index).then_some(node))
            .unwrap_or_else(|| panic!("row {index} is mounted"))
    }
}

#[test]
fn the_list_should_follow_the_published_ladder() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.panel.list_count(fixture.harness.app()), 0);

    fixture.set_book(
        book(
            "0.01",
            &[("0.30", "10"), ("0.25", "20")],
            &[("0.36", "10"), ("0.40", "20")],
        ),
        1,
    );
    fixture.draw();

    assert_eq!(
        fixture.panel.list_count(fixture.harness.app()),
        5,
        "two asks, the spread row and two bids"
    );
    let text = fixture.harness.text_of(fixture.row(2));
    assert!(
        text.contains("Spread:"),
        "the spread row is the third row of this ladder: {text}"
    );
    let best_ask = fixture.harness.text_of(fixture.row(1));
    assert!(
        best_ask.starts_with("36¢"),
        "the best ask sits directly above the spread row: {best_ask}"
    );

    fixture.set_book(book("0.01", &[], &[("0.40", "10")]), 1);
    fixture.draw();
    assert_eq!(
        fixture.panel.list_count(fixture.harness.app()),
        2,
        "the spread row and one ask"
    );
}

#[test]
fn the_first_book_should_anchor_the_spread_row_at_the_centre() {
    let mut fixture = Fixture::new();
    fixture.set_book(deep_book(60), 1);
    assert!(
        !fixture.panel.animation.active,
        "a first book does not move: it lands"
    );
    assert_eq!(fixture.panel.animation.eased, 1.0);
    assert!(fixture.panel.anchor_pending);
    assert!(
        fixture.panel.animation.from.as_ref() == fixture.panel.animation.to.as_ref(),
        "there is no previous frame to move away from"
    );

    fixture.draw();

    assert!(
        !fixture.panel.anchor_pending,
        "the frame that drew the ladder consumed the anchor"
    );
    let asks = fixture
        .panel
        .grouped
        .as_deref()
        .expect("the book is grouped")
        .asks
        .len();
    let offset = fixture
        .panel
        .scroll_offset(fixture.harness.app())
        .expect("the list is attached");
    let expected = asks as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0 - (PAGE.1 - 20.0) / 2.0;
    assert!(
        (offset - expected).abs() < 1.0,
        "the spread row is centred in the ladder: {offset} vs {expected}"
    );
    let rows = fixture.panel.mounted_rows(fixture.harness.app());
    assert!(
        rows.iter().any(|(index, _)| *index == asks),
        "the anchored spread row is mounted"
    );
}

#[test]
fn an_empty_page_should_leave_the_anchor_pending_for_the_next_ladder() {
    let mut fixture = Fixture::new();
    let ladder = |fixture: &mut Fixture| {
        fixture.set_book(
            book(
                "0.001",
                &[("0.341", "10")],
                &[("0.342", "10"), ("0.343", "10")],
            ),
            1,
        );
    };
    ladder(&mut fixture);
    fixture.draw();
    assert!(!fixture.panel.anchor_pending, "the first frame consumed it");

    // A whole-book replace with no level at all is still the same leg and tick,
    // so it is the same ladder — only an empty one.
    fixture.set_book(book("0.001", &[], &[]), 1);
    assert!(fixture.panel.is_empty());
    assert!(
        fixture.panel.anchor_pending,
        "an empty ladder leaves the anchor pending"
    );
    fixture.draw();
    assert!(
        fixture.panel.anchor_pending,
        "the empty frame has no row to bring into view, so it does not consume the anchor"
    );
    assert_eq!(
        fixture.panel.list_count(fixture.harness.app()),
        0,
        "and it draws no row at all"
    );

    // The refill is a new ladder: it is anchored again instead of being dragged
    // as if its rows had been on screen all along.
    ladder(&mut fixture);
    assert!(fixture.panel.anchor_pending);
    fixture.draw();
    assert!(!fixture.panel.anchor_pending);
    assert!(
        fixture.panel.list_count(fixture.harness.app()) > 0,
        "the refilled ladder is drawn"
    );
}

#[test]
fn the_depth_bars_should_be_one_quad_per_visible_row_in_the_side_colour() {
    let mut fixture = Fixture::new();
    fixture.reduce_motion(true);
    fixture.set_book(
        book("0.01", &[("0.30", "10")], &[("0.36", "10"), ("0.40", "20")]),
        1,
    );
    fixture.draw();

    let canvas = fixture.canvas();
    let primitives = fixture.harness.canvas_primitives(canvas);
    assert_eq!(
        primitives.first().map(|primitive| primitive.kind),
        Some(PrimitiveKind::ClipPush),
        "the bars are clipped to the ladder area"
    );
    assert_eq!(
        primitives.last().map(|primitive| primitive.kind),
        Some(PrimitiveKind::ClipPop)
    );
    let quads = fixture.quads();
    assert_eq!(
        quads.len(),
        3,
        "one bar per visible level: two asks, one bid"
    );

    let theme = fixture.harness.services().theme.clone();
    let light = theme.theme_for(Appearance::Light);
    let ask_color = light.role(Role::MarketDownWeak);
    let bid_color = light.role(Role::MarketUpWeak);
    assert_eq!(quads[0].color, Some(ask_color), "the worst ask first");
    assert_eq!(quads[1].color, Some(ask_color), "then the best ask");
    assert_eq!(quads[2].color, Some(bid_color), "then the bid side");

    let bounds = fixture
        .harness
        .tree()
        .get(canvas)
        .expect("the canvas node")
        .layout()
        .bounds;
    let widths: Vec<f32> = quads
        .iter()
        .map(|quad| {
            let quad = quad.bounds.expect("a quad has bounds");
            assert!(
                (quad.origin.x - bounds.origin.x).abs() < 0.01,
                "the bars hang off the left edge of their row"
            );
            quad.size.width / bounds.size.width
        })
        .collect();
    assert!(
        (widths[0] - 1.0).abs() < 0.01 && (widths[1] - 1.0 / 3.0).abs() < 0.01,
        "each bar is its level's share of the side's depth: {widths:?}"
    );
}

#[test]
fn a_steady_sync_should_write_no_canvas_and_ask_for_no_frame() {
    let mut fixture = Fixture::new();
    fixture.reduce_motion(true);
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);
    // Two frames settle the list's own scrollbar geometry as well as the bars.
    fixture.draw();
    fixture.draw();
    assert!(
        !fixture.panel.animation.active,
        "reduced motion lands the retargeted movement at once"
    );

    let canvas = fixture.canvas();
    let before = fixture
        .harness
        .canvas_handle(canvas)
        .expect("the canvas node holds a canvas");
    let requests = fixture.harness.frame_requests();
    assert!(!fixture.sync(), "a steady sync writes no canvas");
    fixture.harness.drain();
    assert!(
        Arc::ptr_eq(
            &before,
            &fixture
                .harness
                .canvas_handle(canvas)
                .expect("the canvas node holds a canvas")
        ),
        "steady state short-circuit: the same canvas is kept"
    );
    assert_eq!(
        fixture.harness.frame_requests(),
        requests,
        "a steady sync asks for no frame (I3)"
    );
    // The kit list's scrollbar fades on the engine clock, so the commit cursor is
    // not the panel's idle surface: it is the frame requests (above) and the
    // untouched canvas (above) that prove the panel wrote nothing.
    assert!(!fixture.panel.timer_armed());
}

#[test]
fn a_live_update_should_ease_over_timer_ticks_and_stop_when_settled() {
    let mut fixture = Fixture::new();
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    fixture.draw();
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);
    fixture.draw();
    assert!(
        fixture.panel.animation.active,
        "a live update moves the bars"
    );
    assert!(
        fixture.panel.timer_armed(),
        "the movement schedules its next 16 ms tick"
    );

    let mut rounds = 0;
    while fixture.panel.animation.active {
        fixture.harness.advance(TICK);
        fixture.sync();
        fixture.harness.drain();
        rounds += 1;
        assert!(rounds < 200, "the movement has to settle");
    }
    assert_eq!(fixture.panel.animation.eased, 1.0);
    assert!(
        !fixture.panel.timer_armed(),
        "a settled movement stops the timer chain"
    );
    let requests = fixture.harness.frame_requests();
    assert!(
        !fixture.harness.advance(TICK),
        "and a settled panel schedules nothing more"
    );
    assert_eq!(fixture.harness.frame_requests(), requests);
}

#[test]
fn reduce_motion_should_land_the_easing_and_stop_the_timer() {
    let mut fixture = Fixture::new();
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "10")]), 1);
    fixture.draw();
    fixture.set_book(book("0.01", &[], &[("0.40", "10"), ("0.50", "30")]), 1);
    fixture.draw();
    assert!(fixture.panel.animation.active);
    assert!(fixture.panel.timer_armed());

    fixture.reduce_motion(true);
    fixture.draw();

    assert_eq!(fixture.panel.animation.eased, 1.0);
    assert!(!fixture.panel.animation.active);
    assert!(
        !fixture.panel.timer_armed(),
        "the timer chain is cancelled when motion is reduced"
    );
    let requests = fixture.harness.frame_requests();
    fixture.harness.advance(TICK);
    assert_eq!(
        fixture.harness.frame_requests(),
        requests,
        "a settled movement asks for no further frames"
    );
}

#[test]
fn the_depth_bars_should_follow_the_lists_scroll_offset() {
    let mut fixture = Fixture::new();
    fixture.reduce_motion(true);
    fixture.set_book(deep_book(60), 1);
    fixture.draw();

    let canvas = fixture.canvas();
    let bounds = fixture
        .harness
        .tree()
        .get(canvas)
        .expect("the canvas node")
        .layout()
        .bounds;
    let offset = fixture
        .panel
        .scroll_offset(fixture.harness.app())
        .expect("the list is attached");
    assert!(offset > 0.0, "the deep ladder is anchored and scrolled");

    let quads = fixture.quads();
    let first = (offset / ROW_HEIGHT).floor() as usize;
    let expected_top = bounds.origin.y + first as f32 * ROW_HEIGHT - offset;
    let first_quad = quads[0].bounds.expect("a quad has bounds");
    assert!(
        (first_quad.origin.y - expected_top).abs() < 0.01,
        "the first bar is the first row of the scrolled window: {} vs {expected_top}",
        first_quad.origin.y
    );
    let visible_rows = ((bounds.size.height / ROW_HEIGHT).ceil() as usize) + 1;
    assert!(
        quads.len() + 1 >= visible_rows.min(121 - first),
        "the canvas carries the bars of the visible window, not of the whole ladder: {}",
        quads.len()
    );

    // The offset is part of what the canvas was drawn from: the list moving without a
    // new book is still a frame the depth bars owe, and the bars on screen have to be
    // laid out from the offset the list is at now.
    fixture
        .panel
        .scroll_to_item_for_test(fixture.harness.app_mut(), 20, ItemAlign::Start)
        .expect("the list is scrolled to the twentieth row");
    let scrolled = fixture
        .panel
        .scroll_offset(fixture.harness.app())
        .expect("the list is attached");
    assert!(
        (scrolled - offset).abs() > 1.0,
        "the programmatic jump moved the ladder: {scrolled} vs {offset}"
    );
    assert!(
        fixture.draw(),
        "a scrolled ladder is a drawing for the bars, not a steady frame"
    );
    let first = (scrolled / ROW_HEIGHT).floor() as usize;
    let expected_top = bounds.origin.y + first as f32 * ROW_HEIGHT - scrolled;
    let first_quad = fixture.quads()[0].bounds.expect("a quad has bounds");
    assert!(
        (first_quad.origin.y - expected_top).abs() < 0.01,
        "the bars follow the offset the list is scrolled to: {} vs {expected_top}",
        first_quad.origin.y
    );
}

#[test]
fn an_empty_page_should_say_so_and_step_aside_for_a_ladder() {
    let mut fixture = Fixture::new();
    fixture.draw();
    let overlay = fixture.node(EMPTY_SELECTOR);
    assert!(
        fixture.harness.visible(overlay),
        "a page with no book says it has no data"
    );
    assert!(
        fixture.harness.text_of(overlay).contains("No data"),
        "the empty state shows the catalogue's sentence"
    );

    fixture.set_book(book("0.01", &[], &[("0.40", "10")]), 1);
    fixture.draw();
    assert!(
        !fixture.harness.visible(overlay),
        "a ladder steps the empty state aside"
    );

    fixture.panel.clear();
    fixture.draw();
    assert!(
        fixture.harness.visible(overlay),
        "and clearing the panel brings it back"
    );
    assert_eq!(fixture.panel.list_count(fixture.harness.app()), 0);
}

#[test]
fn the_header_should_follow_the_language() {
    let mut fixture = Fixture::new();
    let header = fixture.node(HEADER_SELECTOR);
    assert_eq!(
        fixture.harness.text_of(header),
        "PRICE\nSHARES\nTOTAL",
        "the three column labels are drawn from the catalogue"
    );

    fixture.harness.switch_language(LanguageCode::ZhCn);
    fixture.harness.drain();
    assert_eq!(
        fixture.harness.text_of(header),
        "价格\n份额\n总额",
        "a language switch re-evaluates the bound labels"
    );
}

#[test]
fn the_spread_row_should_show_the_leg_the_last_trade_and_the_spread() {
    let mut fixture = Fixture::new();
    fixture
        .panel
        .set_leg_label(Some(SmolStr::new_static("Yes")));
    let mut book = order_book(
        leg(0),
        "0.01",
        levels(&[("0.30", "10")]),
        levels(&[("0.36", "10")]),
    );
    book.last_trade_price = Some("0.35".parse().expect("the fixture price is a fraction"));
    fixture.set_book(Arc::new(book), 1);
    fixture.draw();

    let asks = fixture
        .panel
        .grouped
        .as_deref()
        .expect("the book is grouped")
        .asks
        .len();
    let text = fixture.harness.text_of(fixture.row(asks));
    assert_eq!(
        text, "Yes\nLast: 35¢\nSpread: 6¢",
        "the spread row names the side, the last trade and the spread"
    );
}

#[test]
fn releasing_the_panel_should_return_its_value_slots() {
    let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
    let baseline = harness.signal_slot_stats();
    let mut panel = PredictionBookPanel::new();
    let root = harness.root();
    harness
        .with_cx(|cx| panel.build(cx, root))
        .expect("the panel builds");
    assert!(
        harness.signal_slot_stats().live > baseline.live,
        "a built panel mints values of its own"
    );
    // A ladder with rows and an armed depth tick is the state the pane releases
    // after a frame: the list's row contents must go with the list's scope.
    panel.set_book(
        book("0.01", &[("0.30", "10")], &[("0.36", "10"), ("0.40", "10")]),
        1,
    );
    panel.set_book(
        book("0.01", &[("0.30", "10")], &[("0.36", "10"), ("0.40", "30")]),
        1,
    );
    let harness = &mut harness;
    let panel = &mut panel;
    harness
        .with_cx(|cx| panel.sync(cx))
        .expect("the panel syncs");
    assert!(panel.timer_armed(), "the retargeted movement is ticking");
    harness.drain();

    harness.with_cx(|cx| panel.release(cx));
    assert_eq!(
        harness.signal_slot_stats().live,
        baseline.live,
        "release hands every value slot back: the list's scope and the panel's own"
    );
    let requests = harness.frame_requests();
    harness.advance(TICK);
    assert_eq!(
        harness.frame_requests(),
        requests,
        "release cancels the depth tick with the resting timer"
    );
}
