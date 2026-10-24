//! The half gauge the current reading is drawn on.
//!
//! The gauge is vector geometry rather than a picture: a picture would bake one
//! theme's colours into it and be rasterized at one scale, and this pane has to be
//! drawn in both appearances at every display density. So each of the five bands is
//! a filled annulus sector, the ends are drawn as circles over the cuts to give them
//! round caps, and the current reading is a dot on the centre line of the band — all
//! from the semantic roles, none of it from a palette value or a literal colour.
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

/// How much of the canvas' height is left below the arc.
const BOTTOM_INSET_RATIO: f32 = 0.10;

/// The gap at each end of a band, as a share of the band's thickness.
const GAP_RATIO: f32 = 0.4;

/// The narrowest canvas a gauge is drawn on.
const MIN_CANVAS_WIDTH: f32 = 96.0;

/// The shortest canvas a gauge is drawn on.
const MIN_CANVAS_HEIGHT: f32 = 72.0;

/// The score's font size, as a share of the canvas' height.
const SCORE_TEXT_RATIO: f32 = 0.28;

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
const INDICATOR_RATIO: f32 = 1.15;

/// The width of the ring the indicator is seated in, in pixels.
const INDICATOR_HALO: f32 = 2.0;

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
    /// How far each end of a band is pulled back from its boundary, in degrees.
    gap_half: f32,
}

impl GaugeGeometry {
    /// Places the gauge in `bounds`, or answers nothing when the box is too small
    /// to hold one.
    ///
    /// The band's radius keeps room for its own thickness and for the ends' caps, so
    /// the outermost point of the drawing is exactly the radius the box allows.
    fn compute(bounds: Bounds<Pixels>) -> Option<Self> {
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        if width < MIN_CANVAS_WIDTH || height < MIN_CANVAS_HEIGHT {
            return None;
        }

        let band = (height * BAND_RATIO).min(width * BAND_RATIO);
        let cap_radius = band / 2.0;
        let bottom_inset = height * BOTTOM_INSET_RATIO;
        let limit = (width / 2.0).min(height - bottom_inset);
        // The ends' caps reach one cap radius past the band's centre line, so the
        // band's radius gives back the whole band's thickness rather than half of it.
        let band_radius = limit - band;
        let gap_half = ((band * GAP_RATIO) / band_radius).to_degrees();

        Some(Self {
            center: point(bounds.center().x, bounds.bottom() - px(bottom_inset)),
            cap_radius: px(cap_radius),
            band_radius: px(band_radius),
            outer_radius: px(band_radius + cap_radius),
            inner_radius: px(band_radius - cap_radius),
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
    /// The centres sit on the boundaries the bands were assigned rather than on the
    /// trimmed arcs, so a cap covers the straight cut its own band was drawn with.
    fn cap_centers(&self) -> [(Point<Pixels>, usize); CAP_COUNT] {
        std::array::from_fn(|index| {
            let segment = index / 2;
            let boundary = self.segment_span(segment).0 + SEGMENT_SWEEP * (index % 2) as f32;

            (self.point_at(boundary, self.band_radius), segment)
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
    /// The ring is wider than the dot by [`INDICATOR_HALO`] on every side, so its
    /// corner radius is the dot's radius plus that enlargement — the radius that
    /// rounds it is the radius it is drawn at, not the dot's.
    fn indicator_quads(&self, score: u8, colors: &GaugeColors) -> (PaintQuad, PaintQuad) {
        let center = self.indicator_center(score);
        let radius = self.cap_radius * INDICATOR_RATIO;

        (
            circle_quad(center, radius + px(INDICATOR_HALO), colors.halo),
            circle_quad(center, radius, colors.indicator),
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
    /// The indicator, which is the colour of the band the reading is in.
    indicator: Hsla,
    /// The ring the indicator is seated in.
    halo: Hsla,
    /// The score and the level's name.
    text: Hsla,
}

impl GaugeColors {
    fn resolve(theme: &Theme, level: Level) -> Self {
        Self {
            segments: [
                level_color(theme, Level::ExtremeFear),
                level_color(theme, Level::Fear),
                level_color(theme, Level::Neutral),
                level_color(theme, Level::Greed),
                level_color(theme, Level::ExtremeGreed),
            ],
            indicator: level_color(theme, level),
            halo: theme.roles.surface.sunken.hsla,
            text: theme.roles.fg.default.hsla,
        }
    }
}

/// The colour one level of the scale is drawn in.
///
/// Every colour is a resolved role: the two market roles say "down" and "up" rather
/// than "bad" and "good", and the two status roles are the extremes the market ones
/// do not reach. The two pairs are close in hue but never adjacent on the scale —
/// neutral sits between them — so the five bands stay distinguishable side by side.
pub(crate) fn level_color(theme: &Theme, level: Level) -> Hsla {
    match level {
        Level::ExtremeFear => theme.roles.status.danger.base.hsla,
        Level::Fear => theme.roles.market.down.hsla,
        Level::Neutral => theme.roles.status.warning.base.hsla,
        Level::Greed => theme.roles.market.up.hsla,
        Level::ExtremeGreed => theme.roles.status.success.base.hsla,
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
        let colors = GaugeColors::resolve(cx.theme(), Level::of(score));
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
    let score_size = px(f32::from(bounds.size.height) * SCORE_TEXT_RATIO);
    let level_size = score_size * LEVEL_TEXT_RATIO;
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
    // The ring is the content face's own colour: on a dark theme it is what keeps
    // the dot apart from the band it covers.
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

    /// The drawing fits the box it was given, which is what lets the canvas clip
    /// without cutting anything.
    #[test]
    fn the_bands_and_their_caps_should_stay_inside_the_canvas() {
        let canvas = canvas();
        let geometry = GaugeGeometry::compute(canvas).expect("the fixture canvas holds a gauge");

        assert!(
            (f32::from(geometry.outer_radius)
                - (f32::from(geometry.band_radius) + f32::from(geometry.cap_radius)))
            .abs()
                < 0.01,
            "the outermost point of the drawing is the band plus one cap"
        );
        assert!(
            geometry.center.y - geometry.outer_radius >= canvas.top(),
            "the top of the arc stays below the top of the canvas"
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
    /// theme's roles rather than from a literal.
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
        let colors = GaugeColors::resolve(&theme, Level::Greed);
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
        let dot_radius = geometry.cap_radius * INDICATOR_RATIO;
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
            Corners::all(dot_radius + px(INDICATOR_HALO)),
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
            let score_size = px(f32::from(bounds.size.height) * SCORE_TEXT_RATIO);
            let level_size = score_size * LEVEL_TEXT_RATIO;
            let (score_box, level_box) = text_boxes(
                bounds,
                geometry,
                score_size * LINE_HEIGHT_RATIO,
                level_size * LINE_HEIGHT_RATIO,
            );
            let cy = bounds.bottom() - px(f32::from(bounds.size.height) * BOTTOM_INSET_RATIO);

            assert!(
                score_box.bottom() < level_box.top(),
                "the level's name must not be painted over the score"
            );
            assert!(
                level_box.bottom() < cy,
                "the reading stays above the arc's centre"
            );
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
}
