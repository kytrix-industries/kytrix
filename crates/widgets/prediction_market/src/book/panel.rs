//! The prediction order book panel: one ladder in one scroll region.
//!
//! A prediction book is read as one continuous ladder: the ask side above the
//! spread and the bid side below it, best prices against the spread. The panel
//! owns that reading, the scroll position a reader leaves it at, and the easing of
//! the depth bars behind the rows.
//!
//! The whole ladder is one [`uniform_list`], so a book with thousands of levels
//! still draws only the rows the pane has room for. Column labels are outside the
//! scroll region: they belong to the panel, not to the levels that move under
//! them.
//!
//! A grouping is rebuilt once per accepted book (in [`PredictionBookPanel::set_book`]),
//! never per frame. The depth bars are the only thing that moves between two
//! accepted books, and they are driven from the elapsed time rather than from a
//! frame count, so an update that lands mid-flight retargets the movement instead
//! of restarting it.

use std::{rc::Rc, sync::Arc, time::Instant};

use ahash::AHashMap;
use base_ui::{empty_state::EmptyState, scroll_bar::Scrollbar, style::theme::ThemeExt};
use gpui_kit::{
    AnyElement, App, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels,
    ScrollStrategy, SharedString, Styled, UniformListScrollHandle, base::ScrollbarMode, div, point,
    px, relative, uniform_list,
};
use i18n::t;
use market_participants_model::{
    decimal::{Fraction, Tick},
    prediction::PredictionOrderBook,
};
use smol_str::SmolStr;

use super::model::{
    GroupedBook, GroupedLevel, clamp_ratio, format_notional, format_price_cents, format_shares,
    format_spread_cents, group, settle_grouping,
};

/// The height of one ladder row.
///
/// Every row of the list is exactly this tall: [`uniform_list`] measures the first
/// one and lays the rest out at that height.
const ROW_HEIGHT: Pixels = px(22.0);

/// The height of the fixed column header above the ladder.
const HEADER_HEIGHT: Pixels = px(20.0);

/// The size of one row's text.
const ROW_TEXT_SIZE: Pixels = px(12.0);

/// The size of the column labels.
const HEADER_TEXT_SIZE: Pixels = px(11.0);

/// The room the scrollbar overlays on the right of every row, so no number is
/// drawn underneath it.
const SCROLLBAR_INSET: Pixels = px(10.0);

/// How fast a depth bar closes on its new ratio, in e-folds per second.
const DEPTH_ANIMATION_RESPONSE_PER_SECOND: f32 = 12.0;

/// The movement a bar may still have left and be considered settled: easing the
/// rest of the way costs frames no one can see.
const DEPTH_ANIMATION_SETTLE_EPSILON: f32 = 0.001;

/// The price a depth key is indexed by: the level's own fraction of one share.
type DepthKey = (bool, u128);

/// The depth bars between two books.
///
/// Each level is indexed by its side and its price, so a row is found no matter
/// which side of the ladder it is on, and a level that appears in a new book
/// starts from zero.
pub(crate) struct DepthAnimation {
    /// The ratio each level was drawn at when the movement started.
    pub from: Rc<AHashMap<DepthKey, f32>>,
    /// The ratio each level is moving to, which is the depth of the newest book.
    pub to: Rc<AHashMap<DepthKey, f32>>,
    /// The largest distance any bar still has to cover.
    pub span: f32,
    /// When the movement started.
    pub started: Instant,
    /// Whether a movement is still running.
    pub active: bool,
    /// How far every bar has come, in `0..=1`.
    pub eased: f32,
}

impl DepthAnimation {
    /// A panel whose bars are all where the last book put them.
    fn settled() -> Self {
        Self {
            from: Rc::new(AHashMap::new()),
            to: Rc::new(AHashMap::new()),
            span: 0.0,
            started: Instant::now(),
            active: false,
            eased: 1.0,
        }
    }

    /// Puts every bar where the given book asks for it, without moving.
    ///
    /// A first book, another side, another tick and another grouping are not
    /// movements of the ladder a reader was reading: they are a different ladder,
    /// so its bars arrive at once.
    fn jump(&mut self, grouped: &GroupedBook) {
        let depths = Rc::new(depth_map(grouped));
        self.from = depths.clone();
        self.to = depths;
        self.span = 0.0;
        self.started = Instant::now();
        self.active = false;
        self.eased = 1.0;
    }

    /// Sends the bars after the ratios the newest book asks for.
    ///
    /// The movement starts from what the previous frame was actually painting, so
    /// an update that arrives mid-flight retargets it instead of resetting it. A
    /// movement with nothing visible left in it never starts.
    fn retarget(&mut self, grouped: &GroupedBook, now: Instant) {
        let to = depth_map(grouped);
        let mut from = AHashMap::with_capacity(to.len());
        let mut span = 0.0_f32;
        for (key, target) in to.iter() {
            let current = self.current_ratio(*key);
            from.insert(*key, current);
            span = span.max((target - current).abs());
        }
        self.from = Rc::new(from);
        self.to = Rc::new(to);
        self.span = span;
        self.started = now;
        self.eased = 0.0;
        self.active = span > DEPTH_ANIMATION_SETTLE_EPSILON;
        if !self.active {
            self.eased = 1.0;
        }
    }

    /// Advances the movement to `now`, and answers whether a frame is still worth
    /// drawing.
    ///
    /// A reader who asked the platform for less motion is shown the final
    /// position directly, and the movement stops asking for frames the moment
    /// what is left is smaller than anyone can see.
    fn advance(&mut self, now: Instant, reduce_motion: bool) -> bool {
        if !self.active {
            self.eased = 1.0;
            return false;
        }
        if reduce_motion {
            self.eased = 1.0;
            self.active = false;
            return false;
        }
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        self.eased = 1.0 - (-DEPTH_ANIMATION_RESPONSE_PER_SECOND * elapsed).exp();
        if self.span * (1.0 - self.eased) <= DEPTH_ANIMATION_SETTLE_EPSILON {
            self.eased = 1.0;
            self.active = false;
            false
        } else {
            true
        }
    }

    /// Where one level was when the movement started.
    fn current_ratio(&self, key: DepthKey) -> f32 {
        let from = self.from.get(&key).copied().unwrap_or(0.0);
        let to = self.to.get(&key).copied().unwrap_or(0.0);
        from + (to - from) * self.eased
    }

    /// The ratios a frame draws, shared with the list without copying the maps.
    fn snapshot(&self) -> DepthSnapshot {
        DepthSnapshot {
            from: self.from.clone(),
            to: self.to.clone(),
            eased: self.eased,
        }
    }
}

/// The depth ratios one frame draws.
struct DepthSnapshot {
    from: Rc<AHashMap<DepthKey, f32>>,
    to: Rc<AHashMap<DepthKey, f32>>,
    eased: f32,
}

impl DepthSnapshot {
    /// The bar width of one level in this frame.
    fn ratio(&self, is_ask: bool, price: Fraction) -> f32 {
        let key = (is_ask, price.ratio().inner());
        let from = self.from.get(&key).copied().unwrap_or(0.0);
        let to = self.to.get(&key).copied().unwrap_or(0.0);
        (from + (to - from) * self.eased).clamp(0.0, 1.0)
    }
}

/// One row of the ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PanelRow<'a> {
    /// An ask level, already in the order it is drawn in.
    Ask(&'a GroupedLevel),
    /// The row between the two sides: the last trade and the spread.
    Spread,
    /// A bid level, already in the order it is drawn in.
    Bid(&'a GroupedLevel),
}

/// The row at `index` of the ladder.
///
/// The ask side is drawn worst-first, so the best ask sits directly above the
/// spread row; the bid side is drawn best-first, so it runs downwards from it. The
/// spread row therefore always sits at the ask count.
pub(super) fn row_at(grouped: &GroupedBook, index: usize) -> Option<PanelRow<'_>> {
    let asks = grouped.asks.len();
    if index < asks {
        grouped.asks.get(asks - 1 - index).map(PanelRow::Ask)
    } else if index == asks {
        Some(PanelRow::Spread)
    } else {
        grouped.bids.get(index - asks - 1).map(PanelRow::Bid)
    }
}

/// The ratio the panel takes of the area above it after a splitter drag of
/// `delta_y`.
///
/// The notch is measured from the top of the chart-and-book area, so dragging the
/// splitter down gives the book less of it. The result is kept inside what a
/// splitter may leave; an area that has not been laid out yet leaves the ratio
/// where it was.
pub(crate) fn split_ratio(start: f32, delta_y: Pixels, area_height: Pixels) -> f32 {
    let height = f32::from(area_height);
    if height <= 0.0 {
        return clamp_ratio(start);
    }
    clamp_ratio(start - f32::from(delta_y) / height)
}

/// The full book panel one pane draws.
pub(crate) struct PredictionBookPanel {
    /// The book the grouped ladder was built from, which regrouping reads.
    pub book: Option<Arc<PredictionOrderBook>>,
    /// The grouped ladder a frame draws, rebuilt once per accepted book.
    pub grouped: Option<Arc<GroupedBook>>,
    /// The grouping the ladder is built at, a multiple of the book's tick.
    pub multiplier: u32,
    /// The depth bars between the last two books.
    pub animation: DepthAnimation,
    /// The reader's place in the ladder, kept across frames.
    pub scroll: UniformListScrollHandle,
    /// Whether the next frame should bring the spread row into view.
    pub anchor_pending: bool,
    /// The label of the side the ladder is reading, as the venue publishes it.
    pub leg_label: Option<SmolStr>,
}

impl PredictionBookPanel {
    /// An empty panel, ready for its first book.
    pub(crate) fn new() -> Self {
        Self {
            book: None,
            grouped: None,
            multiplier: 0,
            animation: DepthAnimation::settled(),
            scroll: UniformListScrollHandle::new(),
            anchor_pending: false,
            leg_label: None,
        }
    }

    /// Takes one accepted book, grouped at `multiplier` times its tick.
    ///
    /// A book of the same side and tick as the one on screen is a live update of
    /// the ladder the reader was reading: its bars move to the new depths. Any
    /// other book is a different ladder, so its bars arrive at once and the next
    /// frame brings the spread back into view.
    pub(crate) fn set_book(&mut self, book: Arc<PredictionOrderBook>, multiplier: u32) {
        let same_ladder = self
            .book
            .as_ref()
            .is_some_and(|held| held.leg == book.leg && held.tick_size == book.tick_size);
        let multiplier = settle_grouping(book.tick_size, multiplier);
        let grouped = Arc::new(group(&book, multiplier));
        self.multiplier = multiplier;
        self.book = Some(book);
        if same_ladder && self.grouped.is_some() {
            // The ladder is drawn from the inside out: the spread row sits at the
            // ask count, so a level added above it shifts every row below. The
            // reader's place is a screen position, not an item index, so the
            // viewport is dragged by the same number of rows the side grew.
            if !self.anchor_pending {
                let previous_asks = self.grouped.as_ref().map_or(0, |held| held.asks.len());
                let delta = grouped.asks.len() as isize - previous_asks as isize;
                self.shift_scroll_by_rows(delta);
            }
            self.animation.retarget(&grouped, Instant::now());
        } else {
            self.animation.jump(&grouped);
            self.anchor_pending = true;
        }
        self.grouped = Some(grouped);
    }

    /// Drags the list content by `rows` rows, keeping the reader where they were.
    ///
    /// The offset is measured from the top of the content and is zero or
    /// negative, so a row added above the viewport moves it further down; the
    /// new place is clamped to what the list can actually show, which is the
    /// negative of the handle's (positive) maximum offset.
    fn shift_scroll_by_rows(&self, rows: isize) {
        if rows == 0 {
            return;
        }
        let handle = self.scroll.0.borrow();
        let offset = handle.base_handle.offset();
        let max_offset = handle.base_handle.max_offset();
        let shifted = offset.y - ROW_HEIGHT * rows as f32;
        handle
            .base_handle
            .set_offset(point(offset.x, shifted.clamp(-max_offset.y, px(0.0))));
    }

    /// Regroups the book on screen at another multiplier.
    ///
    /// The stored book is regrouped rather than asked for again, and the new
    /// ladder arrives without a bar animation: a different grouping is a
    /// different set of rows, not a movement of the ones that were there.
    pub(crate) fn set_multiplier(&mut self, multiplier: u32) {
        let Some(book) = self.book.clone() else {
            return;
        };
        let multiplier = settle_grouping(book.tick_size, multiplier);
        if multiplier == self.multiplier {
            return;
        }
        let grouped = Arc::new(group(&book, multiplier));
        self.multiplier = multiplier;
        self.animation.jump(&grouped);
        self.anchor_pending = true;
        self.grouped = Some(grouped);
    }

    /// Forgets the book, the grouped ladder and the reader's place in it.
    pub(crate) fn clear(&mut self) {
        self.book = None;
        self.grouped = None;
        self.leg_label = None;
        self.animation = DepthAnimation::settled();
        self.anchor_pending = false;
    }

    /// Whether there is no ladder to draw.
    pub(crate) fn is_empty(&self) -> bool {
        self.grouped
            .as_ref()
            .is_none_or(|grouped| grouped.is_empty())
    }

    /// The label of the side the ladder reads, shown in the spread row.
    pub(crate) fn set_leg_label(&mut self, label: Option<SmolStr>) {
        self.leg_label = label;
    }

    /// Advances the depth bars to `now`, and answers whether a frame is still
    /// worth drawing.
    pub(crate) fn advance(&mut self, now: Instant, reduce_motion: bool) -> bool {
        self.animation.advance(now, reduce_motion)
    }

    /// Draws the column header and the one scroll region that holds the ladder.
    pub(crate) fn render(&mut self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.roles.fg.muted.hsla;
        let foreground = theme.roles.fg.default.hsla;
        let ask_price = theme.roles.market.down.hsla;
        let bid_price = theme.roles.market.up.hsla;
        let ask_depth = theme.roles.market.down_weak.hsla;
        let bid_depth = theme.roles.market.up_weak.hsla;

        let root = div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            // The padding is the panel's rather than the pane wrapper's: the
            // column header and the rows have to share one content edge, and the
            // depth bar starts at it.
            .px_2()
            .child(column_header(muted, cx));
        if self.is_empty() {
            return root.child(empty_state(cx)).into_any_element();
        }
        let Some(grouped) = self.grouped.clone() else {
            return root.child(empty_state(cx)).into_any_element();
        };
        if self.anchor_pending {
            self.scroll
                .scroll_to_item_strict(grouped.asks.len(), ScrollStrategy::Center);
            self.anchor_pending = false;
        }
        let depths = self.animation.snapshot();
        let leg_label = self.leg_label.clone();
        let scroll = self.scroll.clone();
        let ladder = uniform_list(
            ElementId::Name(SharedString::new_static("PredictionMarketBookList")),
            grouped.asks.len() + 1 + grouped.bids.len(),
            move |range, _, cx| {
                range
                    .map(|index| match row_at(&grouped, index) {
                        Some(PanelRow::Ask(level)) => level_row(
                            level,
                            grouped.tick,
                            depths.ratio(true, level.price),
                            ask_price,
                            ask_depth,
                            foreground,
                        ),
                        Some(PanelRow::Bid(level)) => level_row(
                            level,
                            grouped.tick,
                            depths.ratio(false, level.price),
                            bid_price,
                            bid_depth,
                            foreground,
                        ),
                        Some(PanelRow::Spread) => {
                            spread_row(&grouped, leg_label.clone(), muted, cx)
                        }
                        // The list is told its row count from the same grouped
                        // book this closure reads, so there is no row here.
                        None => div().h(ROW_HEIGHT).into_any_element(),
                    })
                    .collect()
            },
        )
        .size_full()
        .track_scroll(&scroll);
        root.child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(ladder)
                // A deep ladder is read, not scrolled through once: the bar says
                // where in the book the viewport is while the reader is still.
                .child(Scrollbar::vertical(scroll).mode(ScrollbarMode::Always)),
        )
        .into_any_element()
    }

    /// The grouped ladder a frame draws, for tests.
    #[cfg(test)]
    pub(crate) fn grouped(&self) -> Option<&GroupedBook> {
        self.grouped.as_deref()
    }

    /// The grouping the ladder is built at, for tests.
    #[cfg(test)]
    pub(crate) fn multiplier(&self) -> u32 {
        self.multiplier
    }

    /// The reader's place in the ladder, for tests.
    #[cfg(test)]
    pub(crate) fn scroll(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// Whether the next frame will bring the spread row into view, for tests.
    #[cfg(test)]
    pub(crate) fn anchor_pending(&self) -> bool {
        self.anchor_pending
    }
}

/// The depth each level of a book asks for, by side and price.
fn depth_map(grouped: &GroupedBook) -> AHashMap<DepthKey, f32> {
    let mut depths = AHashMap::with_capacity(grouped.asks.len() + grouped.bids.len());
    for level in &grouped.asks {
        depths.insert((true, level.price.ratio().inner()), level.depth);
    }
    for level in &grouped.bids {
        depths.insert((false, level.price.ratio().inner()), level.depth);
    }
    depths
}

/// The column labels above the ladder, which never scroll.
fn column_header(muted: Hsla, cx: &App) -> AnyElement {
    div()
        .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_HEADER".into())
        .flex_none()
        .flex()
        .items_center()
        .h(HEADER_HEIGHT)
        .pr(SCROLLBAR_INSET)
        .text_size(HEADER_TEXT_SIZE)
        .text_color(muted)
        .child(column_cell(t!(cx, "widget.prediction_market.book.price")))
        .child(column_cell(t!(cx, "widget.prediction_market.book.shares")))
        .child(column_cell(t!(cx, "widget.prediction_market.book.total")))
        .into_any_element()
}

/// One right-aligned column label.
fn column_cell(text: SharedString) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .text_right()
        .child(text)
        .into_any_element()
}

/// One grouped level: its price, its shares and the running notional, over a bar
/// as long as its share of the side.
fn level_row(
    level: &GroupedLevel,
    tick: Tick,
    depth: f32,
    price_color: Hsla,
    depth_color: Hsla,
    foreground: Hsla,
) -> AnyElement {
    div()
        .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_ROW".into())
        .relative()
        .flex_none()
        .w_full()
        .h(ROW_HEIGHT)
        .flex()
        .items_center()
        .text_size(ROW_TEXT_SIZE)
        .pr(SCROLLBAR_INSET)
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(relative(depth))
                .bg(depth_color),
        )
        .child(level_cell(
            SharedString::new(format_price_cents(level.price, tick)),
            price_color,
        ))
        .child(level_cell(
            SharedString::new(format_shares(level.size)),
            foreground,
        ))
        .child(level_cell(
            SharedString::new(format_notional(level.total)),
            foreground,
        ))
        .into_any_element()
}

/// One right-aligned number of a level row.
fn level_cell(text: SharedString, color: Hsla) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .text_right()
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The row between the two sides: the side being read, the last trade and the
/// spread.
fn spread_row(
    grouped: &GroupedBook,
    leg_label: Option<SmolStr>,
    muted: Hsla,
    cx: &App,
) -> AnyElement {
    let last = grouped.last.map(|price| {
        t!(
            cx,
            "widget.prediction_market.book.last",
            price = format_price_cents(price, grouped.tick)
        )
    });
    let spread = grouped.spread.map(|spread| {
        t!(
            cx,
            "widget.prediction_market.book.spread",
            spread = format_spread_cents(spread, grouped.tick)
        )
    });
    let mut right = div().flex().items_center().gap_2();
    if let Some(last) = last {
        right = right.child(last);
    }
    if let Some(spread) = spread {
        right = right.child(spread);
    }
    let mut row = div()
        .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_SPREAD".into())
        .flex_none()
        .w_full()
        .h(ROW_HEIGHT)
        .flex()
        .items_center()
        .justify_between()
        .pr(SCROLLBAR_INSET)
        .text_size(ROW_TEXT_SIZE)
        .text_color(muted);
    if let Some(label) = leg_label {
        row = row.child(SharedString::new(label));
    }
    row.child(right).into_any_element()
}

/// The panel's body when there is no level to draw.
fn empty_state(cx: &App) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .child(EmptyState::new(
            "PredictionMarketBookEmpty",
            t!(cx, "common.no_data"),
        ))
        .into_any_element()
}
