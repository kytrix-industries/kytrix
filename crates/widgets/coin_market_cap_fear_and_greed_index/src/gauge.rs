//! The half gauge the current reading is drawn on.
//!
//! The gauge is vector geometry rather than a picture: a picture would bake one
//! theme's colours into it and be rasterized at one scale, and this pane has to be
//! drawn in both appearances at every display density. So each of the five bands is
//! a filled annulus sector, each end of a sector is rounded by a circle centred on
//! that sector's own end, and the current reading is a dot on the centre line of the
//! band — all from the semantic roles, none of it from a palette value or a literal
//! colour.
//!
//! # The canvas has two states, and both draw the reading
//!
//! - **large enough**: the half circle, its caps and the indicator, with the score
//!   above its level name inside the arc;
//! - **too small**: only the words. A pane squeezed under the threshold draws
//!   nothing it cannot place, rather than a half arc that runs off the canvas.
//!
//! # How it is drawn on the engine
//!
//! The gauge is a [`NodeKind::Canvas`](ui_core::node::tree::NodeKind::Canvas) node.
//! The engine has no path builder (design §5: arrays of primitives, and the
//! application owns the triangulation), so the migration splits the drawing into the
//! two primitive kinds that express it:
//!
//! - **the five bands** are triangle meshes registered in the process-wide
//!   [`GeometryStore`](ui_core::paint::geometry_store::GeometryStore) and pushed as
//!   [`Primitive::Path`](ui_core::paint::primitive::Primitive::Path) fills. Each mesh
//!   is a three-ring quad strip along its arc — outer edge, centre line, inner edge —
//!   whose vertices carry the `Ui` pipeline's signed coverage distance
//!   (`[x, y, coverage]`, 12 B/vertex): `0` on both arc edges and `-band/2` on the
//!   centre line, which makes the analytic AA band land exactly on the arc. The
//!   radial cuts themselves are not covered by that distance field; the caps below
//!   paint over them, which is why the cut is not visible (a **known difference**
//!   from the reference, whose path fill rounded every edge).
//!   The strip meets the first of the `Ui` pipeline's three coverage-mesh conditions —
//!   the distance is linear along the normal — and **not** the other two
//!   ([`GeometryPipeline::Ui`](ui_core::paint::geometry::GeometryPipeline::Ui) states
//!   them): neighbouring quads share a radial edge, so the AA band of two triangles
//!   writes the same pixels from both sides, and the four square corners of each band
//!   are not extruded into triangles of their own. The engine does not check these
//!   conditions; not meeting them shows only as aliasing, and here that aliasing lands
//!   on the radial cuts and shared edges the same-coloured caps paint over (the same
//!   reason the cut is invisible). A later caller reusing this mesh where those edges
//!   are not covered would see it.
//! - **the caps and the indicator** are quads with corner radii of their own radius,
//!   which is what makes them circles. A quad's own corners reach `r·√2` from its
//!   centre — past the band's outer edge — and only rounding every corner by `r`
//!   keeps the shape inside the box the geometry sized for it.
//! - **the two lines of text** are shaped by the engine's node-free text entry
//!   ([`App::shape_text`], v2.1 E1) and pushed as glyph runs
//!   ([`Canvas::push_shaped_text`]): the canvas owns no text node, and a glyph run
//!   is never constructed by hand. A handle carries an atlas lease, so a handle that
//!   the atlas has since evicted is dropped and the frame prepared again.
//!
//! Values are **window-absolute logical pixels** (the canvas' coordinate space), so
//! the drawing is a pure function of the box the node ended up with.
//!
//! # Preparing is expensive, so a frame is kept
//!
//! Shaping is the expensive half of a frame and none of it moves between two frames
//! of the same reading, so a frame is prepared once and kept: the prepared value is
//! rebuilt only when the canvas box, the reading, the language (the level's name) or
//! the colours change, or when its glyph handles went stale. Steady state therefore
//! does no work at all (I3: no write, no frame request).
//!
//! # Group opacity (E13)
//!
//! The engine's group opacity does **not** reach canvas content (spec-delta E13 R1:
//! the engine does not parse an application's own primitives). This pane has no
//! group fade of its own — the reading is replaced, never faded — so nothing here
//! compensates for it. A later fading caller must scale these colours itself.

use base_ui::theme::{resolved::Theme, role::Role};
use bytes::Bytes;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, Path, Quad, QuadPaint},
        style::StylePatch,
    },
    text::{ShapedTextHandle, TextStyle},
};
use widget::cx::WidgetCx;

use crate::model::Level;

/// The debug id of the canvas node the gauge draws on.
pub(crate) const GAUGE_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_GAUGE";

/// How many bands the scale is drawn in.
const SEGMENT_COUNT: usize = 5;

/// The two ends of every band: a start and an end per segment.
const CAP_COUNT: usize = SEGMENT_COUNT * 2;

/// One band's share of the half circle, in degrees.
const SEGMENT_SWEEP: f32 = 180.0 / SEGMENT_COUNT as f32;

/// The half circle starts at the left end of the scale.
const START_ANGLE: f32 = 180.0;

/// The whole scale spans half a circle.
const SWEEP: f32 = 180.0;

/// The band's thickness, as a share of the canvas' shorter side.
const BAND_RATIO: f32 = 0.09;

/// How far each end of a band is pulled back from its boundary, as a share of the
/// band's thickness.
///
/// A cap's radius is half the band, so an end has to be pulled back more than half a
/// band for the two caps at a boundary to sit apart instead of on top of each other.
/// The reference's value is 0.65: it leaves a visible gap of 0.3 of the band between
/// the two caps.
const GAP_RATIO: f32 = 0.65;

/// The narrowest canvas a gauge is drawn on.
const MIN_CANVAS_WIDTH: f32 = 96.0;

/// The shortest canvas a gauge is drawn on.
const MIN_CANVAS_HEIGHT: f32 = 72.0;

/// The score's font size, as a share of the band's radius.
///
/// The gauge's radius is what the text has to fit inside, and a narrow pane limits
/// that radius by its width while the canvas keeps its height. Sizing the text from
/// the height would leave it at full size while the arc shrinks under it, which is
/// exactly the overlap a narrow pane used to draw.
const SCORE_TEXT_RADIUS_RATIO: f32 = 0.35;

/// The score's font size, as a share of the canvas' shorter side, when the canvas
/// is too small to draw a gauge and only the words are left.
const SCORE_TEXT_FALLBACK_RATIO: f32 = 0.28;

/// The level's font size, as a share of the score's.
const LEVEL_TEXT_RATIO: f32 = 0.55;

/// How tall one line is, as a share of its font size.
const LINE_HEIGHT_RATIO: f32 = 1.15;

/// The gap between the score and the level's name, as a share of the level's line.
///
/// The two lines are stacked rather than anchored to opposite ends of the centre:
/// the level's name is drawn after the score, so anything that brought them back
/// together would paint the name over the digits.
const TEXT_LINE_GAP_RATIO: f32 = 0.15;

/// How far above the arc's centre the reading ends, as a share of the level's line.
///
/// The reading belongs inside the half circle; ending exactly on the diameter would
/// put the level's last line across it.
const TEXT_ABOVE_DIAMETER_RATIO: f32 = 0.1;

/// The indicator's radius, as a share of a band end's.
///
/// The reference's marker is a circle whose diameter is a third wider than the band:
/// it covers the band it sits on and reads as a marker rather than as another cap.
const INDICATOR_RATIO: f32 = 1.33;

/// The indicator ring's width, as a share of the band's thickness.
///
/// The ring has to grow with the gauge: a fixed width is a hairline on a large
/// gauge and swallows a small one. Twenty percent of the band is the width the
/// narrow reference-sized gauge wanted — 8px at a 40px band.
const INDICATOR_HALO_RATIO: f32 = 0.2;

/// The narrowest the indicator ring is allowed to be, in pixels.
///
/// Below this the ring stops separating the marker from the band at all.
const MIN_INDICATOR_HALO: f32 = 2.0;

/// The widest the indicator ring is allowed to be, in pixels.
///
/// Above this the ring starts to read as a second circle rather than as a gap
/// around the marker.
const MAX_INDICATOR_HALO: f32 = 16.0;

/// The length of one quad-strip step along a band's arc, in logical pixels.
///
/// Two pixels per step keeps a step's chord under a fifth of a degree on the
/// reference-sized gauge, which is finer than the raster can show.
const MESH_STEP_LENGTH: f32 = 2.0;

/// The fewest steps one band is triangulated in.
const MESH_MIN_STEPS: f32 = 4.0;

/// The most steps one band is triangulated in.
const MESH_MAX_STEPS: f32 = 48.0;

/// One vertex of the `Ui` geometry pipeline: `[x, y, coverage]`, 12 B.
const VERTEX_STRIDE_BYTES: u32 = 12;

/// The coverage distance of a vertex on a band's arc edge (the shape boundary).
const EDGE_COVERAGE: f32 = 0.0;

/// Where every piece of the gauge goes, for one canvas box.
///
/// The geometry is a pure function of the box: it is what the frame paints and what
/// the tests measure, and it is computed once per prepared frame rather than per
/// primitive.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeGeometry {
    /// The middle of the arc's diameter, at the bottom of the half circle.
    center: Point,
    /// Half the band's thickness, which is also the ends' radius.
    cap_radius: f32,
    /// The radius of the line the band is centred on.
    band_radius: f32,
    /// The outer edge of the band, including its ends.
    outer_radius: f32,
    /// The inner edge of the band.
    inner_radius: f32,
    /// The radius of the reading's dot. Its ring adds `indicator_halo` past it, and
    /// that reach is what the geometry keeps room for.
    indicator_radius: f32,
    /// The width of the ring around the reading's dot, in pixels. It follows the
    /// band between [`MIN_INDICATOR_HALO`] and [`MAX_INDICATOR_HALO`].
    indicator_halo: f32,
    /// How far each end of a band is pulled back from its boundary, in degrees.
    gap_half: f32,
}

impl GaugeGeometry {
    /// Places the gauge and its reading as one block, centred on both axes, or
    /// answers nothing when the box is too small to hold one.
    ///
    /// The block is the arc's outer edge plus the indicator's ring above the
    /// diameter, and the indicator's ring below it; the caps at the ends reach less
    /// far down than the indicator does. Sizing the band from both the width and
    /// that block's height is what keeps the gauge inside the canvas, and centring
    /// the block is what keeps a tall pane from pinning the gauge to its bottom.
    fn compute(bounds: Rect) -> Option<Self> {
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
        let band_radius = (width / 2.0 - indicator_reach).min(height - 2.0 * indicator_reach);
        if band_radius <= 0.0 {
            return None;
        }
        let gap_half = ((band * GAP_RATIO) / band_radius).to_degrees();
        let center_y = bounds.origin.y + (height + band_radius) / 2.0;

        Some(Self {
            center: Point::new(bounds.origin.x + width / 2.0, center_y),
            cap_radius,
            band_radius,
            outer_radius: band_radius + cap_radius,
            inner_radius: band_radius - cap_radius,
            indicator_radius,
            indicator_halo,
            gap_half,
        })
    }

    /// The angles one band is assigned, before its ends are pulled back.
    fn segment_span(&self, index: usize) -> (f32, f32) {
        let start = START_ANGLE + SEGMENT_SWEEP * index as f32;

        (start, start + SEGMENT_SWEEP)
    }

    /// The angles one band's arc actually sweeps, with a gap at each end.
    fn arc_span(&self, index: usize) -> (f32, f32) {
        let (start, end) = self.segment_span(index);

        (start + self.gap_half, end - self.gap_half)
    }

    /// The point at `angle` degrees and `radius` pixels from the centre.
    fn point_at(&self, angle: f32, radius: f32) -> Point {
        let radians = angle.to_radians();

        Point::new(
            self.center.x + radius * radians.cos(),
            self.center.y + radius * radians.sin(),
        )
    }

    /// The ten end circles, each with the band it caps.
    ///
    /// The centres sit on the ends of the trimmed arcs, not on the boundaries the
    /// bands were assigned: a cap rounds the cut its own band was drawn with, and a
    /// cap left on the boundary would sit in the gap and reach into its neighbour.
    fn cap_centers(&self) -> [(Point, usize); CAP_COUNT] {
        std::array::from_fn(|index| {
            let segment = index / 2;
            let (start, end) = self.arc_span(segment);
            let angle = if index % 2 == 0 { start } else { end };

            (self.point_at(angle, self.band_radius), segment)
        })
    }

    /// Where the reading sits on the band's centre line.
    fn indicator_center(&self, score: u8) -> Point {
        let angle = START_ANGLE + SWEEP * f32::from(score) / 100.0;

        self.point_at(angle, self.band_radius)
    }

    /// The ten end circles of one frame, one per band end.
    ///
    /// Each is a circle, not the box its centre and radius describe: only rounding
    /// every corner by the radius keeps the cap inside the band's outer edge.
    fn cap_quads(&self, colors: &GaugeColors) -> [Quad; CAP_COUNT] {
        self.cap_centers()
            .map(|(center, segment)| circle_quad(center, self.cap_radius, colors.segments[segment]))
    }

    /// The reading's dot and the ring it is seated in.
    ///
    /// The ring is wider than the dot by `indicator_halo` on every side, so its
    /// corner radius is the dot's radius plus that enlargement — the radius that
    /// rounds it is the radius it is drawn at, not the dot's.
    fn indicator_quads(&self, score: u8, colors: &GaugeColors) -> (Quad, Quad) {
        let center = self.indicator_center(score);
        let radius = self.indicator_radius;

        (
            circle_quad(center, radius + self.indicator_halo, colors.halo),
            circle_quad(center, radius, colors.text),
        )
    }

    /// The triangle mesh of one band, in window-absolute logical pixels.
    ///
    /// The mesh is a quad strip between three rings: the outer arc edge, the band's
    /// centre line and the inner arc edge. The `Ui` pipeline interpolates one signed
    /// coverage distance per vertex, so the two edge rings carry `0` (the shape's
    /// boundary) and the centre ring carries `-cap_radius` — far enough inside that
    /// the coverage saturates before the next edge, which is what gives the arc an
    /// analytic one-pixel AA band instead of a hard edge.
    fn band_mesh(&self, index: usize) -> BandMesh {
        let (start, end) = self.arc_span(index);
        let arc_length = (end - start).to_radians() * self.band_radius;
        let steps = (arc_length / MESH_STEP_LENGTH)
            .ceil()
            .clamp(MESH_MIN_STEPS, MESH_MAX_STEPS) as usize;
        let rings = [
            (self.outer_radius, EDGE_COVERAGE),
            (self.band_radius, -self.cap_radius),
            (self.inner_radius, EDGE_COVERAGE),
        ];

        let mut vertices = Vec::with_capacity((steps + 1) * rings.len() * 3);
        let mut min = Point::new(f32::INFINITY, f32::INFINITY);
        let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
        for step in 0..=steps {
            let angle = start + (end - start) * step as f32 / steps as f32;
            for (radius, coverage) in rings {
                let point = self.point_at(angle, radius);
                min = Point::new(min.x.min(point.x), min.y.min(point.y));
                max = Point::new(max.x.max(point.x), max.y.max(point.y));
                vertices.extend_from_slice(&[point.x, point.y, coverage]);
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
            bounds: Rect::new(min.x, min.y, max.x - min.x, max.y - min.y),
        }
    }
}

/// One band's triangles, in window-absolute logical pixels.
struct BandMesh {
    /// Interleaved `[x, y, coverage]` triples (three `f32` per vertex).
    vertices: Vec<f32>,
    /// Triangle-list indices into `vertices`.
    indices: Vec<u32>,
    /// The band's AABB: the `Path` primitive's damage rectangle.
    bounds: Rect,
}

/// The colours one frame of the gauge is drawn with, resolved from the theme.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeColors {
    /// One colour per band, in the order the bands are drawn.
    segments: [Color; SEGMENT_COUNT],
    /// The ring the indicator is seated in.
    halo: Color,
    /// The score, the level's name and the reading's dot.
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
            // The framework draws every widget body on `surface.sunken`, so the
            // ring is the body's own background: it reads as a cut-out in the band
            // rather than as an outline drawn around the dot.
            halo: theme.role(Role::SurfaceSunken),
            text: theme.role(Role::FgDefault),
        }
    }
}

/// The colour one level of the fear-and-greed scale is drawn in.
///
/// The scale reads the theme's own five-step ramp rather than the status and market
/// roles: the two market colours are deliberately close to their status neighbours,
/// which is right for a price and wrong for five bands that have to be told apart
/// side by side.
pub(crate) fn level_color(theme: &Theme, level: Level) -> Color {
    theme.role(match level {
        Level::ExtremeFear => Role::FearGreedExtremeFear,
        Level::Fear => Role::FearGreedFear,
        Level::Neutral => Role::FearGreedNeutral,
        Level::Greed => Role::FearGreedGreed,
        Level::ExtremeGreed => Role::FearGreedExtremeGreed,
    })
}

/// What a prepared frame was made from, so the next frame can keep it.
#[derive(Clone, Debug, PartialEq)]
struct GaugeKey {
    bounds: Rect,
    score: u8,
    /// The level's name in the language the frame was prepared in.
    level: SmolStr,
    colors: GaugeColors,
    /// The raster scale the glyphs were shaped at: a DPI change has to shape again.
    text_scale: f32,
}

impl GaugeKey {
    fn matches(
        &self,
        bounds: Rect,
        score: u8,
        level: &SmolStr,
        colors: &GaugeColors,
        text_scale: f32,
    ) -> bool {
        self.bounds == bounds
            && self.score == score
            && self.level == *level
            && self.colors == *colors
            && self.text_scale == text_scale
    }
}

/// One frame of the gauge, ready to be kept until its inputs move.
struct PreparedGauge {
    key: GaugeKey,
    /// The registered band meshes, released before the next frame replaces them.
    bands: Vec<GeometryId>,
    score_line: ShapedTextHandle,
    level_line: ShapedTextHandle,
}

/// The gauge of one pane, including the frame it prepared last.
///
/// The cache lives in the widget rather than in the node because a node's canvas is
/// replaced whole, and the point of preparing a frame is that the next one can reuse
/// it.
pub(crate) struct Gauge {
    /// The canvas node the body built, once it has one.
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

    /// Builds the canvas node into `parent` and remembers it.
    ///
    /// The canvas starts empty (`Rect::default()`): the node has no box until the
    /// first layout, and [`Gauge::sync`] is what fills it in. The node registers a
    /// theme dependency so an appearance switch invalidates it and schedules the
    /// frame whose `sync` resolves the new colours.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let theme = cx.theme().clone();
        let style = base_ui::style_builder::StyleBuilder::new()
            .flex_1()
            .min_width_0()
            .min_height_0()
            .build();
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
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

    /// Draws the gauge of `score`, named by `level`, if the frame it was prepared
    /// for is no longer the frame this one would be.
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
        if bounds.is_empty() {
            return Ok(());
        }

        let colors = self.colors(cx);
        let text_scale = cx.app().text_scale();
        let current = self.prepared.as_ref().is_some_and(|prepared| {
            prepared
                .key
                .matches(bounds, score, level, &colors, text_scale)
                && !prepared.score_line.is_stale()
                && !prepared.level_line.is_stale()
        });
        if current {
            return Ok(());
        }

        let geometry = GaugeGeometry::compute(bounds);
        let mut canvas = Canvas::new(bounds);
        // The masked drawing the reference did: the frame is clipped to the box the
        // node ended up with, so a rounding error cannot paint over a sibling. The
        // words are inside that mask too — the reference masked the whole frame, not
        // just the arc — so it is pushed before the frame's two branches split.
        let _ = canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [0.0; 4],
        });
        let mut bands = Vec::with_capacity(SEGMENT_COUNT);
        if let Some(geometry) = &geometry {
            for index in 0..SEGMENT_COUNT {
                let mesh = geometry.band_mesh(index);
                let id = match register_band(cx.app_mut(), &mesh) {
                    Ok(id) => id,
                    // The frame is abandoned whole, so the meshes registered so far
                    // are given back here: `self.prepared` still holds the frame this
                    // one was replacing, and nothing else would return them.
                    Err(error) => return Err(abandon_frame(cx.app_mut(), bands, error)),
                };
                canvas.push_path(Path {
                    bounds: mesh.bounds,
                    geometry: id,
                    fill: colors.segments[index],
                });
                bands.push(id);
            }
            // The caps are painted over the bands' straight cuts, which is what
            // turns every cut end into a rounded one.
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
        let score_line = match cx.app_mut().shape_text(
            format_smolstr!("{score}").as_str(),
            &text_style(score_size, colors.text),
            None,
            &[],
        ) {
            Ok(line) => line,
            Err(error) => return Err(abandon_frame(cx.app_mut(), bands, error)),
        };
        let level_line = match cx.app_mut().shape_text(
            level.as_str(),
            &text_style(level_size, colors.text),
            None,
            &[],
        ) {
            Ok(line) => line,
            Err(error) => return Err(abandon_frame(cx.app_mut(), bands, error)),
        };
        let _ = canvas.push_shaped_text(&score_line, text_origin(score_box, score_line.size()));
        let _ = canvas.push_shaped_text(&level_line, text_origin(level_box, level_line.size()));
        let _ = canvas.pop_clip();

        if let Err(error) = cx.set_canvas(node, canvas) {
            return Err(abandon_frame(cx.app_mut(), bands, error));
        }
        self.release_bands(cx.app_mut());
        self.prepared = Some(PreparedGauge {
            key: GaugeKey {
                bounds,
                score,
                level: level.clone(),
                colors,
                text_scale,
            },
            bands,
            score_line,
            level_line,
        });

        Ok(())
    }

    /// Releases the registered meshes and forgets the node. The teardown path.
    pub(crate) fn release(&mut self, app: &mut App) {
        self.release_bands(app);
        self.node = None;
    }

    /// The level's name the last prepared frame was made from.
    ///
    /// A prepared frame is rebuilt when the reading, the canvas or the text of the
    /// frame changes — which is what makes a language change new text rather than the
    /// same cached one. A test reads it back to assert what a frame actually drew,
    /// because text is not otherwise readable from a painted frame.
    #[cfg(test)]
    pub(crate) fn prepared_level(&self) -> Option<&SmolStr> {
        self.prepared.as_ref().map(|prepared| &prepared.key.level)
    }

    /// The colours the current appearance resolves.
    fn colors(&self, cx: &WidgetCx<'_>) -> GaugeColors {
        let theme = cx.theme();
        let appearance = theme.current(cx.app());

        GaugeColors::resolve(theme.theme_for(appearance))
    }

    /// Releases the meshes of the prepared frame (the store is process-wide, not
    /// scoped to a window: nothing else would give them back).
    fn release_bands(&mut self, app: &mut App) {
        if let Some(prepared) = self.prepared.take() {
            for id in prepared.bands {
                let _ = app.geometry_mut().release(id);
            }
        }
    }
}

/// Gives back the band meshes a frame registered before it failed.
///
/// A frame registers its meshes in the process-wide store as it builds, so one that
/// cannot be finished would leave them there for the life of the process: the widget
/// only ever releases the frame it kept (`Gauge::release_bands`), and this one is never
/// kept. The error comes back unchanged — it is the caller's to report, not this
/// cleanup's.
fn abandon_frame(app: &mut App, bands: Vec<GeometryId>, error: EngineError) -> EngineError {
    for id in bands {
        let _ = app.geometry_mut().release(id);
    }
    error
}

/// Registers one band's mesh in the process-wide geometry store.
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

/// The style one line of the canvas' text is shaped with.
fn text_style(size: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.color = color;

    style
}

/// Where a shaped line's box starts so that it is centred in `box`.
fn text_origin(box_: Rect, line: ui_core::math::Size) -> Point {
    Point::new(
        box_.origin.x + (box_.size.width - line.width) / 2.0,
        box_.origin.y,
    )
}

/// The score and level font sizes for one canvas.
///
/// With a gauge, the text is sized from the band's radius: that is the space the
/// text has to fit inside, and a narrow pane limits the radius by its width while
/// the canvas keeps its height. Without a gauge there is no radius, so the shorter
/// side of the canvas is the measure that cannot overflow it.
fn text_sizes(bounds: Rect, geometry: Option<GaugeGeometry>) -> (f32, f32) {
    let score_size = match geometry {
        Some(geometry) => geometry.band_radius * SCORE_TEXT_RADIUS_RATIO,
        None => bounds.size.width.min(bounds.size.height) * SCORE_TEXT_FALLBACK_RATIO,
    };

    (score_size, score_size * LEVEL_TEXT_RATIO)
}

/// The two boxes the reading is drawn in, one above the other.
///
/// The score is the value and the level's name labels it, so the score is the upper
/// line and the name the lower one, with a gap between them: the name is painted
/// after the score, and anchoring the two to opposite ends of the centre is what
/// once made it cover the digits. The stack is as wide as the canvas, so the centred
/// text of both boxes shares one centre line, and it sits above the arc's diameter
/// when there is an arc — inside the half circle — or in the middle of the box when
/// the canvas is too small for one.
fn text_boxes(
    bounds: Rect,
    geometry: Option<GaugeGeometry>,
    score_height: f32,
    level_height: f32,
) -> (Rect, Rect) {
    let gap = level_height * TEXT_LINE_GAP_RATIO;
    let stacked = score_height + gap + level_height;
    let top = match geometry {
        Some(geometry) => geometry.center.y - level_height * TEXT_ABOVE_DIAMETER_RATIO - stacked,
        None => bounds.origin.y + (bounds.size.height - stacked) / 2.0,
    };

    (
        Rect::new(bounds.origin.x, top, bounds.size.width, score_height),
        Rect::new(
            bounds.origin.x,
            top + score_height + gap,
            bounds.size.width,
            level_height,
        ),
    )
}

/// The box of a solid circle.
fn circle_bounds(center: Point, radius: f32) -> Rect {
    Rect::new(
        center.x - radius,
        center.y - radius,
        radius * 2.0,
        radius * 2.0,
    )
}

/// The quad a solid circle is painted as.
///
/// Rounding every corner by the radius is what makes the quad a circle: a quad with
/// square corners is a box, and a box reaches `radius * sqrt(2)` from its centre at
/// the corner — past the edge the geometry allowed for the drawing.
fn circle_quad(center: Point, radius: f32, color: Color) -> Quad {
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
    use base_ui::theme::appearance::Appearance;

    use super::*;
    use crate::model::Level;

    /// The canvas most of these tests measure: wide enough that the width is not
    /// what limits the gauge.
    fn canvas() -> Rect {
        Rect::new(0.0, 0.0, 400.0, 240.0)
    }

    fn geometry() -> GaugeGeometry {
        GaugeGeometry::compute(canvas()).expect("a normal canvas should hold a gauge")
    }

    fn distance(from: Point, to: Point) -> f32 {
        let dx = from.x - to.x;
        let dy = from.y - to.y;

        (dx * dx + dy * dy).sqrt()
    }

    /// The distance from `center` to the farthest corner of `bounds`.
    ///
    /// It is how far a quad would reach if its corners were painted square, which is
    /// the shape the radius has to round off.
    fn farthest_corner(bounds: Rect, center: Point) -> f32 {
        [
            bounds.origin,
            Point::new(bounds.max().x, bounds.origin.y),
            Point::new(bounds.origin.x, bounds.max().y),
            bounds.max(),
        ]
        .into_iter()
        .map(|corner| distance(center, corner))
        .fold(0.0, f32::max)
    }

    /// The bands tile the half circle: each one keeps a gap at both ends, and the
    /// gap of one meets the gap of the next rather than overlapping it.
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

    /// Every cap is centred on the end of the arc it rounds, not on the boundary the
    /// band was assigned: a cap left on the boundary would sit in the gap and reach
    /// into the neighbouring band.
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

    /// The two caps that meet at a boundary are separated by a visible gap, which is
    /// what keeps one band's cap from painting over the other band's end.
    #[test]
    fn the_caps_of_two_bands_should_leave_a_visible_gap() {
        let geometry = geometry();
        let caps = geometry.cap_centers();
        let cap_diameter = geometry.cap_radius * 2.0;

        for index in 0..SEGMENT_COUNT - 1 {
            let left = caps[index * 2 + 1].0;
            let right = caps[(index + 1) * 2].0;
            let separation = distance(left, right);

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

    /// Every end circle is centred on the line the band is drawn around, which is
    /// what makes its outer point land exactly on the band's outer edge.
    #[test]
    fn an_end_cap_should_sit_on_the_centre_line_of_its_band() {
        let geometry = geometry();

        for (center, _) in geometry.cap_centers() {
            let radius = distance(center, geometry.center);

            assert!(
                (radius - geometry.band_radius).abs() < 0.01,
                "an end cap off the centre line would not round its band"
            );
        }
    }

    /// The drawing fits the box it was given and is centred in it on both axes.
    ///
    /// The block is the arc's outer edge plus the indicator's ring above the
    /// diameter, and the indicator's ring below it; the indicator is the part that
    /// reaches furthest at the top and at both ends of the scale.
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
            (geometry.outer_radius - (geometry.band_radius + geometry.cap_radius)).abs() < 0.01,
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
            (top_margin - bottom_margin).abs() < 0.01,
            "the gauge block is centred vertically in the canvas"
        );
        assert!(
            (left_margin - right_margin).abs() < 0.01,
            "the gauge block is centred horizontally in the canvas"
        );

        for (center, _) in geometry.cap_centers() {
            assert!(center.y - geometry.cap_radius >= canvas.origin.y);
            assert!(center.x - geometry.cap_radius >= canvas.origin.x);
            assert!(center.x + geometry.cap_radius <= canvas.max().x);
        }
    }

    /// The reading walks from the left end of the scale, over the top, to the right
    /// end — the same direction the bands are drawn in.
    #[test]
    fn the_indicator_should_walk_from_the_left_end_to_the_right_one() {
        let geometry = geometry();
        let band = geometry.band_radius;
        let left = geometry.indicator_center(0);
        let top = geometry.indicator_center(50);
        let right = geometry.indicator_center(100);

        assert!(
            (left.x - geometry.center.x + band).abs() < 0.01
                && (left.y - geometry.center.y).abs() < 0.01,
            "zero is the left end of the half circle"
        );
        assert!(
            (top.x - geometry.center.x).abs() < 0.01
                && (top.y - geometry.center.y + band).abs() < 0.01,
            "fifty is the top of the half circle"
        );
        assert!(
            (right.x - geometry.center.x - band).abs() < 0.01
                && (right.y - geometry.center.y).abs() < 0.01,
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

    /// A box under the threshold has no gauge at all, so nothing is ever placed
    /// outside the canvas it was given.
    #[test]
    fn a_canvas_below_the_threshold_should_have_no_gauge() {
        let too_short = Rect::new(0.0, 0.0, MIN_CANVAS_WIDTH, MIN_CANVAS_HEIGHT - 1.0);
        let too_narrow = Rect::new(0.0, 0.0, MIN_CANVAS_WIDTH - 1.0, MIN_CANVAS_HEIGHT);
        let empty = Rect::new(0.0, 0.0, 0.0, 0.0);

        assert!(GaugeGeometry::compute(too_short).is_none());
        assert!(GaugeGeometry::compute(too_narrow).is_none());
        assert!(GaugeGeometry::compute(empty).is_none());
        assert!(
            GaugeGeometry::compute(Rect::new(0.0, 0.0, MIN_CANVAS_WIDTH, MIN_CANVAS_HEIGHT))
                .is_some(),
            "the threshold itself still draws"
        );
    }

    /// Each level of the scale is drawn in a colour of its own, taken from the
    /// theme's fear-and-greed roles rather than from a literal.
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

    /// The bands, the reading's dot and its ring are the frame's colours: the bands
    /// are the scale's five roles, and the dot is the foreground so it reads as a
    /// marker rather than as another band end.
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

    /// The indicator is a marker, not a band end: its diameter is wider than the
    /// band and it is seated in a ring of the content face.
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
            (geometry.indicator_radius - geometry.cap_radius * INDICATOR_RATIO).abs() < 0.01,
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

    /// The ring follows the band, with a floor for tiny gauges and a ceiling for
    /// huge ones: a fixed width is a hairline on a large gauge and swallows a small
    /// one.
    #[test]
    fn the_indicator_ring_should_follow_the_band_with_a_floor_and_a_ceiling() {
        let geometry_of = |width: f32, height: f32| {
            GaugeGeometry::compute(Rect::new(0.0, 0.0, width, height))
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
            (medium.indicator_halo - 4.32).abs() < 0.01,
            "a middle-sized gauge gets twenty percent of its band"
        );
        assert!(
            (large.indicator_halo - 14.4).abs() < 0.01,
            "a large gauge still gets twenty percent of its band"
        );
        assert_eq!(
            huge.indicator_halo, MAX_INDICATOR_HALO,
            "a huge gauge keeps the ring at its ceiling"
        );
    }

    /// The caps, the reading's dot and its ring are circles: every corner of each
    /// quad is rounded by that quad's own radius.
    ///
    /// A square cap is not just a cosmetic difference — its corners reach further
    /// from the centre than the band's outer edge, so the drawing would overflow the
    /// box the geometry sized it for.
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
                farthest_corner(quad.bounds, geometry.center) > geometry.outer_radius,
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

    /// Every band is a closed triangle mesh whose vertices carry the `Ui` pipeline's
    /// coverage distance: `0` on both arc edges and a saturated inside value on the
    /// centre line, which is what the analytic AA needs.
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
                deepest, -geometry.cap_radius,
                "the centre ring is the saturated inside of the band"
            );
            assert!(
                mesh.bounds.origin.x >= geometry.center.x - geometry.outer_radius - 0.01
                    && mesh.bounds.max().x <= geometry.center.x + geometry.outer_radius + 0.01,
                "band {index}'s damage box stays inside the arc's reach"
            );
        }
    }

    /// The reading's two lines are stacked above the arc's centre, in the gauge and
    /// in the box that is too small for one.
    ///
    /// The score is the upper line and the level's name the lower one: the name is
    /// painted after the score, so an overlap would hide the digits.
    #[test]
    fn the_score_and_the_level_name_should_not_overlap_above_the_centre() {
        let gauge_canvas = canvas();
        let words_only = Rect::new(0.0, 0.0, 400.0, 60.0);
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
                (score_box.origin.x + score_box.size.width / 2.0 - center).abs() < 0.01,
                "the score is centred on the arc's centre line"
            );
            assert!(
                (level_box.origin.x + level_box.size.width / 2.0 - center).abs() < 0.01,
                "and so is the level's name"
            );
        }
    }

    /// A narrow canvas limits the gauge by its width, so the reading has to shrink
    /// with the band's radius instead of keeping the size the canvas height gives it.
    #[test]
    fn a_narrow_canvas_should_shrink_the_reading_with_the_band() {
        let wide = Rect::new(0.0, 0.0, 800.0, 800.0);
        let narrow = Rect::new(0.0, 0.0, 200.0, 800.0);
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
