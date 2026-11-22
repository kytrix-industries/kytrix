//! Painting one prepared prediction chart.
//!
//! One frame turns the prepared boxes, lines and ticks into quads, registered meshes
//! and shaped text. This module holds no state beyond the shaped-line cache it is
//! handed: the y labels travel with the prepared chart, and the series palette is read
//! from the theme into a small copy, so neither the y-label strings nor the palette is
//! rebuilt per frame. The only text a frame formats is the hover's own — a label per
//! series and the time under the pointer — because those are the only strings that
//! depend on where the pointer is, and the only buffer a frame builds is the small
//! label column those strings sit in.
//!
//! Everything that follows the pointer is drawn here and only here: the crosshair, the
//! right-side fade of every line, the hover dots, the label column and the time label.
//! The prepared chart underneath them is shared with every pointer position, which is
//! what keeps a mouse crossing the plot from rebuilding geometry.
//!
//! # Lines are registered meshes
//!
//! The engine has no path builder: a line is a registered mesh ([`GeometryDesc`]) drawn
//! with [`GeometryDraw`]. [`line_geometry`] builds that mesh — a ribbon through the
//! step-line's points, one quad per segment and a square at every joint — and the chart
//! registers it once per prepared frame, releasing the previous frame's meshes. The
//! ribbon carries no anti-aliasing distance (every vertex is marked as interior), so the
//! line is drawn with a hard edge rather than the feather the old path painter gave it;
//! that is the registered difference from the toolkit's old chart.
//!
//! # Text
//!
//! Labels are shaped through [`super::text::ChartTextCache`] and pushed as glyph runs,
//! so the only cost a hover pays is the two strings whose values changed. Every run a
//! frame pushes is reported back to the chart: they are the atlas leases a steady frame
//! has to check before it decides it may keep the canvas on screen.

use std::sync::Arc;

use base_ui::theme::{resolved::Theme, role::Role};
use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::{Point, Rect, Size, Transform},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology},
        primitive::{Border, ClipRect, GeometryDraw, Quad, QuadPaint},
    },
    text::{ShapedTextHandle, TextStyle},
};

use super::{
    canvas::{PreparedChart, SeriesMeshes},
    geometry::{RectExt, ScreenPoint, y_for_percent, y_for_series},
    hover::{
        HoveredSample, LABEL_COLLISION_GAP, LABEL_GAP, LABEL_HEIGHT, dot_bounds, hover_point,
        label_side, label_tops, label_x, value_at,
    },
    text::{
        AXIS_FONT_SIZE, ChartTextCache, LABEL_FONT_SIZE, REGULAR_WEIGHT, STRONG_WEIGHT,
        TIME_LABEL_FONT_SIZE, format_hover_time, format_percent_one_decimal, label_style,
    },
};

/// The series stroke, which is the reference chart's `stroke-width`.
pub(crate) const SERIES_STROKE_WIDTH: f32 = 1.75;
/// The light halo drawn under a series so crossing lines stay separable.
pub(crate) const SERIES_HALO_WIDTH: f32 = 2.0;
/// What a line is faded to right of the hovered sample.
const SERIES_FADE_ALPHA: f32 = 0.25;

/// The grid is a dot every four pixels: one on, three off.
const GRID_DOT: f32 = 1.0;
const GRID_PERIOD: f32 = 4.0;

/// The crosshair is solid, reaches up into the time strip, and stops at 0%.
const CROSSHAIR_WIDTH: f32 = 1.5;
const CROSSHAIR_OVERHANG_TOP: f32 = 17.0;

/// How far a y label sits right of the plot, and an x label below it.
///
/// The y gap is wider than the endpoint pulse's reach ([`PULSE_MAX_RADIUS`]) so a dot
/// on the plot's right edge can never grow into the label column. It is also the
/// largest integer gap that leaves the widest tick label (`100%`, about 29 px in the
/// window's font) inside the 40 px right gutter: 11 + 29 fits, 12 + 29 does not.
const Y_LABEL_GAP: f32 = 11.0;
const X_LABEL_GAP: f32 = 7.0;
const AXIS_LINE_HEIGHT: f32 = 14.0;

/// The label box: padding 4, colour bar 4x12, gap 4, text, padding 6.
const LABEL_PADDING_LEFT: f32 = 4.0;
const LABEL_BAR_WIDTH: f32 = 4.0;
const LABEL_BAR_HEIGHT: f32 = 12.0;
const LABEL_BAR_GAP: f32 = 4.0;
const LABEL_PADDING_RIGHT: f32 = 6.0;
const LABEL_BAR_RADIUS: f32 = 2.0;
/// From the box's left edge to its text: padding, bar and gap.
const LABEL_TEXT_INSET: f32 = LABEL_PADDING_LEFT + LABEL_BAR_WIDTH + LABEL_BAR_GAP;
/// Everything in a box but its text.
const LABEL_BOX_CHROME: f32 =
    LABEL_PADDING_LEFT + LABEL_BAR_WIDTH + LABEL_BAR_GAP + LABEL_PADDING_RIGHT;
const LABEL_BOX_RADIUS: f32 = 4.0;
const LABEL_BOX_BORDER: f32 = 1.0;
/// The label box is exactly as tall as its text line is set in.
const LABEL_LINE_HEIGHT: f32 = LABEL_HEIGHT;

/// The hover dot: an eight-pixel fill inside a twelve-pixel wash.
const HOVER_DOT_INNER: f32 = 8.0;
const HOVER_DOT_OUTER: f32 = 12.0;
const HOVER_DOT_OUTER_ALPHA: f32 = 0.1;

/// The static dot at every series' newest point, and the pulse around it.
///
/// It is the same fill as the dot where the crosshair meets a line, so the newest
/// reading carries the same weight as a hovered one.
const ENDPOINT_RADIUS: f32 = HOVER_DOT_INNER / 2.0;
/// The pulse starts outside the endpoint dot, so it is visible from the first frame
/// instead of spending the first part of each cycle hidden under a dot its own size.
const PULSE_MIN_RADIUS: f32 = ENDPOINT_RADIUS + 2.0;
const PULSE_MAX_RADIUS: f32 = 10.0;
const PULSE_ALPHA: f32 = 0.4;
const PULSE_DOT_SCALE: f32 = 0.08;

/// The overhang the dot masks add beyond the plot.
///
/// A dot whose centre sits on the plot's edge has half of itself outside it, and the
/// plot's own mask would cut it there. The first sample always lands on the left edge
/// of a window that starts at its oldest point and the newest point always lands on
/// the right edge, so the hover mask gives the left the dot's wash and the right the
/// pulse's full radius. The right overhang stays inside [`Y_LABEL_GAP`], so no dot can
/// reach the label column.
///
/// The endpoint's pulse grows to [`PULSE_MAX_RADIUS`] above and below its centre as
/// well; the hover mask's wash-sized vertical overhang would clip it at the axis
/// extremes, so endpoints paint in [`endpoint_mask`] instead.
const DOT_MASK_LEFT: f32 = HOVER_DOT_OUTER / 2.0;
const DOT_MASK_RIGHT: f32 = PULSE_MAX_RADIUS;
const DOT_MASK_VERTICAL: f32 = HOVER_DOT_OUTER / 2.0;
const ENDPOINT_MASK_VERTICAL: f32 = PULSE_MAX_RADIUS;

/// The time label fills the strip above the plot.
const TIME_LABEL_TOP_INSET: f32 = 1.0;
const TIME_LABEL_LINE_HEIGHT: f32 = 16.0;

/// The colours one frame is drawn with, resolved from the theme.
///
/// The canvas bakes them in, which is why an appearance switch is one of the painted
/// key's inputs: a canvas built for the light appearance is the wrong value for the
/// dark one.
///
/// The palette is the theme's six series roles. The old chart read an eight-colour
/// array out of the theme; six roles is one fewer pair of hues, so a series whose index
/// is six or more wraps — a chart of more than six lines reuses colours, which the old
/// one only did past eight (registered difference).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartColors {
    palette: [Color; 6],
    grid: Color,
    axis: Color,
    crosshair: Color,
    halo: Color,
    foreground: Color,
    muted: Color,
    label_background: Color,
    label_border: Color,
}

impl ChartColors {
    /// The colours the theme resolves, for one appearance.
    pub(crate) fn resolve(theme: &Theme) -> Self {
        Self {
            palette: [
                theme.role(Role::ChartSeries0),
                theme.role(Role::ChartSeries1),
                theme.role(Role::ChartSeries2),
                theme.role(Role::ChartSeries3),
                theme.role(Role::ChartSeries4),
                theme.role(Role::ChartSeries5),
            ],
            grid: theme.role(Role::ChartGrid),
            axis: theme.role(Role::ChartAxis),
            crosshair: theme.role(Role::ChartCrosshair),
            // The halo is the pane body behind the canvas: it only separates crossing
            // lines, so fading it would not change what a reader sees.
            halo: theme.role(Role::SurfaceSunken),
            foreground: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            label_background: theme.role(Role::SurfaceOverlay),
            label_border: theme.role(Role::LineBorder),
        }
    }

    /// The colour of one series, by the index the model gave it.
    fn series(self, index: usize) -> Color {
        self.palette[index % self.palette.len()]
    }
}

/// Draws one prepared chart.
///
/// The layers go down in the design's order: grid, axis labels, series, then
/// everything the pointer adds, and the endpoint pulse last. The pulse is above the
/// crosshair and the labels.
///
/// The second half of the answer says whether every line of text reached the canvas. A
/// line whose atlas lease died while the canvas was being built is refused by the
/// engine rather than encoded from reused texture, so that frame is missing a run and
/// is not one the chart can keep. The handles are the lines the canvas was built from —
/// the leases a steady frame has to check before it keeps this canvas.
pub(crate) fn paint(
    app: &mut App,
    text: &mut ChartTextCache,
    translate: &Translate,
    prepared: &PreparedChart,
    meshes: &[SeriesMeshes],
    colors: &ChartColors,
    hovered: Option<HoveredSample>,
    pulse_phase: Option<f32>,
) -> (Canvas, bool, Vec<Arc<ShapedTextHandle>>) {
    let layout = prepared.layout;
    if prepared.domain.is_none() || layout.plot.size.width <= 0.0 || layout.plot.size.height <= 0.0
    {
        return (Canvas::new(layout.bounds), true, Vec::new());
    }
    let mut context = PaintContext {
        app,
        translate,
        text,
        colors,
        used: Vec::new(),
        whole: true,
    };
    let mut canvas = Canvas::new(layout.bounds);
    paint_grid(&mut canvas, prepared, colors);
    paint_axis_labels(&mut canvas, prepared, &mut context);
    paint_series(&mut canvas, prepared, meshes, hovered, &mut context);
    if let Some(hovered) = hovered {
        paint_hover(&mut canvas, prepared, hovered, &mut context);
    }
    paint_endpoints(&mut canvas, prepared, hovered, pulse_phase, &mut context);
    let whole = context.whole;
    (canvas, whole, context.used)
}

/// The shaped text and theme one paint pass needs.
struct PaintContext<'a> {
    app: &'a mut App,
    translate: &'a Translate,
    text: &'a mut ChartTextCache,
    colors: &'a ChartColors,
    /// The lines this frame pushed, which are the canvas' text.
    used: Vec<Arc<ShapedTextHandle>>,
    /// Whether every line this pass placed reached the canvas.
    whole: bool,
}

impl PaintContext<'_> {
    /// Pushes one line of text with its top-left corner at (`x`, `y`).
    ///
    /// A refused line is one whose atlas lease died between being shaped and being
    /// pushed: its glyphs have been reused, so the run is left out rather than drawn
    /// from stale texture, and the pass is not a whole frame any more.
    fn text(&mut self, canvas: &mut Canvas, text: &str, x: f32, y: f32, style: &TextStyle) {
        if text.is_empty() {
            return;
        }
        let line = self.text.line(self.app, text, style);
        if canvas.push_shaped_text(&line, Point::new(x, y)) {
            #[cfg(test)]
            record_text(text, x, y, style.size, line.size().width);
            self.used.push(line);
        } else {
            self.whole = false;
        }
    }

    /// The width one line of `text` occupies in `style`.
    fn width(&mut self, text: &str, style: &TextStyle) -> f32 {
        self.text.width(self.app, text, style)
    }
}

/// Draws the dotted grid.
///
/// A horizontal row of dots spans the plot; a vertical one runs from the plot's top
/// through the band under it, which is what the reference chart measures, and stops
/// before the strip above the plot that holds the hovered time. The rows are disjoint
/// and so are the columns, so each direction is one run of quads under one clip.
fn paint_grid(canvas: &mut Canvas, prepared: &PreparedChart, colors: &ChartColors) {
    let layout = prepared.layout;
    push_clip(canvas, layout.plot);
    for tick in &prepared.axis.ticks {
        let y = y_for_percent(*tick, &prepared.axis, layout.plot);
        let mut x = layout.plot.origin.x;
        while x < layout.plot.right() {
            let width = GRID_DOT.min(layout.plot.right() - x);
            push_quad(canvas, Rect::new(x, y, width, GRID_DOT), colors.grid);
            x += GRID_PERIOD;
        }
    }
    pop_clip(canvas);
    push_clip(canvas, layout.bounds);
    for tick in prepared.time_ticks.iter() {
        let mut y = layout.plot.origin.y;
        while y < layout.bounds.bottom() {
            let height = GRID_DOT.min(layout.bounds.bottom() - y);
            push_quad(canvas, Rect::new(tick.x, y, GRID_DOT, height), colors.grid);
            y += GRID_PERIOD;
        }
    }
    pop_clip(canvas);
}

/// Draws the y values right of the plot and the x times under it.
///
/// The y labels were formatted when the chart was prepared; the x labels come with
/// their ticks.
fn paint_axis_labels(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let style = label_style(
        AXIS_FONT_SIZE,
        AXIS_LINE_HEIGHT,
        context.colors.axis,
        REGULAR_WEIGHT,
    );
    let x_origin = layout.plot.right() + Y_LABEL_GAP;
    for (tick, label) in prepared.axis.ticks.iter().zip(prepared.axis_labels.iter()) {
        let y = y_for_percent(*tick, &prepared.axis, layout.plot) - AXIS_LINE_HEIGHT / 2.0;
        context.text(canvas, label.as_str(), x_origin, y, &style);
    }

    let y_origin = layout.bottom_band().origin.y + X_LABEL_GAP;
    let left = layout.bounds.origin.x;
    let right = layout.bounds.right();
    for tick in prepared.time_ticks.iter() {
        if tick.label.is_empty() {
            // A repeated clock label keeps its gridline but draws no text.
            continue;
        }
        let width = context.width(tick.label.as_str(), &style);
        let x = (tick.x - width / 2.0).clamp(left, (right - width).max(left));
        context.text(canvas, tick.label.as_str(), x, y_origin, &style);
    }
}

/// Draws every series: its halo first, then its step line.
///
/// Which side of the crosshair a line is on is decided by a clip, not by splitting the
/// points: the prepared mesh is built once and both halves draw the same vertices, so
/// the two sides join exactly at the indicator. The faded side is painted first, which
/// is what puts the full-colour side and the dots above it.
fn paint_series(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    meshes: &[SeriesMeshes],
    hovered: Option<HoveredSample>,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    push_clip(canvas, layout.plot);
    for (index, series) in prepared.series.iter().enumerate() {
        let Some(mesh) = meshes.get(index) else {
            continue;
        };
        if series.screen.len() < 2 {
            // A line of one point has no segment; its endpoint dot is all it draws.
            continue;
        }
        let color = context.colors.series(series.color);
        if let Some(halo) = mesh.halo {
            canvas.push_geometry(GeometryDraw::full(
                halo,
                Transform::IDENTITY,
                context.colors.halo,
            ));
        }
        let Some(line) = mesh.line else {
            continue;
        };
        match hovered {
            Some(hovered) => {
                let (left, right) = fade_masks(layout.plot, hovered.x);
                push_clip(canvas, right);
                canvas.push_geometry(GeometryDraw::full(
                    line,
                    Transform::IDENTITY,
                    color.with_opacity(SERIES_FADE_ALPHA),
                ));
                pop_clip(canvas);
                push_clip(canvas, left);
                canvas.push_geometry(GeometryDraw::full(line, Transform::IDENTITY, color));
                pop_clip(canvas);
            }
            None => {
                canvas.push_geometry(GeometryDraw::full(line, Transform::IDENTITY, color));
            }
        }
    }
    pop_clip(canvas);
}

/// The two masks a hovering frame splits every line into.
fn fade_masks(plot: Rect, crosshair_x: f32) -> (Rect, Rect) {
    let x = crosshair_x.clamp(plot.origin.x, plot.right());
    let left = Rect::from_origin_size(
        plot.origin,
        Size::new((x - plot.origin.x).max(0.0), plot.size.height),
    );
    let right = Rect::from_origin_size(
        Point::new(x, plot.origin.y),
        Size::new((plot.right() - x).max(0.0), plot.size.height),
    );
    (left, right)
}

/// The mask hover dots paint in, which is the plot plus their wash.
///
/// A hover dot's centre can sit on either vertical edge of the plot, so both edges get
/// the wash's radius. The right overhang stays inside [`Y_LABEL_GAP`], and the x labels
/// start one pixel past the bottom overhang.
fn dot_mask(plot: Rect) -> Rect {
    Rect::from_origin_size(
        Point::new(
            plot.origin.x - DOT_MASK_LEFT,
            plot.origin.y - DOT_MASK_VERTICAL,
        ),
        Size::new(
            plot.size.width + (DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + DOT_MASK_VERTICAL * 2.0,
        ),
    )
}

/// The mask endpoint dots and the pulse around them paint in.
///
/// The pulse ring grows past the hover dot's wash above and below its centre, so the
/// endpoint gets a mask with the pulse's full vertical reach. The left overhang is the
/// hover wash's, for a one-point chart whose padded window puts its only sample in
/// the middle of the plot rather than on an edge.
fn endpoint_mask(plot: Rect) -> Rect {
    Rect::from_origin_size(
        Point::new(
            plot.origin.x - DOT_MASK_LEFT,
            plot.origin.y - ENDPOINT_MASK_VERTICAL,
        ),
        Size::new(
            plot.size.width + (DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + ENDPOINT_MASK_VERTICAL * 2.0,
        ),
    )
}

/// Draws the static endpoint dot of every series, and the pulse around it.
///
/// The endpoint belongs to the faded side of the chart when the crosshair is left of
/// it, so it is dimmed by the same alpha the line under it is. It is drawn in the
/// endpoint mask rather than the plot's own, because its centre is the plot's right
/// edge and the pulse reaches past it at the top, bottom and right.
fn paint_endpoints(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    hovered: Option<HoveredSample>,
    pulse_phase: Option<f32>,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let pulse = pulse_phase.and_then(pulse_ring);
    let scale = pulse_phase.map_or(1.0, dot_scale);
    push_clip(canvas, endpoint_mask(layout.plot));
    for series in prepared.series.iter() {
        let Some(last) = series.last else {
            continue;
        };
        let color = context.colors.series(series.color);
        if let Some((radius, alpha)) = pulse {
            paint_circle(canvas, last, radius, color.with_opacity(alpha));
        }
        let dimmed = hovered.is_some_and(|hovered| last.x > hovered.x);
        let dot = if dimmed {
            color.with_opacity(SERIES_FADE_ALPHA)
        } else {
            color
        };
        paint_circle(canvas, last, ENDPOINT_RADIUS * scale, dot);
    }
    pop_clip(canvas);
}

/// The expanding ring's radius and alpha for a pulse phase, if the phase is running.
fn pulse_ring(phase: f32) -> Option<(f32, f32)> {
    if !(0.0..1.0).contains(&phase) {
        return None;
    }
    let radius = PULSE_MIN_RADIUS + (PULSE_MAX_RADIUS - PULSE_MIN_RADIUS) * phase;
    let alpha = PULSE_ALPHA * (1.0 - phase);
    Some((radius, alpha))
}

/// How much the endpoint dot itself grows at a pulse phase.
fn dot_scale(phase: f32) -> f32 {
    if !(0.0..1.0).contains(&phase) {
        return 1.0;
    }
    let triangle = 1.0 - (2.0 * phase - 1.0).abs();
    1.0 + PULSE_DOT_SCALE * triangle
}

/// Draws everything the pointer adds: crosshair, dots, label column and time label.
///
/// The crosshair goes under the dots: the indicator is a line and the dots mark where
/// it meets each series, so the dot has to sit on top rather than have the line cut
/// through it. It ends on the 0% line instead of running through the x-label band.
/// The labels come after both, because they name what the dots mark.
fn paint_hover(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    hovered: HoveredSample,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    push_clip(canvas, layout.bounds);
    let x = hovered.x - CROSSHAIR_WIDTH / 2.0;
    let top = layout.plot.origin.y - CROSSHAIR_OVERHANG_TOP;
    let height = layout.plot.size.height + CROSSHAIR_OVERHANG_TOP;
    push_quad(
        canvas,
        Rect::new(x, top, CROSSHAIR_WIDTH, height),
        context.colors.crosshair,
    );
    pop_clip(canvas);
    push_clip(canvas, dot_mask(layout.plot));
    for series in prepared.series.iter() {
        if !series.covers(hovered.timestamp) {
            continue;
        }
        let Some(center) = hover_point(&series.samples, &prepared.axis, layout.plot, hovered)
        else {
            continue;
        };
        let color = context.colors.series(series.color);
        paint_circle(
            canvas,
            center,
            HOVER_DOT_OUTER / 2.0,
            color.with_opacity(HOVER_DOT_OUTER_ALPHA),
        );
        paint_circle(canvas, center, HOVER_DOT_INNER / 2.0, color);
    }
    pop_clip(canvas);
    paint_label_column(canvas, prepared, hovered, context);
    paint_hover_time(canvas, prepared, hovered, context);
}

/// Draws the label boxes of every series that has a value at the hovered sample.
///
/// The column's side is decided from the widest label, so the boxes stay aligned even
/// though they differ in width; their vertical order is resolved bottom to top, so the
/// box of the lowest point keeps its own y and the ones above give way. The column is
/// the one buffer a hover frame builds, because its text and widths depend on the
/// sample under the pointer.
fn paint_label_column(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    hovered: HoveredSample,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let font = label_style(
        LABEL_FONT_SIZE,
        LABEL_LINE_HEIGHT,
        context.colors.foreground,
        STRONG_WEIGHT,
    );
    let mut labels: Vec<(usize, SmolStr, f32, f32)> = Vec::with_capacity(prepared.series.len());
    for series in prepared.series.iter() {
        if !series.covers(hovered.timestamp) {
            continue;
        }
        let Some(percent) = value_at(&series.samples, hovered.timestamp) else {
            continue;
        };
        let text = format_smolstr!("{} {}", series.label, format_percent_one_decimal(percent));
        let width = context.width(text.as_str(), &font) + LABEL_BOX_CHROME;
        let anchor = y_for_series(percent, &prepared.axis, layout.plot);
        labels.push((series.color, text, width, anchor));
    }
    if labels.is_empty() {
        return;
    }
    let widest = labels
        .iter()
        .map(|(_, _, width, _)| *width)
        .fold(0.0, f32::max);
    let side = label_side(hovered.x, widest, layout.plot);
    let anchors: Vec<f32> = labels.iter().map(|(_, _, _, anchor)| *anchor).collect();
    let tops = label_tops(&anchors, LABEL_HEIGHT, LABEL_COLLISION_GAP, layout.plot);
    push_clip(canvas, layout.bounds);
    for ((color, text, width, _), top) in labels.iter().zip(tops.iter()) {
        let bounds = Rect::new(
            label_x(hovered.x, *width, side, layout.plot),
            *top,
            *width,
            LABEL_HEIGHT,
        );
        canvas.push_quad(Quad {
            bounds,
            corner_radius: [LABEL_BOX_RADIUS; 4],
            paint: QuadPaint::Solid {
                fill: context.colors.label_background,
                border: Some(Border {
                    width: LABEL_BOX_BORDER,
                    color: context.colors.label_border,
                }),
            },
        });
        let bar = Rect::new(
            bounds.origin.x + LABEL_PADDING_LEFT,
            bounds.origin.y + (LABEL_HEIGHT - LABEL_BAR_HEIGHT) / 2.0,
            LABEL_BAR_WIDTH,
            LABEL_BAR_HEIGHT,
        );
        canvas.push_quad(Quad {
            bounds: bar,
            corner_radius: [LABEL_BAR_RADIUS; 4],
            paint: QuadPaint::Solid {
                fill: context.colors.series(*color),
                border: None,
            },
        });
        context.text(
            canvas,
            text.as_str(),
            bounds.origin.x + LABEL_TEXT_INSET,
            bounds.origin.y,
            &font,
        );
    }
    pop_clip(canvas);
}

/// Draws the hovered sample's time in the strip above the plot.
///
/// It sits right of the indicator; when that would run past the plot it flips to the
/// left of the indicator instead, and it is clamped into the element either way so a
/// label at the very edge is never clipped away.
fn paint_hover_time(
    canvas: &mut Canvas,
    prepared: &PreparedChart,
    hovered: HoveredSample,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let font = label_style(
        TIME_LABEL_FONT_SIZE,
        TIME_LABEL_LINE_HEIGHT,
        context.colors.muted,
        STRONG_WEIGHT,
    );
    let text = format_hover_time(hovered.timestamp, prepared.time_zone, context.translate);
    let width = context.width(text.as_str(), &font);
    let right_of = hovered.x + LABEL_GAP;
    let x = if right_of + width <= layout.plot.right() {
        right_of
    } else {
        hovered.x - LABEL_GAP - width
    };
    let left = layout.bounds.origin.x;
    let x = x.clamp(left, (layout.bounds.right() - width).max(left));
    let y = layout.top_strip().origin.y + TIME_LABEL_TOP_INSET;
    context.text(canvas, text.as_str(), x, y, &font);
}

/// The mesh of one series line: a ribbon of `width` through the step line's points.
///
/// A step line runs horizontally at each sample's value up to the next sample's x and
/// then jumps vertically, so the ribbon's points are the screen points with the
/// vertical run inserted between every pair. Every segment becomes one quad and every
/// point a square joint, which is what keeps the ribbon continuous around a corner. Two
/// consecutive points that are the same are folded first: a flat stretch would
/// otherwise contribute zero-length segments and a duplicate joint per sample.
///
/// Every vertex is marked as interior (a negative distance, which the engine's coverage
/// shader reads as full coverage), because this mesh is not anti-aliased: the engine's
/// `Ui` pipeline is a coverage-distance pipeline and a filled ribbon is its degenerate
/// case.
pub(crate) fn line_geometry(screen: &[ScreenPoint], width: f32) -> Option<GeometryDesc> {
    let mut points: Vec<Point> = Vec::with_capacity(screen.len() * 2);
    let first = screen.first()?;
    points.push(Point::new(first.x, first.y));
    for pair in screen.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let horizontal = Point::new(to.x, from.y);
        if points.last().is_none_or(|last| *last != horizontal) {
            points.push(horizontal);
        }
        let next = Point::new(to.x, to.y);
        if points.last().is_none_or(|last| *last != next) {
            points.push(next);
        }
    }
    if points.len() < 2 {
        return None;
    }

    let half = (width / 2.0).max(0.5);
    let mut vertices = Vec::<[f32; 3]>::with_capacity(points.len() * 8);
    let mut indices = Vec::<u32>::with_capacity(points.len() * 12);
    // The mesh's AABB is accumulated from the corners themselves rather than from
    // rectangles: a zero-size rect is "empty" to the geometry helpers, and a union of
    // empty rects is the last one of them.
    let mut min = Point::new(f32::INFINITY, f32::INFINITY);
    let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut push_quad = |corners: [Point; 4]| {
        let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
        for corner in corners {
            vertices.push([corner.x, corner.y, -1.0]);
            min = Point::new(min.x.min(corner.x), min.y.min(corner.y));
            max = Point::new(max.x.max(corner.x), max.y.max(corner.y));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    let square = |center: Point| {
        [
            Point::new(center.x - half, center.y - half),
            Point::new(center.x + half, center.y - half),
            Point::new(center.x + half, center.y + half),
            Point::new(center.x - half, center.y + half),
        ]
    };

    for window in points.windows(2) {
        let (from, to) = (window[0], window[1]);
        let delta = Point::new(to.x - from.x, to.y - from.y);
        let length = delta.x.hypot(delta.y);
        if !length.is_finite() || length <= f32::EPSILON {
            continue;
        }
        let normal = Point::new(-delta.y / length * half, delta.x / length * half);
        push_quad([
            Point::new(from.x + normal.x, from.y + normal.y),
            Point::new(to.x + normal.x, to.y + normal.y),
            Point::new(to.x - normal.x, to.y - normal.y),
            Point::new(from.x - normal.x, from.y - normal.y),
        ]);
    }
    for point in &points {
        push_quad(square(*point));
    }
    let bounds = Rect::new(min.x, min.y, max.x - min.x, max.y - min.y).outset(half / 2.0);
    if vertices.is_empty() || indices.is_empty() || !bounds.is_finite() {
        return None;
    }

    let vertex_bytes = vertices
        .iter()
        .flat_map(|vertex| vertex.iter().flat_map(|value| value.to_le_bytes()))
        .collect::<Vec<u8>>();
    let index_bytes = indices
        .iter()
        .flat_map(|index| index.to_le_bytes())
        .collect::<Vec<u8>>();
    Some(GeometryDesc {
        vertex_bytes: vertex_bytes.into(),
        index_bytes: index_bytes.into(),
        vertex_stride: GeometryPipeline::Ui.vertex_stride(),
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds,
        pipeline: GeometryPipeline::Ui,
    })
}

/// Draws a filled circle as a rounded quad.
fn paint_circle(canvas: &mut Canvas, center: ScreenPoint, radius: f32, color: Color) {
    if radius <= 0.0 {
        return;
    }
    canvas.push_quad(Quad {
        bounds: dot_bounds(center, radius * 2.0),
        corner_radius: [radius; 4],
        paint: QuadPaint::Solid {
            fill: color,
            border: None,
        },
    });
}

/// Pushes one solid quad.
fn push_quad(canvas: &mut Canvas, bounds: Rect, fill: Color) {
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid { fill, border: None },
    });
}

/// Pushes one axis-aligned clip.
fn push_clip(canvas: &mut Canvas, rect: Rect) {
    let _ = canvas.push_clip(ClipRect {
        rect,
        corner_radius: [0.0; 4],
    });
}

/// Pops the clip pushed last.
fn pop_clip(canvas: &mut Canvas) {
    let _ = canvas.pop_clip();
}

/// What one painted line of text reported drawing.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TextRecord {
    pub text: SmolStr,
    pub origin: Point,
    pub font_size: f32,
    /// What the shaped line measured, which is the width its box has to hold.
    pub shaped_width: f32,
}

// The paint pass's record of the last frame, for tests.
//
// Glyph runs are checked from the painted frame's primitives (a run's placed glyphs
// are readable), but what the run *says* is not in the scene: a shaped handle is
// opaque once pushed. The production pass therefore reports the lines it drew here,
// boxed behind `#[cfg(test)]`, and the tests assert the labels' text and placement
// against it alongside the quads they read from `WidgetTestApp::canvas_primitives`.
#[cfg(test)]
std::thread_local! {
    static TEXT_LOG: std::cell::RefCell<Vec<TextRecord>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The lines the last paint pass drew, taken so the next one starts empty.
#[cfg(test)]
pub(crate) fn take_text_log() -> Vec<TextRecord> {
    TEXT_LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

/// Forgets everything the previous paint pass reported.
#[cfg(test)]
pub(crate) fn reset_text_log() {
    TEXT_LOG.with(|log| log.borrow_mut().clear());
}

#[cfg(test)]
fn record_text(text: &str, x: f32, y: f32, font_size: f32, shaped_width: f32) {
    TEXT_LOG.with(|log| {
        log.borrow_mut().push(TextRecord {
            text: SmolStr::new(text),
            origin: Point::new(x, y),
            font_size,
            shaped_width,
        });
    });
}
