//! The canvas that draws one heat map.
//!
//! A map is twenty coloured rounded blocks with two labels each, and it is
//! redrawn whenever the pointer moves across it. The tiles are laid out edge to
//! edge and each block is then drawn inside its own box, so what separates two
//! neighbours is the pane body showing through rather than an edge they share.
//! Two things therefore have to be cheap, and they are what this module is
//! organised around:
//!
//! - **The geometry.** The tiles are laid out by a binary split that halves the
//!   remaining weight, which makes every tile's box a function of the box the
//!   canvas was given. A frame whose box and whose map are the ones the frame
//!   before already had keeps that geometry rather than computing it again, and
//!   the pointer is resolved against it instead of against a second copy.
//! - **The text.** Every label is a shaped line, which is the expensive part of a
//!   frame. The shaped lines of the map that is on screen are kept in a small
//!   bounded cache, keyed by everything that decides how a line looks, so a hover
//!   — which changes no text at all — re-shapes nothing.
//!
//! The pointer is one frame behind on purpose. Moves arrive in bursts (one event
//! per pixel of travel), so a position is *queued* and *committed* by the frame
//! that follows it: a mouse crossing the map costs one frame, not one per pixel.
//! The commit happens in the paint pass, because the canvas is an element rather
//! than an entity — nothing renders it, so nothing else would ever apply what it
//! had queued.
//!
//! The canvas paints no background of its own: the pane body behind it is the
//! background, so the map never covers the body's rounded corners — and it is
//! what shows between two blocks.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    AnyElement, App, Bounds, ContentMask, Corners, Edges, ElementId, Hsla, InteractiveElement,
    IntoElement, MouseMoveEvent, ParentElement, Pixels, Point, ShapedLine, SharedString, Size,
    StatefulInteractiveElement, Styled, TextAlign, TextStyle, Window, canvas, div, fill, point, px,
    size,
};

use crate::model::{HeatMapEntry, PriceDirection, TILE_LIMIT};

/// The inset every label keeps from the edges of its block.
const TILE_TEXT_INSET: Pixels = px(4.0);
/// The smallest and largest label of the map.
const TILE_MIN_FONT_SIZE: Pixels = px(10.0);
const TILE_MAX_FONT_SIZE: Pixels = px(30.0);
/// How much taller a label's line is than its font, so two labels do not touch.
const TILE_LINE_HEIGHT_EXTRA: Pixels = px(3.0);
/// The outline of one block.
const TILE_BORDER_WIDTH: Pixels = px(1.0);
/// The radius one block's corners are rounded to.
///
/// It is the framework's `rounded_sm`, which is a quarter of the window's rem:
/// four pixels at the sixteen-pixel rem this application runs at, and the same
/// value the pane body's own corners are cut to.
const TILE_CORNER_RADIUS: Pixels = px(4.0);
/// How far a block is drawn inside the box the layout gave it.
///
/// Two neighbours therefore part by two pixels of pane body, which is what makes
/// the map read as separate blocks instead of a grid of cells that touch.
const TILE_GUTTER: Pixels = px(1.0);
/// How many shaped lines a map keeps.
///
/// Two per tile: a map has at most two labels each, and the labels of the map
/// before are still worth keeping while a new one arrives.
const TEXT_CACHE_CAPACITY: usize = TILE_LIMIT * 2;

/// One tile, ready to paint: what it says and which way its price went.
#[derive(Clone)]
struct PreparedTile {
    base_asset_label: SharedString,
    notional_label: SharedString,
    direction: PriceDirection,
}

/// One tile's share of the layout.
#[derive(Clone, Copy, Debug)]
struct LayoutItem {
    index: usize,
    weight: f64,
}

/// The map as a frame draws it: the labels already lifted out of the model, and
/// the weights the layout is a function of.
struct PreparedHeatMap {
    entries: Arc<[HeatMapEntry]>,
    tiles: Box<[PreparedTile]>,
    layout_items: Box<[LayoutItem]>,
}

impl PreparedHeatMap {
    fn new(entries: Arc<[HeatMapEntry]>) -> Self {
        let maximum = entries
            .iter()
            .map(|entry| entry.notional.inner())
            .max()
            .unwrap_or_default();
        let maximum = maximum as f64;
        let mut tiles = Vec::with_capacity(entries.len());
        let mut layout_items = Vec::with_capacity(entries.len());

        for (index, entry) in entries.iter().enumerate() {
            let weight = if maximum > 0.0 {
                entry.notional.inner() as f64 / maximum
            } else {
                0.0
            };
            tiles.push(PreparedTile {
                base_asset_label: SharedString::new(entry.base_asset_label.as_str()),
                notional_label: SharedString::new(entry.notional_label.as_str()),
                direction: entry.direction,
            });
            // A tile with no volume has no share of the map and is not laid out at
            // all: it would otherwise be given a sliver of the smallest size the
            // layout can express, which is a tile drawn for a market that did not
            // trade.
            if weight.is_finite() && weight > 0.0 {
                layout_items.push(LayoutItem { index, weight });
            }
        }

        Self {
            entries,
            tiles: tiles.into_boxed_slice(),
            layout_items: layout_items.into_boxed_slice(),
        }
    }
}

/// One shaped line, and what it was shaped from.
struct CachedTextLine {
    text: SharedString,
    color: Hsla,
    font_size: Pixels,
    line: Arc<ShapedLine>,
}

impl CachedTextLine {
    fn matches(&self, text: &SharedString, color: Hsla, font_size: Pixels) -> bool {
        self.text == *text && self.color == color && self.font_size == font_size
    }
}

/// The shaped lines of the map that is on screen.
///
/// A cache of forty entries with the least recently used one dropped is enough for
/// a map of twenty tiles twice over, and it is what makes a hover free: the tiles
/// all keep the same labels, so a frame that only moves the highlight finds every
/// line it needs.
struct ChartTextCache {
    text_style: Option<TextStyle>,
    lines: Vec<CachedTextLine>,
}

impl Default for ChartTextCache {
    fn default() -> Self {
        Self {
            text_style: None,
            lines: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }
}

impl ChartTextCache {
    /// Drops every line when the style they were shaped with is no longer the one
    /// the window uses, which is what a style or font change looks like here.
    fn sync_text_style(&mut self, text_style: &TextStyle) {
        if self.text_style.as_ref() != Some(text_style) {
            self.text_style = Some(text_style.clone());
            self.lines.clear();
        }
    }

    fn insert(&mut self, line: CachedTextLine) {
        if self.lines.len() >= TEXT_CACHE_CAPACITY {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }

    fn get(
        &mut self,
        text: &SharedString,
        color: Hsla,
        font_size: Pixels,
    ) -> Option<Arc<ShapedLine>> {
        let index = self
            .lines
            .iter()
            .position(|line| line.matches(text, color, font_size))?;
        // A hit is moved to the back, so what is dropped next is what has gone
        // unused the longest.
        let line = self.lines.remove(index);
        let shaped = line.line.clone();
        self.lines.push(line);
        Some(shaped)
    }

    fn line(
        &mut self,
        text: SharedString,
        color: Hsla,
        font_size: Pixels,
        window: &Window,
    ) -> Arc<ShapedLine> {
        let text_style = window.text_style();
        self.sync_text_style(&text_style);
        if let Some(line) = self.get(&text, color, font_size) {
            return line;
        }

        let text_len = text.len();
        let mut run = text_style.to_run(text_len);
        run.color = color;
        let line = Arc::new(
            window
                .text_system()
                .shape_line(text.clone(), font_size, &[run], None),
        );
        self.insert(CachedTextLine {
            text,
            color,
            font_size,
            line: line.clone(),
        });
        line
    }
}

/// The geometry of the frame that is already laid out.
///
/// It is keyed by the map and the box, which are the only two things the geometry
/// is a function of: a pointer position is resolved against the tiles rather than
/// against a second computation of them.
struct LayoutCache {
    entries: Option<Arc<[HeatMapEntry]>>,
    size: Option<Size<Pixels>>,
    origin: Point<Pixels>,
    tiles: Arc<[Bounds<Pixels>]>,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            entries: None,
            size: None,
            origin: point(Pixels::ZERO, Pixels::ZERO),
            tiles: Arc::from([]),
        }
    }
}

impl LayoutCache {
    /// Lays the map out for `bounds`, reporting whether that was a computation.
    ///
    /// The answer is what the caller needs to know that the pointer is no longer
    /// over what it was over: a recomputation means the tiles moved, and an index
    /// into the frame before describes a rectangle that is not there any more.
    fn resolve(&mut self, data: &PreparedHeatMap, bounds: Bounds<Pixels>) -> bool {
        self.origin = bounds.origin;
        let matches_entries = self
            .entries
            .as_ref()
            .is_some_and(|entries| Arc::ptr_eq(entries, &data.entries));
        if matches_entries && self.size == Some(bounds.size) {
            return false;
        }

        self.tiles = layout_tiles(&data.layout_items, data.tiles.len(), bounds.size).into();
        self.entries = Some(data.entries.clone());
        self.size = Some(bounds.size);
        true
    }

    /// The tile under `position`, which is a window position rather than a local
    /// one.
    fn hit_test(&self, position: Point<Pixels>) -> Option<usize> {
        if !position.x.as_f32().is_finite() || !position.y.as_f32().is_finite() {
            return None;
        }
        let local = point(position.x - self.origin.x, position.y - self.origin.y);
        self.tiles
            .iter()
            .position(|bounds| contains_half_open(*bounds, local))
    }
}

/// A pointer position the map has queued for the next frame.
#[derive(Default)]
struct HoverFrame {
    current: Option<Point<Pixels>>,
    pending: Option<Point<Pixels>>,
    scheduled: bool,
}

impl HoverFrame {
    /// Records where the pointer is, reporting whether a frame is worth asking for.
    fn queue(&mut self, next: Option<Point<Pixels>>) -> bool {
        self.pending = next;
        if self.scheduled || self.current == next {
            return false;
        }
        self.scheduled = true;
        true
    }

    /// Applies the queued position, reporting whether anything moved.
    fn commit(&mut self) -> bool {
        self.scheduled = false;
        if self.current == self.pending {
            return false;
        }
        self.current = self.pending;
        true
    }

    /// Applies the queued position and reports where the pointer is now.
    fn commit_current(&mut self) -> Option<Point<Pixels>> {
        self.commit();
        self.current
    }

    fn reset(&mut self) {
        self.current = None;
        self.pending = None;
    }
}

/// Everything one map's frames reuse between them.
///
/// It is held behind an `Rc` because the element that draws a map is rebuilt every
/// frame while this is not: the canvas callbacks take a clone of the `Rc` and the
/// widget keeps the original.
struct ChartState {
    layout: RefCell<LayoutCache>,
    text: RefCell<ChartTextCache>,
    hover: RefCell<HoverFrame>,
    /// The tile the last painted frame drew as hovered, which is what a test reads
    /// back and what the map before this one is compared against.
    hovered: Cell<Option<usize>>,
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            layout: RefCell::new(LayoutCache::default()),
            text: RefCell::new(ChartTextCache::default()),
            hover: RefCell::new(HoverFrame::default()),
            hovered: Cell::new(None),
        }
    }
}

/// One heat map: the tiles it draws and the frames it reuses.
pub(super) struct VolumeHeatMapChart {
    data: Rc<RefCell<PreparedHeatMap>>,
    state: Rc<ChartState>,
}

impl VolumeHeatMapChart {
    pub(super) fn new(entries: Arc<[HeatMapEntry]>) -> Self {
        Self {
            data: Rc::new(RefCell::new(PreparedHeatMap::new(entries))),
            state: Rc::new(ChartState::default()),
        }
    }

    /// Rebuilds the map from a new snapshot, reporting whether there is anything
    /// to redraw.
    pub(super) fn set_entries(&mut self, entries: Arc<[HeatMapEntry]>) -> bool {
        if Arc::ptr_eq(&self.data.borrow().entries, &entries) {
            return false;
        }
        *self.data.borrow_mut() = PreparedHeatMap::new(entries);
        // Where the pointer was is an index into the map that was just replaced.
        // The next frame lays the new one out and would clear this anyway; doing it
        // here as well is what makes a hover that is already drawn impossible to
        // see over the new map for even one frame.
        self.state.hover.borrow_mut().reset();
        self.state.hovered.set(None);
        true
    }

    /// Whether the map holds nothing to draw.
    ///
    /// A published map that covers no instrument is a fact about the venue rather
    /// than a pane that is still waiting, so the pane has to be able to tell the two
    /// apart: it says so over the empty canvas instead of leaving it blank.
    pub(super) fn is_empty(&self) -> bool {
        self.data.borrow().entries.is_empty()
    }

    /// The element a pane embeds, which is the map and the pointer handling it
    /// needs.
    ///
    /// The id comes from the pane rather than from this module because a window can
    /// hold more than one map, and interactive state is keyed by it: two panes must
    /// not share one.
    pub(super) fn element(&self, id: ElementId) -> AnyElement {
        let prepaint_data = self.data.clone();
        let prepaint_state = self.state.clone();
        let paint_data = self.data.clone();
        let paint_state = self.state.clone();
        let pointer_state = self.state.clone();
        let leave_state = self.state.clone();

        let map = canvas(
            move |bounds, _window, _cx| {
                let data = prepaint_data.borrow();
                resolve_frame(&data, &prepaint_state, bounds);
            },
            move |bounds, (), window, app| {
                let data = paint_data.borrow();
                let layout = paint_state.layout.borrow();
                // Committing here rather than in a `Render` is what makes a queued
                // move happen at all: a map is a canvas inside the pane's body,
                // not an entity, so nothing renders it and nothing else would ever
                // apply what it had queued.
                let pointer = paint_state.hover.borrow_mut().commit_current();
                let hovered = pointer.and_then(|position| layout.hit_test(position));
                paint_state.hovered.set(hovered);
                paint_heatmap(
                    &data,
                    bounds,
                    &layout,
                    hovered,
                    &paint_state.text,
                    window,
                    app,
                );
            },
        )
        .size_full();

        div()
            .id(id)
            .p_0p5()
            .size_full()
            .on_mouse_move(move |event: &MouseMoveEvent, window, _| {
                if pointer_state.hover.borrow_mut().queue(Some(event.position)) {
                    // One frame for the burst, not one per event.
                    window.on_next_frame(|window, _| window.refresh());
                }
            })
            .on_hover(move |hovering, window, _| {
                if !*hovering && leave_state.hover.borrow_mut().queue(None) {
                    window.on_next_frame(|window, _| window.refresh());
                }
            })
            .child(map)
            .into_any_element()
    }

    /// The tile the last painted frame drew as hovered.
    #[cfg(test)]
    pub(super) fn hovered(&self) -> Option<usize> {
        self.state.hovered.get()
    }

    /// The tiles of the frame that is laid out, in window coordinates.
    #[cfg(test)]
    pub(super) fn tile_bounds(&self) -> Vec<Bounds<Pixels>> {
        let layout = self.state.layout.borrow();
        layout
            .tiles
            .iter()
            .map(|bounds| *bounds + layout.origin)
            .collect()
    }
}

/// Lays out the frame the canvas is about to paint.
///
/// A geometry that had to be computed is one the frame before did not have, so the
/// pointer that described it is dropped. That is the whole of the rule this widget
/// has about resizing: a pane whose bounds changed — by being resized, by being
/// split, or by another pane taking its room — stops highlighting a tile until the
/// pointer says where it is again, rather than carrying an index from the old
/// geometry into a map of new rectangles.
fn resolve_frame(data: &PreparedHeatMap, state: &ChartState, bounds: Bounds<Pixels>) {
    if state.layout.borrow_mut().resolve(data, bounds) {
        state.hover.borrow_mut().reset();
        state.hovered.set(None);
    }
}

/// Draws one frame of a map.
///
/// Two passes, and the order is the whole of the drawing: every block, then every
/// label. A block is one quad — its fill, its rounded corners and its outline are
/// the same primitive — and the blocks share one scene order because the layout
/// produces disjoint boxes. The labels come last because `ShapedLine::paint` opens
/// a layer of its own: interleaving them with the blocks would break the quads
/// into one batch per tile.
fn paint_heatmap(
    data: &PreparedHeatMap,
    bounds: Bounds<Pixels>,
    layout: &LayoutCache,
    hovered: Option<usize>,
    text_cache: &RefCell<ChartTextCache>,
    window: &mut Window,
    app: &mut App,
) {
    let theme = app.theme();
    let border = theme.roles.line.divider.hsla;
    let hover_veil = theme.roles.veil.hover.hsla;
    let chart = &theme.components.chart;
    window.paint_layer(bounds, |window| {
        for (index, (tile, local_bounds)) in data.tiles.iter().zip(layout.tiles.iter()).enumerate()
        {
            let Some(block) = tile_block_bounds(*local_bounds + layout.origin) else {
                continue;
            };
            // A tile keeps the colour of its direction under the pointer: a hover is a
            // neutral wash laid over the block rather than a second colour for it, so one
            // veil is correct on a bullish, a bearish and a flat tile alike (§6.5).
            let background = match tile.direction {
                PriceDirection::Up => chart.bullish.hsla,
                PriceDirection::Down => chart.bearish.hsla,
                PriceDirection::Flat => theme.roles.surface.raised.hsla,
            };
            let corner_radii = tile_corner_radii(block.size);
            window.paint_quad(
                fill(block, background)
                    .corner_radii(corner_radii)
                    .border_widths(Edges::all(TILE_BORDER_WIDTH))
                    .border_color(border),
            );
            if hovered == Some(index) {
                window.paint_quad(fill(block, hover_veil).corner_radii(corner_radii));
            }
        }
    });

    // A label on a saturated block is drawn in the colour the theme pairs with a filled
    // surface; a flat block is a neutral surface, so its label is ordinary text.
    let on_brand = app.theme().roles.fg.on_brand.hsla;
    let on_surface = app.theme().roles.fg.default.hsla;
    for (tile, local_bounds) in data.tiles.iter().zip(layout.tiles.iter()) {
        let Some(block) = tile_block_bounds(*local_bounds + layout.origin) else {
            continue;
        };
        let font_size = tile_font_size(block, bounds);
        let text_color = match tile.direction {
            PriceDirection::Up | PriceDirection::Down => on_brand,
            PriceDirection::Flat => on_surface,
        };
        paint_tile_labels(tile, block, text_color, font_size, text_cache, window, app);
    }
}

/// The box one tile is drawn in: the box the layout gave it, pulled in by the
/// gutter on every side.
///
/// A tile too small to keep a pixel in both directions after that is not drawn at
/// all, rather than drawn as the sliver the gutter would leave of it.
fn tile_block_bounds(bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let block = Bounds::new(
        bounds.origin + point(TILE_GUTTER, TILE_GUTTER),
        size(
            (bounds.size.width - TILE_GUTTER * 2.0).max(Pixels::ZERO),
            (bounds.size.height - TILE_GUTTER * 2.0).max(Pixels::ZERO),
        ),
    );
    has_positive_area(block).then_some(block)
}

/// The corner radii of one block: the same radius on all four corners, and never
/// more than half of the block's short side.
///
/// Without the clamp a block smaller than its radius would get the sharp corners
/// that arcs larger than their box meet at, which is what a sliver of a tile looks
/// like; clamped, it is a pill or a dot.
fn tile_corner_radii(block_size: Size<Pixels>) -> Corners<Pixels> {
    Corners::all(TILE_CORNER_RADIUS).clamp_radii_for_quad_size(block_size)
}

/// Paints both labels of one tile, masked to the block.
///
/// The mask is what lets the labels be drawn without measuring them: a label wider
/// than its block is cut off at the edge instead of spilling into the gutter or
/// into the next block, which is also why nothing here needs the shaped line's own
/// width.
fn paint_tile_labels(
    tile: &PreparedTile,
    bounds: Bounds<Pixels>,
    color: Hsla,
    font_size: Pixels,
    text_cache: &RefCell<ChartTextCache>,
    window: &mut Window,
    app: &mut App,
) {
    let line_height = tile_line_height(font_size);
    let available_width = (bounds.size.width - TILE_TEXT_INSET * 2).max(Pixels::ZERO);
    if available_width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
        return;
    }
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        let origin = bounds.origin + point(TILE_TEXT_INSET, TILE_TEXT_INSET);
        paint_single_line(
            tile.base_asset_label.clone(),
            Bounds::new(origin, size(available_width, line_height)),
            font_size,
            color,
            text_cache,
            window,
            app,
        );
        paint_single_line(
            tile.notional_label.clone(),
            Bounds::new(
                point(origin.x, origin.y + line_height),
                size(available_width, line_height),
            ),
            font_size,
            color,
            text_cache,
            window,
            app,
        );
    });
}

fn paint_single_line(
    text: SharedString,
    bounds: Bounds<Pixels>,
    font_size: Pixels,
    color: Hsla,
    text_cache: &RefCell<ChartTextCache>,
    window: &mut Window,
    app: &mut App,
) {
    if text.is_empty() {
        return;
    }
    let line = text_cache.borrow_mut().line(text, color, font_size, window);
    let _ = line.paint(
        bounds.origin,
        tile_line_height(font_size),
        TextAlign::Left,
        Some(bounds.size.width),
        window,
        app,
    );
}

/// How large a tile's labels are, which follows the share of the map it takes.
///
/// The ratio is taken to its square root because a font size is a length while the
/// share is an area: a tile four times the size of another then reads twice as
/// large rather than four times, which is what keeps the biggest tile from
/// drowning the rest.
fn tile_font_size(bounds: Bounds<Pixels>, chart_bounds: Bounds<Pixels>) -> Pixels {
    let chart_area = bounds_area(chart_bounds);
    let area_ratio = if chart_area > 0.0 {
        bounds_area(bounds) / chart_area
    } else {
        0.0
    };
    font_size_for_area_ratio(area_ratio)
}

fn font_size_for_area_ratio(area_ratio: f32) -> Pixels {
    let ratio = if area_ratio.is_finite() {
        area_ratio.clamp(0.0, 1.0).sqrt()
    } else {
        0.0
    };
    let minimum = TILE_MIN_FONT_SIZE.as_f32();
    let range = TILE_MAX_FONT_SIZE.as_f32() - minimum;
    px(minimum + range * ratio)
}

fn tile_line_height(font_size: Pixels) -> Pixels {
    font_size + TILE_LINE_HEIGHT_EXTRA
}

/// Places every tile of the map in the box the canvas was given.
///
/// The result is indexed by tile rather than by layout item: a tile with no weight
/// is not laid out and keeps an empty box, so the caller can zip it against the
/// tiles without tracking which ones were skipped.
fn layout_tiles(
    items: &[LayoutItem],
    tile_count: usize,
    canvas_size: Size<Pixels>,
) -> Vec<Bounds<Pixels>> {
    let empty = Bounds::new(
        point(Pixels::ZERO, Pixels::ZERO),
        size(Pixels::ZERO, Pixels::ZERO),
    );
    let mut result = vec![empty; tile_count];
    if items.is_empty() || !valid_size(canvas_size) {
        return result;
    }
    let total = items.iter().map(|item| item.weight).sum::<f64>();
    if !total.is_finite() || total <= 0.0 {
        return result;
    }

    layout_binary(
        items,
        total,
        Bounds::new(point(Pixels::ZERO, Pixels::ZERO), canvas_size),
        &mut result,
    );
    result
}

/// Splits the remaining box in two, along its longer side, and recurses.
///
/// The split is chosen so that the first half carries half of the weight, which is
/// what makes big tiles big and keeps a tile's box from becoming a sliver: a
/// one-dimensional split of a two-dimensional share is only an approximation, but
/// it is one that never leaves a gap and never overlaps.
fn layout_binary(
    items: &[LayoutItem],
    total: f64,
    bounds: Bounds<Pixels>,
    result: &mut [Bounds<Pixels>],
) {
    let Some(first) = items.first() else {
        return;
    };
    if items.len() == 1 {
        if let Some(slot) = result.get_mut(first.index) {
            *slot = bounds;
        }
        return;
    }

    let (split, first_total) = balanced_split(items, total);
    if split == 0 || split >= items.len() || first_total <= 0.0 || first_total >= total {
        if let Some(slot) = result.get_mut(first.index) {
            *slot = bounds;
        }
        return;
    }
    let ratio = (first_total / total).clamp(0.0, 1.0) as f32;
    let (first_bounds, second_bounds) = if bounds.size.width >= bounds.size.height {
        let first_width = bounds.size.width * ratio;
        (
            Bounds::new(bounds.origin, size(first_width, bounds.size.height)),
            Bounds::new(
                point(bounds.origin.x + first_width, bounds.origin.y),
                size(bounds.size.width - first_width, bounds.size.height),
            ),
        )
    } else {
        let first_height = bounds.size.height * ratio;
        (
            Bounds::new(bounds.origin, size(bounds.size.width, first_height)),
            Bounds::new(
                point(bounds.origin.x, bounds.origin.y + first_height),
                size(bounds.size.width, bounds.size.height - first_height),
            ),
        )
    };

    layout_binary(&items[..split], first_total, first_bounds, result);
    layout_binary(&items[split..], total - first_total, second_bounds, result);
}

/// The split whose first half comes closest to carrying half of the weight.
fn balanced_split(items: &[LayoutItem], total: f64) -> (usize, f64) {
    let target = total / 2.0;
    let mut prefix = 0.0;
    let mut best_split = 1;
    let mut best_total = items.first().map_or(0.0, |item| item.weight);
    let mut best_distance = (best_total - target).abs();

    for split in 1..items.len() {
        prefix += items[split - 1].weight;
        let distance = (prefix - target).abs();
        if distance < best_distance {
            best_split = split;
            best_total = prefix;
            best_distance = distance;
        }
    }
    (best_split, best_total)
}

fn valid_size(value: Size<Pixels>) -> bool {
    value.width.as_f32().is_finite()
        && value.height.as_f32().is_finite()
        && value.width > Pixels::ZERO
        && value.height > Pixels::ZERO
}

fn has_positive_area(bounds: Bounds<Pixels>) -> bool {
    valid_size(bounds.size)
}

fn bounds_area(bounds: Bounds<Pixels>) -> f32 {
    if !has_positive_area(bounds) {
        return 0.0;
    }
    bounds.size.width.as_f32() * bounds.size.height.as_f32()
}

/// Whether a point is inside a box, with the right and bottom edges belonging to
/// the tile after it.
///
/// Half-open edges are what make two neighbouring tiles exactly one pixel of
/// border apart instead of sharing the pixel the pointer is on.
fn contains_half_open(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
    has_positive_area(bounds)
        && position.x >= bounds.origin.x
        && position.x < bounds.right()
        && position.y >= bounds.origin.y
        && position.y < bounds.bottom()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use base_ui::style::theme::Theme;
    use gpui_kit::{
        Context, Entity, IntoElement, Modifiers, ParentElement, Render, TestAppContext,
        VisualTestContext, Window, div, hsla,
    };
    use market_participants_model::decimal::Quantity;

    use super::*;

    /// The room the harness keeps around the map, so that a test has somewhere to
    /// put the pointer that is not the map.
    const HARNESS_PADDING: Pixels = px(16.0);

    /// One map, as a window mounts it.
    struct Harness {
        chart: VolumeHeatMapChart,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let canvas = self
                .chart
                .element(ElementId::Name(SharedString::new_static("TestCanvas")));
            div().size_full().p(HARNESS_PADDING).child(canvas)
        }
    }

    fn entry(base: &str, volume: &str, label: &str) -> HeatMapEntry {
        HeatMapEntry {
            notional_asset: market_participants_connector::catalog::test_support::token(
                "fixture", "USDT",
            ),
            instrument: market_participants_connector::catalog::test_support::spot_token(
                "fixture", base, "USDT",
            ),
            base_asset_label: base.into(),
            notional: Quantity::from_str(volume).expect("test quantity should parse"),
            notional_label: label.into(),
            direction: PriceDirection::Up,
        }
    }

    fn prepared(entries: Arc<[HeatMapEntry]>) -> PreparedHeatMap {
        PreparedHeatMap::new(entries)
    }

    fn area(bounds: Bounds<Pixels>) -> f32 {
        bounds.size.width.as_f32() * bounds.size.height.as_f32()
    }

    fn overlap_area(left: Bounds<Pixels>, right: Bounds<Pixels>) -> f32 {
        let width =
            (left.right().min(right.right()) - left.origin.x.max(right.origin.x)).max(Pixels::ZERO);
        let height = (left.bottom().min(right.bottom()) - left.origin.y.max(right.origin.y))
            .max(Pixels::ZERO);
        width.as_f32() * height.as_f32()
    }

    /// Whether a painted quad is where the map asked for it.
    ///
    /// `paint_quad` snaps a quad's edges to the device grid, so a painted box can
    /// sit a fraction of a logical pixel from the box it was given — while the one
    /// pixel the gutter holds back is twice that and stays visible here.
    fn snaps_to(painted: Bounds<Pixels>, asked_for: Bounds<Pixels>) -> bool {
        let close = |painted: Pixels, asked_for: Pixels| {
            (painted.as_f32() - asked_for.as_f32()).abs() <= 0.5
        };
        close(painted.left(), asked_for.left())
            && close(painted.top(), asked_for.top())
            && close(painted.right(), asked_for.right())
            && close(painted.bottom(), asked_for.bottom())
    }

    /// The box the tiles of a laid-out map together cover, which is the canvas
    /// they were laid out in.
    fn canvas_of(tiles: &[Bounds<Pixels>]) -> Bounds<Pixels> {
        tiles.iter().copied().fold(tiles[0], |canvas, tile| {
            Bounds::from_corners(
                point(canvas.left().min(tile.left()), canvas.top().min(tile.top())),
                point(
                    canvas.right().max(tile.right()),
                    canvas.bottom().max(tile.bottom()),
                ),
            )
        })
    }

    /// Whether a painted quad lies inside `canvas`, with the same slack the device
    /// grid needs.
    fn within(painted: Bounds<Pixels>, canvas: Bounds<Pixels>) -> bool {
        painted.left() >= canvas.left() - px(0.5)
            && painted.top() >= canvas.top() - px(0.5)
            && painted.right() <= canvas.right() + px(0.5)
            && painted.bottom() <= canvas.bottom() + px(0.5)
    }

    fn assert_geometry_is_safe(rectangles: &[Bounds<Pixels>], canvas: Size<Pixels>) {
        for rectangle in rectangles.iter().copied() {
            assert!(rectangle.origin.x.as_f32().is_finite());
            assert!(rectangle.origin.y.as_f32().is_finite());
            assert!(rectangle.size.width.as_f32().is_finite());
            assert!(rectangle.size.height.as_f32().is_finite());
            assert!(rectangle.origin.x >= Pixels::ZERO);
            assert!(rectangle.origin.y >= Pixels::ZERO);
            assert!(rectangle.right() <= canvas.width);
            assert!(rectangle.bottom() <= canvas.height);
        }
        for (index, left) in rectangles.iter().copied().enumerate() {
            for right in rectangles.iter().copied().skip(index + 1) {
                assert!(overlap_area(left, right) <= f32::EPSILON);
            }
        }
    }

    /// Mounts one map in a window of its own.
    ///
    /// A canvas resolves its geometry in prepaint, so a frame has to be drawn
    /// before anything is known about where a tile is.
    fn chart_window(
        cx: &mut TestAppContext,
        entries: Arc<[HeatMapEntry]>,
    ) -> (Entity<Harness>, &mut VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(base_ui::style::theme::install);
        let (harness, cx) = cx.add_window_view(|_, _| Harness {
            chart: VolumeHeatMapChart::new(entries),
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (harness, cx)
    }

    /// Delivers the frame a pointer move asked for.
    ///
    /// The move itself only queues a position: the frame that follows is what
    /// applies it, and that frame is the one the next-frame callback requests. A
    /// test has no frame loop, so it delivers both halves by hand.
    fn draw_the_frame_the_pointer_asked_for(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
        });
        cx.run_until_parked();
    }

    fn hovered(harness: &Entity<Harness>, cx: &mut VisualTestContext) -> Option<usize> {
        harness.read_with(cx, |harness, _| harness.chart.hovered())
    }

    fn tile_bounds(harness: &Entity<Harness>, cx: &mut VisualTestContext) -> Vec<Bounds<Pixels>> {
        harness.read_with(cx, |harness, _| harness.chart.tile_bounds())
    }

    /// The shaped lines of the map on screen, as pointers, so a test can tell a
    /// line that was reused from one that was shaped again.
    fn shaped_lines(harness: &Entity<Harness>, cx: &mut VisualTestContext) -> Vec<Arc<ShapedLine>> {
        harness.read_with(cx, |harness, _| {
            harness
                .chart
                .state
                .text
                .borrow()
                .lines
                .iter()
                .map(|line| line.line.clone())
                .collect()
        })
    }

    #[test]
    fn a_hovered_tile_should_be_told_from_a_resting_one() {
        let theme = Theme::default();
        let chart = &theme.components.chart;
        let veil = theme.roles.veil.hover.hsla;

        assert!(
            veil.a > 0.0 && veil.a < 1.0,
            "a hover is a wash laid over the tile, not a colour of its own"
        );
        for (name, resting) in [
            ("a bullish tile", chart.bullish.hsla),
            ("a bearish tile", chart.bearish.hsla),
            ("a flat tile", theme.roles.surface.raised.hsla),
        ] {
            assert_ne!(
                resting.blend(veil),
                resting,
                "{name} has to look different with the pointer on it"
            );
        }
    }

    #[test]
    fn binary_layout_should_be_contained_non_overlapping_and_area_weighted() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([
            entry("BTC", "6", "6"),
            entry("ETH", "3", "3"),
            entry("SOL", "1", "1"),
        ]);
        let data = prepared(entries);
        let canvas = size(px(300.0), px(200.0));

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_geometry_is_safe(&rectangles, canvas);
        let total_area = rectangles.iter().copied().map(area).sum::<f32>();
        assert!((total_area - area(Bounds::new(point(px(0.0), px(0.0)), canvas))).abs() < 0.1);
        assert!((area(rectangles[0]) / total_area - 0.6).abs() < 0.001);
        assert!((area(rectangles[1]) / total_area - 0.3).abs() < 0.001);
        assert!((area(rectangles[2]) / total_area - 0.1).abs() < 0.001);
    }

    #[test]
    fn background_layer_should_remain_non_overlapping_at_tile_capacity() {
        let items = (0..TILE_LIMIT)
            .map(|index| LayoutItem {
                index,
                weight: (TILE_LIMIT - index) as f64,
            })
            .collect::<Vec<_>>();
        let canvas = size(px(853.0), px(479.0));

        let rectangles = layout_tiles(&items, items.len(), canvas);

        assert_geometry_is_safe(&rectangles, canvas);
    }

    #[test]
    fn a_tile_should_be_drawn_inset_by_the_gutter_on_every_side() {
        let tile = Bounds::new(point(px(10.0), px(20.0)), size(px(100.0), px(60.0)));

        let block = tile_block_bounds(tile).expect("a tile this size keeps a block");

        assert_eq!(
            block,
            Bounds::new(point(px(11.0), px(21.0)), size(px(98.0), px(58.0)))
        );
        assert_eq!(
            block.origin - tile.origin,
            point(TILE_GUTTER, TILE_GUTTER),
            "the block is what the labels and the outline are laid out in"
        );
    }

    /// Two neighbours part by two gutters, which is what makes the map read as
    /// separate blocks: the seam is the pane body rather than an edge they share.
    #[test]
    fn two_neighbours_should_part_by_two_gutters() {
        let entries: Arc<[HeatMapEntry]> =
            Arc::from([entry("BTC", "1", "1"), entry("ETH", "1", "1")]);
        let data = prepared(entries);
        let canvas = size(px(200.0), px(100.0));

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);
        let left = tile_block_bounds(rectangles[0]).expect("the fixture lays both tiles out");
        let right = tile_block_bounds(rectangles[1]).expect("the fixture lays both tiles out");

        assert_eq!(right.left() - left.right(), TILE_GUTTER * 2.0);
    }

    #[test]
    fn a_tile_the_gutter_would_eat_should_not_be_drawn() {
        let laid_out = Bounds::new(point(px(4.0), px(4.0)), size(px(40.0), px(2.0)));
        let not_laid_out = Bounds::new(point(px(0.0), px(0.0)), size(Pixels::ZERO, Pixels::ZERO));

        assert!(tile_block_bounds(laid_out).is_none());
        assert!(tile_block_bounds(not_laid_out).is_none());
    }

    #[test]
    fn a_tile_corner_should_be_rounded_to_rounded_sm_and_never_past_half_the_tile() {
        assert_eq!(
            TILE_CORNER_RADIUS,
            px(4.0),
            "the framework's rounded_sm is a quarter of the sixteen pixel rem"
        );
        assert_eq!(TILE_GUTTER, px(1.0));

        assert_eq!(
            tile_corner_radii(size(px(120.0), px(60.0))),
            Corners::all(TILE_CORNER_RADIUS)
        );
        assert_eq!(
            tile_corner_radii(size(px(6.0), px(240.0))),
            Corners::all(px(3.0)),
            "a block narrower than its radius is a pill rather than a shape with broken corners"
        );
    }

    #[test]
    fn binary_layout_should_fill_the_canvas_for_one_positive_entry() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", "1", "1")]);
        let data = prepared(entries);
        let canvas = size(px(123.0), px(57.0));

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_eq!(rectangles[0], Bounds::new(point(px(0.0), px(0.0)), canvas));
    }

    #[test]
    fn binary_layout_should_ignore_zero_weight_without_dividing_by_zero() {
        let entries: Arc<[HeatMapEntry]> =
            Arc::from([entry("BTC", "10", "10"), entry("ZERO", "0", "0")]);
        let data = prepared(entries);
        let canvas = size(px(100.0), px(80.0));

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_eq!(rectangles[0], Bounds::new(point(px(0.0), px(0.0)), canvas));
        assert!(!has_positive_area(rectangles[1]));
    }

    #[test]
    fn binary_layout_should_return_zero_rectangles_for_all_zero_or_invalid_size() {
        let entries: Arc<[HeatMapEntry]> =
            Arc::from([entry("BTC", "0", "0"), entry("ETH", "0", "0")]);
        let data = prepared(entries);

        let zero = layout_tiles(
            &data.layout_items,
            data.tiles.len(),
            size(px(100.0), px(80.0)),
        );
        let invalid = layout_tiles(
            &[LayoutItem {
                index: 0,
                weight: 1.0,
            }],
            1,
            size(px(f32::NAN), px(f32::INFINITY)),
        );

        assert!(zero.iter().all(|bounds| !has_positive_area(*bounds)));
        assert!(invalid.iter().all(|bounds| !has_positive_area(*bounds)));
    }

    #[test]
    fn layout_cache_should_reuse_geometry_for_same_data_and_size_across_origins() {
        let entries: Arc<[HeatMapEntry]> =
            Arc::from([entry("BTC", "2", "2"), entry("ETH", "1", "1")]);
        let data = prepared(entries);
        let mut cache = LayoutCache::default();
        let dimensions = size(px(240.0), px(160.0));

        let first = Bounds::new(point(px(10.0), px(20.0)), dimensions);
        let second = Bounds::new(point(px(100.0), px(200.0)), dimensions);
        assert!(cache.resolve(&data, first));
        assert!(!cache.resolve(&data, second));

        let local = cache.tiles[0];
        assert_eq!(cache.hit_test(local.center() + second.origin), Some(0));
        assert_eq!(
            cache.hit_test(local.center() + first.origin),
            None,
            "hit testing should use the latest origin"
        );
    }

    #[test]
    fn layout_cache_should_invalidate_for_size_or_entries_arc_changes() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", "1", "1")]);
        let data = prepared(entries.clone());
        let equal_but_distinct = prepared(Arc::from(entries.to_vec()));
        let mut cache = LayoutCache::default();

        assert!(cache.resolve(
            &data,
            Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(80.0)))
        ));
        assert!(
            cache.resolve(
                &data,
                Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(80.0)))
            ),
            "a different box is a different geometry"
        );
        assert!(
            cache.resolve(
                &equal_but_distinct,
                Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(80.0)))
            ),
            "a different map is a different geometry, even at the same size"
        );
        assert!(
            !cache.resolve(
                &equal_but_distinct,
                Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(80.0)))
            ),
            "and the same one twice is not computed twice"
        );
    }

    #[test]
    fn hit_test_should_use_half_open_edges_and_never_hit_zero_area() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([
            entry("BTC", "1", "1"),
            entry("ETH", "1", "1"),
            entry("ZERO", "0", "0"),
        ]);
        let data = prepared(entries);
        let mut cache = LayoutCache::default();
        cache.resolve(
            &data,
            Bounds::new(point(px(10.0), px(20.0)), size(px(100.0), px(50.0))),
        );
        let first = cache.tiles[0] + point(px(10.0), px(20.0));

        assert_eq!(cache.hit_test(first.center()), Some(0));
        assert_ne!(
            cache.hit_test(point(first.right(), first.center().y)),
            Some(0)
        );
        assert_eq!(cache.hit_test(point(px(f32::NAN), px(20.0))), None);
        assert!(!has_positive_area(cache.tiles[2]));
    }

    #[test]
    fn prepared_heatmap_should_cache_display_labels_without_reformatting() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", "1250000", "1.25M")]);

        let data = prepared(entries);

        assert_eq!(data.tiles[0].base_asset_label.as_str(), "BTC");
        assert_eq!(data.tiles[0].notional_label.as_str(), "1.25M");
        assert!(data.layout_items[0].weight.is_finite());
    }

    #[test]
    fn hover_updates_should_commit_only_the_latest_pointer_per_frame() {
        let first = point(px(10.0), px(20.0));
        let latest = point(px(30.0), px(40.0));
        let mut state = HoverFrame::default();

        assert!(state.queue(Some(first)));
        assert!(!state.queue(Some(latest)));
        assert_eq!(state.current, None);
        assert!(state.commit());
        assert_eq!(state.current, Some(latest));
        assert!(!state.queue(Some(latest)));
        assert!(state.queue(None));
        assert!(state.commit());
        assert_eq!(state.current, None);
    }

    #[test]
    fn text_cache_should_keep_a_bounded_lru_window() {
        let mut cache = ChartTextCache::default();
        cache.sync_text_style(&TextStyle::default());

        for index in 0..TEXT_CACHE_CAPACITY {
            cache.insert(CachedTextLine {
                text: SharedString::new(format!("label-{index}")),
                color: hsla(0.0, 0.0, 0.5, 1.0),
                font_size: px(12.0),
                line: Arc::new(ShapedLine::default()),
            });
        }
        let first = SharedString::new_static("label-0");
        assert!(
            cache
                .get(&first, hsla(0.0, 0.0, 0.5, 1.0), px(12.0))
                .is_some()
        );
        cache.insert(CachedTextLine {
            text: SharedString::new_static("new-label"),
            color: hsla(0.0, 0.0, 0.5, 1.0),
            font_size: px(12.0),
            line: Arc::new(ShapedLine::default()),
        });

        assert_eq!(cache.lines.len(), TEXT_CACHE_CAPACITY);
        assert!(
            cache
                .lines
                .iter()
                .any(|line| line.text == SharedString::new_static("label-0"))
        );
        assert!(
            !cache
                .lines
                .iter()
                .any(|line| line.text == SharedString::new_static("label-1"))
        );
        assert!(
            cache
                .lines
                .iter()
                .any(|line| line.text == SharedString::new_static("new-label"))
        );
    }

    #[test]
    fn text_cache_should_invalidate_for_style_color_and_font_size_changes() {
        let text = SharedString::new_static("BTC");
        let color = hsla(0.0, 0.8, 0.5, 1.0);
        let alternate_color = hsla(0.6, 0.8, 0.5, 1.0);
        let font_size = px(12.0);
        let line = CachedTextLine {
            text: text.clone(),
            color,
            font_size,
            line: Arc::new(ShapedLine::default()),
        };

        assert!(line.matches(&text, color, font_size));
        assert!(!line.matches(&text, alternate_color, font_size));
        assert!(!line.matches(&text, color, px(13.0)));

        let mut cache = ChartTextCache::default();
        let style = TextStyle::default();
        cache.sync_text_style(&style);
        cache.insert(line);
        assert!(cache.get(&text, alternate_color, font_size).is_none());
        assert!(cache.get(&text, color, px(13.0)).is_none());
        assert!(cache.get(&text, color, font_size).is_some());
        cache.sync_text_style(&style);
        assert_eq!(cache.lines.len(), 1);

        let mut changed_style = style;
        changed_style.font_family = SharedString::new_static("different-font");
        cache.sync_text_style(&changed_style);
        assert!(cache.lines.is_empty());
    }

    #[test]
    fn set_entries_should_reuse_prepared_data_for_the_same_arc() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", "1", "1")]);
        let mut chart = VolumeHeatMapChart::new(entries.clone());
        let prepared = chart.data.clone();

        assert!(!chart.set_entries(entries));
        assert!(Arc::ptr_eq(
            &prepared.borrow().entries,
            &chart.data.borrow().entries
        ));
    }

    #[test]
    fn set_entries_should_drop_a_hover_into_the_map_it_replaced() {
        let mut chart = VolumeHeatMapChart::new(Arc::from([entry("BTC", "1", "1")]));
        chart
            .state
            .hover
            .borrow_mut()
            .queue(Some(point(px(5.0), px(5.0))));
        chart.state.hovered.set(Some(0));

        assert!(chart.set_entries(Arc::from([entry("ETH", "1", "1")])));

        assert_eq!(chart.hovered(), None);
        assert_eq!(chart.state.hover.borrow().current, None);
    }

    #[gpui_kit::test]
    fn a_pointer_over_a_tile_should_hover_it(cx: &mut TestAppContext) {
        let (harness, cx) = chart_window(
            cx,
            Arc::from([entry("BTC", "2", "2"), entry("ETH", "1", "1")]),
        );
        let tiles = tile_bounds(&harness, cx);
        assert_eq!(tiles.len(), 2, "the fixture builds two tiles");

        cx.simulate_mouse_move(tiles[1].center(), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);
        assert_eq!(hovered(&harness, cx), Some(1));

        cx.simulate_mouse_move(tiles[0].center(), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);
        assert_eq!(
            hovered(&harness, cx),
            Some(0),
            "the highlight follows the tile under the pointer"
        );
    }

    #[gpui_kit::test]
    fn a_pointer_outside_the_map_should_hover_nothing(cx: &mut TestAppContext) {
        let (harness, cx) = chart_window(cx, Arc::from([entry("BTC", "1", "1")]));
        let tile = tile_bounds(&harness, cx)[0];

        cx.simulate_mouse_move(tile.center(), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);
        assert_eq!(hovered(&harness, cx), Some(0));

        cx.simulate_mouse_move(point(px(2.0), px(2.0)), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);
        assert_eq!(
            hovered(&harness, cx),
            None,
            "a pointer that left the map is not over a tile"
        );
    }

    /// What one tile is worth of a frame: a single quad, drawn inside the box the
    /// layout gave it, rounded and outlined. It is what the change from four passes
    /// to one quad per tile bought, so it is asserted against the painted scene
    /// rather than against the code that asks for it.
    #[gpui_kit::test]
    fn a_tile_should_be_painted_as_one_inset_rounded_quad(cx: &mut TestAppContext) {
        let (harness, cx) = chart_window(
            cx,
            Arc::from([entry("BTC", "2", "2"), entry("ETH", "1", "1")]),
        );
        let tiles = tile_bounds(&harness, cx);
        let painted = cx.update(|window, _| {
            let scale_factor = window.scale_factor();
            window
                .painted_quads()
                .into_iter()
                .map(|quad| {
                    (
                        quad.bounds.map(|scaled| px(scaled.0 / scale_factor)),
                        quad.corner_radii.top_left.0 / scale_factor,
                        quad.border_widths.top.0 / scale_factor,
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(tiles.len(), 2, "the fixture builds two tiles");
        let canvas = canvas_of(&tiles);
        assert_eq!(
            painted
                .iter()
                .filter(|(painted, _, _)| within(*painted, canvas))
                .count(),
            tiles.len(),
            "the map is one quad per tile and nothing else: a pass of its own, such as an \
             outline, would be four more primitives per tile"
        );

        for tile in tiles {
            let block = tile_block_bounds(tile).expect("a tile this size keeps a block");
            let matching = painted
                .iter()
                .filter(|(painted, _, _)| snaps_to(*painted, block))
                .collect::<Vec<_>>();
            assert_eq!(
                matching.len(),
                1,
                "a tile is one quad — its fill, its corners and its outline — at the block's box"
            );
            assert_eq!(matching[0].1, TILE_CORNER_RADIUS.as_f32());
            assert_eq!(matching[0].2, TILE_BORDER_WIDTH.as_f32());
            assert!(
                !painted
                    .iter()
                    .any(|(painted, _, _)| snaps_to(*painted, tile)),
                "the block is inset rather than drawn edge to edge"
            );
        }
    }

    #[gpui_kit::test]
    fn a_hover_should_not_shape_any_label_again(cx: &mut TestAppContext) {
        let (harness, cx) = chart_window(cx, Arc::from([entry("BTC", "1", "1")]));
        let tile = tile_bounds(&harness, cx)[0];
        let before = shaped_lines(&harness, cx);
        assert_eq!(before.len(), 2, "one tile has two labels");

        cx.simulate_mouse_move(tile.center(), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);

        let after = shaped_lines(&harness, cx);
        assert_eq!(after.len(), before.len());
        assert!(
            after
                .iter()
                .zip(before.iter())
                .all(|(after, before)| Arc::ptr_eq(after, before)),
            "a hover changes no text, so every line it draws is the one already shaped"
        );
    }

    #[test]
    fn a_recomputed_frame_should_drop_the_pointer_that_described_it() {
        let chart =
            VolumeHeatMapChart::new(Arc::from([entry("BTC", "2", "2"), entry("ETH", "1", "1")]));
        let data = chart.data.borrow();
        let state = &chart.state;
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(120.0)));
        let pointer = point(px(30.0), px(40.0));

        // The first frame computes the geometry, and there is no earlier one for a
        // pointer to have described.
        resolve_frame(&data, state, bounds);
        state.hover.borrow_mut().queue(Some(pointer));
        state.hover.borrow_mut().commit();
        state.hovered.set(Some(0));

        resolve_frame(&data, state, bounds);
        assert_eq!(
            state.hover.borrow().current,
            Some(pointer),
            "a frame that reused its geometry keeps the hover it was drawn with"
        );

        resolve_frame(
            &data,
            state,
            Bounds::new(point(px(0.0), px(0.0)), size(px(260.0), px(120.0))),
        );
        assert_eq!(
            state.hover.borrow().current,
            None,
            "a pointer that described the geometry just replaced is dropped"
        );
        assert_eq!(state.hovered.get(), None);
    }

    #[gpui_kit::test]
    fn a_resize_should_leave_nothing_hovered(cx: &mut TestAppContext) {
        let (harness, cx) = chart_window(
            cx,
            Arc::from([
                entry("BTC", "3", "3"),
                entry("ETH", "2", "2"),
                entry("SOL", "1", "1"),
            ]),
        );
        let tiles = tile_bounds(&harness, cx);
        cx.simulate_mouse_move(tiles[2].center(), None, Modifiers::default());
        draw_the_frame_the_pointer_asked_for(cx);
        assert_eq!(hovered(&harness, cx), Some(2));

        // A window that grew, so the pointer is still inside it: what moved is the
        // geometry, not the pointer.
        let window_size = cx.update(|window, _| window.bounds().size);
        cx.simulate_resize(size(
            window_size.width + px(64.0),
            window_size.height + px(64.0),
        ));
        draw_the_frame_the_pointer_asked_for(cx);

        assert_eq!(hovered(&harness, cx), None);
    }
}
