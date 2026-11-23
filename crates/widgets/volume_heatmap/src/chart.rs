//! The canvas that draws one heat map.
//!
//! A map is twenty coloured rounded blocks with two labels each, and it is redrawn
//! whenever the pointer moves across it. The tiles are laid out edge to edge and each
//! block is then drawn inside its own box, so what separates two neighbours is the pane
//! body showing through rather than an edge they share. Two things therefore have to be
//! cheap, and they are what this module is organised around:
//!
//! - **The geometry.** The tiles are laid out by a binary split that halves the
//!   remaining weight, which makes every tile's box a function of the box the canvas
//!   was given. A frame whose box and whose map are the ones the frame before already
//!   had keeps that geometry rather than computing it again, and the pointer is
//!   resolved against it instead of against a second copy.
//! - **The text.** Every label is a shaped line, which is the expensive part of a
//!   frame. The shaped lines of the map that is on screen are kept in a small bounded
//!   cache, keyed by everything that decides how a line looks, so a hover — which
//!   changes no text at all — re-shapes nothing.
//!
//! # The engine's canvas is a value
//!
//! A body is built once and the map is a [`NodeKind::Canvas`] node inside it; what a
//! frame draws is a [`Canvas`] value the widget hands to
//! [`WidgetCx::set_canvas`](widget::cx::WidgetCx::set_canvas) when, and only when, what
//! it would draw moved. The frame it was built from is kept beside it: a steady frame
//! therefore replaces nothing and asks for nothing (I3), and a frame that *did* have to
//! be built is recognised by a key that names every input the drawing is a function of —
//! the box, the map, the pointer, the appearance and the atlas generation the labels were
//! shaped against.
//!
//! The tiles themselves are plain quads: a rounded corner and an inner border are two
//! properties of one [`Quad`], so a block is one primitive and nothing here registers
//! GPU geometry. Only the labels are shaped (through [`App::shape_text`]) and pushed as
//! glyph runs, and a handle whose atlas lease is gone is re-shaped rather than drawn from
//! stale texture.
//!
//! # The pointer is followed one reading behind, on purpose
//!
//! A mouse crossing the map produces one move event per pixel, so a move only records the
//! tile it resolved to and the frame that follows draws from it: two positions inside one
//! tile are one drawing, so a movement that does not change the tile is not a frame.
//!
//! The canvas paints no background of its own: the pane body behind it is the
//! background, so the map never covers the body's rounded corners — and it is what shows
//! between two blocks.

use std::sync::Arc;

use base_ui::{
    style_builder::StyleBuilder,
    theme::{resolved::Theme, role::Role},
};
use i18n::t_smol;
use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, ClipRect, Quad, QuadPaint},
        style::StylePatch,
    },
    text::{ShapedTextHandle, TextAlign, TextStyle},
};
use widget::cx::WidgetCx;

use crate::model::{HeatMapEntry, PriceDirection, TILE_LIMIT};

/// The inset every label keeps from the edges of its block.
const TILE_TEXT_INSET: f32 = 4.0;
/// The smallest label of the map.
const TILE_MIN_FONT_SIZE: f32 = 10.0;
/// The largest label of the map.
const TILE_MAX_FONT_SIZE: f32 = 30.0;
/// How much taller a label's line is than its font, so two labels do not touch.
const TILE_LINE_HEIGHT_EXTRA: f32 = 3.0;
/// The outline of one block.
const TILE_BORDER_WIDTH: f32 = 1.0;
/// The radius one block's corners are rounded to.
///
/// It is the framework's `rounded_sm`, which is a quarter of the window's rem:
/// four pixels at the sixteen-pixel rem this application runs at, and the same
/// value the pane body's own corners are cut to.
const TILE_CORNER_RADIUS: f32 = 4.0;
/// How far a block is drawn inside the box the layout gave it.
///
/// Two neighbours therefore part by two pixels of pane body, which is what makes
/// the map read as separate blocks instead of a grid of cells that touch.
const TILE_GUTTER: f32 = 1.0;
/// How many shaped lines a map keeps.
///
/// Two per tile: a map has at most two labels each, and the labels of the map
/// before are still worth keeping while a new one arrives.
const TEXT_CACHE_CAPACITY: usize = TILE_LIMIT * 2;

/// The debug id of the canvas node the map draws on.
pub(crate) const HEATMAP_SELECTOR: &str = "TEST_VOLUME_HEATMAP_CANVAS";

/// One tile, ready to paint: what it says and which way its price went.
#[derive(Clone, Debug)]
struct PreparedTile {
    base_asset_label: SmolStr,
    notional_label: SmolStr,
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
    /// Lifts one map's labels out of the model, so a frame formats nothing.
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
                base_asset_label: entry.base_asset_label.clone(),
                notional_label: entry.notional_label.clone(),
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

/// One shaped line, and the style it was shaped in.
struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

/// The shaped lines of the map that is on screen.
///
/// A cache of forty entries with the least recently used one dropped is enough for
/// a map of twenty tiles twice over, and it is what makes a hover free: the tiles
/// all keep the same labels, so a frame that only moves the highlight finds every
/// line it needs.
///
/// The cache keeps lines a later map no longer draws until they are evicted, so it is
/// not what a steady frame asks about its atlas leases: only the lines the canvas on
/// screen was built from matter, and the chart records those ([`VolumeHeatMapChart::painted_text`]).
struct ChartTextCache {
    entries: Vec<CachedText>,
}

impl ChartTextCache {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }

    /// Shapes `text` in `style`, or returns the line that was shaped before.
    ///
    /// A handle whose atlas lease is gone is dropped and shaped again: the glyphs it
    /// named have been reused, so drawing it would be drawing another line's pixels.
    fn line(&mut self, app: &mut App, text: &str, style: &TextStyle) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            // Refreshing recency by moving the entry to the back is what makes the
            // capacity below evict the least recently used label.
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }
        let handle = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
            // Text that cannot be shaped at all is drawn as nothing rather
            // than failing the frame: a label is not worth a dropped frame,
            // and the next frame tries again.
            app.shape_text("", style, None, &[])
                .expect("an empty line always shapes")
        }));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedText {
            text: SmolStr::new(text),
            style: style.clone(),
            handle: Arc::clone(&handle),
        });
        handle
    }
}

/// The geometry of the frame that is already laid out.
///
/// It is keyed by the map and the box, which are the only two things the geometry
/// is a function of: a pointer position is resolved against the tiles rather than
/// against a second computation of them.
struct Layout {
    /// The box the map was laid out in, in window coordinates.
    bounds: Rect,
    /// The revision of the map it was laid out from.
    revision: u64,
    /// The tiles' boxes, in window coordinates.
    tiles: Box<[Rect]>,
}

/// The colours one frame of the map is drawn with, resolved from the theme.
///
/// The canvas bakes them in, which is why an appearance switch is one of the key's
/// inputs: a canvas built for the light appearance is the wrong value for the dark one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChartColors {
    bullish: Color,
    bearish: Color,
    flat: Color,
    border: Color,
    hover: Color,
    on_brand: Color,
    on_surface: Color,
}

impl ChartColors {
    /// The colours the theme resolves, for one appearance.
    fn resolve(theme: &Theme) -> Self {
        Self {
            bullish: theme.components().chart.bullish,
            bearish: theme.components().chart.bearish,
            flat: theme.role(Role::SurfaceRaised),
            border: theme.role(Role::LineDivider),
            // A tile keeps the colour of its direction under the pointer: a hover is a
            // neutral wash laid over the block rather than a second colour for it, so one
            // veil is correct on a bullish, a bearish and a flat tile alike.
            hover: theme.role(Role::VeilHover),
            on_brand: theme.role(Role::FgOnBrand),
            on_surface: theme.role(Role::FgDefault),
        }
    }

    /// The fill of one tile.
    fn tile(self, direction: PriceDirection) -> Color {
        match direction {
            PriceDirection::Up => self.bullish,
            PriceDirection::Down => self.bearish,
            PriceDirection::Flat => self.flat,
        }
    }

    /// The label colour of one tile.
    ///
    /// A label on a saturated block is drawn in the colour the theme pairs with a filled
    /// surface; a flat block is a neutral surface, so its label is ordinary text.
    fn label(self, direction: PriceDirection) -> Color {
        match direction {
            PriceDirection::Up | PriceDirection::Down => self.on_brand,
            PriceDirection::Flat => self.on_surface,
        }
    }
}

/// The inputs one canvas was built from.
///
/// Everything a frame reads has to be in here, or a change to it would keep serving
/// the canvas from before the change: the box it was laid out in, the map it draws,
/// the pointer, the colours, and the raster scale the labels were shaped at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    bounds: Rect,
    revision: u64,
    hover: Option<usize>,
    colors: ChartColors,
    text_scale: f32,
}

/// One heat map: the map it holds, the cache a frame reuses, and the node it draws into.
pub(crate) struct VolumeHeatMapChart {
    /// The canvas node, once the body built it.
    node: Option<NodeId>,
    data: PreparedHeatMap,
    /// Moves whenever `data` is rebuilt, which is how a prepared frame knows the map
    /// under it changed.
    revision: u64,
    layout: Option<Layout>,
    /// What the canvas on screen was built from.
    painted: Option<PaintedKey>,
    /// The shaped lines the canvas on screen was built from.
    ///
    /// A canvas is a value and the chart keeps the one it built, but the *glyphs* a
    /// canvas names are leased from a shared atlas: an atlas generation change or a
    /// per-tile eviction leaves the handles it was built from pointing at reused
    /// texture, and the engine drops such runs at encode time. The lines recorded here
    /// are the ones whose lease a steady frame has to check — a line the cache still
    /// holds from a map that is no longer drawn is not on the canvas, so it cannot
    /// keep a steady chart rebuilding.
    painted_text: Vec<Arc<ShapedTextHandle>>,
    text: ChartTextCache,
    /// The tile the pointer is over.
    hover: Option<usize>,
}

impl VolumeHeatMapChart {
    /// An empty map, with no canvas node yet.
    pub(crate) fn new() -> Self {
        Self {
            node: None,
            data: PreparedHeatMap::new(Arc::from([])),
            revision: 0,
            layout: None,
            painted: None,
            painted_text: Vec::new(),
            text: ChartTextCache::new(),
            hover: None,
        }
    }

    /// Rebuilds the map from a new snapshot, reporting whether there is anything
    /// to redraw.
    pub(crate) fn set_entries(&mut self, entries: Arc<[HeatMapEntry]>) -> bool {
        if Arc::ptr_eq(&self.data.entries, &entries) {
            return false;
        }
        self.data = PreparedHeatMap::new(entries);
        self.revision = self.revision.wrapping_add(1);
        self.painted = None;
        // Where the pointer was is an index into the map that was just replaced: the
        // frame that lays the new one out would drop it anyway, and doing it here as
        // well is what makes a hover that is already drawn impossible to see over the
        // new map for even one frame.
        self.layout = None;
        self.hover = None;
        true
    }

    /// Whether the map holds nothing to draw.
    ///
    /// A published map that covers no instrument is a fact about the venue rather
    /// than a pane that is still waiting, so the pane has to be able to tell the two
    /// apart: it says so over the empty canvas instead of leaving it blank.
    pub(crate) fn is_empty(&self) -> bool {
        self.data.entries.is_empty()
    }

    /// The canvas node, once the body built it.
    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// Builds the canvas node into `parent` and remembers it.
    ///
    /// The node starts empty (`Rect::default()`): it has no box until the first layout,
    /// and [`Self::sync`] is what fills it in. It registers two dependencies on purpose:
    /// the appearance (a canvas bakes its colours in, so an appearance switch has to
    /// schedule the frame that rebuilds it) and the language (the canvas' accessible name
    /// is resolved in the language showing now, and the read is what makes a language
    /// switch schedule the frame that rebuilds the pane's own copy).
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(HEATMAP_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(t_smol!(translate, "widget.names.volume_heatmap")),
                ..A11yOverride::default()
            })
            .bind_a11y(move |read| {
                let _language = read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.names.volume_heatmap")),
                    ..A11yPatch::default()
                }
            })
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    /// Rebuilds the canvas the map is drawn on, reporting whether it changed.
    ///
    /// The work is split in two: the *geometry* is rebuilt only when the box or the map
    /// moved, and the *canvas* is rebuilt only when what it would draw moved — the
    /// geometry, the pointer, the colours, the raster scale, or the atlas lease of a
    /// label it was built from.
    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(false);
        }

        // A frame that had to lay the map out again is a map whose tiles moved, so the
        // pointer that described the frame before it is dropped rather than carried
        // over rectangles that are not there any more.
        if self.ensure_layout(bounds) {
            self.hover = None;
        }

        let appearance = cx.theme().current(cx.app());
        let colors = ChartColors::resolve(cx.theme().theme_for(appearance));
        let text_scale = cx.app().text_scale();
        let key = PaintedKey {
            bounds,
            revision: self.revision,
            hover: self.hover,
            colors,
            text_scale,
        };
        if self.painted == Some(key) && !self.painted_text.iter().any(|handle| handle.is_stale()) {
            return Ok(false);
        }

        let Self {
            data,
            layout,
            text,
            hover,
            ..
        } = self;
        let Some(layout) = layout.as_ref() else {
            return Ok(false);
        };
        let (canvas, whole, painted_text) = paint(cx.app_mut(), data, layout, *hover, colors, text);
        let changed = cx.set_canvas(node, canvas)?;
        // An incomplete canvas is not what the chart draws: leaving the key unrecorded is
        // what makes the next frame build it again (I3 holds — a whole frame is still a
        // short circuit).
        if whole {
            self.painted = Some(key);
            self.painted_text = painted_text;
        }
        Ok(changed)
    }

    /// Lays the map out for `bounds`, reporting whether that was a computation.
    fn ensure_layout(&mut self, bounds: Rect) -> bool {
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.bounds == bounds && layout.revision == self.revision)
        {
            return false;
        }
        let tiles = layout_tiles(&self.data.layout_items, self.data.tiles.len(), bounds.size)
            .into_iter()
            .map(|tile| tile.translate(bounds.origin))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        self.layout = Some(Layout {
            bounds,
            revision: self.revision,
            tiles,
        });
        true
    }

    /// The tile under `position`, which is a window position rather than a local one.
    fn hit_test(&self, position: Point) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if !position.x.is_finite() || !position.y.is_finite() {
            return None;
        }
        layout
            .tiles
            .iter()
            .position(|bounds| contains_half_open(*bounds, position))
    }

    /// Records where the pointer is, reporting whether the tile under it moved.
    ///
    /// Moving inside one tile is not a movement: the drawing is the same, so a frame
    /// would draw the same pixels.
    pub(crate) fn point_at(&mut self, position: Point) -> bool {
        let next = self.hit_test(position);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    /// Records that the pointer left the map, reporting whether it had been on it.
    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    /// Releases the node this chart draws into.
    ///
    /// The node is the chart's own: the pane's body is built once, so a chart that goes
    /// away takes its canvas node with it. A node the pane already removed is not a
    /// failure — the framework tears the body down after this hook.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.layout = None;
        self.painted = None;
        self.painted_text.clear();
        self.hover = None;
    }

    /// The tile the last painted frame drew as hovered.
    #[cfg(test)]
    pub(crate) fn hovered(&self) -> Option<usize> {
        self.hover
    }

    /// The tiles of the frame that is laid out, in window coordinates.
    #[cfg(test)]
    pub(crate) fn tile_bounds(&self) -> Vec<Rect> {
        self.layout
            .as_ref()
            .map(|layout| layout.tiles.to_vec())
            .unwrap_or_default()
    }

    /// The canvas node's current content.
    #[cfg(test)]
    pub(crate) fn canvas_for_test<'a>(
        &self,
        app: &'a App,
        window: ui_core::window::WindowId,
    ) -> Option<&'a Canvas> {
        let node = self.node?;
        let target = app.window(window)?.tree().get(node).ok()?;
        match target.kind() {
            ui_core::node::tree::NodeKind::Canvas(canvas) => Some(canvas),
            _ => None,
        }
    }
}

/// Draws one frame of a map into a canvas.
///
/// Two passes, and the order is the whole of the drawing: every block, then every
/// label. A block is one quad — its fill, its rounded corners and its outline are
/// the same primitive — and the blocks share one scene order because the layout
/// produces disjoint boxes. Each label is clipped to its own block, which is what
/// lets it be drawn without measuring it: a label wider than its block is cut off at
/// the edge instead of spilling into the gutter or into the next block.
///
/// The returned flag says whether every label reached the canvas. A line whose atlas
/// lease died between being shaped and being pushed is refused by the engine rather
/// than encoded from reused texture, so the frame that ran into it is missing that run:
/// it is not a frame the chart can keep. The returned handles are the lines the canvas
/// was built from — the only ones whose lease the next steady frame has to check.
fn paint(
    app: &mut App,
    data: &PreparedHeatMap,
    layout: &Layout,
    hover: Option<usize>,
    colors: ChartColors,
    text: &mut ChartTextCache,
) -> (Canvas, bool, Vec<Arc<ShapedTextHandle>>) {
    let mut canvas = Canvas::new(layout.bounds);
    // The masked drawing the reference did: the frame is clipped to the box the node
    // ended up with, so a rounding error cannot paint over a sibling.
    let _ = canvas.push_clip(ClipRect {
        rect: layout.bounds,
        corner_radius: [0.0; 4],
    });
    for (index, (tile, bounds)) in data.tiles.iter().zip(layout.tiles.iter()).enumerate() {
        let Some(block) = tile_block_bounds(*bounds) else {
            continue;
        };
        let corner_radius = tile_corner_radii(block);
        canvas.push_quad(Quad {
            bounds: block,
            corner_radius,
            paint: QuadPaint::Solid {
                fill: colors.tile(tile.direction),
                border: Some(Border {
                    width: TILE_BORDER_WIDTH,
                    color: colors.border,
                }),
            },
        });
        if hover == Some(index) {
            canvas.push_quad(Quad {
                bounds: block,
                corner_radius,
                paint: QuadPaint::Solid {
                    fill: colors.hover,
                    border: None,
                },
            });
        }
    }

    let mut context = TextContext {
        app,
        text,
        used: Vec::with_capacity(data.tiles.len() * 2),
        whole: true,
    };
    for (tile, bounds) in data.tiles.iter().zip(layout.tiles.iter()) {
        let Some(block) = tile_block_bounds(*bounds) else {
            continue;
        };
        let font_size = tile_font_size(block, layout.bounds);
        let line_height = tile_line_height(font_size);
        let available_width = (block.size.width - TILE_TEXT_INSET * 2.0).max(0.0);
        if available_width <= 0.0 || block.size.height <= 0.0 {
            continue;
        }
        let style = label_style(font_size, line_height, colors.label(tile.direction));
        let _ = canvas.push_clip(ClipRect {
            rect: block,
            corner_radius: [0.0; 4],
        });
        let origin = Point::new(
            block.origin.x + TILE_TEXT_INSET,
            block.origin.y + TILE_TEXT_INSET,
        );
        context.label(&mut canvas, &tile.base_asset_label, origin, &style);
        context.label(
            &mut canvas,
            &tile.notional_label,
            Point::new(origin.x, origin.y + line_height),
            &style,
        );
        let _ = canvas.pop_clip();
    }
    let whole = context.whole;
    let painted_text = context.used;
    let _ = canvas.pop_clip();
    (canvas, whole, painted_text)
}

/// The shaped text one frame pushes, and whether every line reached the canvas.
struct TextContext<'a> {
    app: &'a mut App,
    text: &'a mut ChartTextCache,
    /// The lines this frame pushed, which are the canvas' text.
    used: Vec<Arc<ShapedTextHandle>>,
    whole: bool,
}

impl TextContext<'_> {
    /// Pushes one shaped label at `origin`.
    ///
    /// A refused line is one whose atlas lease died between being shaped and being
    /// pushed: its glyphs have been reused, so the run is left out rather than drawn
    /// from stale texture, and the pass is not a whole frame any more.
    fn label(&mut self, canvas: &mut Canvas, text: &str, origin: Point, style: &TextStyle) {
        if text.is_empty() {
            return;
        }
        let line = self.text.line(self.app, text, style);
        if canvas.push_shaped_text(&line, origin) {
            self.used.push(line);
        } else {
            self.whole = false;
        }
    }
}

/// The box one tile is drawn in: the box the layout gave it, pulled in by the
/// gutter on every side.
///
/// A tile too small to keep a pixel in both directions after that is not drawn at
/// all, rather than drawn as the sliver the gutter would leave of it.
fn tile_block_bounds(bounds: Rect) -> Option<Rect> {
    let block = Rect::new(
        bounds.origin.x + TILE_GUTTER,
        bounds.origin.y + TILE_GUTTER,
        (bounds.size.width - TILE_GUTTER * 2.0).max(0.0),
        (bounds.size.height - TILE_GUTTER * 2.0).max(0.0),
    );
    has_positive_area(block).then_some(block)
}

/// The corner radii of one block: the same radius on all four corners, and never
/// more than half of the block's short side.
///
/// Without the clamp a block smaller than its radius would get the sharp corners
/// that arcs larger than their box meet at, which is what a sliver of a tile looks
/// like; clamped, it is a pill or a dot.
fn tile_corner_radii(block: Rect) -> [f32; 4] {
    let limit = block.size.width.min(block.size.height) / 2.0;
    [TILE_CORNER_RADIUS.min(limit); 4]
}

/// How large a tile's labels are, which follows the share of the map it takes.
///
/// The ratio is taken to its square root because a font size is a length while the
/// share is an area: a tile four times the size of another then reads twice as
/// large rather than four times, which is what keeps the biggest tile from
/// drowning the rest.
fn tile_font_size(block: Rect, chart_bounds: Rect) -> f32 {
    let chart_area = bounds_area(chart_bounds);
    let area_ratio = if chart_area > 0.0 {
        bounds_area(block) / chart_area
    } else {
        0.0
    };
    font_size_for_area_ratio(area_ratio)
}

fn font_size_for_area_ratio(area_ratio: f32) -> f32 {
    let ratio = if area_ratio.is_finite() {
        area_ratio.clamp(0.0, 1.0).sqrt()
    } else {
        0.0
    };
    TILE_MIN_FONT_SIZE + (TILE_MAX_FONT_SIZE - TILE_MIN_FONT_SIZE) * ratio
}

fn tile_line_height(font_size: f32) -> f32 {
    font_size + TILE_LINE_HEIGHT_EXTRA
}

/// The style one tile's label is shaped in.
///
/// Alignment is *not* part of the placement: the label is placed from its block's own
/// corner, and its clipping is what keeps it inside.
fn label_style(font_size: f32, line_height: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.line_height = Some(line_height / font_size);
    style.alignment = TextAlign::Left;
    style
}

/// Places every tile of the map in the box the canvas was given.
///
/// The result is indexed by tile rather than by layout item: a tile with no weight
/// is not laid out and keeps an empty box, so the caller can zip it against the
/// tiles without tracking which ones were skipped.
fn layout_tiles(items: &[LayoutItem], tile_count: usize, canvas_size: Size) -> Vec<Rect> {
    let empty = Rect::new(0.0, 0.0, 0.0, 0.0);
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
        Rect::new(0.0, 0.0, canvas_size.width, canvas_size.height),
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
fn layout_binary(items: &[LayoutItem], total: f64, bounds: Rect, result: &mut [Rect]) {
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
            Rect::new(
                bounds.origin.x,
                bounds.origin.y,
                first_width,
                bounds.size.height,
            ),
            Rect::new(
                bounds.origin.x + first_width,
                bounds.origin.y,
                bounds.size.width - first_width,
                bounds.size.height,
            ),
        )
    } else {
        let first_height = bounds.size.height * ratio;
        (
            Rect::new(
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                first_height,
            ),
            Rect::new(
                bounds.origin.x,
                bounds.origin.y + first_height,
                bounds.size.width,
                bounds.size.height - first_height,
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

fn valid_size(value: Size) -> bool {
    value.width.is_finite() && value.height.is_finite() && value.width > 0.0 && value.height > 0.0
}

fn has_positive_area(bounds: Rect) -> bool {
    bounds.is_finite() && valid_size(bounds.size)
}

fn bounds_area(bounds: Rect) -> f32 {
    if !has_positive_area(bounds) {
        return 0.0;
    }
    bounds.size.width * bounds.size.height
}

/// Whether a point is inside a box, with the right and bottom edges belonging to
/// the tile after it.
///
/// Half-open edges are what make two neighbouring tiles exactly one pixel of
/// border apart instead of sharing the pixel the pointer is on.
fn contains_half_open(bounds: Rect, position: Point) -> bool {
    has_positive_area(bounds)
        && position.x >= bounds.origin.x
        && position.x < bounds.origin.x + bounds.size.width
        && position.y >= bounds.origin.y
        && position.y < bounds.origin.y + bounds.size.height
}

#[cfg(test)]
mod tests {
    use base_ui::theme::appearance::Appearance;
    use market_participants_model::decimal::Quantity;
    use ui_core::{
        math::Point,
        node::{builder::NodeBuilder, tree::NodeKind},
        paint::primitive::Primitive,
        window::WindowId,
    };
    use widget::test_support::WidgetTestApp;

    use super::*;

    /// The room the map is laid out in.
    const WIDTH: f32 = 480.0;
    const HEIGHT: f32 = 320.0;

    fn entry(base: &str, volume: u64, label: &str) -> HeatMapEntry {
        HeatMapEntry {
            instrument: market_participants_connector::catalog::test_support::spot_token(
                "fixture", base, "USDT",
            ),
            base_asset_label: SmolStr::new(base),
            notional_asset: market_participants_connector::catalog::test_support::token(
                "fixture", "USDT",
            ),
            notional: Quantity::from(volume),
            notional_label: SmolStr::new(label),
            direction: PriceDirection::Up,
        }
    }

    fn entries(bases: &[&str]) -> Arc<[HeatMapEntry]> {
        Arc::from(
            bases
                .iter()
                .map(|base| entry(base, 1, "1"))
                .collect::<Vec<_>>(),
        )
    }

    fn prepared(entries: Arc<[HeatMapEntry]>) -> PreparedHeatMap {
        PreparedHeatMap::new(entries)
    }

    fn area(bounds: Rect) -> f32 {
        bounds.size.width * bounds.size.height
    }

    fn overlap_area(left: Rect, right: Rect) -> f32 {
        let width = (left.origin.x + left.size.width).min(right.origin.x + right.size.width)
            - left.origin.x.max(right.origin.x);
        let height = (left.origin.y + left.size.height).min(right.origin.y + right.size.height)
            - left.origin.y.max(right.origin.y);
        width.max(0.0) * height.max(0.0)
    }

    fn assert_geometry_is_safe(rectangles: &[Rect], canvas: ui_core::math::Size) {
        for rectangle in rectangles.iter().copied() {
            assert!(rectangle.origin.x.is_finite());
            assert!(rectangle.origin.y.is_finite());
            assert!(rectangle.size.width.is_finite());
            assert!(rectangle.size.height.is_finite());
            assert!(rectangle.origin.x >= 0.0);
            assert!(rectangle.origin.y >= 0.0);
            assert!(rectangle.origin.x + rectangle.size.width <= canvas.width);
            assert!(rectangle.origin.y + rectangle.size.height <= canvas.height);
        }
        for (index, left) in rectangles.iter().copied().enumerate() {
            for right in rectangles.iter().copied().skip(index + 1) {
                assert!(overlap_area(left, right) <= f32::EPSILON);
            }
        }
    }

    /// One chart, drawn into a page of a fixed size.
    struct ChartHarness {
        app: WidgetTestApp,
        chart: VolumeHeatMapChart,
        window: WindowId,
        page: NodeId,
    }

    impl ChartHarness {
        fn sync(&mut self) -> bool {
            let Self { app, chart, .. } = self;
            app.with_cx(|cx| chart.sync(cx)).expect("the chart syncs")
        }

        fn canvas(&self) -> &Canvas {
            self.chart
                .canvas_for_test(self.app.app(), self.window)
                .expect("the chart drew a canvas")
        }

        fn release(&mut self) {
            let Self { app, chart, .. } = self;
            app.with_cx(|cx| chart.release(cx));
        }

        fn change_appearance(&mut self, appearance: Appearance) {
            let theme = self.app.services().theme.clone();
            theme.set_appearance(self.app.app_mut(), appearance);
        }

        /// Lays the page out at another size, the way a resize does.
        fn resize(&mut self, width: f32, height: f32) {
            let Self { app, page, .. } = self;
            let style = StyleBuilder::new()
                .width(width)
                .height(height)
                .flex_col()
                .build();
            app.with_cx(|cx| cx.tree_mut().set_style(*page, style))
                .expect("the page restyles");
            app.drain();
        }
    }

    /// Builds a chart over a page of `width` × `height`, lays it out, and paints it.
    fn harness_in(width: f32, height: f32, entries: Arc<[HeatMapEntry]>) -> ChartHarness {
        let mut app = WidgetTestApp::new();
        let window = app.window();
        let page = page(&mut app, width, height);
        let mut chart = app.with_cx(|_| VolumeHeatMapChart::new());
        chart.set_entries(entries);
        app.with_cx(|cx| chart.build(cx, page))
            .expect("the chart builds");
        // The first frame lays the page (and the canvas inside it) out; the sync after
        // it is the frame that paints the map the box was measured for.
        app.drain();
        let mut harness = ChartHarness {
            app,
            chart,
            window,
            page,
        };
        harness.sync();
        harness
    }

    fn harness() -> ChartHarness {
        harness_in(WIDTH, HEIGHT, entries(&["BTC", "ETH", "SOL"]))
    }

    /// A box of a fixed size, which is the shape a chart is laid out in.
    fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> NodeId {
        app.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_VOLUME_HEATMAP_PAGE")
                .style(
                    StyleBuilder::new()
                        .width(width)
                        .height(height)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the page builds")
        })
    }

    /// The canvas node's rendered primitives.
    fn primitives(harness: &ChartHarness) -> Vec<&Primitive> {
        harness.canvas().primitives().iter().collect()
    }

    /// The canvas value the chart's node holds, which says whether a frame rebuilt it.
    fn canvas_arc(harness: &ChartHarness) -> Arc<Canvas> {
        match harness
            .app
            .tree()
            .get(harness.chart.node().expect("the chart built"))
            .expect("the canvas node exists")
            .kind()
        {
            NodeKind::Canvas(canvas) => Arc::clone(canvas),
            other => panic!("the chart node is a canvas, not {other:?}"),
        }
    }

    fn quads(harness: &ChartHarness) -> Vec<&Quad> {
        primitives(harness)
            .into_iter()
            .filter_map(|primitive| match primitive {
                Primitive::Quad(quad) => Some(quad),
                _ => None,
            })
            .collect()
    }

    /// The generation of the atlas the canvas' glyphs were rasterized in, if it has any.
    fn glyph_generation(harness: &ChartHarness) -> Option<u32> {
        primitives(harness)
            .into_iter()
            .find_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first(),
                _ => None,
            })
            .map(|glyph| glyph.tile.generation)
    }

    /// A pointer in the middle of one of the laid-out tiles.
    fn middle_of(harness: &ChartHarness, index: usize) -> Point {
        let tile = harness.chart.tile_bounds()[index];
        Point::new(
            tile.origin.x + tile.size.width / 2.0,
            tile.origin.y + tile.size.height / 2.0,
        )
    }

    #[test]
    fn a_hover_should_be_a_wash_rather_than_a_colour_of_its_own() {
        let theme = Theme::of(Appearance::Light);
        let colors = ChartColors::resolve(&theme);
        let veil = ui_core::paint::color::ColorResolver.resolve(colors.hover);
        assert!(
            !veil.is_transparent(),
            "a hover is a wash laid over the tile, so it has an alpha"
        );
        assert!(
            !veil.is_opaque(),
            "and it is not opaque: the tile keeps the colour of its direction under it"
        );
    }

    #[test]
    fn binary_layout_should_be_contained_non_overlapping_and_area_weighted() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([
            entry("BTC", 6, "6"),
            entry("ETH", 3, "3"),
            entry("SOL", 1, "1"),
        ]);
        let data = prepared(entries);
        let canvas = ui_core::math::Size::new(300.0, 200.0);

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_geometry_is_safe(&rectangles, canvas);
        let total_area = rectangles.iter().copied().map(area).sum::<f32>();
        assert!((total_area - area(Rect::new(0.0, 0.0, canvas.width, canvas.height))).abs() < 0.1);
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
        let canvas = ui_core::math::Size::new(853.0, 479.0);

        let rectangles = layout_tiles(&items, items.len(), canvas);

        assert_geometry_is_safe(&rectangles, canvas);
    }

    #[test]
    fn a_tile_should_be_drawn_inset_by_the_gutter_on_every_side() {
        let tile = Rect::new(10.0, 20.0, 100.0, 60.0);

        let block = tile_block_bounds(tile).expect("a tile this size keeps a block");

        assert_eq!(block, Rect::new(11.0, 21.0, 98.0, 58.0));
        assert_eq!(
            block.origin,
            Point::new(tile.origin.x + TILE_GUTTER, tile.origin.y + TILE_GUTTER),
            "the block is what the labels and the outline are laid out in"
        );
    }

    /// Two neighbours part by two gutters, which is what makes the map read as
    /// separate blocks: the seam is the pane body rather than an edge they share.
    #[test]
    fn two_neighbours_should_part_by_two_gutters() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", 1, "1"), entry("ETH", 1, "1")]);
        let data = prepared(entries);
        let canvas = ui_core::math::Size::new(200.0, 100.0);

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);
        let left = tile_block_bounds(rectangles[0]).expect("the fixture lays both tiles out");
        let right = tile_block_bounds(rectangles[1]).expect("the fixture lays both tiles out");

        assert_eq!(
            right.origin.x - (left.origin.x + left.size.width),
            TILE_GUTTER * 2.0
        );
    }

    #[test]
    fn a_tile_the_gutter_would_eat_should_not_be_drawn() {
        let laid_out = Rect::new(4.0, 4.0, 40.0, 2.0);
        let not_laid_out = Rect::new(0.0, 0.0, 0.0, 0.0);

        assert!(tile_block_bounds(laid_out).is_none());
        assert!(tile_block_bounds(not_laid_out).is_none());
    }

    #[test]
    fn a_tile_corner_should_be_rounded_to_rounded_sm_and_never_past_half_the_tile() {
        assert_eq!(
            TILE_CORNER_RADIUS, 4.0,
            "the framework's rounded_sm is a quarter of the sixteen pixel rem"
        );
        assert_eq!(TILE_GUTTER, 1.0);

        assert_eq!(
            tile_corner_radii(Rect::new(0.0, 0.0, 120.0, 60.0)),
            [TILE_CORNER_RADIUS; 4]
        );
        assert_eq!(
            tile_corner_radii(Rect::new(0.0, 0.0, 6.0, 240.0)),
            [3.0; 4],
            "a block narrower than its radius is a pill rather than a shape with broken corners"
        );
    }

    #[test]
    fn binary_layout_should_fill_the_canvas_for_one_positive_entry() {
        let data = prepared(entries(&["BTC"]));
        let canvas = ui_core::math::Size::new(123.0, 57.0);

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_eq!(rectangles[0], Rect::new(0.0, 0.0, 123.0, 57.0));
    }

    #[test]
    fn binary_layout_should_ignore_zero_weight_without_dividing_by_zero() {
        let entries: Arc<[HeatMapEntry]> =
            Arc::from([entry("BTC", 10, "10"), entry("ZERO", 0, "0")]);
        let data = prepared(entries);
        let canvas = ui_core::math::Size::new(100.0, 80.0);

        let rectangles = layout_tiles(&data.layout_items, data.tiles.len(), canvas);

        assert_eq!(rectangles[0], Rect::new(0.0, 0.0, 100.0, 80.0));
        assert!(!has_positive_area(rectangles[1]));
    }

    #[test]
    fn binary_layout_should_return_zero_rectangles_for_all_zero_or_invalid_size() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", 0, "0"), entry("ETH", 0, "0")]);
        let data = prepared(entries);

        let zero = layout_tiles(
            &data.layout_items,
            data.tiles.len(),
            ui_core::math::Size::new(100.0, 80.0),
        );
        let invalid = layout_tiles(
            &[LayoutItem {
                index: 0,
                weight: 1.0,
            }],
            1,
            ui_core::math::Size::new(f32::NAN, f32::INFINITY),
        );

        assert!(zero.iter().all(|bounds| !has_positive_area(*bounds)));
        assert!(invalid.iter().all(|bounds| !has_positive_area(*bounds)));
    }

    #[test]
    fn hit_test_should_use_half_open_edges_and_never_hit_zero_area() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([
            entry("BTC", 1, "1"),
            entry("ETH", 1, "1"),
            entry("ZERO", 0, "0"),
        ]);
        let data = prepared(entries);
        let bounds = Rect::new(10.0, 20.0, 100.0, 50.0);
        let canvas = ui_core::math::Size::new(100.0, 50.0);
        let tiles = layout_tiles(&data.layout_items, data.tiles.len(), canvas)
            .into_iter()
            .map(|tile| tile.translate(bounds.origin))
            .collect::<Vec<_>>();
        let layout = Layout {
            bounds,
            revision: 0,
            tiles: tiles.into_boxed_slice(),
        };
        let first = layout.tiles[0];

        assert_eq!(
            layout.tiles.iter().position(|tile| contains_half_open(
                *tile,
                Point::new(first.origin.x + 1.0, first.origin.y + 1.0)
            )),
            Some(0)
        );
        assert!(
            !contains_half_open(
                first,
                Point::new(first.origin.x + first.size.width, first.origin.y + 1.0)
            ),
            "the right edge belongs to the tile after it"
        );
        assert_eq!(
            layout
                .tiles
                .iter()
                .position(|tile| contains_half_open(*tile, Point::new(f32::NAN, 20.0))),
            None
        );
        assert!(!has_positive_area(layout.tiles[2]));
    }

    #[test]
    fn prepared_heatmap_should_cache_display_labels_without_reformatting() {
        let entries: Arc<[HeatMapEntry]> = Arc::from([entry("BTC", 1_250_000, "1.25M")]);

        let data = prepared(entries);

        assert_eq!(data.tiles[0].base_asset_label.as_str(), "BTC");
        assert_eq!(data.tiles[0].notional_label.as_str(), "1.25M");
        assert!(data.layout_items[0].weight.is_finite());
    }

    #[test]
    fn text_cache_should_keep_a_bounded_lru_window() {
        let mut app = WidgetTestApp::new();
        let mut cache = ChartTextCache::new();
        let style = label_style(12.0, 15.0, Color::WHITE);
        for index in 0..TEXT_CACHE_CAPACITY {
            cache.line(app.app_mut(), &format!("label-{index}"), &style);
        }
        assert_eq!(cache.entries.len(), TEXT_CACHE_CAPACITY);
        cache.line(app.app_mut(), "new-label", &style);

        assert_eq!(cache.entries.len(), TEXT_CACHE_CAPACITY);
        assert!(
            !cache
                .entries
                .iter()
                .any(|line| line.text == SmolStr::new_static("label-0")),
            "the least recently used label is the one dropped"
        );
        assert!(
            cache
                .entries
                .iter()
                .any(|line| line.text == SmolStr::new_static("new-label"))
        );
    }

    #[test]
    fn text_cache_should_invalidate_for_style_color_and_font_size_changes() {
        let mut app = WidgetTestApp::new();
        let mut cache = ChartTextCache::new();
        let style = label_style(12.0, 15.0, Color::WHITE);
        let alternate_color = label_style(12.0, 15.0, Color::BLACK);
        let alternate_size = label_style(13.0, 16.25, Color::WHITE);

        let first = cache.line(app.app_mut(), "BTC", &style);
        let same = cache.line(app.app_mut(), "BTC", &style);
        assert!(Arc::ptr_eq(&first, &same), "the same line is shaped once");

        let recoloured = cache.line(app.app_mut(), "BTC", &alternate_color);
        assert!(!Arc::ptr_eq(&first, &recoloured));
        let resized = cache.line(app.app_mut(), "BTC", &alternate_size);
        assert!(!Arc::ptr_eq(&first, &resized));
        assert_eq!(cache.entries.len(), 3);
    }

    #[test]
    fn set_entries_should_reuse_prepared_data_for_the_same_arc() {
        let entries = entries(&["BTC"]);
        let mut chart = VolumeHeatMapChart::new();
        chart.set_entries(Arc::clone(&entries));

        assert!(!chart.set_entries(Arc::clone(&entries)));
        assert!(Arc::ptr_eq(&chart.data.entries, &entries));
    }

    #[test]
    fn set_entries_should_drop_a_hover_into_the_map_it_replaced() {
        let mut chart = VolumeHeatMapChart::new();
        chart.set_entries(entries(&["BTC"]));
        chart.hover = Some(0);

        assert!(chart.set_entries(entries(&["ETH"])));

        assert_eq!(chart.hovered(), None);
    }

    #[test]
    fn a_hovered_tile_should_be_told_apart_from_a_resting_one() {
        let mut harness = harness();
        let before = quads(&harness).len();
        let position = middle_of(&harness, 1);

        assert!(
            harness.chart.point_at(position),
            "the pointer moved onto a tile"
        );
        assert!(harness.sync(), "and the frame it asked for is a change");
        assert_eq!(harness.chart.hovered(), Some(1));

        let after = quads(&harness);
        assert_eq!(
            after.len(),
            before + 1,
            "a hover is one wash laid over the block it is on"
        );
        let veil = ChartColors::resolve(&Theme::of(Appearance::Light)).hover;
        assert_eq!(
            after
                .iter()
                .filter(|quad| matches!(&quad.paint, QuadPaint::Solid { fill, border: None } if *fill == veil))
                .count(),
            1,
            "the wash is the theme's own hover veil"
        );
    }

    #[test]
    fn a_pointer_move_inside_one_tile_should_not_be_a_change() {
        let mut harness = harness();
        let tile = harness.chart.tile_bounds()[0];
        let left = Point::new(tile.origin.x + 2.0, tile.origin.y + 2.0);
        let right = Point::new(tile.origin.x + 3.0, tile.origin.y + 3.0);

        assert!(harness.chart.point_at(left));
        assert!(!harness.chart.point_at(right), "one tile is one drawing");
        assert_eq!(harness.chart.hovered(), Some(0));
    }

    #[test]
    fn a_pointer_outside_the_map_should_hover_nothing() {
        let mut harness = harness();
        assert!(harness.chart.point_at(middle_of(&harness, 0)));
        assert_eq!(harness.chart.hovered(), Some(0));
        assert!(harness.sync(), "the hover is a frame");
        assert_eq!(
            quads(&harness).len(),
            4,
            "the wash is drawn over the hovered tile"
        );

        assert!(
            harness.chart.point_at(Point::new(-4.0, -4.0)),
            "leaving the map is a change of the tile under the pointer"
        );
        assert_eq!(harness.chart.hovered(), None);
        assert!(harness.sync());
        assert_eq!(quads(&harness).len(), 3, "one quad per tile and no wash");
    }

    #[test]
    fn a_resize_should_leave_nothing_hovered() {
        let mut harness = harness();
        assert!(harness.chart.point_at(middle_of(&harness, 2)));
        assert!(harness.sync());
        assert_eq!(harness.chart.hovered(), Some(2));
        let tiles_before = harness.chart.tile_bounds();

        // The page is laid out again at another size: what moved is the geometry, not
        // the pointer.
        harness.resize(WIDTH + 64.0, HEIGHT + 64.0);

        assert!(harness.sync(), "a new box is a new drawing");
        assert_eq!(
            harness.chart.hovered(),
            None,
            "a pointer that described the geometry just replaced is dropped"
        );
        assert_ne!(
            harness.chart.tile_bounds(),
            tiles_before,
            "the geometry really is the new one"
        );
    }

    #[test]
    fn a_steady_canvas_should_be_kept() {
        let mut harness = harness();
        let drawn = canvas_arc(&harness);
        assert!(!harness.sync(), "a steady frame paints nothing");
        assert!(!harness.sync(), "and keeps on painting nothing");
        assert!(
            Arc::ptr_eq(&drawn, &canvas_arc(&harness)),
            "a steady frame keeps the canvas it is drawn from"
        );
    }

    #[test]
    fn a_dark_appearance_should_repaint_the_map() {
        let mut harness = harness();
        let light = canvas_arc(&harness);
        assert_eq!(
            quads(&harness)[0].paint,
            QuadPaint::Solid {
                fill: ChartColors::resolve(&Theme::of(Appearance::Light)).tile(PriceDirection::Up),
                border: Some(Border {
                    width: TILE_BORDER_WIDTH,
                    color: ChartColors::resolve(&Theme::of(Appearance::Light)).border,
                }),
            }
        );

        harness.change_appearance(Appearance::Dark);

        assert!(
            harness.sync(),
            "another appearance is a change the pane has to draw"
        );
        let dark = canvas_arc(&harness);
        assert!(
            !Arc::ptr_eq(&light, &dark),
            "the redraw prepared a canvas of its own"
        );
        assert_eq!(
            quads(&harness)[0].paint,
            QuadPaint::Solid {
                fill: ChartColors::resolve(&Theme::of(Appearance::Dark)).tile(PriceDirection::Up),
                border: Some(Border {
                    width: TILE_BORDER_WIDTH,
                    color: ChartColors::resolve(&Theme::of(Appearance::Dark)).border,
                }),
            },
            "the dark appearance repaints the blocks in the dark roles"
        );
    }

    /// A tile is one quad — its fill, its corners and its outline — at the block's box.
    #[test]
    fn a_tile_should_be_painted_as_one_inset_rounded_quad() {
        let harness = harness();
        let tiles = harness.chart.tile_bounds();
        assert_eq!(tiles.len(), 3, "the fixture builds three tiles");
        let quads = quads(&harness);
        assert_eq!(
            quads.len(),
            tiles.len(),
            "the map is one quad per tile and nothing else"
        );
        let colors = ChartColors::resolve(&Theme::of(Appearance::Light));

        for (tile, quad) in tiles.iter().zip(&quads) {
            let block = tile_block_bounds(*tile).expect("a tile this size keeps a block");
            assert_eq!(quad.bounds, block, "a tile is drawn inside its gutter");
            assert_eq!(quad.corner_radius, [TILE_CORNER_RADIUS; 4]);
            assert_eq!(
                quad.paint,
                QuadPaint::Solid {
                    fill: colors.tile(PriceDirection::Up),
                    border: Some(Border {
                        width: TILE_BORDER_WIDTH,
                        color: colors.border,
                    }),
                }
            );
        }
    }

    /// A map of one tile draws one label pair, so a hover re-shapes nothing.
    #[test]
    fn a_hover_should_not_shape_any_label_again() {
        let mut harness = harness_in(WIDTH, HEIGHT, entries(&["BTC"]));
        let labels = harness.chart.text.entries.len();
        assert_eq!(labels, 2, "one tile has two labels");
        let before = primitives(&harness)
            .into_iter()
            .filter_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first().copied(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let held = harness
            .chart
            .text
            .entries
            .iter()
            .map(|entry| Arc::clone(&entry.handle))
            .collect::<Vec<_>>();

        assert!(harness.chart.point_at(middle_of(&harness, 0)));
        assert!(harness.sync());

        assert_eq!(harness.chart.text.entries.len(), labels);
        let after = harness
            .chart
            .text
            .entries
            .iter()
            .map(|entry| Arc::clone(&entry.handle))
            .collect::<Vec<_>>();
        assert!(
            after
                .iter()
                .zip(&held)
                .all(|(after, held)| Arc::ptr_eq(after, held)),
            "a hover changes no text, so every line it draws is the one already shaped"
        );
        let glyphs = primitives(&harness)
            .into_iter()
            .filter_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first().copied(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            glyphs.len(),
            before.len(),
            "the hover redraw kept both labels"
        );
    }

    /// The atlas a canvas' glyphs were leased from can be reset under a chart that would
    /// otherwise look steady. The engine drops runs whose lease is gone at encode time, so
    /// a chart that kept the canvas it already had would lose its labels for good. A moved
    /// lease is a frame the chart owes.
    #[test]
    fn a_chart_should_reshape_the_labels_when_their_atlas_lease_is_gone() {
        let mut harness = harness_in(WIDTH, HEIGHT, entries(&["BTC"]));
        let primitives_before = harness.canvas().primitives().len();
        let generation = glyph_generation(&harness).expect("the map labels its tiles");

        // An atlas reset under the same font stack: every handle the canvas was built from
        // is leased from an atlas that no longer exists, while the glyphs shape back the same.
        let stack = harness.app.app().font_stack().clone();
        harness.app.app_mut().set_font_stack(stack);

        assert!(
            harness.sync(),
            "a label whose lease moved is a frame the chart owes, not an idle one"
        );
        assert_ne!(
            glyph_generation(&harness),
            Some(generation),
            "every label was shaped for the atlas that exists now"
        );
        assert_eq!(
            harness.canvas().primitives().len(),
            primitives_before,
            "and none of them disappeared with the old lease"
        );
        assert!(
            !harness.sync(),
            "with fresh leases the frame is steady again (I3)"
        );
    }

    /// The labels of a map that replaced a bigger one stay in the text cache until they
    /// are evicted. An atlas reset makes those lines stale, but they are not on the
    /// canvas any more: only the lines the canvas was built from may keep a frame from
    /// being steady, or the chart would rebuild its canvas on every frame after such a
    /// reset.
    #[test]
    fn a_map_that_left_its_labels_behind_should_still_be_steady() {
        let big_map: Arc<[HeatMapEntry]> = Arc::from(
            (0..TILE_LIMIT)
                .map(|index| entry(&format!("ASSET{index}"), 1, &format!("{index}")))
                .collect::<Vec<_>>(),
        );
        let mut harness = harness_in(WIDTH, HEIGHT, big_map);
        assert_eq!(
            harness.chart.text.entries.len(),
            TEXT_CACHE_CAPACITY,
            "the first map fills the cache with lines of its own"
        );

        harness.chart.set_entries(entries(&["BTC"]));
        assert!(harness.sync(), "the smaller map is a frame");
        assert_eq!(
            harness.chart.painted_text.len(),
            2,
            "the canvas on screen is drawn from the two lines of the map that is there"
        );

        // An atlas reset under the same font stack: every line shaped so far is leased
        // from an atlas that no longer exists.
        let stack = harness.app.app().font_stack().clone();
        harness.app.app_mut().set_font_stack(stack);
        assert!(harness.sync(), "the lines on the canvas are shaped again");
        assert!(
            harness
                .chart
                .text
                .entries
                .iter()
                .any(|cached| cached.handle.is_stale()),
            "the cache still holds lines the canvas does not draw"
        );

        let kept = canvas_arc(&harness);
        assert!(
            !harness.sync(),
            "a stale line the canvas does not draw is not a frame the chart owes"
        );
        assert!(
            Arc::ptr_eq(&kept, &canvas_arc(&harness)),
            "so the steady frame keeps the canvas it is drawn from"
        );
    }

    /// A quad-only map registers no GPU geometry, and the store it does not use stays as
    /// it was: a frame that starts registering a mesh has to hand the previous one back.
    #[test]
    fn preparing_and_releasing_should_leave_the_geometry_store_alone() {
        let mut harness = harness();
        let baseline = harness.app.app().geometry().len();
        for _ in 0..3 {
            harness.sync();
            harness.chart.set_entries(entries(&["BTC", "ETH"]));
        }
        harness.sync();
        assert_eq!(
            harness.app.app().geometry().len(),
            baseline,
            "the map draws quads, not registered meshes"
        );

        harness.release();
        assert_eq!(
            harness.app.app().geometry().len(),
            baseline,
            "and releasing it leaves nothing behind"
        );
    }

    #[test]
    fn a_chart_should_not_draw_before_it_is_laid_out() {
        let mut app = WidgetTestApp::new();
        let mut chart = app.with_cx(|_| VolumeHeatMapChart::new());
        chart.set_entries(entries(&["BTC"]));
        let root = app.with_cx(|cx| cx.tree().root());
        app.with_cx(|cx| chart.build(cx, root))
            .expect("the chart builds");

        // No frame has laid the node out yet: it has no box, so there is nothing to draw.
        assert!(
            !app.with_cx(|cx| chart.sync(cx)).expect("the chart syncs"),
            "a chart that was not laid out draws nothing"
        );
    }

    #[test]
    fn an_empty_map_should_draw_nothing_but_its_clip() {
        let harness = harness_in(WIDTH, HEIGHT, Arc::from([]));
        assert!(harness.chart.is_empty());
        assert_eq!(
            primitives(&harness).len(),
            2,
            "an empty map is the frame's own clip and nothing else"
        );
    }
}
