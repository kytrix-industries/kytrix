//! The prediction order book panel: one ladder in one scroll region.
//!
//! A prediction book is read as one continuous ladder: the ask side above the
//! spread and the bid side below it, best prices against the spread. The panel
//! owns that reading — the grouping, the reader's place in it and the easing of
//! the depth bars behind the rows. The chart's splitter owns how much of the body
//! the ladder gets; the panel is handed the area that is left.
//!
//! # Three layers, one region
//!
//! The ladder is a [`List`] of row labels drawn over one depth-bar [`Canvas`],
//! with the group's "no data" [`EmptyState`] over both. The rows are ordinary
//! nodes: the engine lays them out, shapes their text, clips them and hands them
//! to a screen reader, and a language or appearance switch re-evaluates their
//! `bind_*` closures instead of rebuilding them. The canvas carries quads only,
//! so no glyph lease or registered geometry has to be handed back; a steady
//! `sync` writes no canvas at all (I3).
//!
//! Column labels are outside the scroll region: they belong to the panel, not to
//! the levels that move under them.
//!
//! # The depth animation
//!
//! A grouping is rebuilt once per accepted book (in [`PredictionBookPanel::set_book`]),
//! never per frame. The depth bars are the only thing that moves between two
//! accepted books. The engine has no wall clock to read, so the movement is
//! driven by a one-shot [`App::after_scoped`] timer chain scoped to the canvas
//! node, at the old panel's 16 ms frame tick: the timer only asks for the next
//! frame, and [`PredictionBookPanel::sync`] advances the easing by one tick when
//! that frame arrives, then either re-arms the chain or cancels it. The chain is
//! scoped to the canvas, so a body that goes away takes a pending tick with it,
//! and `release` (or a movement that settled, or a reader who asked the platform
//! to reduce motion) stops it explicitly.
//!
//! # What is the same as the old panel, and what is not
//!
//! The pure state is the old one under the same names: the grouping, the easing,
//! the row mapping and the splitter arithmetic. Two engine facts change the
//! drawing:
//!
//! - the old `uniform_list` measured rows itself; the kit list is told a fixed row
//!   height (`ROW_HEIGHT`) and rebuilds its visible window on a count change;
//! - the kit list owns its scroll state, and its API sets a position by item and
//!   alignment rather than by pixel offset. The old panel nudged the offset by the
//!   number of rows the ask side grew so a live update did not move the rows the
//!   reader was reading; there is no equivalent setter, so a live update keeps the
//!   item the list anchors on rather than the exact pixel place. The spread row is
//!   still brought into view by item and centred, which is the old
//!   `scroll_to_item_strict(asks.len(), Center)`.
//!
//! The kit list also paints its scrollbar over the right edge of the rows and
//! draws a hover veil; the old panel reserved a 10 px inset on the header and the
//! rows instead and drew nothing on hover. Both are kit-owned drawing decisions,
//! not panel state.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use ahash::AHashMap;
use base_ui::{
    kit::{
        empty_state::EmptyState,
        list::{List, ListOptions, SelectionMode},
    },
    motion::{DEPTH_ANIMATION_SETTLE_EPSILON, depth_animation_progress_after},
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_model::{
    decimal::{Fraction, Tick},
    prediction::PredictionOrderBook,
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    interaction::ItemAlign,
    math::Rect,
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{ClipRect, Quad, QuadPaint},
        style::StylePatch,
    },
    text::{TextAlign, TextStyle},
    window::WindowId,
};
use widget::cx::WidgetCx;

use super::model::{
    GroupedBook, GroupedLevel, clamp_ratio, format_cents, format_notional, format_shares, group,
    grouping_steps, settle_grouping,
};

/// The height of one ladder row.
///
/// Every row of the list is exactly this tall: the list is told a fixed row height
/// and the depth bars are laid out on the same stride.
const ROW_HEIGHT: f32 = 22.0;

/// The height of the fixed column header above the ladder.
const HEADER_HEIGHT: f32 = 20.0;

/// The size of the column labels, in logical pixels.
///
/// The old panel drew the header at 11 px and the rows at the small control
/// token's 12 px. No theme token states 11 px, so this is the header's own named
/// step; the rows use `theme.tokens().small.font_size`.
const HEADER_TEXT_SIZE: f32 = 11.0;

/// The panel's horizontal edge padding.
///
/// The header and the rows share one content edge, and the depth bars start at it;
/// the padding is the panel's rather than the pane wrapper's, so the splitter area
/// and the ladder line up.
const PANEL_PADDING_X: f32 = 8.0;

/// The tick of the depth easing.
///
/// The engine has no wall-clock read, so the old panel's elapsed-time easing is
/// driven one 16 ms step at a time: every timer tick advances the movement by the
/// amount the old panel would have advanced a frame that arrived 16 ms later.
/// `ComponentTokens::animation_tick` is the old toolkit's 150 ms control tick, not
/// this ladder's frame cadence.
const DEPTH_ANIMATION_TICK: Duration = Duration::from_millis(16);

/// The debug ids the panel's own nodes carry.
const HEADER_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_HEADER";
const LADDER_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_LADDER";
const DEPTH_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_DEPTH";
const EMPTY_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_EMPTY";
const ROW_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_ROW";

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
        self.active = false;
        self.eased = 1.0;
    }

    /// Sends the bars after the ratios the newest book asks for.
    ///
    /// The movement starts from what the previous frame was actually painting, so
    /// an update that arrives mid-flight retargets it instead of resetting it. A
    /// movement with nothing visible left in it never starts.
    fn retarget(&mut self, grouped: &GroupedBook) {
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
        self.eased = 0.0;
        self.active = span > DEPTH_ANIMATION_SETTLE_EPSILON;
        if !self.active {
            self.eased = 1.0;
        }
    }

    /// Advances the movement by one 16 ms tick, and answers whether a frame is
    /// still worth drawing.
    ///
    /// A reader who asked the platform for less motion is shown the final
    /// position directly, and the movement stops asking for frames the moment
    /// what is left is smaller than anyone can see.
    fn advance(&mut self, reduce_motion: bool) -> bool {
        if !self.active {
            self.eased = 1.0;
            return false;
        }
        if reduce_motion {
            self.eased = 1.0;
            self.active = false;
            return false;
        }
        self.eased = depth_animation_progress_after(self.eased, self.span, DEPTH_ANIMATION_TICK);
        if self.eased >= 1.0 {
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

    /// The ratios a frame draws, shared with the canvas without copying the maps.
    pub(crate) fn snapshot(&self) -> DepthSnapshot {
        DepthSnapshot {
            from: self.from.clone(),
            to: self.to.clone(),
            eased: self.eased,
        }
    }
}

/// The depth ratios one frame draws.
pub(crate) struct DepthSnapshot {
    from: Rc<AHashMap<DepthKey, f32>>,
    to: Rc<AHashMap<DepthKey, f32>>,
    eased: f32,
}

impl DepthSnapshot {
    /// The bar width of one level in this frame, clamped to the row.
    pub(crate) fn ratio(&self, is_ask: bool, price: Fraction) -> f32 {
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
pub(crate) fn split_ratio(start: f32, delta_y: f32, area_height: f32) -> f32 {
    if area_height <= 0.0 {
        return clamp_ratio(start);
    }
    clamp_ratio(start - delta_y / area_height)
}

/// A grouped ladder as the body's `bind_*` closures read it.
///
/// The signal carries the shared allocation rather than the drawing itself, so two
/// publishes of the same grouped book compare equal by pointer and a steady `sync`
/// writes nothing. The leg label travels with the ladder because the spread row
/// shows it: a reader of two sides of one event needs to know which side the
/// ladder belongs to.
#[derive(Clone)]
pub(crate) struct BookRows {
    grouped: Option<Arc<GroupedBook>>,
    leg: Option<SmolStr>,
}

impl PartialEq for BookRows {
    fn eq(&self, other: &Self) -> bool {
        self.leg == other.leg
            && match (&self.grouped, &other.grouped) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

impl BookRows {
    /// The rows the ladder draws: the asks, the spread row and the bids.
    ///
    /// An empty page draws no row at all — the spread row is a row of the ladder,
    /// not a statement about a book that has no price.
    fn row_count(&self) -> usize {
        match self.grouped.as_deref() {
            Some(grouped) if !grouped.is_empty() => grouped.asks.len() + 1 + grouped.bids.len(),
            _ => 0,
        }
    }
}

/// What a prepared depth canvas was drawn from.
///
/// The grouped book compares by its shared allocation: the panel only ever replaces
/// a grouped ladder wholesale, so two pointers compare equal exactly when there is
/// nothing to redraw. The eased progress, the box, the scroll offset and the two
/// colours are part of the key because a frame may draw different bars from the
/// same book.
struct DepthBarsKey {
    grouped: Option<Arc<GroupedBook>>,
    eased: f32,
    bounds: Rect,
    offset: f32,
    ask_color: Color,
    bid_color: Color,
}

impl DepthBarsKey {
    fn matches(
        &self,
        grouped: Option<&Arc<GroupedBook>>,
        eased: f32,
        bounds: Rect,
        offset: f32,
        ask_color: Color,
        bid_color: Color,
    ) -> bool {
        self.eased == eased
            && self.bounds == bounds
            && self.offset == offset
            && self.ask_color == ask_color
            && self.bid_color == bid_color
            && match (&self.grouped, grouped) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

/// The one-shot timer chain that drives the depth easing.
///
/// The timer callback owns no widget context, so it does not advance anything: it
/// marks the tick pending and asks the engine to repaint the canvas. The panel's
/// `sync` (which runs in the frame hook, before that frame's paint) consumes the
/// mark, advances the easing by one tick and either re-arms the chain or lets it
/// stop. A release or a settled movement cancels the chain.
struct DepthTicker {
    window: WindowId,
    node: NodeId,
    pending: Cell<bool>,
    timer: RefCell<Option<TimerId>>,
    alive: Cell<bool>,
}

impl DepthTicker {
    fn new(window: WindowId, node: NodeId) -> Rc<Self> {
        Rc::new(Self {
            window,
            node,
            pending: Cell::new(false),
            timer: RefCell::new(None),
            alive: Cell::new(true),
        })
    }

    /// Schedules the next 16 ms tick, keeping the timer id for `release`.
    fn schedule(self: &Rc<Self>, app: &mut App) {
        let state = Rc::clone(self);
        let result = app.after_scoped(self.window, self.node, DEPTH_ANIMATION_TICK, move |app| {
            if !state.alive.get() {
                return;
            }
            state.timer.borrow_mut().take();
            state.pending.set(true);
            let _ = app.request_node_repaint(state.window, state.node);
        });
        match result {
            Ok(timer) => *self.timer.borrow_mut() = Some(timer),
            Err(error) => log::warn!("the book's depth tick could not be scheduled: {error}"),
        }
    }

    /// Takes the pending tick, if the timer fired since the last `sync`.
    fn take_pending(&self) -> bool {
        self.pending.replace(false)
    }

    /// Drops a pending or scheduled tick without advancing the movement.
    fn disarm(&self, app: &mut App) {
        self.pending.set(false);
        if let Some(timer) = self.timer.borrow_mut().take() {
            app.cancel_timer(&timer);
        }
    }

    /// Stops the chain for good: no later callback may touch the panel's state.
    fn release(&self, app: &mut App) {
        self.alive.set(false);
        self.disarm(app);
    }
}

/// The nodes and signals one built body owns.
///
/// Building is the only moment the panel needs an application context to read the
/// language or the theme: everything after that goes through the signals and the
/// stored [`ThemeHandle`], so a rebuilt body gets fresh signals while the pure
/// ladder state survives.
struct BuiltPanel {
    canvas: NodeId,
    list: List,
    ticker: Rc<DepthTicker>,
    prepared: Option<DepthBarsKey>,
    rows: Signal<BookRows>,
    empty: Signal<bool>,
    empty_text: Signal<SmolStr>,
    empty_text_language: Option<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    scope: SignalScope,
    list_count: usize,
}

impl BuiltPanel {
    /// Draws the depth bars if the frame this one would be is not the one already
    /// drawn, and answers whether the canvas was written.
    fn sync_bars(
        &mut self,
        cx: &mut WidgetCx<'_>,
        grouped: Option<&Arc<GroupedBook>>,
        snapshot: &DepthSnapshot,
    ) -> Result<bool, EngineError> {
        let Ok(target) = cx.tree().get(self.canvas) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Ok(false);
        }
        let offset = self.list.offset(cx.app()).unwrap_or(0.0);
        let appearance = self.theme.current(cx.app());
        let resolved = self.theme.theme_for(appearance);
        let ask_color = resolved.role(Role::MarketDownWeak);
        let bid_color = resolved.role(Role::MarketUpWeak);
        if self.prepared.as_ref().is_some_and(|prepared| {
            prepared.matches(
                grouped,
                snapshot.eased,
                bounds,
                offset,
                ask_color,
                bid_color,
            )
        }) {
            return Ok(false);
        }
        let canvas = depth_canvas(
            bounds,
            offset,
            grouped.map(|grouped| grouped.as_ref()),
            snapshot,
            ask_color,
            bid_color,
        );
        if !cx.set_canvas(self.canvas, canvas)? {
            return Ok(false);
        }
        self.prepared = Some(DepthBarsKey {
            grouped: grouped.cloned(),
            eased: snapshot.eased,
            bounds,
            offset,
            ask_color,
            bid_color,
        });
        Ok(true)
    }
}

/// The full book panel one pane draws.
///
/// The pure state (the book, the grouped ladder, the grouping, the easing and the
/// pending anchor) outlives a built body: the pane stores a book while the ladder
/// is hidden and draws the newest reading when it comes back.
pub(crate) struct PredictionBookPanel {
    /// The book the grouped ladder was built from, which regrouping reads.
    pub book: Option<Arc<PredictionOrderBook>>,
    /// The grouped ladder a frame draws, rebuilt once per accepted book.
    pub grouped: Option<Arc<GroupedBook>>,
    /// The grouping the ladder is built at, a multiple of the book's tick.
    pub multiplier: u32,
    /// The depth bars between the last two books.
    pub animation: DepthAnimation,
    /// Whether the next frame should bring the spread row into view.
    pub anchor_pending: bool,
    /// The label of the side the ladder is reading, as the venue publishes it.
    pub leg_label: Option<SmolStr>,
    /// The nodes and signals of the body, while it is built.
    built: Option<BuiltPanel>,
}

impl PredictionBookPanel {
    /// An empty panel, ready for its first book.
    pub(crate) fn new() -> Self {
        Self {
            book: None,
            grouped: None,
            multiplier: 0,
            animation: DepthAnimation::settled(),
            anchor_pending: false,
            leg_label: None,
            built: None,
        }
    }

    /// Builds the column header, the depth-bar canvas and the row list into
    /// `parent` (one root box).
    ///
    /// The canvas is built before the list so the labels paint over the bars, and
    /// the empty state last so it covers both. The depth timer is scoped to the
    /// canvas: a body that is taken down cancels the tick that was waiting for it.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        if self.built.is_some() {
            return Ok(());
        }
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let language = cx.language();
        let scope = cx.app_mut().signal_scope();
        let rows = cx.app_mut().signal_scoped(
            scope,
            BookRows {
                grouped: self.grouped.clone(),
                leg: self.leg_label.clone(),
            },
        );
        let empty = cx.app_mut().signal_scoped(scope, self.is_empty());
        let empty_text = cx
            .app_mut()
            .signal_scoped(scope, t_smol!(translate, "common.no_data"));

        let root = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_BOOK")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_height_0()
                    .padding_x(PANEL_PADDING_X)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let header = NodeBuilder::boxed()
            .id(HEADER_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .flex_none()
                    .height(HEADER_HEIGHT)
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        for key in [
            "widget.prediction_market.book.price",
            "widget.prediction_market.book.shares",
            "widget.prediction_market.book.total",
        ] {
            let translate = translate.clone();
            let cell_theme = theme.clone();
            NodeBuilder::text(SmolStr::default(), aligned_text(HEADER_TEXT_SIZE))
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .bind_text(move |read| {
                    let _language = *read.read(&language);
                    t_smol!(translate, key)
                })
                .bind_style(move |read| StylePatch {
                    foreground: Some(cell_theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), header)?;
        }

        let ladder = NodeBuilder::boxed()
            .id(LADDER_SELECTOR)
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;

        let canvas_theme = theme.clone();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(DEPTH_SELECTOR)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(0.0)
                    .top(0.0)
                    .size_full()
                    .build(),
            )
            .bind_style(move |read| {
                let _appearance = canvas_theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), ladder)?;

        let list_key = cx.unique_key("prediction_market.book.ladder");
        let list_theme = theme.clone();
        let list_translate = translate.clone();
        let font_size = theme.tokens().small.font_size;
        let list = List::new(
            cx.app_mut(),
            theme.clone(),
            list_key,
            ListOptions::fixed(ROW_HEIGHT)
                .padding(0.0, 0.0)
                .selection(SelectionMode::None),
            move |tree, parent, index| {
                build_row(
                    tree,
                    parent,
                    index,
                    rows,
                    language,
                    list_translate.clone(),
                    list_theme.clone(),
                    font_size,
                )
            },
        );
        let count = BookRows {
            grouped: self.grouped.clone(),
            leg: self.leg_label.clone(),
        }
        .row_count();
        list.set_initial_count(cx.app_mut(), count);
        let window = cx.window();
        list.mount(cx.app_mut(), window, ladder)?;

        let overlay_theme = theme.clone();
        let overlay_key = cx.unique_key("prediction_market.book.empty");
        let overlay = NodeBuilder::boxed()
            .id(EMPTY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(0.0)
                    .top(0.0)
                    .size_full()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                visible: Some(*read.read(&empty)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), ladder)?;
        EmptyState::new(overlay_theme, overlay_key)
            .bind_text(empty_text)
            .build(cx.tree_mut(), overlay)?;

        self.built = Some(BuiltPanel {
            canvas,
            ticker: DepthTicker::new(window, canvas),
            list,
            prepared: None,
            rows,
            empty,
            empty_text,
            empty_text_language: Some(*cx.read(&language)),
            translate,
            theme,
            scope,
            list_count: count,
        });
        Ok(())
    }

    /// Takes one accepted book, grouped at `multiplier` times its tick.
    ///
    /// A book of the same side and tick as the one on screen is a live update of
    /// the ladder the reader was reading: its bars move to the new depths. Any
    /// other book is a different ladder, so its bars arrive at once and the next
    /// frame brings the spread back into view.
    ///
    /// An empty ladder is not a ladder: the levels of the book that fills it are
    /// a new one, even when its side and tick are the same as the empty page's,
    /// so the spread is anchored again instead of being treated as a movement of
    /// rows that were never drawn. The anchor stays pending while the ladder is
    /// empty, because there is nothing to bring into view yet.
    pub(crate) fn set_book(&mut self, book: Arc<PredictionOrderBook>, multiplier: u32) {
        let same_ladder = self
            .book
            .as_ref()
            .is_some_and(|held| held.leg == book.leg && held.tick_size == book.tick_size);
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        // An empty page has no level to group, and the tick it is stated at is a
        // fallback rather than the venue's: settling a stored grouping against it
        // would rewrite a choice the user made on a book that does have levels.
        let multiplier = if has_levels {
            settle_grouping(book.tick_size, multiplier)
        } else {
            multiplier
        };
        let grouped = Arc::new(group(&book, multiplier));
        self.multiplier = multiplier;
        self.book = Some(book);
        let previous_empty = self.grouped.as_ref().is_none_or(|held| held.is_empty());
        if same_ladder && !previous_empty && has_levels {
            self.animation.retarget(&grouped);
        } else {
            self.animation.jump(&grouped);
            self.anchor_pending = true;
        }
        self.grouped = Some(grouped);
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

    /// Forgets the book, the grouped ladder, the leg label and the movement.
    ///
    /// A body that is still built publishes the empty ladder on its next `sync`
    /// and disarms the depth tick there: this method has no application context,
    /// so the timer it leaves behind can at worst ask for one frame nobody needs.
    pub(crate) fn clear(&mut self) {
        self.book = None;
        self.grouped = None;
        self.leg_label = None;
        self.animation = DepthAnimation::settled();
        self.anchor_pending = false;
    }

    /// Whether there is no grouped level to draw — an empty page.
    ///
    /// A cleared panel and a book the venue published with no resting level are
    /// both empty pages; a book that has not arrived yet is not, because nobody
    /// has said what it holds.
    pub(crate) fn is_empty(&self) -> bool {
        self.grouped
            .as_ref()
            .is_none_or(|grouped| grouped.is_empty())
    }

    /// The label of the side the ladder reads, shown in the spread row.
    pub(crate) fn set_leg_label(&mut self, label: Option<SmolStr>) {
        self.leg_label = label;
    }

    /// Whether the panel's book offers a grouping choice at all.
    ///
    /// An empty page has no level to group, and a tick that offers one step offers
    /// nothing to choose: both leave the header without a picker.
    pub(crate) fn has_grouping_choice(&self) -> bool {
        let Some(book) = self.book.as_ref() else {
            return false;
        };
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        has_levels && grouping_steps(book.tick_size).len() > 1
    }

    /// The label of the grouping the ladder is built at, falling back to the
    /// venue's own tick.
    pub(crate) fn grouping_label(&self) -> Option<SmolStr> {
        let book = self.book.as_ref()?;
        let settled = settle_grouping(book.tick_size, self.multiplier);
        let label = grouping_steps(book.tick_size)
            .into_iter()
            .find(|(multiplier, _)| *multiplier == settled)
            .map_or_else(|| format_smolstr!("{}", book.tick_size), |(_, label)| label);
        Some(label)
    }

    /// Advances the depth bars one 16 ms tick, and answers whether a frame is
    /// still worth drawing.
    ///
    /// The panel's own timer chain calls this through [`Self::sync`], which is the
    /// only driver a pane needs: a caller that also ticks the panel itself would
    /// advance a running movement twice per frame.
    pub(crate) fn advance(&mut self, reduce_motion: bool) -> bool {
        self.animation.advance(reduce_motion)
    }

    /// Publishes the ladder to the rows and paints the depth bars.
    ///
    /// Returns whether the canvas was written. Everything the panel owns moves
    /// here, once per frame, in one order: consume the depth tick, publish the
    /// ladder (the row closures and the empty state read those signals), resize the
    /// list, consume the pending anchor, paint the bars, and finally re-arm or stop
    /// the depth timer. A frame with nothing to do writes nothing and asks for no
    /// frame (I3).
    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let reduce_motion = *cx.read(&cx.services().reduce_motion);
        let ticked = self
            .built
            .as_ref()
            .is_some_and(|built| built.ticker.take_pending());
        if self.animation.active && (ticked || reduce_motion) {
            self.advance(reduce_motion);
        }
        let snapshot = self.animation.snapshot();
        let rows_now = BookRows {
            grouped: self.grouped.clone(),
            leg: self.leg_label.clone(),
        };
        let count = rows_now.row_count();
        let empty_now = self.is_empty();
        let language = *cx.read(&cx.language());
        let Some(built) = self.built.as_mut() else {
            return Ok(false);
        };
        let mut changed = false;

        if *cx.read(&built.rows) != rows_now {
            // `BookRows` compares by allocation; the clone is the value handed to
            // the signal, this frame keeps painting from `rows_now`.
            cx.write(&built.rows, rows_now.clone());
            changed = true;
        }
        if *cx.read(&built.empty) != empty_now {
            cx.write(&built.empty, empty_now);
            changed = true;
        }
        if built.empty_text_language != Some(language) {
            built.empty_text_language = Some(language);
            let text = t_smol!(built.translate, "common.no_data");
            cx.write(&built.empty_text, text);
            changed = true;
        }

        changed |= built.list.sync(cx.app_mut())?;
        if built.list_count != count {
            built.list.set_count(cx.app_mut(), count)?;
            built.list_count = count;
            changed = true;
        }
        if self.anchor_pending
            && !empty_now
            && let Some(grouped) = rows_now.grouped.as_deref()
        {
            built
                .list
                .scroll_to_item(cx.app_mut(), grouped.asks.len(), ItemAlign::Center)?;
            self.anchor_pending = false;
            changed = true;
        }

        let painted = built.sync_bars(cx, rows_now.grouped.as_ref(), &snapshot)?;
        if self.animation.active && !reduce_motion {
            let ticker = Rc::clone(&built.ticker);
            if ticker.timer.borrow().is_none() {
                ticker.schedule(cx.app_mut());
            }
        } else {
            let ticker = Rc::clone(&built.ticker);
            ticker.disarm(cx.app_mut());
        }

        if changed || painted {
            cx.request_frame()?;
        }
        Ok(painted)
    }

    /// Releases the list and the panel's own signal scope, and stops the depth
    /// tick. The nodes belong to the body, which the framework removes.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(built) = self.built.take() else {
            return;
        };
        built.ticker.release(cx.app_mut());
        built.list.release(cx.app_mut());
        cx.app_mut().release_scope(built.scope);
    }

    /// The grouping the ladder is built at, for a test that reads the state back.
    #[cfg(test)]
    pub(crate) fn multiplier(&self) -> u32 {
        self.multiplier
    }

    /// The canvas node of the built body, for a test that reads what it drew.
    #[cfg(test)]
    pub(crate) fn built_canvas(&self) -> Option<NodeId> {
        self.built.as_ref().map(|built| built.canvas)
    }

    /// The list's row count as the last `sync` set it.
    #[cfg(test)]
    pub(crate) fn list_count(&self, app: &App) -> usize {
        self.built.as_ref().map_or(0, |built| built.list.count(app))
    }

    /// The rows the list currently has mounted.
    #[cfg(test)]
    pub(crate) fn mounted_rows(&self, app: &App) -> Vec<(usize, NodeId)> {
        self.built
            .as_ref()
            .map_or_else(Vec::new, |built| built.list.mounted_rows(app))
    }

    /// The list's vertical scroll offset, when it is attached.
    #[cfg(test)]
    pub(crate) fn scroll_offset(&self, app: &App) -> Option<f32> {
        self.built.as_ref().and_then(|built| built.list.offset(app))
    }

    /// Whether the depth timer is waiting for its next tick.
    #[cfg(test)]
    pub(crate) fn timer_armed(&self) -> bool {
        self.built
            .as_ref()
            .is_some_and(|built| built.ticker.timer.borrow().is_some())
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

/// A text style right-aligned in its column, which is what every ladder number is.
fn aligned_text(size: f32) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.alignment = TextAlign::Right;
    style
}

/// Builds one row of the ladder: the three labels, each bound to the rows signal.
///
/// The row factory runs **without an application context** (AGENTS §9), so every
/// dynamic value here is a `bind_text` / `bind_style` closure over the signal the
/// panel owns rather than something read while the row is built. The row keeps the
/// same three cells on both sides of the ladder: the spread row puts the leg
/// label, the last trade and the spread where a level puts its price, shares and
/// notional, so the ladder reads as one table.
fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    rows: Signal<BookRows>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    font_size: f32,
) -> Result<(), EngineError> {
    let container = NodeBuilder::boxed()
        .id(ROW_SELECTOR)
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .size_full()
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;
    for column in 0..3_usize {
        let cell_theme = theme.clone();
        let cell_translate = translate.clone();
        NodeBuilder::text(SmolStr::default(), aligned_text(font_size))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .height_full()
                    .build(),
            )
            .bind_text(move |read| row_text(read, rows, index, column, language, &cell_translate))
            .bind_style(move |read| {
                let role = cell_role(read, rows, index, column);
                StylePatch {
                    foreground: Some(cell_theme.role(read, role)),
                    ..StylePatch::default()
                }
            })
            .build(tree, container)?;
    }
    Ok(())
}

/// The text one cell of a ladder row draws.
///
/// A level shows its price, its shares and the running notional; the spread row
/// shows the leg the ladder belongs to, the last trade and the spread, each in the
/// language showing now. Everything comes from the rows signal, so a published
/// ladder re-evaluates the cells without rebuilding the row.
fn row_text(
    read: &mut ReadCtx<'_>,
    rows: Signal<BookRows>,
    index: usize,
    column: usize,
    language: Signal<LanguageCode>,
    translate: &Translate,
) -> SmolStr {
    let rows = read.read(&rows);
    let Some(grouped) = rows.grouped.as_deref() else {
        return SmolStr::default();
    };
    match row_at(grouped, index) {
        Some(PanelRow::Ask(level)) => level_cell(level, grouped.tick, column),
        Some(PanelRow::Bid(level)) => level_cell(level, grouped.tick, column),
        Some(PanelRow::Spread) => {
            let _language = *read.read(&language);
            match column {
                0 => rows.leg.clone().unwrap_or_default(),
                1 => grouped.last.map_or_else(SmolStr::default, |price| {
                    t_smol!(
                        translate,
                        "widget.prediction_market.book.last",
                        price = format_cents(price, grouped.tick)
                    )
                }),
                _ => grouped.spread.map_or_else(SmolStr::default, |spread| {
                    t_smol!(
                        translate,
                        "widget.prediction_market.book.spread",
                        spread = format_cents(spread, grouped.tick)
                    )
                }),
            }
        }
        None => SmolStr::default(),
    }
}

/// One cell of a level row: the price, the shares or the running notional.
fn level_cell(level: &GroupedLevel, tick: Tick, column: usize) -> SmolStr {
    match column {
        0 => format_cents(level.price, tick),
        1 => format_shares(level.size),
        _ => format_notional(level.total),
    }
}

/// The themed colour of one row cell.
///
/// The price column is the side's colour, the sizes and totals are ordinary
/// foreground, and the spread row is muted: it separates the two sides rather than
/// belonging to either.
fn cell_role(read: &mut ReadCtx<'_>, rows: Signal<BookRows>, index: usize, column: usize) -> Role {
    let rows = read.read(&rows);
    let Some(grouped) = rows.grouped.as_deref() else {
        return Role::FgDefault;
    };
    match row_at(grouped, index) {
        Some(PanelRow::Ask(_)) if column == 0 => Role::MarketDown,
        Some(PanelRow::Bid(_)) if column == 0 => Role::MarketUp,
        Some(PanelRow::Spread) => Role::FgMuted,
        _ => Role::FgDefault,
    }
}

/// One depth bar canvas over `bounds`, anchored to the left of the row.
///
/// Rows are counted from the top of the ladder with the list's scroll offset
/// applied, and only the rows the box can show are drawn; the clip the canvas
/// installs keeps a partly visible row inside the region. The bars are quads and
/// nothing else — no glyph is constructed or carried here.
fn depth_canvas(
    bounds: Rect,
    offset: f32,
    grouped: Option<&GroupedBook>,
    snapshot: &DepthSnapshot,
    ask_color: Color,
    bid_color: Color,
) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let _ = canvas.push_clip(ClipRect {
        rect: bounds,
        corner_radius: [0.0; 4],
    });
    if let Some(grouped) = grouped {
        let count = grouped.asks.len() + 1 + grouped.bids.len();
        let first = (offset / ROW_HEIGHT).floor().max(0.0) as usize;
        let visible = ((bounds.size.height / ROW_HEIGHT).ceil().max(1.0) as usize) + 1;
        let last = (first + visible).min(count);
        for index in first..last {
            let top = bounds.origin.y + index as f32 * ROW_HEIGHT - offset;
            let row = Rect::new(bounds.origin.x, top, bounds.size.width, ROW_HEIGHT);
            if row.origin.y + row.size.height <= bounds.origin.y
                || row.origin.y >= bounds.origin.y + bounds.size.height
            {
                continue;
            }
            let (ratio, color) = match row_at(grouped, index) {
                Some(PanelRow::Ask(level)) => (snapshot.ratio(true, level.price), ask_color),
                Some(PanelRow::Bid(level)) => (snapshot.ratio(false, level.price), bid_color),
                Some(PanelRow::Spread) | None => continue,
            };
            let width = row.size.width * ratio;
            if width <= 0.0 {
                continue;
            }
            canvas.push_quad(Quad {
                bounds: Rect::new(row.origin.x, row.origin.y, width, row.size.height),
                corner_radius: [0.0; 4],
                paint: QuadPaint::Solid {
                    fill: color,
                    border: None,
                },
            });
        }
    }
    let _ = canvas.pop_clip();
    canvas
}
