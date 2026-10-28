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
//! The canvas has two states, and both draw the reading:
//!
//! - **large enough**: the half circle, its caps and the indicator, with the score
//!   above its level name inside the arc;
//! - **too small**: only the words. A pane squeezed under the threshold draws
//!   nothing it cannot place, rather than a half arc that runs off the canvas.
//!
//! Shaping is the expensive half of a frame and none of it moves between two
//! frames of the same reading, so a frame is prepared once and kept: the prepared
//! value is rebuilt only when the canvas, the reading, the language or the colours
//! change.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use base_ui::style::theme::{Theme, ThemeExt};
use gpui_kit::{
    App, Bounds, ContentMask, Hsla, IntoElement, PaintQuad, Path, PathBuilder, Pixels, Point,
    ShapedLine, SharedString, Styled, TextAlign, TextRun, TextStyle, Window, canvas, fill, point,
    px, size,
};
use smol_str::format_smolstr;

use crate::model::Level;

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

/// Where every piece of the gauge goes, for one canvas box.
///
/// The geometry is a pure function of the box: it is what the frame paints and what
/// the tests measure, and it is computed once per prepared frame rather than per
/// element.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeGeometry {
    /// The middle of the arc's diameter, at the bottom of the half circle.
    center: Point<Pixels>,
    /// Half the band's thickness, which is also the ends' radius.
    cap_radius: Pixels,
    /// The radius of the line the band is centred on.
    band_radius: Pixels,
    /// The outer edge of the band, including its ends.
    outer_radius: Pixels,
    /// The inner edge of the band.
    inner_radius: Pixels,
    /// The radius of the reading's dot. Its ring adds `indicator_halo` past it, and
    /// that reach is what the geometry keeps room for.
    indicator_radius: Pixels,
    /// The width of the ring around the reading's dot, in pixels. It follows the
    /// band between [`MIN_INDICATOR_HALO`] and [`MAX_INDICATOR_HALO`].
    indicator_halo: Pixels,
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
    fn compute(bounds: Bounds<Pixels>) -> Option<Self> {
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
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
        let center_y = bounds.top() + px((height + band_radius) / 2.0);

        Some(Self {
            center: point(bounds.center().x, center_y),
            cap_radius: px(cap_radius),
            band_radius: px(band_radius),
            outer_radius: px(band_radius + cap_radius),
            inner_radius: px(band_radius - cap_radius),
            indicator_radius: px(indicator_radius),
            indicator_halo: px(indicator_halo),
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
    fn point_at(&self, angle: f32, radius: Pixels) -> Point<Pixels> {
        let radians = angle.to_radians();

        point(
            self.center.x + radius * radians.cos(),
            self.center.y + radius * radians.sin(),
        )
    }

    /// The ten end circles, each with the band it caps.
    ///
    /// The centres sit on the ends of the trimmed arcs, not on the boundaries the
    /// bands were assigned: a cap rounds the cut its own band was drawn with, and a
    /// cap left on the boundary would sit in the gap and reach into its neighbour.
    fn cap_centers(&self) -> [(Point<Pixels>, usize); CAP_COUNT] {
        std::array::from_fn(|index| {
            let segment = index / 2;
            let (start, end) = self.arc_span(segment);
            let angle = if index % 2 == 0 { start } else { end };

            (self.point_at(angle, self.band_radius), segment)
        })
    }

    /// Where the reading sits on the band's centre line.
    fn indicator_center(&self, score: u8) -> Point<Pixels> {
        let angle = START_ANGLE + SWEEP * f32::from(score) / 100.0;

        self.point_at(angle, self.band_radius)
    }

    /// The ten end circles of one frame, one per band end.
    ///
    /// Each is a circle, not the box its centre and radius describe: the box's own
    /// corners reach `cap_radius * sqrt(2)` from the centre, past the band's outer
    /// edge, and only rounding every corner by the radius keeps the cap inside it.
    fn cap_quads(&self, colors: &GaugeColors) -> [PaintQuad; CAP_COUNT] {
        self.cap_centers()
            .map(|(center, segment)| circle_quad(center, self.cap_radius, colors.segments[segment]))
    }

    /// The reading's dot and the ring it is seated in.
    ///
    /// The ring is wider than the dot by `indicator_halo` on every side, so its
    /// corner radius is the dot's radius plus that enlargement — the radius that
    /// rounds it is the radius it is drawn at, not the dot's.
    fn indicator_quads(&self, score: u8, colors: &GaugeColors) -> (PaintQuad, PaintQuad) {
        let center = self.indicator_center(score);
        let radius = self.indicator_radius;

        (
            circle_quad(center, radius + self.indicator_halo, colors.halo),
            circle_quad(center, radius, colors.text),
        )
    }

    /// The filled annulus sector of one band.
    ///
    /// The outer arc runs forwards over the top and the inner one runs back, so the
    /// two radial cuts close the ring. Both are well under half a circle, which is
    /// why neither has to ask for the long arc.
    fn segment_path(&self, index: usize) -> Option<Path<Pixels>> {
        let (start, end) = self.arc_span(index);
        let radii = |radius| point(radius, radius);
        let mut path = PathBuilder::fill();
        path.move_to(self.point_at(start, self.outer_radius));
        path.arc_to(
            radii(self.outer_radius),
            px(0.0),
            false,
            true,
            self.point_at(end, self.outer_radius),
        );
        path.line_to(self.point_at(end, self.inner_radius));
        path.arc_to(
            radii(self.inner_radius),
            px(0.0),
            false,
            false,
            self.point_at(start, self.inner_radius),
        );
        path.close();

        path.build().ok()
    }
}

/// The colours one frame of the gauge is drawn with, resolved from the theme.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GaugeColors {
    /// One colour per band, in the order the bands are drawn.
    segments: [Hsla; SEGMENT_COUNT],
    /// The ring the indicator is seated in.
    halo: Hsla,
    /// The score, the level's name and the reading's dot.
    text: Hsla,
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
            halo: theme.roles.surface.sunken.hsla,
            text: theme.roles.fg.default.hsla,
        }
    }
}

/// The colour one level of the fear-and-greed scale is drawn in.
///
/// The scale reads the theme's own five-step ramp rather than the status and market
/// roles: the two market colours are deliberately close to their status neighbours,
/// which is right for a price and wrong for five bands that have to be told apart
/// side by side.
pub(crate) fn level_color(theme: &Theme, level: Level) -> Hsla {
    match level {
        Level::ExtremeFear => theme.roles.fear_greed.extreme_fear.hsla,
        Level::Fear => theme.roles.fear_greed.fear.hsla,
        Level::Neutral => theme.roles.fear_greed.neutral.hsla,
        Level::Greed => theme.roles.fear_greed.greed.hsla,
        Level::ExtremeGreed => theme.roles.fear_greed.extreme_greed.hsla,
    }
}

/// What a prepared frame was made from, so the next frame can keep it.
#[derive(Clone, PartialEq)]
struct GaugeKey {
    bounds: Bounds<Pixels>,
    score: u8,
    level: SharedString,
    text_style: TextStyle,
    colors: GaugeColors,
}

impl GaugeKey {
    fn matches(
        &self,
        bounds: Bounds<Pixels>,
        score: u8,
        level: &SharedString,
        colors: &GaugeColors,
        text_style: &TextStyle,
    ) -> bool {
        self.bounds == bounds
            && self.score == score
            && self.level == *level
            && self.colors == *colors
            && self.text_style == *text_style
    }
}

/// One frame of the gauge, ready to paint.
struct PreparedGauge {
    key: GaugeKey,
    geometry: Option<GaugeGeometry>,
    score_line: Arc<ShapedLine>,
    level_line: Arc<ShapedLine>,
    score_origin: Point<Pixels>,
    level_origin: Point<Pixels>,
    score_size: Pixels,
    level_size: Pixels,
}

/// The gauge of one pane, including the frame it prepared last.
///
/// The cache lives in the widget rather than in the element because an element is
/// rebuilt every frame, and the point of preparing a frame is that the next one can
/// reuse it.
pub(crate) struct Gauge {
    prepared: Rc<RefCell<Option<Arc<PreparedGauge>>>>,
}

impl Gauge {
    pub(crate) fn new() -> Self {
        Self {
            prepared: Rc::new(RefCell::new(None)),
        }
    }

    /// The element a frame embeds: the gauge of `score`, named by `level`.
    ///
    /// The level's name is resolved by the caller, in the language the frame draws
    /// in, which is also what makes a language change a new prepared frame.
    pub(crate) fn element(&self, score: u8, level: SharedString, cx: &App) -> impl IntoElement {
        let colors = GaugeColors::resolve(cx.theme());
        let prepared = self.prepared.clone();

        canvas(
            move |bounds, window, _| prepare(&prepared, bounds, score, &level, &colors, window),
            |bounds, prepared: Arc<PreparedGauge>, window, app| {
                paint(prepared.as_ref(), bounds, window, app);
            },
        )
        .size_full()
    }

    /// The level's name the last prepared frame was made from.
    ///
    /// A prepared frame is rebuilt when the reading, the canvas or the text of the
    /// frame changes — which is what makes a language change new text rather than the
    /// same cached one. A test reads it back to assert what a frame actually drew,
    /// because text is not otherwise readable from a painted frame.
    #[cfg(test)]
    pub(crate) fn prepared_level(&self) -> Option<SharedString> {
        let prepared = self.prepared.borrow();

        prepared.as_ref().map(|prepared| prepared.key.level.clone())
    }
}

/// Returns the frame of `bounds`, shaping its text only when its inputs changed.
fn prepare(
    cache: &RefCell<Option<Arc<PreparedGauge>>>,
    bounds: Bounds<Pixels>,
    score: u8,
    level: &SharedString,
    colors: &GaugeColors,
    window: &Window,
) -> Arc<PreparedGauge> {
    let text_style = window.text_style();
    {
        let cached = cache.borrow();
        if let Some(prepared) = cached.as_ref()
            && prepared
                .key
                .matches(bounds, score, level, colors, &text_style)
        {
            return prepared.clone();
        }
    }

    let geometry = GaugeGeometry::compute(bounds);
    let (score_size, level_size) = text_sizes(bounds, geometry);
    let score_height = score_size * LINE_HEIGHT_RATIO;
    let level_height = level_size * LINE_HEIGHT_RATIO;
    let (score_box, level_box) = text_boxes(bounds, geometry, score_height, level_height);
    let score_line = Arc::new(shape(
        SharedString::new(format_smolstr!("{score}").as_str()),
        score_size,
        colors.text,
        &text_style,
        window,
    ));
    let level_line = Arc::new(shape(
        level.clone(),
        level_size,
        colors.text,
        &text_style,
        window,
    ));

    let prepared = Arc::new(PreparedGauge {
        key: GaugeKey {
            bounds,
            score,
            level: level.clone(),
            text_style,
            colors: *colors,
        },
        geometry,
        score_line,
        level_line,
        score_origin: score_box.origin,
        level_origin: level_box.origin,
        score_size,
        level_size,
    });
    *cache.borrow_mut() = Some(prepared.clone());

    prepared
}

/// Shapes one line of text in one colour.
fn shape(
    text: SharedString,
    font_size: Pixels,
    color: Hsla,
    text_style: &TextStyle,
    window: &Window,
) -> ShapedLine {
    let len = text.len();

    window.text_system().shape_line(
        text,
        font_size,
        &[TextRun {
            len,
            font: text_style.font(),
            color,
            ..Default::default()
        }],
        None,
    )
}

/// The score and level font sizes for one canvas.
///
/// With a gauge, the text is sized from the band's radius: that is the space the
/// text has to fit inside, and a narrow pane limits the radius by its width while
/// the canvas keeps its height. Without a gauge there is no radius, so the shorter
/// side of the canvas is the measure that cannot overflow it.
fn text_sizes(bounds: Bounds<Pixels>, geometry: Option<GaugeGeometry>) -> (Pixels, Pixels) {
    let score_size = match geometry {
        Some(geometry) => geometry.band_radius * SCORE_TEXT_RADIUS_RATIO,
        None => px(
            f32::from(bounds.size.width).min(f32::from(bounds.size.height))
                * SCORE_TEXT_FALLBACK_RATIO,
        ),
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
    bounds: Bounds<Pixels>,
    geometry: Option<GaugeGeometry>,
    score_height: Pixels,
    level_height: Pixels,
) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let gap = level_height * TEXT_LINE_GAP_RATIO;
    let stacked = score_height + gap + level_height;
    let top = match geometry {
        Some(geometry) => geometry.center.y - level_height * TEXT_ABOVE_DIAMETER_RATIO - stacked,
        None => bounds.center().y - stacked * 0.5,
    };

    (
        Bounds::new(
            point(bounds.left(), top),
            size(bounds.size.width, score_height),
        ),
        Bounds::new(
            point(bounds.left(), top + score_height + gap),
            size(bounds.size.width, level_height),
        ),
    )
}

/// Draws one prepared frame, clipped to the canvas it was prepared for.
fn paint(prepared: &PreparedGauge, bounds: Bounds<Pixels>, window: &mut Window, app: &mut App) {
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        if let Some(geometry) = prepared.geometry.as_ref() {
            for index in 0..SEGMENT_COUNT {
                if let Some(path) = geometry.segment_path(index) {
                    window.paint_path(path, prepared.key.colors.segments[index]);
                }
            }
            // The caps are painted over the bands' straight cuts, which is what
            // turns every cut end into a rounded one.
            for quad in geometry.cap_quads(&prepared.key.colors) {
                window.paint_quad(quad);
            }
            paint_indicator(geometry, prepared.key.score, &prepared.key.colors, window);
        }

        let _ = prepared.score_line.paint(
            prepared.score_origin,
            prepared.score_size * LINE_HEIGHT_RATIO,
            TextAlign::Center,
            Some(bounds.size.width),
            window,
            app,
        );
        let _ = prepared.level_line.paint(
            prepared.level_origin,
            prepared.level_size * LINE_HEIGHT_RATIO,
            TextAlign::Center,
            Some(bounds.size.width),
            window,
            app,
        );
    });
}

/// Draws the reading's dot on the band's centre line.
fn paint_indicator(geometry: &GaugeGeometry, score: u8, colors: &GaugeColors, window: &mut Window) {
    let (halo, dot) = geometry.indicator_quads(score, colors);
    // The ring is the content face's own colour: it is what keeps the foreground
    // dot apart from the band it covers in either appearance.
    window.paint_quad(halo);
    window.paint_quad(dot);
}

/// The box of a solid circle.
fn circle_bounds(center: Point<Pixels>, radius: Pixels) -> Bounds<Pixels> {
    Bounds::new(
        point(center.x - radius, center.y - radius),
        size(radius * 2.0, radius * 2.0),
    )
}

/// The quad a solid circle is painted as.
///
/// Rounding every corner by the radius is what makes the quad a circle: a quad with
/// square corners is a box, and a box reaches `radius * sqrt(2)` from its centre at
/// the corner — past the edge the geometry allowed for the drawing.
fn circle_quad(center: Point<Pixels>, radius: Pixels, color: Hsla) -> PaintQuad {
    fill(circle_bounds(center, radius), color).corner_radii(radius)
}

#[cfg(test)]
mod tests {
    use base_ui::style::palette::Appearance;
    use gpui_kit::{Bounds, Corners, point, px, size};

    use super::*;
    use crate::model::Level;

    /// The canvas most of these tests measure: wide enough that the width is not
    /// what limits the gauge.
    fn canvas() -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)))
    }

    fn geometry() -> GaugeGeometry {
        GaugeGeometry::compute(canvas()).expect("a normal canvas should hold a gauge")
    }

    fn distance(from: Point<Pixels>, to: Point<Pixels>) -> f32 {
        let dx = f32::from(from.x - to.x);
        let dy = f32::from(from.y - to.y);

        (dx * dx + dy * dy).sqrt()
    }

    /// The distance from `center` to the farthest corner of `bounds`.
    ///
    /// It is how far a quad would reach if its corners were painted square, which is
    /// the shape the radius has to round off.
    fn farthest_corner(bounds: Bounds<Pixels>, center: Point<Pixels>) -> f32 {
        [
            bounds.origin,
            point(bounds.right(), bounds.top()),
            point(bounds.left(), bounds.bottom()),
            point(bounds.right(), bounds.bottom()),
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
        let cap_diameter = f32::from(geometry.cap_radius) * 2.0;

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
                (radius - f32::from(geometry.band_radius)).abs() < 0.01,
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
        let reach = f32::from(geometry.band_radius)
            + f32::from(geometry.indicator_radius)
            + f32::from(geometry.indicator_halo);
        let top = geometry.center.y - px(reach);
        let bottom = geometry.center.y + geometry.indicator_radius + geometry.indicator_halo;
        let left = geometry.center.x - px(reach);
        let right = geometry.center.x + px(reach);

        assert!(
            (f32::from(geometry.outer_radius)
                - (f32::from(geometry.band_radius) + f32::from(geometry.cap_radius)))
            .abs()
                < 0.01,
            "the band's outermost point is the band plus one cap"
        );
        assert!(
            top >= canvas.top(),
            "the top of the indicator stays below the top of the canvas"
        );
        assert!(
            bottom <= canvas.bottom(),
            "the indicator at either end of the scale stays above the bottom"
        );
        assert!(
            left >= canvas.left(),
            "the left end of the indicator stays inside the canvas"
        );
        assert!(
            right <= canvas.right(),
            "the right end of the indicator stays inside the canvas"
        );

        let top_margin = f32::from(top - canvas.top());
        let bottom_margin = f32::from(canvas.bottom() - bottom);
        let left_margin = f32::from(left - canvas.left());
        let right_margin = f32::from(canvas.right() - right);
        assert!(
            (top_margin - bottom_margin).abs() < 0.01,
            "the gauge block is centred vertically in the canvas"
        );
        assert!(
            (left_margin - right_margin).abs() < 0.01,
            "the gauge block is centred horizontally in the canvas"
        );

        for (center, _) in geometry.cap_centers() {
            assert!(center.y - geometry.cap_radius >= canvas.top());
            assert!(center.x - geometry.cap_radius >= canvas.left());
            assert!(center.x + geometry.cap_radius <= canvas.right());
        }
    }

    /// The reading walks from the left end of the scale, over the top, to the right
    /// end — the same direction the bands are drawn in.
    #[test]
    fn the_indicator_should_walk_from_the_left_end_to_the_right_one() {
        let geometry = geometry();
        let band = f32::from(geometry.band_radius);
        let left = geometry.indicator_center(0);
        let top = geometry.indicator_center(50);
        let right = geometry.indicator_center(100);

        assert!(
            (f32::from(left.x - geometry.center.x) + band).abs() < 0.01
                && f32::from(left.y - geometry.center.y).abs() < 0.01,
            "zero is the left end of the half circle"
        );
        assert!(
            f32::from(top.x - geometry.center.x).abs() < 0.01
                && (f32::from(top.y - geometry.center.y) + band).abs() < 0.01,
            "fifty is the top of the half circle"
        );
        assert!(
            (f32::from(right.x - geometry.center.x) - band).abs() < 0.01
                && f32::from(right.y - geometry.center.y).abs() < 0.01,
            "one hundred is the right end of the half circle"
        );

        let mut previous = f32::from(left.x);
        for score in 1..=100_u8 {
            let x = f32::from(geometry.indicator_center(score).x);
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
        let too_short = Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(MIN_CANVAS_WIDTH), px(MIN_CANVAS_HEIGHT - 1.0)),
        );
        let too_narrow = Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(MIN_CANVAS_WIDTH - 1.0), px(MIN_CANVAS_HEIGHT)),
        );
        let empty = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0)));

        assert!(GaugeGeometry::compute(too_short).is_none());
        assert!(GaugeGeometry::compute(too_narrow).is_none());
        assert!(GaugeGeometry::compute(empty).is_none());
        assert!(
            GaugeGeometry::compute(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(MIN_CANVAS_WIDTH), px(MIN_CANVAS_HEIGHT)),
            ))
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
                theme.roles.fear_greed.extreme_fear.hsla,
                theme.roles.fear_greed.fear.hsla,
                theme.roles.fear_greed.neutral.hsla,
                theme.roles.fear_greed.greed.hsla,
                theme.roles.fear_greed.extreme_greed.hsla,
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
                theme.roles.fear_greed.extreme_fear.hsla,
                theme.roles.fear_greed.fear.hsla,
                theme.roles.fear_greed.neutral.hsla,
                theme.roles.fear_greed.greed.hsla,
                theme.roles.fear_greed.extreme_greed.hsla,
            ]
        );
        assert_eq!(colors.text, theme.roles.fg.default.hsla);
        assert_eq!(colors.halo, theme.roles.surface.sunken.hsla);
    }

    /// The indicator is a marker, not a band end: its diameter is wider than the
    /// band and it is seated in a ring of the content face.
    #[test]
    fn the_indicator_should_be_wider_than_the_band() {
        let theme = Theme::of(Appearance::Light);
        let colors = GaugeColors::resolve(&theme);
        let geometry = geometry();
        let band = f32::from(geometry.cap_radius) * 2.0;

        assert!(
            f32::from(geometry.indicator_radius) * 2.0 > band,
            "the marker's diameter is wider than the band"
        );
        assert!(
            (f32::from(geometry.indicator_radius)
                - f32::from(geometry.cap_radius) * INDICATOR_RATIO)
                .abs()
                < 0.01,
            "the marker's radius is the band end's radius times the reference's ratio"
        );

        let (halo, dot) = geometry.indicator_quads(46, &colors);
        assert_eq!(dot.background, colors.text.into());
        assert_eq!(halo.background, colors.halo.into());
        assert_eq!(
            halo.corner_radii,
            Corners::all(geometry.indicator_radius + geometry.indicator_halo),
            "the ring is rounded by its own radius, enlargement included"
        );
    }

    /// The ring follows the band, with a floor for tiny gauges and a ceiling for
    /// huge ones: a fixed width is a hairline on a large gauge and swallows a small
    /// one.
    #[test]
    fn the_indicator_ring_should_follow_the_band_with_a_floor_and_a_ceiling() {
        let geometry_of = |width: f32, height: f32| {
            GaugeGeometry::compute(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(width), px(height)),
            ))
            .expect("the fixture canvas holds a gauge")
        };

        let tiny = geometry_of(96.0, 72.0);
        let medium = geometry_of(400.0, 240.0);
        let large = geometry_of(1600.0, 800.0);
        let huge = geometry_of(2400.0, 1200.0);

        assert_eq!(
            tiny.indicator_halo,
            px(MIN_INDICATOR_HALO),
            "a tiny gauge keeps the ring at its floor"
        );
        assert!(
            (f32::from(medium.indicator_halo) - 4.32).abs() < 0.01,
            "a middle-sized gauge gets twenty percent of its band"
        );
        assert!(
            (f32::from(large.indicator_halo) - 14.4).abs() < 0.01,
            "a large gauge still gets twenty percent of its band"
        );
        assert_eq!(
            huge.indicator_halo,
            px(MAX_INDICATOR_HALO),
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
                quad.corner_radii,
                Corners::all(geometry.cap_radius),
                "every corner of a cap is rounded by the cap's radius"
            );
            assert!(
                farthest_corner(quad.bounds, geometry.center) > f32::from(geometry.outer_radius),
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
            dot.corner_radii,
            Corners::all(dot_radius),
            "the dot is a circle rather than a square"
        );
        assert_eq!(
            halo.corner_radii,
            Corners::all(dot_radius + geometry.indicator_halo),
            "the ring is rounded by its own radius, enlargement included"
        );
        assert!(
            halo.corner_radii.top_left > dot.corner_radii.top_left,
            "the ring is the dot plus its enlargement on every side"
        );
    }

    /// The reading's two lines are stacked above the arc's centre, in the gauge and
    /// in the box that is too small for one.
    ///
    /// The score is the upper line and the level's name the lower one: the name is
    /// painted after the score, so an overlap would hide the digits.
    #[test]
    fn the_score_and_the_level_name_should_not_overlap_above_the_centre() {
        let gauge_canvas = canvas();
        let words_only = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(60.0)));
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
                score_box.bottom() < level_box.top(),
                "the level's name must not be painted over the score"
            );
            match geometry {
                Some(geometry) => assert!(
                    level_box.bottom() < geometry.center.y,
                    "the reading stays above the arc's centre"
                ),
                None => assert!(
                    level_box.bottom() <= bounds.bottom(),
                    "the reading stays inside a canvas with no arc"
                ),
            }
            assert!(
                score_box.top() >= bounds.top(),
                "the reading starts inside the canvas"
            );
            assert_eq!(
                score_box.center().x,
                bounds.center().x,
                "the score is centred on the arc's centre line"
            );
            assert_eq!(
                level_box.center().x,
                bounds.center().x,
                "and so is the level's name"
            );
        }
    }

    /// A narrow canvas limits the gauge by its width, so the reading has to shrink
    /// with the band's radius instead of keeping the size the canvas height gives it.
    #[test]
    fn a_narrow_canvas_should_shrink_the_reading_with_the_band() {
        let wide = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(800.0)));
        let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(800.0)));
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
            score_box.top() >= inner_top,
            "the reading stays inside the arc's inner edge on a narrow canvas"
        );
    }
}
