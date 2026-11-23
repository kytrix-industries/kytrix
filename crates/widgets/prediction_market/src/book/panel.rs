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
        motion::{DEPTH_ANIMATION_SETTLE_EPSILON, depth_animation_progress_after},
    },
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

const ROW_HEIGHT: f32 = 22.0;

const HEADER_HEIGHT: f32 = 20.0;

const HEADER_TEXT_SIZE: f32 = 11.0;

const PANEL_PADDING_X: f32 = 8.0;

const DEPTH_ANIMATION_TICK: Duration = Duration::from_millis(16);

const HEADER_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_HEADER";
const LADDER_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_LADDER";
const DEPTH_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_DEPTH";
const EMPTY_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_EMPTY";
const ROW_SELECTOR: &str = "TEST_PREDICTION_MARKET_BOOK_ROW";

type DepthKey = (bool, u128);

pub(crate) struct DepthAnimation {
    pub from: Rc<AHashMap<DepthKey, f32>>,
    pub to: Rc<AHashMap<DepthKey, f32>>,
    pub span: f32,
    pub active: bool,
    pub eased: f32,
}

impl DepthAnimation {
    fn settled() -> Self {
        Self {
            from: Rc::new(AHashMap::new()),
            to: Rc::new(AHashMap::new()),
            span: 0.0,
            active: false,
            eased: 1.0,
        }
    }

    fn jump(&mut self, grouped: &GroupedBook) {
        let depths = Rc::new(depth_map(grouped));
        self.from = depths.clone();
        self.to = depths;
        self.span = 0.0;
        self.active = false;
        self.eased = 1.0;
    }

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

    fn current_ratio(&self, key: DepthKey) -> f32 {
        let from = self.from.get(&key).copied().unwrap_or(0.0);
        let to = self.to.get(&key).copied().unwrap_or(0.0);
        from + (to - from) * self.eased
    }

    pub(crate) fn snapshot(&self) -> DepthSnapshot {
        DepthSnapshot {
            from: self.from.clone(),
            to: self.to.clone(),
            eased: self.eased,
        }
    }
}

pub(crate) struct DepthSnapshot {
    from: Rc<AHashMap<DepthKey, f32>>,
    to: Rc<AHashMap<DepthKey, f32>>,
    eased: f32,
}

impl DepthSnapshot {
    pub(crate) fn ratio(&self, is_ask: bool, price: Fraction) -> f32 {
        let key = (is_ask, price.ratio().inner());
        let from = self.from.get(&key).copied().unwrap_or(0.0);
        let to = self.to.get(&key).copied().unwrap_or(0.0);
        (from + (to - from) * self.eased).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PanelRow<'a> {
    Ask(&'a GroupedLevel),
    Spread,
    Bid(&'a GroupedLevel),
}

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

pub(crate) fn split_ratio(start: f32, delta_y: f32, area_height: f32) -> f32 {
    if area_height <= 0.0 {
        return clamp_ratio(start);
    }
    clamp_ratio(start - delta_y / area_height)
}

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
    fn row_count(&self) -> usize {
        match self.grouped.as_deref() {
            Some(grouped) if !grouped.is_empty() => grouped.asks.len() + 1 + grouped.bids.len(),
            _ => 0,
        }
    }
}

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

    fn take_pending(&self) -> bool {
        self.pending.replace(false)
    }

    fn disarm(&self, app: &mut App) {
        self.pending.set(false);
        if let Some(timer) = self.timer.borrow_mut().take() {
            app.cancel_timer(&timer);
        }
    }

    fn release(&self, app: &mut App) {
        self.alive.set(false);
        self.disarm(app);
    }
}

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

pub(crate) struct PredictionBookPanel {
    pub book: Option<Arc<PredictionOrderBook>>,
    pub grouped: Option<Arc<GroupedBook>>,
    pub multiplier: u32,
    pub animation: DepthAnimation,
    pub anchor_pending: bool,
    pub leg_label: Option<SmolStr>,
    built: Option<BuiltPanel>,
}

impl PredictionBookPanel {
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

    pub(crate) fn set_book(&mut self, book: Arc<PredictionOrderBook>, multiplier: u32) {
        let same_ladder = self
            .book
            .as_ref()
            .is_some_and(|held| held.leg == book.leg && held.tick_size == book.tick_size);
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
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

    pub(crate) fn clear(&mut self) {
        self.book = None;
        self.grouped = None;
        self.leg_label = None;
        self.animation = DepthAnimation::settled();
        self.anchor_pending = false;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.grouped
            .as_ref()
            .is_none_or(|grouped| grouped.is_empty())
    }

    pub(crate) fn set_leg_label(&mut self, label: Option<SmolStr>) {
        self.leg_label = label;
    }

    pub(crate) fn has_grouping_choice(&self) -> bool {
        let Some(book) = self.book.as_ref() else {
            return false;
        };
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        has_levels && grouping_steps(book.tick_size).len() > 1
    }

    pub(crate) fn grouping_label(&self) -> Option<SmolStr> {
        let book = self.book.as_ref()?;
        let settled = settle_grouping(book.tick_size, self.multiplier);
        let label = grouping_steps(book.tick_size)
            .into_iter()
            .find(|(multiplier, _)| *multiplier == settled)
            .map_or_else(|| format_smolstr!("{}", book.tick_size), |(_, label)| label);
        Some(label)
    }

    pub(crate) fn advance(&mut self, reduce_motion: bool) -> bool {
        self.animation.advance(reduce_motion)
    }

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

    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(built) = self.built.take() else {
            return;
        };
        built.ticker.release(cx.app_mut());
        built.list.release(cx.app_mut());
        cx.app_mut().release_scope(built.scope);
    }

    #[cfg(test)]
    pub(crate) fn multiplier(&self) -> u32 {
        self.multiplier
    }

    #[cfg(test)]
    pub(crate) fn built_canvas(&self) -> Option<NodeId> {
        self.built.as_ref().map(|built| built.canvas)
    }

    #[cfg(test)]
    pub(crate) fn list_count(&self, app: &App) -> usize {
        self.built.as_ref().map_or(0, |built| built.list.count(app))
    }

    #[cfg(test)]
    pub(crate) fn mounted_rows(&self, app: &App) -> Vec<(usize, NodeId)> {
        self.built
            .as_ref()
            .map_or_else(Vec::new, |built| built.list.mounted_rows(app))
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset(&self, app: &App) -> Option<f32> {
        self.built.as_ref().and_then(|built| built.list.offset(app))
    }

    #[cfg(test)]
    pub(crate) fn scroll_to_item_for_test(
        &self,
        app: &mut App,
        index: usize,
        align: ItemAlign,
    ) -> Result<(), EngineError> {
        match self.built.as_ref() {
            Some(built) => built.list.scroll_to_item(app, index, align),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn timer_armed(&self) -> bool {
        self.built
            .as_ref()
            .is_some_and(|built| built.ticker.timer.borrow().is_some())
    }
}

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

fn aligned_text(size: f32) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.alignment = TextAlign::Right;
    style
}

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

fn level_cell(level: &GroupedLevel, tick: Tick, column: usize) -> SmolStr {
    match column {
        0 => format_cents(level.price, tick),
        1 => format_shares(level.size),
        _ => format_notional(level.total),
    }
}

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
