use bytes::Bytes;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, Path, Quad, QuadPaint},
        style::StylePatch,
    },
    text::{TextContent, TextStyle},
};
use ui_kit::theme::{resolved::Theme, role::Role};
use widget::cx::WidgetCx;

use crate::model::Level;

pub(crate) const GAUGE_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_GAUGE";

const SEGMENT_COUNT: usize = 5;

const CAP_COUNT: usize = SEGMENT_COUNT * 2;

const SEGMENT_SWEEP: f32 = 180.0 / SEGMENT_COUNT as f32;

const START_ANGLE: f32 = 180.0;

const SWEEP: f32 = 180.0;

const BAND_RATIO: f32 = 0.09;

const GAP_RATIO: f32 = 0.65;

const MIN_CANVAS_WIDTH: LogicalPx = px(96.0);

const MIN_CANVAS_HEIGHT: LogicalPx = px(72.0);

const SCORE_TEXT_RADIUS_RATIO: f32 = 0.35;

const SCORE_TEXT_FALLBACK_RATIO: f32 = 0.28;

const LEVEL_TEXT_RATIO: f32 = 0.55;

const LINE_HEIGHT_RATIO: f32 = 1.15;

const TEXT_LINE_GAP_RATIO: f32 = 0.15;

const TEXT_ABOVE_DIAMETER_RATIO: f32 = 0.1;

const INDICATOR_RATIO: f32 = 1.33;

const INDICATOR_HALO_RATIO: f32 = 0.2;

const MIN_INDICATOR_HALO: LogicalPx = px(2.0);

const MAX_INDICATOR_HALO: LogicalPx = px(16.0);

const MESH_STEP_LENGTH: LogicalPx = px(2.0);

const MESH_MIN_STEPS: f32 = 4.0;

const MESH_MAX_STEPS: f32 = 48.0;

const VERTEX_STRIDE_BYTES: u32 = 12;

const EDGE_COVERAGE: f32 = 0.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeGeometry {
    center: LogicalPoint,
    cap_radius: LogicalPx,
    band_radius: LogicalPx,
    outer_radius: LogicalPx,
    inner_radius: LogicalPx,
    indicator_radius: LogicalPx,
    indicator_halo: LogicalPx,
    gap_half: f32,
}

impl GaugeGeometry {
    fn compute(bounds: LogicalRect) -> Option<Self> {
        let width = bounds.size.width;
        let height = bounds.size.height;
        if !width.is_finite() || !height.is_finite() {
            return None;
        }
        if width < MIN_CANVAS_WIDTH || height < MIN_CANVAS_HEIGHT {
            return None;
        }

        let band = (height * BAND_RATIO).min(width * BAND_RATIO);
        let cap_radius = band / 2.0;
        let indicator_radius = cap_radius * INDICATOR_RATIO;
        let indicator_halo =
            (band * INDICATOR_HALO_RATIO).clamp(MIN_INDICATOR_HALO, MAX_INDICATOR_HALO);
        let indicator_reach = indicator_radius + indicator_halo;
        let band_radius = (width / 2.0 - indicator_reach).min(height - indicator_reach * 2.0);
        if band_radius <= LogicalPx::ZERO {
            return None;
        }
        let gap_half = ((band * GAP_RATIO) / band_radius).to_degrees();

        Some(Self {
            center: LogicalPoint::new(
                bounds.origin.x + width / 2.0,
                bounds.origin.y + (height + band_radius) / 2.0,
            ),
            cap_radius,
            band_radius,
            outer_radius: band_radius + cap_radius,
            inner_radius: band_radius - cap_radius,
            indicator_radius,
            indicator_halo,
            gap_half,
        })
    }

    fn segment_span(&self, index: usize) -> (f32, f32) {
        let start = START_ANGLE + SEGMENT_SWEEP * index as f32;

        (start, start + SEGMENT_SWEEP)
    }

    fn arc_span(&self, index: usize) -> (f32, f32) {
        let (start, end) = self.segment_span(index);

        (start + self.gap_half, end - self.gap_half)
    }

    fn point_at(&self, angle: f32, radius: LogicalPx) -> LogicalPoint {
        let radians = angle.to_radians();

        LogicalPoint::new(
            self.center.x + radius * radians.cos(),
            self.center.y + radius * radians.sin(),
        )
    }

    fn cap_centers(&self) -> [(LogicalPoint, usize); CAP_COUNT] {
        std::array::from_fn(|index| {
            let segment = index / 2;
            let (start, end) = self.arc_span(segment);
            let angle = if index % 2 == 0 { start } else { end };

            (self.point_at(angle, self.band_radius), segment)
        })
    }

    fn indicator_center(&self, score: u8) -> LogicalPoint {
        let angle = START_ANGLE + SWEEP * f32::from(score) / 100.0;

        self.point_at(angle, self.band_radius)
    }

    fn cap_quads(&self, colors: &GaugeColors) -> [Quad; CAP_COUNT] {
        self.cap_centers()
            .map(|(center, segment)| circle_quad(center, self.cap_radius, colors.segments[segment]))
    }

    fn indicator_quads(&self, score: u8, colors: &GaugeColors) -> (Quad, Quad) {
        let center = self.indicator_center(score);
        let radius = self.indicator_radius;

        (
            circle_quad(center, radius + self.indicator_halo, colors.halo),
            circle_quad(center, radius, colors.text),
        )
    }

    fn band_mesh(&self, index: usize) -> BandMesh {
        let (start, end) = self.arc_span(index);
        let arc_length = self.band_radius * (end - start).to_radians();
        let steps = (arc_length / MESH_STEP_LENGTH)
            .ceil()
            .clamp(MESH_MIN_STEPS, MESH_MAX_STEPS) as usize;
        let rings = [
            (self.outer_radius, EDGE_COVERAGE),
            (self.band_radius, -self.cap_radius.value()),
            (self.inner_radius, EDGE_COVERAGE),
        ];

        let mut vertices = Vec::with_capacity((steps + 1) * rings.len() * 3);
        let mut min = LogicalPoint::new(px(f32::INFINITY), px(f32::INFINITY));
        let mut max = LogicalPoint::new(px(f32::NEG_INFINITY), px(f32::NEG_INFINITY));
        for step in 0..=steps {
            let angle = start + (end - start) * step as f32 / steps as f32;
            for (radius, coverage) in rings {
                let point = self.point_at(angle, radius);
                min = LogicalPoint::new(min.x.min(point.x), min.y.min(point.y));
                max = LogicalPoint::new(max.x.max(point.x), max.y.max(point.y));
                vertices.extend_from_slice(&[point.x.value(), point.y.value(), coverage]);
            }
        }

        let mut indices = Vec::with_capacity(steps * 12);
        for step in 0..steps {
            let near = (step * rings.len()) as u32;
            let far = near + rings.len() as u32;
            indices.extend_from_slice(&[near, near + 1, far, far, near + 1, far + 1]);
            indices.extend_from_slice(&[near + 1, near + 2, far + 1, far + 1, near + 2, far + 2]);
        }

        BandMesh {
            vertices,
            indices,
            bounds: LogicalRect::new(min.x, min.y, max.x - min.x, max.y - min.y),
        }
    }
}

struct BandMesh {
    vertices: Vec<f32>,
    indices: Vec<u32>,
    bounds: LogicalRect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeColors {
    segments: [Color; SEGMENT_COUNT],
    halo: Color,
    text: Color,
}

impl GaugeColors {
    fn resolve(theme: &Theme) -> Self {
        Self {
            segments: [
                level_color(theme, Level::ExtremeFear),
                level_color(theme, Level::Fear),
                level_color(theme, Level::Neutral),
                level_color(theme, Level::Greed),
                level_color(theme, Level::ExtremeGreed),
            ],
            halo: theme.role(Role::SurfaceSunken),
            text: theme.role(Role::FgDefault),
        }
    }
}

pub(crate) fn level_color(theme: &Theme, level: Level) -> Color {
    theme.role(match level {
        Level::ExtremeFear => Role::FearGreedExtremeFear,
        Level::Fear => Role::FearGreedFear,
        Level::Neutral => Role::FearGreedNeutral,
        Level::Greed => Role::FearGreedGreed,
        Level::ExtremeGreed => Role::FearGreedExtremeGreed,
    })
}

#[derive(Clone, Debug, PartialEq)]
struct GaugeKey {
    size: LogicalSize,
    score: u8,
    level: SmolStr,
    colors: GaugeColors,
}

impl GaugeKey {
    fn matches(&self, size: LogicalSize, score: u8, level: &SmolStr, colors: &GaugeColors) -> bool {
        self.size == size && self.score == score && self.level == *level && self.colors == *colors
    }
}

struct PreparedGauge {
    key: GaugeKey,
    bands: Vec<GeometryId>,
}

pub(crate) struct Gauge {
    node: Option<NodeId>,
    prepared: Option<PreparedGauge>,
}

impl Gauge {
    pub(crate) const fn new() -> Self {
        Self {
            node: None,
            prepared: None,
        }
    }

    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let theme = cx.theme().clone();
        let style = ui_core::paint::style_builder::StyleBuilder::new()
            .flex_1()
            .min_width_0()
            .min_height_0()
            .build();
        let node = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
            .id(GAUGE_SELECTOR)
            .style(style)
            .bind_style(move |read| {
                let _ = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);

        Ok(node)
    }

    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        score: u8,
        level: &SmolStr,
    ) -> Result<(), EngineError> {
        let Some(node) = self.node else {
            return Ok(());
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(());
        };
        let bounds = target.layout().bounds;
        let size = bounds.size;
        if size.is_empty() {
            return Ok(());
        }

        let colors = self.colors(cx);
        let current = self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.key.matches(size, score, level, &colors));
        if current {
            return Ok(());
        }

        let bounds = LogicalRect::from_origin_size(LogicalPoint::ZERO, size);
        let geometry = GaugeGeometry::compute(bounds);
        let mut canvas = Canvas::new(bounds);
        let _ = canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [px(0.0); 4],
        });
        let mut bands = Vec::with_capacity(SEGMENT_COUNT);
        if let Some(geometry) = &geometry {
            for index in 0..SEGMENT_COUNT {
                let mesh = geometry.band_mesh(index);
                let id = match register_band(cx.app_mut(), &mesh) {
                    Ok(id) => id,
                    Err(error) => return Err(abandon_frame(cx.app_mut(), bands, error)),
                };
                canvas.push_path(Path {
                    bounds: mesh.bounds,
                    geometry: id,
                    fill: colors.segments[index],
                });
                bands.push(id);
            }
            for quad in geometry.cap_quads(&colors) {
                canvas.push_quad(quad);
            }
            let (halo, dot) = geometry.indicator_quads(score, &colors);
            canvas.push_quad(halo);
            canvas.push_quad(dot);
        }

        let (score_size, level_size) = text_sizes(bounds, geometry);
        let (score_box, level_box) = text_boxes(
            bounds,
            geometry,
            score_size * LINE_HEIGHT_RATIO,
            level_size * LINE_HEIGHT_RATIO,
        );
        let score_content = TextContent::new(
            format_smolstr!("{score}"),
            text_style(score_size, colors.text),
        );
        let level_content = TextContent::new(level.clone(), text_style(level_size, colors.text));
        let score_measured = cx.app_mut().measure_text(&score_content, None);
        let level_measured = cx.app_mut().measure_text(&level_content, None);
        let _ = canvas.push_text(score_content, text_origin(score_box, score_measured));
        let _ = canvas.push_text(level_content, text_origin(level_box, level_measured));
        let _ = canvas.pop_clip();

        if let Err(error) = cx.set_canvas(node, canvas) {
            return Err(abandon_frame(cx.app_mut(), bands, error));
        }
        self.release_bands(cx.app_mut());
        self.prepared = Some(PreparedGauge {
            key: GaugeKey {
                size,
                score,
                level: level.clone(),
                colors,
            },
            bands,
        });

        Ok(())
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        self.release_bands(app);
        self.node = None;
    }

    #[cfg(test)]
    pub(crate) fn prepared_level(&self) -> Option<&SmolStr> {
        self.prepared.as_ref().map(|prepared| &prepared.key.level)
    }

    fn colors(&self, cx: &WidgetCx<'_>) -> GaugeColors {
        let theme = cx.theme();
        let appearance = theme.current(cx.app());

        GaugeColors::resolve(theme.theme_for(appearance))
    }

    fn release_bands(&mut self, app: &mut App) {
        if let Some(prepared) = self.prepared.take() {
            for id in prepared.bands {
                let _ = app.geometry_mut().release(id);
            }
        }
    }
}

fn abandon_frame(app: &mut App, bands: Vec<GeometryId>, error: EngineError) -> EngineError {
    for id in bands {
        let _ = app.geometry_mut().release(id);
    }
    error
}

fn register_band(app: &mut App, mesh: &BandMesh) -> Result<GeometryId, EngineError> {
    let mut vertex_bytes = Vec::with_capacity(mesh.vertices.len() * 4);
    for value in &mesh.vertices {
        vertex_bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut index_bytes = Vec::with_capacity(mesh.indices.len() * 4);
    for index in &mesh.indices {
        index_bytes.extend_from_slice(&index.to_le_bytes());
    }

    app.geometry_mut().insert(GeometryDesc {
        vertex_bytes: Bytes::from(vertex_bytes),
        index_bytes: Bytes::from(index_bytes),
        vertex_stride: VERTEX_STRIDE_BYTES,
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds: mesh.bounds,
        pipeline: GeometryPipeline::Ui,
    })
}

fn text_style(size: LogicalPx, color: Color) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.color = color;

    style
}

fn text_origin(box_: LogicalRect, line: LogicalSize) -> LogicalPoint {
    LogicalPoint::new(
        box_.origin.x + (box_.size.width - line.width) / 2.0,
        box_.origin.y,
    )
}

fn text_sizes(bounds: LogicalRect, geometry: Option<GaugeGeometry>) -> (LogicalPx, LogicalPx) {
    let score_size = match geometry {
        Some(geometry) => geometry.band_radius * SCORE_TEXT_RADIUS_RATIO,
        None => bounds.size.width.min(bounds.size.height) * SCORE_TEXT_FALLBACK_RATIO,
    };

    (score_size, score_size * LEVEL_TEXT_RATIO)
}

fn text_boxes(
    bounds: LogicalRect,
    geometry: Option<GaugeGeometry>,
    score_height: LogicalPx,
    level_height: LogicalPx,
) -> (LogicalRect, LogicalRect) {
    let gap = level_height * TEXT_LINE_GAP_RATIO;
    let stacked = score_height + gap + level_height;
    let top = match geometry {
        Some(geometry) => geometry.center.y - level_height * TEXT_ABOVE_DIAMETER_RATIO - stacked,
        None => bounds.origin.y + (bounds.size.height - stacked) / 2.0,
    };

    (
        LogicalRect::new(bounds.origin.x, top, bounds.size.width, score_height),
        LogicalRect::new(
            bounds.origin.x,
            top + score_height + gap,
            bounds.size.width,
            level_height,
        ),
    )
}

fn circle_bounds(center: LogicalPoint, radius: LogicalPx) -> LogicalRect {
    LogicalRect::new(
        center.x - radius,
        center.y - radius,
        radius * 2.0,
        radius * 2.0,
    )
}

fn circle_quad(center: LogicalPoint, radius: LogicalPx, color: Color) -> Quad {
    Quad {
        bounds: circle_bounds(center, radius),
        corner_radius: [radius; 4],
        paint: QuadPaint::Solid {
            fill: color,
            border: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use ui_kit::theme::appearance::Appearance;

    use super::*;
    use crate::model::Level;

    fn canvas() -> LogicalRect {
        LogicalRect::new(px(0.0), px(0.0), px(400.0), px(240.0))
    }

    fn geometry() -> GaugeGeometry {
        GaugeGeometry::compute(canvas()).expect("a normal canvas should hold a gauge")
    }

    fn distance(from: LogicalPoint, to: LogicalPoint) -> f32 {
        let dx = from.x.value() - to.x.value();
        let dy = from.y.value() - to.y.value();

        (dx * dx + dy * dy).sqrt()
    }

    fn farthest_corner(bounds: LogicalRect, center: LogicalPoint) -> f32 {
        [
            bounds.origin,
            LogicalPoint::new(bounds.max().x, bounds.origin.y),
            LogicalPoint::new(bounds.origin.x, bounds.max().y),
            bounds.max(),
        ]
        .into_iter()
        .map(|corner| distance(center, corner))
        .fold(0.0, f32::max)
    }

    #[test]
    fn the_bands_should_meet_with_a_gap_and_never_overlap() {
        let geometry = geometry();

        for index in 0..SEGMENT_COUNT {
            let (start, end) = geometry.segment_span(index);
            let (arc_start, arc_end) = geometry.arc_span(index);

            assert!(
                (end - start - SEGMENT_SWEEP).abs() < f32::EPSILON,
                "band {index} takes its own share of the circle"
            );
            assert!(
                arc_start > start && arc_end < end,
                "band {index} keeps a gap at both ends"
            );

            if index + 1 < SEGMENT_COUNT {
                let (next_start, _) = geometry.segment_span(index + 1);
                let (next_arc_start, _) = geometry.arc_span(index + 1);

                assert!(
                    (end - next_start).abs() < f32::EPSILON,
                    "band {} starts where band {index} ends",
                    index + 1
                );
                assert!(
                    arc_end < next_arc_start,
                    "the gap between band {index} and band {} is open",
                    index + 1
                );
            }
        }
    }

    #[test]
    fn every_end_cap_should_sit_at_its_own_arc_end() {
        let geometry = geometry();
        let caps = geometry.cap_centers();

        for index in 0..SEGMENT_COUNT {
            let (start, end) = geometry.arc_span(index);
            let start_cap = caps[index * 2].0;
            let end_cap = caps[index * 2 + 1].0;

            assert!(
                distance(start_cap, geometry.point_at(start, geometry.band_radius)) < 0.01,
                "the start cap of band {index} belongs at the start of its own arc"
            );
            assert!(
                distance(end_cap, geometry.point_at(end, geometry.band_radius)) < 0.01,
                "the end cap of band {index} belongs at the end of its own arc"
            );
        }
    }

    #[test]
    fn the_caps_of_two_bands_should_leave_a_visible_gap() {
        let geometry = geometry();
        let caps = geometry.cap_centers();
        let cap_diameter = geometry.cap_radius * 2.0;

        for index in 0..SEGMENT_COUNT - 1 {
            let left = caps[index * 2 + 1].0;
            let right = caps[(index + 1) * 2].0;
            let separation = px(distance(left, right));

            assert!(
                separation > cap_diameter,
                "the caps of bands {index} and {} overlap by {:.2} px",
                index + 1,
                cap_diameter - separation
            );
            assert!(
                separation - cap_diameter > cap_diameter * 0.2,
                "the gap between bands {index} and {} is narrower than a fifth of a cap",
                index + 1
            );
        }
    }

    #[test]
    fn an_end_cap_should_sit_on_the_centre_line_of_its_band() {
        let geometry = geometry();

        for (center, _) in geometry.cap_centers() {
            let radius = distance(center, geometry.center);

            assert!(
                (px(radius) - geometry.band_radius).abs() < px(0.01),
                "an end cap off the centre line would not round its band"
            );
        }
    }

    #[test]
    fn the_gauge_should_stay_inside_the_canvas_and_be_centred() {
        let canvas = canvas();
        let geometry = GaugeGeometry::compute(canvas).expect("the fixture canvas holds a gauge");
        let reach = geometry.band_radius + geometry.indicator_radius + geometry.indicator_halo;
        let top = geometry.center.y - reach;
        let bottom = geometry.center.y + geometry.indicator_radius + geometry.indicator_halo;
        let left = geometry.center.x - reach;
        let right = geometry.center.x + reach;

        assert!(
            (geometry.outer_radius - (geometry.band_radius + geometry.cap_radius)).abs() < px(0.01),
            "the band's outermost point is the band plus one cap"
        );
        assert!(
            top >= canvas.origin.y,
            "the top of the indicator stays below the top of the canvas"
        );
        assert!(
            bottom <= canvas.max().y,
            "the indicator at either end of the scale stays above the bottom"
        );
        assert!(
            left >= canvas.origin.x,
            "the left end of the indicator stays inside the canvas"
        );
        assert!(
            right <= canvas.max().x,
            "the right end of the indicator stays inside the canvas"
        );

        let top_margin = top - canvas.origin.y;
        let bottom_margin = canvas.max().y - bottom;
        let left_margin = left - canvas.origin.x;
        let right_margin = canvas.max().x - right;
        assert!(
            (top_margin - bottom_margin).abs() < px(0.01),
            "the gauge block is centred vertically in the canvas"
        );
        assert!(
            (left_margin - right_margin).abs() < px(0.01),
            "the gauge block is centred horizontally in the canvas"
        );

        for (center, _) in geometry.cap_centers() {
            assert!(center.y - geometry.cap_radius >= canvas.origin.y);
            assert!(center.x - geometry.cap_radius >= canvas.origin.x);
            assert!(center.x + geometry.cap_radius <= canvas.max().x);
        }
    }

    #[test]
    fn the_indicator_should_walk_from_the_left_end_to_the_right_one() {
        let geometry = geometry();
        let band = geometry.band_radius;
        let left = geometry.indicator_center(0);
        let top = geometry.indicator_center(50);
        let right = geometry.indicator_center(100);

        assert!(
            (left.x - geometry.center.x + band).abs() < px(0.01)
                && (left.y - geometry.center.y).abs() < px(0.01),
            "zero is the left end of the half circle"
        );
        assert!(
            (top.x - geometry.center.x).abs() < px(0.01)
                && (top.y - geometry.center.y + band).abs() < px(0.01),
            "fifty is the top of the half circle"
        );
        assert!(
            (right.x - geometry.center.x - band).abs() < px(0.01)
                && (right.y - geometry.center.y).abs() < px(0.01),
            "one hundred is the right end of the half circle"
        );

        let mut previous = left.x;
        for score in 1..=100_u8 {
            let x = geometry.indicator_center(score).x;
            assert!(
                x > previous,
                "the reading moves right as the score rises, at {score}"
            );
            previous = x;
        }
    }

    #[test]
    fn a_canvas_below_the_threshold_should_have_no_gauge() {
        let too_short =
            LogicalRect::new(px(0.0), px(0.0), MIN_CANVAS_WIDTH, MIN_CANVAS_HEIGHT - 1.0);
        let too_narrow =
            LogicalRect::new(px(0.0), px(0.0), MIN_CANVAS_WIDTH - 1.0, MIN_CANVAS_HEIGHT);
        let empty = LogicalRect::new(px(0.0), px(0.0), px(0.0), px(0.0));

        assert!(GaugeGeometry::compute(too_short).is_none());
        assert!(GaugeGeometry::compute(too_narrow).is_none());
        assert!(GaugeGeometry::compute(empty).is_none());
        assert!(
            GaugeGeometry::compute(LogicalRect::new(
                px(0.0),
                px(0.0),
                MIN_CANVAS_WIDTH,
                MIN_CANVAS_HEIGHT
            ))
            .is_some(),
            "the threshold itself still draws"
        );
    }

    #[test]
    fn every_level_should_be_drawn_in_a_colour_of_its_own() {
        let theme = Theme::of(Appearance::Light);
        let levels = [
            Level::ExtremeFear,
            Level::Fear,
            Level::Neutral,
            Level::Greed,
            Level::ExtremeGreed,
        ];
        let colors = levels.map(|level| level_color(&theme, level));

        for (index, color) in colors.iter().enumerate() {
            assert!(
                !colors[..index].contains(color),
                "{:?} is drawn in a colour another level already uses",
                levels[index]
            );
        }
        assert_eq!(
            colors,
            [
                theme.role(Role::FearGreedExtremeFear),
                theme.role(Role::FearGreedFear),
                theme.role(Role::FearGreedNeutral),
                theme.role(Role::FearGreedGreed),
                theme.role(Role::FearGreedExtremeGreed),
            ],
            "the gauge reads the scale's own roles"
        );
    }

    #[test]
    fn the_gauge_should_take_its_colours_from_the_scale_and_the_foreground() {
        let theme = Theme::of(Appearance::Light);
        let colors = GaugeColors::resolve(&theme);

        assert_eq!(
            colors.segments,
            [
                theme.role(Role::FearGreedExtremeFear),
                theme.role(Role::FearGreedFear),
                theme.role(Role::FearGreedNeutral),
                theme.role(Role::FearGreedGreed),
                theme.role(Role::FearGreedExtremeGreed),
            ]
        );
        assert_eq!(colors.text, theme.role(Role::FgDefault));
        assert_eq!(colors.halo, theme.role(Role::SurfaceSunken));
    }

    #[test]
    fn the_indicator_should_be_wider_than_the_band() {
        let theme = Theme::of(Appearance::Light);
        let colors = GaugeColors::resolve(&theme);
        let geometry = geometry();
        let band = geometry.cap_radius * 2.0;

        assert!(
            geometry.indicator_radius * 2.0 > band,
            "the marker's diameter is wider than the band"
        );
        assert!(
            (geometry.indicator_radius - geometry.cap_radius * INDICATOR_RATIO).abs() < px(0.01),
            "the marker's radius is the band end's radius times the reference's ratio"
        );

        let (halo, dot) = geometry.indicator_quads(46, &colors);
        assert_eq!(
            dot.paint,
            QuadPaint::Solid {
                fill: colors.text,
                border: None
            }
        );
        assert_eq!(
            halo.paint,
            QuadPaint::Solid {
                fill: colors.halo,
                border: None
            }
        );
        assert_eq!(
            halo.corner_radius,
            [geometry.indicator_radius + geometry.indicator_halo; 4],
            "the ring is rounded by its own radius, enlargement included"
        );
    }

    #[test]
    fn the_indicator_ring_should_follow_the_band_with_a_floor_and_a_ceiling() {
        let geometry_of = |width: f32, height: f32| {
            GaugeGeometry::compute(LogicalRect::new(px(0.0), px(0.0), px(width), px(height)))
                .expect("the fixture canvas holds a gauge")
        };

        let tiny = geometry_of(96.0, 72.0);
        let medium = geometry_of(400.0, 240.0);
        let large = geometry_of(1600.0, 800.0);
        let huge = geometry_of(2400.0, 1200.0);

        assert_eq!(
            tiny.indicator_halo, MIN_INDICATOR_HALO,
            "a tiny gauge keeps the ring at its floor"
        );
        assert!(
            (medium.indicator_halo - px(4.32)).abs() < px(0.01),
            "a middle-sized gauge gets twenty percent of its band"
        );
        assert!(
            (large.indicator_halo - px(14.4)).abs() < px(0.01),
            "a large gauge still gets twenty percent of its band"
        );
        assert_eq!(
            huge.indicator_halo, MAX_INDICATOR_HALO,
            "a huge gauge keeps the ring at its ceiling"
        );
    }

    #[test]
    fn every_circle_should_be_painted_with_corner_radii_of_its_own_radius() {
        let theme = Theme::of(Appearance::Light);
        let colors = GaugeColors::resolve(&theme);
        let geometry = geometry();

        let cap_quads = geometry.cap_quads(&colors);
        for ((center, _), quad) in geometry.cap_centers().into_iter().zip(cap_quads) {
            assert_eq!(
                quad.bounds,
                circle_bounds(center, geometry.cap_radius),
                "an end cap covers exactly the circle its centre and radius describe"
            );
            assert_eq!(
                quad.corner_radius, [geometry.cap_radius; 4],
                "every corner of a cap is rounded by the cap's radius"
            );
            assert!(
                px(farthest_corner(quad.bounds, geometry.center)) > geometry.outer_radius,
                "a square cap's corner lies outside the band's outer edge, which is \
                 why that corner is not what the quad paints"
            );
        }

        let (halo, dot) = geometry.indicator_quads(46, &colors);
        let dot_radius = geometry.indicator_radius;
        assert_eq!(
            dot.bounds,
            circle_bounds(geometry.indicator_center(46), dot_radius),
            "the dot is the circle the geometry places"
        );
        assert_eq!(
            dot.corner_radius, [dot_radius; 4],
            "the dot is a circle rather than a square"
        );
        assert_eq!(
            halo.corner_radius,
            [dot_radius + geometry.indicator_halo; 4],
            "the ring is rounded by its own radius, enlargement included"
        );
        assert!(
            halo.corner_radius[0] > dot.corner_radius[0],
            "the ring is the dot plus its enlargement on every side"
        );
    }

    #[test]
    fn every_band_should_be_a_solid_triangle_strip_with_its_own_coverage() {
        let geometry = geometry();

        for index in 0..SEGMENT_COUNT {
            let mesh = geometry.band_mesh(index);

            assert_eq!(
                mesh.vertices.len() % 3,
                0,
                "band {index} stores [x, y, coverage] triples"
            );
            assert_eq!(
                mesh.vertices.len() / 3 * 4,
                (mesh.vertices.len() / 3) * 4,
                "band {index}'s vertex bytes are 12 per vertex"
            );
            assert!(!mesh.indices.is_empty(), "band {index} has triangles");
            assert_eq!(mesh.indices.len() % 3, 0, "band {index} is a triangle list");
            assert!(
                mesh.indices
                    .iter()
                    .all(|index| (*index as usize) < mesh.vertices.len() / 3),
                "band {index} indexes its own vertices"
            );
            let mut deepest = 0.0_f32;
            for vertex in 0..mesh.vertices.len() / 3 {
                let coverage = mesh.vertices[vertex * 3 + 2];
                assert!(
                    coverage <= 0.0,
                    "every vertex is on or inside the shape (band {index})"
                );
                deepest = deepest.min(coverage);
            }
            assert_eq!(
                deepest,
                -geometry.cap_radius.value(),
                "the centre ring is the saturated inside of the band"
            );
            assert!(
                mesh.bounds.origin.x >= geometry.center.x - geometry.outer_radius - 0.01
                    && mesh.bounds.max().x <= geometry.center.x + geometry.outer_radius + 0.01,
                "band {index}'s damage box stays inside the arc's reach"
            );
        }
    }

    #[test]
    fn the_score_and_the_level_name_should_not_overlap_above_the_centre() {
        let gauge_canvas = canvas();
        let words_only = LogicalRect::new(px(0.0), px(0.0), px(400.0), px(60.0));
        let gauge = GaugeGeometry::compute(gauge_canvas);
        let no_gauge = GaugeGeometry::compute(words_only);
        assert!(gauge.is_some(), "the fixture canvas holds a gauge");
        assert!(
            no_gauge.is_none(),
            "the words-only canvas is too short for one"
        );

        for (bounds, geometry) in [(gauge_canvas, gauge), (words_only, no_gauge)] {
            let (score_size, level_size) = text_sizes(bounds, geometry);
            let (score_box, level_box) = text_boxes(
                bounds,
                geometry,
                score_size * LINE_HEIGHT_RATIO,
                level_size * LINE_HEIGHT_RATIO,
            );

            assert!(
                score_box.max().y < level_box.origin.y,
                "the level's name must not be painted over the score"
            );
            match geometry {
                Some(geometry) => assert!(
                    level_box.max().y < geometry.center.y,
                    "the reading stays above the arc's centre"
                ),
                None => assert!(
                    level_box.max().y <= bounds.max().y,
                    "the reading stays inside a canvas with no arc"
                ),
            }
            assert!(
                score_box.origin.y >= bounds.origin.y,
                "the reading starts inside the canvas"
            );
            let center = bounds.origin.x + bounds.size.width / 2.0;
            assert!(
                (score_box.origin.x + score_box.size.width / 2.0 - center).abs() < px(0.01),
                "the score is centred on the arc's centre line"
            );
            assert!(
                (level_box.origin.x + level_box.size.width / 2.0 - center).abs() < px(0.01),
                "and so is the level's name"
            );
        }
    }

    #[test]
    fn a_narrow_canvas_should_shrink_the_reading_with_the_band() {
        let wide = LogicalRect::new(px(0.0), px(0.0), px(800.0), px(800.0));
        let narrow = LogicalRect::new(px(0.0), px(0.0), px(200.0), px(800.0));
        let wide_geometry = GaugeGeometry::compute(wide).expect("the wide canvas holds a gauge");
        let narrow_geometry =
            GaugeGeometry::compute(narrow).expect("the narrow canvas holds a gauge");
        let (wide_score, _) = text_sizes(wide, Some(wide_geometry));
        let (narrow_score, narrow_level) = text_sizes(narrow, Some(narrow_geometry));

        assert!(
            narrow_score < wide_score,
            "the narrow gauge's score shrinks with its band"
        );
        assert_eq!(
            narrow_score,
            narrow_geometry.band_radius * SCORE_TEXT_RADIUS_RATIO,
            "the score is sized from the band's radius"
        );

        let score_height = narrow_score * LINE_HEIGHT_RATIO;
        let level_height = narrow_level * LINE_HEIGHT_RATIO;
        let (score_box, _) = text_boxes(narrow, Some(narrow_geometry), score_height, level_height);
        let inner_top = narrow_geometry.center.y - narrow_geometry.inner_radius;

        assert!(
            score_box.origin.y >= inner_top,
            "the reading stays inside the arc's inner edge on a narrow canvas"
        );
    }
}
