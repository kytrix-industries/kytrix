//! The canvas that draws one side of the book.
//!
//! A ladder is a few dozen rows of dense, fast-moving text, and every frame that
//! follows a depth bar is a frame that would otherwise re-shape a hundred lines.
//! So a frame is prepared once and painted many times:
//!
//! - **prepaint** resolves the row boxes and shapes the three labels of every
//!   visible level. The result is kept in the widget's caches and is only rebuilt
//!   when the levels, the box they are drawn into, or the colours and text style
//!   change — none of which an easing frame touches.
//! - **paint** turns the prepared levels into quads and shaped lines, and is the
//!   only pass that reads the animation progress.
//!
//! Both sides are drawn by the same code: an ask ladder grows upwards from the
//! mid price and a bid ladder downwards from it, which is the only thing that
//! differs between them.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use ahash::HashMap;
use gpui_kit::{
    App, Bounds, ContentMask, Hsla, IntoElement, Pixels, ShapedLine, SharedString, Styled,
    TextAlign, TextStyle, Window, WindowTextSystem, canvas, fill, point, px, size,
};

use super::model::{GroupedLevel, WINDOW_SIZES, interpolated_ratio};

/// Row geometry. A row is the level's own height plus the gap that separates it
/// from the next, and the text sits slightly above the row's centre.
pub(super) const LEVEL_FONT_SIZE: Pixels = px(12.0);
pub(super) const LEVEL_HEIGHT: Pixels = px(16.0);
pub(super) const LEVEL_TEXT_TOP_PADDING: Pixels = px(2.0);
pub(super) const LEVEL_GAP: Pixels = px(1.0);
/// The heights of the book's fixed rows, which the two ladders share the pane
/// with: the column labels, the mid price, and the imbalance bar.
pub(super) const HEADER_HEIGHT: Pixels = px(24.0);
pub(super) const MID_PRICE_HEIGHT: Pixels = px(48.0);
pub(super) const IMBALANCE_HEIGHT: Pixels = px(24.0);
/// The inset every label keeps from the edges of its column, so that two columns
/// never touch even when both are as wide as they are allowed to be.
const LEVEL_TEXT_INSET: f32 = 8.0;
/// How fast a depth bar closes on its new ratio, in e-folds per second.
pub(super) const DEPTH_ANIMATION_RESPONSE_PER_SECOND: f32 = 12.0;
/// The movement a bar may still have left and be considered settled: easing the
/// rest of the way costs frames that no one can see.
pub(super) const DEPTH_ANIMATION_SETTLE_EPSILON: f32 = 0.001;

/// One level ready to paint: the shaped labels plus the ratios the bar eases
/// between.
#[derive(Clone)]
struct PreparedLevel {
    previous_ratio: f32,
    current_ratio: f32,
    price_text: SharedString,
    quantity_text: SharedString,
    cumulative_quantity_text: SharedString,
    price: ShapedLine,
    quantity: ShapedLine,
    cumulative_quantity: ShapedLine,
}

/// Everything a frame of one side was prepared from.
///
/// The levels are identified by their shared allocation rather than by their
/// contents: the widget only ever replaces a grouped side wholesale, so two
/// pointers compare equal exactly when there is nothing to re-shape.
struct PreparedKey {
    levels: Arc<[GroupedLevel]>,
    bounds: Bounds<Pixels>,
    price_color: Hsla,
    quantity_color: Hsla,
    text_style: TextStyle,
}

impl PreparedKey {
    fn matches(
        &self,
        levels: &Arc<[GroupedLevel]>,
        bounds: Bounds<Pixels>,
        price_color: Hsla,
        quantity_color: Hsla,
        text_style: &TextStyle,
    ) -> bool {
        Arc::ptr_eq(&self.levels, levels)
            && self.bounds == bounds
            && self.price_color == price_color
            && self.quantity_color == quantity_color
            && self.text_style == *text_style
    }
}

/// One prepared side of the book.
struct PreparedSide {
    key: PreparedKey,
    levels: Arc<[PreparedLevel]>,
}

/// The prepared frames of one book, plus the row count the pane currently has
/// room for.
///
/// They live in the widget rather than in the element because an element is
/// rebuilt every frame, and the whole point of preparing a frame is that the next
/// one can reuse it.
pub(super) struct OrderBookPaintCaches {
    asks: RefCell<Option<Arc<PreparedSide>>>,
    bids: RefCell<Option<Arc<PreparedSide>>>,
    /// The rows one side has room for. The canvases fill it in from the box they
    /// are given, and the widget reads it back to decide how much of the ladder is
    /// worth grouping.
    drawable_rows: Cell<usize>,
}

impl OrderBookPaintCaches {
    pub(super) fn new() -> Self {
        Self {
            asks: RefCell::new(None),
            bids: RefCell::new(None),
            drawable_rows: Cell::new(WINDOW_SIZES[WINDOW_SIZES.len() - 1]),
        }
    }

    /// The rows one side currently has room for, which is what limits how many
    /// levels are worth keeping in a group.
    pub(super) fn drawable_rows(&self) -> usize {
        self.drawable_rows.get()
    }

    /// Records the rows one side has room for, reporting whether that changed.
    ///
    /// Both canvases report it, because either of them may be the first to be laid
    /// out; only the one that saw a different number asks for the regroup.
    pub(super) fn set_drawable_rows(&self, rows: usize) -> bool {
        self.drawable_rows.replace(rows) != rows
    }

    /// Drops every prepared frame, for a book whose levels no longer exist.
    pub(super) fn clear(&self) {
        *self.asks.borrow_mut() = None;
        *self.bids.borrow_mut() = None;
    }
}

/// One side of the book, as the body draws it.
pub(super) struct SideCanvas {
    pub levels: Arc<[GroupedLevel]>,
    /// The colour of this side's prices.
    pub price_color: Hsla,
    /// The colour of this side's quantities, which are not directional.
    pub quantity_color: Hsla,
    /// The colour of this side's depth bars.
    pub depth_color: Hsla,
    /// How far the depth bars have eased towards their new ratios.
    pub animation_progress: f32,
    /// Whether this side grows upwards from the mid price.
    pub asks: bool,
}

impl SideCanvas {
    /// Builds the canvas of one side.
    ///
    /// The rows that fit are a function of the box the canvas ends up with, and
    /// only the layout pass knows it, so a box that changed is reported to
    /// whoever owns the grouped book: it is what decides how many levels are
    /// worth grouping next.
    pub(super) fn render(
        self,
        caches: Rc<OrderBookPaintCaches>,
        on_layout_change: impl FnOnce(&mut App) + 'static,
    ) -> impl IntoElement {
        let Self {
            levels,
            price_color,
            quantity_color,
            depth_color,
            animation_progress,
            asks,
        } = self;
        let prepaint_levels = levels;
        let prepaint_caches = caches;

        canvas(
            move |bounds, window, cx| {
                if prepaint_caches.set_drawable_rows(drawable_window_size(bounds)) {
                    on_layout_change(cx);
                }
                prepare(
                    &prepaint_caches,
                    &prepaint_levels,
                    bounds,
                    price_color,
                    quantity_color,
                    asks,
                    window,
                )
            },
            move |bounds, prepared, window, app| {
                paint_side(
                    prepared.as_ref(),
                    bounds,
                    depth_color,
                    asks,
                    animation_progress,
                    window,
                    app,
                );
            },
        )
        .size_full()
    }
}

/// Returns the frame of one side, re-shaping it only when its inputs changed.
fn prepare(
    caches: &OrderBookPaintCaches,
    levels: &Arc<[GroupedLevel]>,
    bounds: Bounds<Pixels>,
    price_color: Hsla,
    quantity_color: Hsla,
    asks: bool,
    window: &mut Window,
) -> Arc<PreparedSide> {
    let text_style = window.text_style();
    let cache = if asks { &caches.asks } else { &caches.bids };
    let mut cache = cache.borrow_mut();
    if let Some(prepared) = cache.as_ref()
        && prepared
            .key
            .matches(levels, bounds, price_color, quantity_color, &text_style)
    {
        return prepared.clone();
    }

    let prepared = Arc::new(PreparedSide {
        key: PreparedKey {
            levels: levels.clone(),
            bounds,
            price_color,
            quantity_color,
            text_style: text_style.clone(),
        },
        levels: shape_levels(
            levels,
            price_color,
            quantity_color,
            &text_style,
            window.text_system(),
            cache.as_deref(),
        ),
    });
    *cache = Some(prepared.clone());
    prepared
}

/// Shapes the three labels of every visible level.
///
/// A label that was already shaped for the same text is handed over from the
/// frame before: a book that moved only at the top of the ladder shifts every row
/// below it, and re-shaping them would be the most expensive part of the frame.
fn shape_levels(
    levels: &[GroupedLevel],
    price_color: Hsla,
    quantity_color: Hsla,
    text_style: &TextStyle,
    text_system: &WindowTextSystem,
    previous: Option<&PreparedSide>,
) -> Arc<[PreparedLevel]> {
    let previous_levels = previous.map(|prepared| prepared.levels.as_ref());
    let previous_lines = PreviousShapedLines::new(previous_levels);
    let mut shaped = Vec::with_capacity(levels.len());
    for (index, level) in levels.iter().enumerate() {
        let previous_at_index = previous_levels.and_then(|levels| levels.get(index));
        let price = previous_at_index
            .filter(|previous| previous.price_text == level.price_text)
            .map(|previous| previous.price.clone())
            .or_else(|| previous_lines.price(&level.price_text))
            .unwrap_or_else(|| {
                shape_level_text(&level.price_text, price_color, text_style, text_system)
            });
        let quantity = previous_at_index
            .filter(|previous| previous.quantity_text == level.quantity_text)
            .map(|previous| previous.quantity.clone())
            .or_else(|| previous_lines.quantity(&level.quantity_text))
            .unwrap_or_else(|| {
                shape_level_text(
                    &level.quantity_text,
                    quantity_color,
                    text_style,
                    text_system,
                )
            });
        let cumulative_quantity = previous_at_index
            .filter(|previous| previous.cumulative_quantity_text == level.cumulative_quantity_text)
            .map(|previous| previous.cumulative_quantity.clone())
            .or_else(|| previous_lines.quantity(&level.cumulative_quantity_text))
            .unwrap_or_else(|| {
                shape_level_text(
                    &level.cumulative_quantity_text,
                    quantity_color,
                    text_style,
                    text_system,
                )
            });
        shaped.push(PreparedLevel {
            previous_ratio: level.previous_ratio,
            current_ratio: level.current_ratio,
            price_text: level.price_text.clone(),
            quantity_text: level.quantity_text.clone(),
            cumulative_quantity_text: level.cumulative_quantity_text.clone(),
            price,
            quantity,
            cumulative_quantity,
        });
    }
    shaped.into()
}

/// The shaped lines of the previous frame, looked up by the text they show.
///
/// Both quantity columns share one lookup because they are drawn in the same
/// colour: a cumulative quantity that moves into the quantity column is the same
/// line, and neither column re-shapes it.
struct PreviousShapedLines<'a> {
    price: HashMap<&'a str, &'a ShapedLine>,
    quantity: HashMap<&'a str, &'a ShapedLine>,
}

impl<'a> PreviousShapedLines<'a> {
    fn new(previous: Option<&'a [PreparedLevel]>) -> Self {
        let mut price = HashMap::default();
        let mut quantity = HashMap::default();
        let previous_len = previous.map_or(0, <[PreparedLevel]>::len);
        price.reserve(previous_len);
        quantity.reserve(previous_len.saturating_mul(2));
        if let Some(previous) = previous {
            for level in previous {
                price.insert(level.price_text.as_ref(), &level.price);
                quantity.insert(level.quantity_text.as_ref(), &level.quantity);
                quantity.insert(
                    level.cumulative_quantity_text.as_ref(),
                    &level.cumulative_quantity,
                );
            }
        }
        Self { price, quantity }
    }

    fn price(&self, text: &SharedString) -> Option<ShapedLine> {
        self.price.get(text.as_ref()).map(|line| (*line).clone())
    }

    fn quantity(&self, text: &SharedString) -> Option<ShapedLine> {
        self.quantity.get(text.as_ref()).map(|line| (*line).clone())
    }
}

fn shape_level_text(
    text: &SharedString,
    color: Hsla,
    text_style: &TextStyle,
    text_system: &WindowTextSystem,
) -> ShapedLine {
    let mut run = text_style.to_run(text.len());
    run.color = color;
    text_system.shape_line(text.clone(), LEVEL_FONT_SIZE, &[run], None)
}

/// Paints one side: first every depth bar, then every label.
///
/// The bars share one pass so that no bar is painted over another's text, and
/// both passes are masked to the canvas so a bar that grows past the pane's edge
/// cannot bleed into the mid price or the imbalance bar.
fn paint_side(
    prepared: &PreparedSide,
    bounds: Bounds<Pixels>,
    depth_color: Hsla,
    asks: bool,
    animation_progress: f32,
    window: &mut Window,
    app: &mut App,
) {
    if prepared.levels.is_empty() {
        return;
    }

    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for (index, level) in prepared.levels.iter().enumerate() {
            let row_bounds = level_row_bounds(bounds, index, asks);
            let ratio = interpolated_ratio(
                level.previous_ratio,
                level.current_ratio,
                animation_progress,
            );
            window.paint_quad(fill(depth_bar_bounds(row_bounds, ratio), depth_color));
        }

        for (index, level) in prepared.levels.iter().enumerate() {
            paint_level_text(level_row_bounds(bounds, index, asks), level, window, app);
        }
    });
}

/// Paints the three labels of one level, each inside its own third of the row.
fn paint_level_text(
    row_bounds: Bounds<Pixels>,
    level: &PreparedLevel,
    window: &mut Window,
    app: &mut App,
) {
    let column_width = row_bounds.size.width / COLUMN_COUNT;
    paint_level_cell(row_bounds, 0, column_width, &level.price, window, app);
    paint_level_cell(row_bounds, 1, column_width, &level.quantity, window, app);
    paint_level_cell(
        row_bounds,
        2,
        column_width,
        &level.cumulative_quantity,
        window,
        app,
    );
}

/// The columns every level is drawn in: price, quantity, cumulative quantity.
const COLUMN_COUNT: f32 = 3.0;

fn paint_level_cell(
    row_bounds: Bounds<Pixels>,
    column: usize,
    column_width: Pixels,
    line: &ShapedLine,
    window: &mut Window,
    app: &mut App,
) {
    let cell_bounds = level_cell_bounds(row_bounds, column, column_width, px(LEVEL_TEXT_INSET));
    window.with_content_mask(
        Some(ContentMask {
            bounds: cell_bounds,
        }),
        |window| {
            let _ = line.paint(
                point(
                    cell_bounds.origin.x,
                    cell_bounds.origin.y + LEVEL_TEXT_TOP_PADDING,
                ),
                LEVEL_FONT_SIZE,
                TextAlign::Right,
                Some(cell_bounds.size.width),
                window,
                app,
            );
        },
    );
}

/// The box of one row of `index`, counting away from the mid price.
pub(super) fn level_row_bounds(
    side_bounds: Bounds<Pixels>,
    index: usize,
    asks: bool,
) -> Bounds<Pixels> {
    let offset = index * (LEVEL_HEIGHT + LEVEL_GAP);
    let row_y = if asks {
        side_bounds.bottom() - LEVEL_HEIGHT - offset
    } else {
        side_bounds.top() + offset
    };
    Bounds::new(
        point(side_bounds.left(), row_y),
        size(side_bounds.size.width, LEVEL_HEIGHT),
    )
}

/// The box of one depth bar: as wide as its share of the visible quantity, and
/// anchored to the right edge of the row so that every bar starts at the same
/// place.
pub(super) fn depth_bar_bounds(row_bounds: Bounds<Pixels>, ratio: f32) -> Bounds<Pixels> {
    let bar_width = row_bounds.size.width * ratio;
    Bounds::new(
        point(row_bounds.right() - bar_width, row_bounds.top()),
        size(bar_width, row_bounds.size.height),
    )
}

/// The box one label is painted into, inset from its column's edges.
pub(super) fn level_cell_bounds(
    row_bounds: Bounds<Pixels>,
    column: usize,
    column_width: Pixels,
    horizontal_padding: Pixels,
) -> Bounds<Pixels> {
    debug_assert!(column < COLUMN_COUNT as usize);
    Bounds::new(
        point(
            row_bounds.left() + column * column_width + horizontal_padding,
            row_bounds.top(),
        ),
        size(
            (column_width - 2.0 * horizontal_padding).max(px(0.0)),
            row_bounds.size.height,
        ),
    )
}

/// How many rows one side of the given box has room for.
///
/// The count is capped by the widest window the user may pick: a taller pane
/// shows empty space below the ladder rather than more levels than the model was
/// asked to keep.
pub(super) fn drawable_window_size(side_bounds: Bounds<Pixels>) -> usize {
    let row_stride = f32::from(LEVEL_HEIGHT + LEVEL_GAP);
    let rows = ((f32::from(side_bounds.size.height) + f32::from(LEVEL_GAP)) / row_stride).floor();
    (rows.max(1.0) as usize).min(WINDOW_SIZES[WINDOW_SIZES.len() - 1])
}

/// How far a depth-bar easing has come after `elapsed`, given where it started.
///
/// The approach is exponential rather than linear, so a bar that is interrupted
/// by another update keeps its speed instead of restarting, and the movement is
/// computed from the progress rather than from a frame count, which makes it
/// independent of how the frames happen to be spaced.
pub(super) fn depth_animation_progress_after(
    progress: f32,
    transition_span: f32,
    elapsed: Duration,
) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    let remaining_progress =
        (1.0 - progress) * (-DEPTH_ANIMATION_RESPONSE_PER_SECOND * elapsed.as_secs_f32()).exp();
    if transition_span * remaining_progress <= DEPTH_ANIMATION_SETTLE_EPSILON {
        1.0
    } else {
        1.0 - remaining_progress
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Instant};

    use market_participants_model::decimal::{Price, Quantity};

    use super::*;
    use crate::model::GroupedLevel;

    fn grouped_level(price_text: &'static str, ratio: f32) -> GroupedLevel {
        GroupedLevel {
            price: Price::from_str("100").expect("price should parse"),
            quantity: Quantity::from_str("1").expect("quantity should parse"),
            cumulative_quantity: Quantity::from_str("1").expect("quantity should parse"),
            price_text: SharedString::new_static(price_text),
            quantity_text: SharedString::new_static("1"),
            cumulative_quantity_text: SharedString::new_static("1"),
            previous_ratio: ratio,
            current_ratio: ratio,
        }
    }

    #[test]
    fn drawable_window_should_match_the_row_geometry() {
        let side_height = 25 * LEVEL_HEIGHT + 24 * LEVEL_GAP;
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(0.0), px(0.0)),
            gpui_kit::size(px(640.0), side_height),
        );

        assert_eq!(drawable_window_size(bounds), 25);
    }

    #[test]
    fn drawable_window_should_never_exceed_the_widest_window() {
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(0.0), px(0.0)),
            gpui_kit::size(px(640.0), px(4_000.0)),
        );

        assert_eq!(
            drawable_window_size(bounds),
            WINDOW_SIZES[WINDOW_SIZES.len() - 1]
        );
    }

    #[test]
    fn drawable_window_should_keep_one_row_in_a_collapsed_pane() {
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(0.0), px(0.0)),
            gpui_kit::size(px(640.0), px(0.0)),
        );

        assert_eq!(drawable_window_size(bounds), 1);
    }

    #[test]
    fn level_rows_should_grow_away_from_the_midpoint() {
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(10.0), px(20.0)),
            size(px(300.0), px(100.0)),
        );
        let best_ask = level_row_bounds(bounds, 0, true);
        let next_ask = level_row_bounds(bounds, 1, true);
        let best_bid = level_row_bounds(bounds, 0, false);
        let next_bid = level_row_bounds(bounds, 1, false);

        assert_eq!(best_ask.bottom(), bounds.bottom());
        assert!(next_ask.top() < best_ask.top());
        assert_eq!(best_bid.top(), bounds.top());
        assert!(next_bid.top() > best_bid.top());
    }

    #[test]
    fn depth_bars_should_not_overlap_within_one_side() {
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(10.0), px(20.0)),
            size(px(300.0), px(500.0)),
        );
        let bars = [true, false].into_iter().flat_map(|asks| {
            (0..25).map(move |index| depth_bar_bounds(level_row_bounds(bounds, index, asks), 0.75))
        });
        let bars = bars.collect::<Vec<_>>();

        assert!(bars.as_chunks::<25>().0.iter().all(|side| {
            side.windows(2)
                .all(|pair| pair[0].bottom() <= pair[1].top() || pair[1].bottom() <= pair[0].top())
        }));
    }

    #[test]
    fn depth_bars_should_be_anchored_to_the_right_edge() {
        let row = gpui_kit::bounds(
            gpui_kit::point(px(10.0), px(20.0)),
            size(px(300.0), LEVEL_HEIGHT),
        );

        assert_eq!(depth_bar_bounds(row, 0.0).size.width, px(0.0));
        assert_eq!(depth_bar_bounds(row, 1.0), row);
        assert_eq!(depth_bar_bounds(row, 0.5).right(), row.right());
    }

    #[test]
    fn level_cell_masks_should_stay_inside_their_columns() {
        let row = gpui_kit::bounds(
            gpui_kit::point(px(10.0), px(20.0)),
            size(px(300.0), LEVEL_HEIGHT),
        );
        let column_width = row.size.width / COLUMN_COUNT;
        let cells = (0..COLUMN_COUNT as usize)
            .map(|column| level_cell_bounds(row, column, column_width, px(8.0)))
            .collect::<Vec<_>>();

        assert!(cells.iter().all(|cell| {
            cell.left() >= row.left()
                && cell.right() <= row.right()
                && cell.top() == row.top()
                && cell.bottom() == row.bottom()
        }));
        assert!(
            cells
                .windows(2)
                .all(|pair| pair[0].right() < pair[1].left())
        );
    }

    #[test]
    fn depth_animation_should_advance_before_the_old_thirty_fps_interval() {
        let started_at = std::time::Instant::now();
        let mut animation = TestAnimation::new(1.0);

        assert_eq!(animation.advance(started_at), 0.0);
        let progress = animation.advance(started_at + std::time::Duration::from_millis(1));

        assert!(progress > 0.0);
        assert!(progress < 1.0);
    }

    #[test]
    fn depth_animation_should_not_be_forced_complete_at_150_ms() {
        let started_at = std::time::Instant::now();
        let mut animation = TestAnimation::new(1.0);

        animation.advance(started_at);
        let progress = animation.advance(started_at + std::time::Duration::from_millis(150));

        assert!(progress < 1.0);
    }

    #[test]
    fn depth_animation_should_be_independent_of_frame_partitioning() {
        let started_at = std::time::Instant::now();
        let mut single_frame = TestAnimation::new(1.0);
        let mut split_frames = TestAnimation::new(1.0);

        single_frame.advance(started_at);
        let single_progress =
            single_frame.advance(started_at + std::time::Duration::from_millis(33));
        split_frames.advance(started_at);
        split_frames.advance(started_at + std::time::Duration::from_millis(16));
        let split_progress =
            split_frames.advance(started_at + std::time::Duration::from_millis(33));

        assert!((single_progress - split_progress).abs() < 1e-6);
    }

    #[test]
    fn depth_animation_should_settle_after_visual_convergence() {
        let started_at = std::time::Instant::now();
        let mut animation = TestAnimation::new(1.0);

        animation.advance(started_at);
        let progress = animation.advance(started_at + std::time::Duration::from_secs(1));

        assert_eq!(progress, 1.0);
    }

    /// Drives the easing rule the same way the widget does, so the rule is tested
    /// without a window.
    struct TestAnimation {
        progress: f32,
        span: f32,
        last_frame: Option<Instant>,
    }

    impl TestAnimation {
        fn new(span: f32) -> Self {
            Self {
                progress: 0.0,
                span,
                last_frame: None,
            }
        }

        fn advance(&mut self, now: Instant) -> f32 {
            if let Some(previous) = self.last_frame.replace(now) {
                self.progress = depth_animation_progress_after(
                    self.progress,
                    self.span,
                    now.saturating_duration_since(previous),
                );
            }
            self.progress
        }
    }

    #[test]
    fn previous_shaped_lines_should_reuse_text_after_a_row_shift() {
        fn prepared_level(text: &'static str, cumulative: &'static str) -> PreparedLevel {
            PreparedLevel {
                previous_ratio: 0.25,
                current_ratio: 0.75,
                price_text: gpui_kit::SharedString::new_static("100"),
                quantity_text: gpui_kit::SharedString::new_static(text),
                cumulative_quantity_text: gpui_kit::SharedString::new_static(cumulative),
                price: ShapedLine::default(),
                quantity: ShapedLine::default(),
                cumulative_quantity: ShapedLine::default(),
            }
        }

        let previous = [
            prepared_level("2", "total-2"),
            prepared_level("3", "total-3"),
        ];
        let lines = PreviousShapedLines::new(Some(&previous));
        let moved_quantity = lines
            .quantity(&gpui_kit::SharedString::new_static("3"))
            .expect("moved quantity should be reusable");
        let moved_cumulative = lines
            .quantity(&gpui_kit::SharedString::new_static("total-3"))
            .expect("cumulative quantity should share the lookup");

        assert!(std::ptr::eq::<gpui_kit::LineLayout>(
            &**moved_quantity,
            &**previous[1].quantity
        ));
        assert!(std::ptr::eq::<gpui_kit::LineLayout>(
            &**moved_cumulative,
            &**previous[1].cumulative_quantity
        ));
    }

    #[test]
    fn a_prepared_side_should_be_reused_for_a_frame_with_the_same_inputs() {
        let levels: Arc<[GroupedLevel]> = Arc::from([grouped_level("100", 0.5)]);
        let bounds = gpui_kit::bounds(
            gpui_kit::point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
        );
        let color = gpui_kit::hsla(0.0, 0.0, 0.0, 1.0);
        let key = PreparedKey {
            levels: levels.clone(),
            bounds,
            price_color: color,
            quantity_color: color,
            text_style: TextStyle::default(),
        };

        assert!(key.matches(&levels, bounds, color, color, &TextStyle::default()));
        assert!(!key.matches(
            &Arc::from([grouped_level("101", 0.5)]),
            bounds,
            color,
            color,
            &TextStyle::default()
        ));
        assert!(!key.matches(
            &levels,
            gpui_kit::bounds(
                gpui_kit::point(px(0.0), px(0.0)),
                size(px(301.0), px(100.0)),
            ),
            color,
            color,
            &TextStyle::default()
        ));
    }
}
