use std::sync::Arc;

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
use ui_kit::{
    style_builder::StyleBuilder,
    theme::{resolved::Theme, role::Role},
};
use widget::cx::WidgetCx;

use crate::model::{HeatMapEntry, PriceDirection, TILE_LIMIT};

const TILE_TEXT_INSET: f32 = 4.0;
const TILE_MIN_FONT_SIZE: f32 = 10.0;
const TILE_MAX_FONT_SIZE: f32 = 30.0;
const TILE_LINE_HEIGHT_EXTRA: f32 = 3.0;
const TILE_BORDER_WIDTH: f32 = 1.0;
const TILE_CORNER_RADIUS: f32 = 4.0;
const TILE_GUTTER: f32 = 1.0;
const TEXT_CACHE_CAPACITY: usize = TILE_LIMIT * 2;

pub(crate) const HEATMAP_SELECTOR: &str = "TEST_VOLUME_HEATMAP_CANVAS";

#[derive(Clone, Debug)]
struct PreparedTile {
    base_asset_label: SmolStr,
    notional_label: SmolStr,
    direction: PriceDirection,
}

#[derive(Clone, Copy, Debug)]
struct LayoutItem {
    index: usize,
    weight: f64,
}

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
                base_asset_label: entry.base_asset_label.clone(),
                notional_label: entry.notional_label.clone(),
                direction: entry.direction,
            });
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

struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

struct ChartTextCache {
    entries: Vec<CachedText>,
}

impl ChartTextCache {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }

    fn line(&mut self, app: &mut App, text: &str, style: &TextStyle) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }
        let handle = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
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

struct Layout {
    bounds: Rect,
    revision: u64,
    tiles: Box<[Rect]>,
}

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
    fn resolve(theme: &Theme) -> Self {
        Self {
            bullish: theme.components().chart.bullish,
            bearish: theme.components().chart.bearish,
            flat: theme.role(Role::SurfaceRaised),
            border: theme.role(Role::LineDivider),
            hover: theme.role(Role::VeilHover),
            on_brand: theme.role(Role::FgOnBrand),
            on_surface: theme.role(Role::FgDefault),
        }
    }

    fn tile(self, direction: PriceDirection) -> Color {
        match direction {
            PriceDirection::Up => self.bullish,
            PriceDirection::Down => self.bearish,
            PriceDirection::Flat => self.flat,
        }
    }

    fn label(self, direction: PriceDirection) -> Color {
        match direction {
            PriceDirection::Up | PriceDirection::Down => self.on_brand,
            PriceDirection::Flat => self.on_surface,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    bounds: Rect,
    revision: u64,
    hover: Option<usize>,
    colors: ChartColors,
    text_scale: f32,
}

pub(crate) struct VolumeHeatMapChart {
    node: Option<NodeId>,
    data: PreparedHeatMap,
    revision: u64,
    layout: Option<Layout>,
    painted: Option<PaintedKey>,
    painted_text: Vec<Arc<ShapedTextHandle>>,
    text: ChartTextCache,
    hover: Option<usize>,
}

impl VolumeHeatMapChart {
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

    pub(crate) fn set_entries(&mut self, entries: Arc<[HeatMapEntry]>) -> bool {
        if Arc::ptr_eq(&self.data.entries, &entries) {
            return false;
        }
        self.data = PreparedHeatMap::new(entries);
        self.revision = self.revision.wrapping_add(1);
        self.painted = None;
        self.layout = None;
        self.hover = None;
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

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

    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Ok(false);
        }

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
        if whole {
            self.painted = Some(key);
            self.painted_text = painted_text;
        }
        Ok(changed)
    }

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

    pub(crate) fn point_at(&mut self, position: Point) -> bool {
        let next = self.hit_test(position);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

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

    #[cfg(test)]
    pub(crate) fn hovered(&self) -> Option<usize> {
        self.hover
    }

    #[cfg(test)]
    pub(crate) fn tile_bounds(&self) -> Vec<Rect> {
        self.layout
            .as_ref()
            .map(|layout| layout.tiles.to_vec())
            .unwrap_or_default()
    }

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

fn paint(
    app: &mut App,
    data: &PreparedHeatMap,
    layout: &Layout,
    hover: Option<usize>,
    colors: ChartColors,
    text: &mut ChartTextCache,
) -> (Canvas, bool, Vec<Arc<ShapedTextHandle>>) {
    let mut canvas = Canvas::new(layout.bounds);
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

struct TextContext<'a> {
    app: &'a mut App,
    text: &'a mut ChartTextCache,
    used: Vec<Arc<ShapedTextHandle>>,
    whole: bool,
}

impl TextContext<'_> {
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

fn tile_block_bounds(bounds: Rect) -> Option<Rect> {
    let block = Rect::new(
        bounds.origin.x + TILE_GUTTER,
        bounds.origin.y + TILE_GUTTER,
        (bounds.size.width - TILE_GUTTER * 2.0).max(0.0),
        (bounds.size.height - TILE_GUTTER * 2.0).max(0.0),
    );
    has_positive_area(block).then_some(block)
}

fn tile_corner_radii(block: Rect) -> [f32; 4] {
    let limit = block.size.width.min(block.size.height) / 2.0;
    [TILE_CORNER_RADIUS.min(limit); 4]
}

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

fn label_style(font_size: f32, line_height: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.line_height = Some(line_height / font_size);
    style.alignment = TextAlign::Left;
    style
}

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

fn contains_half_open(bounds: Rect, position: Point) -> bool {
    has_positive_area(bounds)
        && position.x >= bounds.origin.x
        && position.x < bounds.origin.x + bounds.size.width
        && position.y >= bounds.origin.y
        && position.y < bounds.origin.y + bounds.size.height
}

#[cfg(test)]
mod tests {
    use market_participants_model::decimal::Quantity;
    use ui_core::{
        math::Point,
        node::{builder::NodeBuilder, tree::NodeKind},
        paint::primitive::Primitive,
        window::WindowId,
    };
    use ui_kit::theme::appearance::Appearance;
    use widget::test_support::WidgetTestApp;

    use super::*;

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

    fn harness_in(width: f32, height: f32, entries: Arc<[HeatMapEntry]>) -> ChartHarness {
        let mut app = WidgetTestApp::new();
        let window = app.window();
        let page = page(&mut app, width, height);
        let mut chart = app.with_cx(|_| VolumeHeatMapChart::new());
        chart.set_entries(entries);
        app.with_cx(|cx| chart.build(cx, page))
            .expect("the chart builds");
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

    fn primitives(harness: &ChartHarness) -> Vec<&Primitive> {
        harness.canvas().primitives().iter().collect()
    }

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

    fn glyph_generation(harness: &ChartHarness) -> Option<u32> {
        primitives(harness)
            .into_iter()
            .find_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first(),
                _ => None,
            })
            .map(|glyph| glyph.tile.generation)
    }

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

    #[test]
    fn a_chart_should_reshape_the_labels_when_their_atlas_lease_is_gone() {
        let mut harness = harness_in(WIDTH, HEIGHT, entries(&["BTC"]));
        let primitives_before = harness.canvas().primitives().len();
        let generation = glyph_generation(&harness).expect("the map labels its tiles");

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
