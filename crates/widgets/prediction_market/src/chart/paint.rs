//! Painting one prepared prediction chart.
//!
//! One frame turns the prepared boxes, paths and ticks into quads and strokes. This
//! module holds no state: the y labels travel with the prepared chart, and the series
//! palette is a six-colour stack array read from the theme, so neither the y-label
//! strings nor the palette is rebuilt per frame. The only text a frame formats is the
//! hover's own — a label per series and the time under the pointer — because those are
//! the only strings that depend on where the pointer is, and the only buffer a frame
//! builds is the small label column those strings sit in.
//!
//! Everything that follows the pointer is drawn here and only here: the crosshair,
//! the right-side fade of every line, the hover dots, the label column and the time
//! label. The prepared chart underneath them is shared with every pointer position,
//! which is what keeps a mouse crossing the plot from rebuilding geometry.

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    App, Bounds, ContentMask, Hsla, Pixels, Point, SharedString, TextAlign, Window, fill, point,
    px, size,
};
use i18n::translator::types::I18n;
use smol_str::{SmolStr, format_smolstr};

use super::{
    canvas::{ChartCanvasOptions, ChartPaintCaches, PreparedChart},
    geometry::{ScreenPoint, y_for_percent, y_for_series},
    hover::{
        HoveredSample, LABEL_COLLISION_GAP, LABEL_GAP, LABEL_HEIGHT, dot_bounds, hover_point,
        label_side, label_tops, label_x, value_at,
    },
    text::{
        AXIS_FONT_SIZE, ChartFont, ChartTextCache, LABEL_FONT_SIZE, STRONG_WEIGHT,
        TIME_LABEL_FONT_SIZE, format_hover_time, format_percent_one_decimal,
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

/// The two things a paint pass needs that only a frame can hand over.
///
/// Text is shaped through the cache rather than through the window, and the window
/// cannot be borrowed while a shaped line is being told about its own accessibility,
/// so both travel together.
struct PaintContext<'a> {
    text_cache: &'a mut ChartTextCache,
    window: &'a mut Window,
    app: &'a mut App,
}

impl PaintContext<'_> {
    /// Shapes and draws one line of text, through the cache.
    fn text(
        &mut self,
        text: &str,
        origin: Point<Pixels>,
        font: ChartFont,
        align: TextAlign,
        width: Pixels,
        line_height: Pixels,
    ) {
        paint_text(
            self.text_cache,
            self.window,
            self.app,
            text,
            origin,
            font,
            align,
            width,
            line_height,
        );
    }

    /// The width `text` takes in `font`, shaped through the cache.
    fn width(&mut self, text: &str, font: ChartFont) -> Pixels {
        let window = &*self.window;
        self.text_cache.width(text, font, window)
    }

    /// Runs `draw` clipped to `mask`.
    fn with_mask<R>(
        &mut self,
        mask: Bounds<Pixels>,
        draw: impl FnOnce(&mut ChartTextCache, &mut Window, &mut App) -> R,
    ) -> R {
        let Self {
            text_cache,
            window,
            app,
        } = self;
        window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
            draw(text_cache, window, app)
        })
    }
}

/// Draws one prepared chart.
///
/// The layers go down in the design's order: grid, axis labels, series, then
/// everything the pointer adds, and the endpoint pulse last. The pulse is above the
/// crosshair and the labels.
pub(crate) fn paint(
    prepared: &PreparedChart,
    options: &ChartCanvasOptions,
    caches: &ChartPaintCaches,
    window: &mut Window,
    app: &mut App,
) {
    let layout = prepared.layout;
    if prepared.domain.is_none()
        || layout.plot.size.width <= Pixels::ZERO
        || layout.plot.size.height <= Pixels::ZERO
    {
        return;
    }
    let hovered = options
        .pointer
        .and_then(|pointer| prepared.hover_at(pointer));
    let mut text_cache = caches.text_cache().borrow_mut();
    let context = &mut PaintContext {
        text_cache: &mut text_cache,
        window,
        app,
    };
    paint_grid(prepared, context);
    paint_axis_labels(prepared, context);
    paint_series(prepared, hovered.as_ref(), context);
    if let Some(hovered) = hovered.as_ref() {
        paint_hover(prepared, hovered, context);
    }
    paint_endpoints(prepared, hovered.as_ref(), options.pulse_phase, context);
}

/// Draws the dotted grid.
///
/// A horizontal row of dots spans the plot; a vertical one runs from the plot's top
/// through the band under it, which is what the reference chart measures, and stops
/// before the strip above the plot that holds the hovered time. The rows are disjoint
/// and so are the columns, so each direction is one layer the scene builder can batch
/// instead of one insertion per dot.
fn paint_grid(prepared: &PreparedChart, context: &mut PaintContext<'_>) {
    let layout = prepared.layout;
    let color = context.app.theme().roles.chart.grid.hsla;
    context.window.paint_layer(layout.plot, |window| {
        for tick in &prepared.axis.ticks {
            let y = y_for_percent(*tick, &prepared.axis, layout.plot);
            let mut x = layout.plot.origin.x;
            while x < layout.plot.right() {
                let width = px(GRID_DOT).min(layout.plot.right() - x);
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(width, px(GRID_DOT))),
                    color,
                ));
                x += px(GRID_PERIOD);
            }
        }
    });
    context.window.paint_layer(layout.bounds, |window| {
        for tick in prepared.time_ticks.iter() {
            let mut y = layout.plot.origin.y;
            while y < layout.bounds.bottom() {
                let height = px(GRID_DOT).min(layout.bounds.bottom() - y);
                window.paint_quad(fill(
                    Bounds::new(point(tick.x, y), size(px(GRID_DOT), height)),
                    color,
                ));
                y += px(GRID_PERIOD);
            }
        }
    });
}

/// Draws the y values right of the plot and the x times under it.
///
/// The y labels were formatted when the chart was prepared; the x labels come with
/// their ticks.
fn paint_axis_labels(prepared: &PreparedChart, context: &mut PaintContext<'_>) {
    let layout = prepared.layout;
    let color = context.app.theme().roles.chart.axis.hsla;
    let font = ChartFont::new(AXIS_FONT_SIZE, gpui_kit::FontWeight::NORMAL, color);
    let x_origin = layout.plot.right() + px(Y_LABEL_GAP);
    let y_width = (layout.bounds.right() - x_origin).max(Pixels::ZERO);
    for (tick, label) in prepared.axis.ticks.iter().zip(prepared.axis_labels.iter()) {
        let y = y_for_percent(*tick, &prepared.axis, layout.plot) - px(AXIS_LINE_HEIGHT / 2.0);
        context.text(
            label.as_str(),
            point(x_origin, y),
            font,
            TextAlign::Left,
            y_width,
            px(AXIS_LINE_HEIGHT),
        );
    }

    let y_origin = layout.bottom_band().origin.y + px(X_LABEL_GAP);
    let left = layout.bounds.origin.x;
    let right = layout.bounds.right();
    for tick in prepared.time_ticks.iter() {
        if tick.label.is_empty() {
            // A repeated clock label keeps its gridline but draws no text.
            continue;
        }
        let width = context.width(tick.label.as_str(), font);
        let x = (tick.x - width / 2.0).clamp(left, (right - width).max(left));
        context.text(
            tick.label.as_str(),
            point(x, y_origin),
            font,
            TextAlign::Left,
            width,
            px(AXIS_LINE_HEIGHT),
        );
    }
}

/// Draws every series: its halo first, then its step path.
///
/// Which side of the crosshair a stroke is on is decided by a content mask, not by
/// splitting the points: the prepared path is built once and both halves paint the
/// same vertices, so the two sides join exactly at the indicator. The faded side is
/// painted first, which is what puts the full-colour side and the dots above it.
fn paint_series(
    prepared: &PreparedChart,
    hovered: Option<&HoveredSample>,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let palette = series_palette(context);
    let halo_color = context.app.theme().roles.surface.sunken.hsla;
    let plot_mask = ContentMask {
        bounds: layout.plot,
    };
    for series in prepared.series.iter() {
        if series.screen.len() < 2 {
            // A line of one point has no segment; its endpoint dot is all it draws.
            continue;
        }
        let color = palette[series.color % palette.len()];
        context.window.with_content_mask(Some(plot_mask), |window| {
            if let Some(halo) = series.halo.as_ref() {
                // The halo is the background's own colour: it only separates
                // crossing lines, so fading it would not change what a reader sees.
                window.paint_path(halo.clone(), halo_color);
                #[cfg(test)]
                record_stroke(halo_color, SERIES_HALO_WIDTH, layout.plot);
            }
            let Some(path) = series.path.as_ref() else {
                return;
            };
            match hovered {
                Some(hovered) => {
                    let (left, right) = fade_masks(layout.plot, hovered.x);
                    let faded = Hsla {
                        a: SERIES_FADE_ALPHA,
                        ..color
                    };
                    window.with_content_mask(Some(ContentMask { bounds: right }), |window| {
                        window.paint_path(path.clone(), faded);
                        #[cfg(test)]
                        record_stroke(faded, SERIES_STROKE_WIDTH, right);
                    });
                    window.with_content_mask(Some(ContentMask { bounds: left }), |window| {
                        window.paint_path(path.clone(), color);
                        #[cfg(test)]
                        record_stroke(color, SERIES_STROKE_WIDTH, left);
                    });
                }
                None => {
                    window.paint_path(path.clone(), color);
                    #[cfg(test)]
                    record_stroke(color, SERIES_STROKE_WIDTH, layout.plot);
                }
            }
        });
    }
}

/// The theme colour of every series, in palette order.
///
/// The palette is the theme's fixed six, so a frame reads it into a stack array and
/// indexes it with the series' own colour; a normal frame allocates nothing for it.
fn series_palette(context: &PaintContext<'_>) -> [Hsla; 6] {
    context.app.theme().roles.chart.series.map(|role| role.hsla)
}

/// The two masks a hovering frame splits every line into.
fn fade_masks(plot: Bounds<Pixels>, crosshair_x: Pixels) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let x = crosshair_x.clamp(plot.origin.x, plot.right());
    let left = Bounds::new(
        plot.origin,
        size((x - plot.origin.x).max(Pixels::ZERO), plot.size.height),
    );
    let right = Bounds::new(
        point(x, plot.origin.y),
        size((plot.right() - x).max(Pixels::ZERO), plot.size.height),
    );
    (left, right)
}

/// The mask hover dots paint in, which is the plot plus their wash.
///
/// A hover dot's centre can sit on either vertical edge of the plot, so both edges get
/// the wash's radius. The right overhang stays inside [`Y_LABEL_GAP`], and the x labels
/// start one pixel past the bottom overhang.
fn dot_mask(plot: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(
            plot.origin.x - px(DOT_MASK_LEFT),
            plot.origin.y - px(DOT_MASK_VERTICAL),
        ),
        size(
            plot.size.width + px(DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + px(DOT_MASK_VERTICAL * 2.0),
        ),
    )
}

/// The mask endpoint dots and the pulse around them paint in.
///
/// The pulse ring grows past the hover dot's wash above and below its centre, so the
/// endpoint gets a mask with the pulse's full vertical reach. The left overhang is the
/// hover wash's, for a one-point chart whose padded window puts its only sample in
/// the middle of the plot rather than on an edge.
fn endpoint_mask(plot: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(
            plot.origin.x - px(DOT_MASK_LEFT),
            plot.origin.y - px(ENDPOINT_MASK_VERTICAL),
        ),
        size(
            plot.size.width + px(DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + px(ENDPOINT_MASK_VERTICAL * 2.0),
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
    prepared: &PreparedChart,
    hovered: Option<&HoveredSample>,
    pulse_phase: Option<f32>,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let palette = series_palette(context);
    let pulse = pulse_phase.and_then(pulse_ring);
    let scale = pulse_phase.map_or(1.0, dot_scale);
    context.window.with_content_mask(
        Some(ContentMask {
            bounds: endpoint_mask(layout.plot),
        }),
        |window| {
            for series in prepared.series.iter() {
                let Some(last) = series.last else {
                    continue;
                };
                let color = palette[series.color % palette.len()];
                if let Some((radius, alpha)) = pulse {
                    paint_circle(window, last, px(radius), Hsla { a: alpha, ..color });
                }
                let dimmed = hovered.is_some_and(|hovered| last.x > hovered.x);
                let dot = if dimmed {
                    Hsla {
                        a: SERIES_FADE_ALPHA,
                        ..color
                    }
                } else {
                    color
                };
                paint_circle(window, last, px(ENDPOINT_RADIUS * scale), dot);
            }
        },
    );
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
fn paint_hover(prepared: &PreparedChart, hovered: &HoveredSample, context: &mut PaintContext<'_>) {
    let layout = prepared.layout;
    let palette = series_palette(context);
    let crosshair_color = context.app.theme().roles.chart.crosshair.hsla;
    context.with_mask(layout.bounds, |_, window, _| {
        let x = hovered.x - px(CROSSHAIR_WIDTH / 2.0);
        let top = layout.plot.origin.y - px(CROSSHAIR_OVERHANG_TOP);
        let height = layout.plot.size.height + px(CROSSHAIR_OVERHANG_TOP);
        window.paint_quad(fill(
            Bounds::new(point(x, top), size(px(CROSSHAIR_WIDTH), height)),
            crosshair_color,
        ));
    });
    context.window.with_content_mask(
        Some(ContentMask {
            bounds: dot_mask(layout.plot),
        }),
        |window| {
            for series in prepared.series.iter() {
                if !series.covers(hovered.timestamp) {
                    continue;
                }
                let Some(center) =
                    hover_point(&series.samples, &prepared.axis, layout.plot, *hovered)
                else {
                    continue;
                };
                let color = palette[series.color % palette.len()];
                paint_circle(
                    window,
                    center,
                    px(HOVER_DOT_OUTER / 2.0),
                    Hsla {
                        a: HOVER_DOT_OUTER_ALPHA,
                        ..color
                    },
                );
                paint_circle(window, center, px(HOVER_DOT_INNER / 2.0), color);
            }
        },
    );
    paint_label_column(prepared, hovered, &palette, context);
    paint_hover_time(prepared, hovered, context);
}

/// Draws the label boxes of every series that has a value at the hovered sample.
///
/// The column's side is decided from the widest label, so the boxes stay aligned even
/// though they differ in width; their vertical order is resolved bottom to top, so the
/// box of the lowest point keeps its own y and the ones above give way. The column is
/// the one buffer a hover frame builds, because its text and widths depend on the
/// sample under the pointer.
fn paint_label_column(
    prepared: &PreparedChart,
    hovered: &HoveredSample,
    palette: &[Hsla; 6],
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let text_color = context.app.theme().roles.fg.default.hsla;
    let font = ChartFont::new(LABEL_FONT_SIZE, STRONG_WEIGHT, text_color);
    let mut labels: Vec<(usize, SmolStr, Pixels, Pixels)> =
        Vec::with_capacity(prepared.series.len());
    for series in prepared.series.iter() {
        if !series.covers(hovered.timestamp) {
            continue;
        }
        let Some(percent) = value_at(&series.samples, hovered.timestamp) else {
            continue;
        };
        let text = format_smolstr!("{} {}", series.label, format_percent_one_decimal(percent));
        let width = context.width(text.as_str(), font) + px(LABEL_BOX_CHROME);
        let anchor = y_for_series(percent, &prepared.axis, layout.plot);
        labels.push((series.color, text, width, anchor));
    }
    if labels.is_empty() {
        return;
    }
    let widest = labels
        .iter()
        .map(|(_, _, width, _)| *width)
        .fold(Pixels::ZERO, Pixels::max);
    let side = label_side(hovered.x, widest, layout.plot);
    let anchors: Vec<Pixels> = labels.iter().map(|(_, _, _, anchor)| *anchor).collect();
    let tops = label_tops(
        &anchors,
        px(LABEL_HEIGHT),
        px(LABEL_COLLISION_GAP),
        layout.plot,
    );
    let background = context.app.theme().roles.surface.overlay.hsla;
    let border = context.app.theme().roles.line.border.hsla;
    context.with_mask(layout.bounds, |text_cache, window, app| {
        for (label, top) in labels.iter().zip(tops.iter()) {
            let (color, text, width, _) = label;
            let bounds = Bounds::new(
                point(label_x(hovered.x, *width, side, layout.plot), *top),
                size(*width, px(LABEL_HEIGHT)),
            );
            window.paint_quad(
                fill(bounds, background)
                    .border_widths(px(LABEL_BOX_BORDER))
                    .border_color(border)
                    .corner_radii(px(LABEL_BOX_RADIUS)),
            );
            let bar = Bounds::new(
                point(
                    bounds.origin.x + px(LABEL_PADDING_LEFT),
                    bounds.origin.y + px((LABEL_HEIGHT - LABEL_BAR_HEIGHT) / 2.0),
                ),
                size(px(LABEL_BAR_WIDTH), px(LABEL_BAR_HEIGHT)),
            );
            window.paint_quad(
                fill(bar, palette[*color % palette.len()]).corner_radii(px(LABEL_BAR_RADIUS)),
            );
            paint_text(
                text_cache,
                window,
                app,
                text.as_str(),
                point(bounds.origin.x + px(LABEL_TEXT_INSET), bounds.origin.y),
                font,
                TextAlign::Left,
                (*width - px(LABEL_TEXT_INSET)).max(Pixels::ZERO),
                px(LABEL_LINE_HEIGHT),
            );
        }
    });
}

/// Draws the hovered sample's time in the strip above the plot.
///
/// It sits right of the indicator; when that would run past the plot it flips to the
/// left of the indicator instead, and it is clamped into the element either way so a
/// label at the very edge is never clipped away.
fn paint_hover_time(
    prepared: &PreparedChart,
    hovered: &HoveredSample,
    context: &mut PaintContext<'_>,
) {
    let layout = prepared.layout;
    let color = context.app.theme().roles.fg.muted.hsla;
    let font = ChartFont::new(TIME_LABEL_FONT_SIZE, STRONG_WEIGHT, color);
    let text = {
        let i18n = context.app.global::<I18n>();
        format_hover_time(
            hovered.timestamp,
            prepared.time_zone,
            i18n,
            prepared.language,
        )
    };
    let width = context.width(text.as_str(), font);
    let right_of = hovered.x + px(LABEL_GAP);
    let x = if right_of + width <= layout.plot.right() {
        right_of
    } else {
        hovered.x - px(LABEL_GAP) - width
    };
    let left = layout.bounds.origin.x;
    let x = x.clamp(left, (layout.bounds.right() - width).max(left));
    let y = layout.top_strip().origin.y + px(TIME_LABEL_TOP_INSET);
    context.text(
        text.as_str(),
        point(x, y),
        font,
        TextAlign::Left,
        width,
        px(TIME_LABEL_LINE_HEIGHT),
    );
}

/// Draws a filled circle as a rounded quad.
fn paint_circle(window: &mut Window, center: ScreenPoint, radius: Pixels, color: Hsla) {
    if radius <= Pixels::ZERO {
        return;
    }
    window.paint_quad(fill(dot_bounds(center, radius * 2.0), color).corner_radii(radius));
}

/// Shapes, records and draws one line of text.
fn paint_text(
    text_cache: &mut ChartTextCache,
    window: &mut Window,
    app: &mut App,
    text: &str,
    origin: Point<Pixels>,
    font: ChartFont,
    align: TextAlign,
    width: Pixels,
    line_height: Pixels,
) {
    if width <= Pixels::ZERO {
        return;
    }
    let line = text_cache.line(SharedString::new(text), font, &*window);
    let _ = line.paint(origin, line_height, align, Some(width), window, app);
    #[cfg(test)]
    record_text(text, origin, font, width, line.width());
}

/// What one painted stroke reported drawing.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct StrokeRecord {
    pub color: Hsla,
    pub width: f32,
    pub mask: Bounds<Pixels>,
}

/// What one painted line of text reported drawing.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TextRecord {
    pub text: SharedString,
    pub origin: Point<Pixels>,
    pub font_size: Pixels,
    /// The room the line was given, not what it used.
    pub width: Pixels,
    /// What the shaped line measured, which is what the room has to hold.
    pub shaped_width: Pixels,
}

/// The paint pass's record of the last frame, for tests.
///
/// Glyphs and stroked paths are not readable from a painted frame — they live in a
/// private scene — so the production paint pass reports what it drew here, boxed
/// behind `#[cfg(test)]`. Quads are asserted through the window's own
/// `painted_quads`; this log covers the two things that leaves out.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PaintLog {
    pub strokes: Vec<StrokeRecord>,
    pub texts: Vec<TextRecord>,
}

#[cfg(test)]
std::thread_local! {
    static PAINT_LOG: std::cell::RefCell<PaintLog> = std::cell::RefCell::new(PaintLog::default());
}

/// The log of the last frame, taken so the next one starts empty.
#[cfg(test)]
pub(crate) fn take_log() -> PaintLog {
    PAINT_LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

/// Forgets everything the previous frame reported.
#[cfg(test)]
pub(crate) fn reset_log() {
    PAINT_LOG.with(|log| *log.borrow_mut() = PaintLog::default());
}

#[cfg(test)]
fn record_stroke(color: Hsla, width: f32, mask: Bounds<Pixels>) {
    PAINT_LOG.with(|log| {
        log.borrow_mut()
            .strokes
            .push(StrokeRecord { color, width, mask });
    });
}

#[cfg(test)]
fn record_text(
    text: &str,
    origin: Point<Pixels>,
    font: ChartFont,
    width: Pixels,
    shaped_width: Pixels,
) {
    PAINT_LOG.with(|log| {
        log.borrow_mut().texts.push(TextRecord {
            text: SharedString::new(text),
            origin,
            font_size: font.size,
            width,
            shaped_width,
        });
    });
}
