use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, Transform, px},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology},
        primitive::{Border, ClipRect, GeometryDraw, Quad, QuadPaint},
    },
    text::TextStyle,
};
use ui_kit::theme::{resolved::Theme, role::Role};

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

pub(crate) const SERIES_STROKE_WIDTH: LogicalPx = LogicalPx::new(1.75);
pub(crate) const SERIES_HALO_WIDTH: LogicalPx = LogicalPx::new(2.0);
const SERIES_FADE_ALPHA: f32 = 0.25;

const GRID_DOT: LogicalPx = LogicalPx::new(1.0);
const GRID_PERIOD: LogicalPx = LogicalPx::new(4.0);

const CROSSHAIR_WIDTH: LogicalPx = LogicalPx::new(1.5);
const CROSSHAIR_OVERHANG_TOP: LogicalPx = LogicalPx::new(17.0);

const Y_LABEL_GAP: LogicalPx = LogicalPx::new(11.0);
const X_LABEL_GAP: LogicalPx = LogicalPx::new(7.0);
const AXIS_LINE_HEIGHT: LogicalPx = LogicalPx::new(14.0);

const LABEL_PADDING_LEFT: LogicalPx = LogicalPx::new(4.0);
const LABEL_BAR_WIDTH: LogicalPx = LogicalPx::new(4.0);
const LABEL_BAR_HEIGHT: LogicalPx = LogicalPx::new(12.0);
const LABEL_BAR_GAP: LogicalPx = LogicalPx::new(4.0);
const LABEL_PADDING_RIGHT: LogicalPx = LogicalPx::new(6.0);
const LABEL_BAR_RADIUS: LogicalPx = LogicalPx::new(2.0);
const LABEL_TEXT_INSET: LogicalPx =
    LogicalPx::new(LABEL_PADDING_LEFT.value() + LABEL_BAR_WIDTH.value() + LABEL_BAR_GAP.value());
const LABEL_BOX_CHROME: LogicalPx = LogicalPx::new(
    LABEL_PADDING_LEFT.value()
        + LABEL_BAR_WIDTH.value()
        + LABEL_BAR_GAP.value()
        + LABEL_PADDING_RIGHT.value(),
);
const LABEL_BOX_RADIUS: LogicalPx = LogicalPx::new(4.0);
const LABEL_BOX_BORDER: LogicalPx = LogicalPx::new(1.0);
const LABEL_LINE_HEIGHT: LogicalPx = LABEL_HEIGHT;

const HOVER_DOT_INNER: LogicalPx = LogicalPx::new(8.0);
const HOVER_DOT_OUTER: LogicalPx = LogicalPx::new(12.0);
const HOVER_DOT_OUTER_ALPHA: f32 = 0.1;

const ENDPOINT_RADIUS: LogicalPx = LogicalPx::new(HOVER_DOT_INNER.value() / 2.0);
const PULSE_MIN_RADIUS: LogicalPx = LogicalPx::new(ENDPOINT_RADIUS.value() + 2.0);
const PULSE_MAX_RADIUS: LogicalPx = LogicalPx::new(10.0);
const PULSE_ALPHA: f32 = 0.4;
const PULSE_DOT_SCALE: f32 = 0.08;

const DOT_MASK_LEFT: LogicalPx = LogicalPx::new(HOVER_DOT_OUTER.value() / 2.0);
const DOT_MASK_RIGHT: LogicalPx = PULSE_MAX_RADIUS;
const DOT_MASK_VERTICAL: LogicalPx = LogicalPx::new(HOVER_DOT_OUTER.value() / 2.0);
const ENDPOINT_MASK_VERTICAL: LogicalPx = PULSE_MAX_RADIUS;

const TIME_LABEL_TOP_INSET: LogicalPx = LogicalPx::new(1.0);
const TIME_LABEL_LINE_HEIGHT: LogicalPx = LogicalPx::new(16.0);

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
            halo: theme.role(Role::SurfaceSunken),
            foreground: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            label_background: theme.role(Role::SurfaceOverlay),
            label_border: theme.role(Role::LineBorder),
        }
    }

    fn series(self, index: usize) -> Color {
        self.palette[index % self.palette.len()]
    }
}

pub(crate) fn paint(
    app: &mut App,
    text: &mut ChartTextCache,
    translate: &Translate,
    prepared: &PreparedChart,
    meshes: &[SeriesMeshes],
    colors: &ChartColors,
    hovered: Option<HoveredSample>,
    pulse_phase: Option<f32>,
) -> Canvas {
    let layout = prepared.layout;
    if prepared.domain.is_none()
        || layout.plot.size.width <= px(0.0)
        || layout.plot.size.height <= px(0.0)
    {
        return Canvas::new(layout.bounds);
    }
    let mut context = PaintContext {
        app,
        translate,
        text,
        colors,
    };
    let mut canvas = Canvas::new(layout.bounds);
    paint_grid(&mut canvas, prepared, colors);
    paint_axis_labels(&mut canvas, prepared, &mut context);
    paint_series(&mut canvas, prepared, meshes, hovered, &mut context);
    if let Some(hovered) = hovered {
        paint_hover(&mut canvas, prepared, hovered, &mut context);
    }
    paint_endpoints(&mut canvas, prepared, hovered, pulse_phase, &mut context);
    canvas
}

struct PaintContext<'a> {
    app: &'a mut App,
    translate: &'a Translate,
    text: &'a mut ChartTextCache,
    colors: &'a ChartColors,
}

impl PaintContext<'_> {
    fn text(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: LogicalPx,
        y: LogicalPx,
        style: &TextStyle,
    ) {
        if text.is_empty() {
            return;
        }
        let (content, line_width) = self.text.line(self.app, text, style);
        #[cfg(test)]
        record_text(text, x, y, style.size, line_width);
        #[cfg(not(test))]
        let _ = line_width;
        let _ = canvas.push_text(content, LogicalPoint::new(x, y));
    }

    fn width(&mut self, text: &str, style: &TextStyle) -> LogicalPx {
        self.text.width(self.app, text, style)
    }
}

fn paint_grid(canvas: &mut Canvas, prepared: &PreparedChart, colors: &ChartColors) {
    let layout = prepared.layout;
    let plot_right = layout.plot.right();
    push_clip(canvas, layout.plot);
    for tick in &prepared.axis.ticks {
        let y = y_for_percent(*tick, &prepared.axis, layout.plot);
        let mut x = layout.plot.origin.x;
        while x < plot_right {
            let width = GRID_DOT.min(plot_right - x);
            push_quad(canvas, LogicalRect::new(x, y, width, GRID_DOT), colors.grid);
            x += GRID_PERIOD;
        }
    }
    pop_clip(canvas);
    let bounds_bottom = layout.bounds.bottom();
    push_clip(canvas, layout.bounds);
    for tick in prepared.time_ticks.iter() {
        let mut y = layout.plot.origin.y;
        while y < bounds_bottom {
            let height = GRID_DOT.min(bounds_bottom - y);
            push_quad(
                canvas,
                LogicalRect::new(tick.x, y, GRID_DOT, height),
                colors.grid,
            );
            y += GRID_PERIOD;
        }
    }
    pop_clip(canvas);
}

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
            continue;
        }
        let width = context.width(tick.label.as_str(), &style);
        let x = (tick.x - width / 2.0).clamp(left, (right - width).max(left));
        context.text(canvas, tick.label.as_str(), x, y_origin, &style);
    }
}

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

fn fade_masks(plot: LogicalRect, crosshair_x: LogicalPx) -> (LogicalRect, LogicalRect) {
    let left_edge = plot.origin.x;
    let right_edge = plot.right();
    let x = crosshair_x.clamp(left_edge, right_edge);
    let left = LogicalRect::from_origin_size(
        plot.origin,
        LogicalSize::new((x - left_edge).max(px(0.0)), plot.size.height),
    );
    let right = LogicalRect::from_origin_size(
        LogicalPoint::new(x, plot.origin.y),
        LogicalSize::new((right_edge - x).max(px(0.0)), plot.size.height),
    );
    (left, right)
}

fn dot_mask(plot: LogicalRect) -> LogicalRect {
    LogicalRect::from_origin_size(
        LogicalPoint::new(
            plot.origin.x - DOT_MASK_LEFT,
            plot.origin.y - DOT_MASK_VERTICAL,
        ),
        LogicalSize::new(
            plot.size.width + (DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + DOT_MASK_VERTICAL * 2.0,
        ),
    )
}

fn endpoint_mask(plot: LogicalRect) -> LogicalRect {
    LogicalRect::from_origin_size(
        LogicalPoint::new(
            plot.origin.x - DOT_MASK_LEFT,
            plot.origin.y - ENDPOINT_MASK_VERTICAL,
        ),
        LogicalSize::new(
            plot.size.width + (DOT_MASK_LEFT + DOT_MASK_RIGHT),
            plot.size.height + ENDPOINT_MASK_VERTICAL * 2.0,
        ),
    )
}

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

fn pulse_ring(phase: f32) -> Option<(LogicalPx, f32)> {
    if !(0.0..1.0).contains(&phase) {
        return None;
    }
    let radius = PULSE_MIN_RADIUS + (PULSE_MAX_RADIUS - PULSE_MIN_RADIUS) * phase;
    let alpha = PULSE_ALPHA * (1.0 - phase);
    Some((radius, alpha))
}

fn dot_scale(phase: f32) -> f32 {
    if !(0.0..1.0).contains(&phase) {
        return 1.0;
    }
    let triangle = 1.0 - (2.0 * phase - 1.0).abs();
    1.0 + PULSE_DOT_SCALE * triangle
}

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
        LogicalRect::new(x, top, CROSSHAIR_WIDTH, height),
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
    let mut labels: Vec<(usize, SmolStr, LogicalPx, LogicalPx)> =
        Vec::with_capacity(prepared.series.len());
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
        .fold(LogicalPx::ZERO, LogicalPx::max);
    let side = label_side(hovered.x, widest, layout.plot);
    let anchors: Vec<LogicalPx> = labels.iter().map(|(_, _, _, anchor)| *anchor).collect();
    let tops = label_tops(&anchors, LABEL_HEIGHT, LABEL_COLLISION_GAP, layout.plot);
    push_clip(canvas, layout.bounds);
    for ((color, text, width, _), top) in labels.iter().zip(tops.iter()) {
        let bounds = LogicalRect::new(
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
        let bar = LogicalRect::new(
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

pub(crate) fn line_geometry(screen: &[ScreenPoint], width: LogicalPx) -> Option<GeometryDesc> {
    let mut points: Vec<ScreenPoint> = Vec::with_capacity(screen.len() * 2);
    let first = screen.first()?;
    points.push(*first);
    for pair in screen.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let horizontal = ScreenPoint { x: to.x, y: from.y };
        if points.last().is_none_or(|last| *last != horizontal) {
            points.push(horizontal);
        }
        let next = ScreenPoint { x: to.x, y: to.y };
        if points.last().is_none_or(|last| *last != next) {
            points.push(next);
        }
    }
    if points.len() < 2 {
        return None;
    }

    let half = (width / 2.0).max(px(0.5));
    let mut vertices = Vec::<[f32; 3]>::with_capacity(points.len() * 8);
    let mut indices = Vec::<u32>::with_capacity(points.len() * 12);
    let mut min = ScreenPoint {
        x: px(f32::INFINITY),
        y: px(f32::INFINITY),
    };
    let mut max = ScreenPoint {
        x: px(f32::NEG_INFINITY),
        y: px(f32::NEG_INFINITY),
    };
    let mut push_quad = |corners: [ScreenPoint; 4]| {
        let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
        for corner in corners {
            vertices.push([corner.x.value(), corner.y.value(), -1.0]);
            min = ScreenPoint {
                x: min.x.min(corner.x),
                y: min.y.min(corner.y),
            };
            max = ScreenPoint {
                x: max.x.max(corner.x),
                y: max.y.max(corner.y),
            };
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    let square = |center: ScreenPoint| {
        [
            ScreenPoint {
                x: center.x - half,
                y: center.y - half,
            },
            ScreenPoint {
                x: center.x + half,
                y: center.y - half,
            },
            ScreenPoint {
                x: center.x + half,
                y: center.y + half,
            },
            ScreenPoint {
                x: center.x - half,
                y: center.y + half,
            },
        ]
    };

    for window in points.windows(2) {
        let (from, to) = (window[0], window[1]);
        let delta = ScreenPoint {
            x: to.x - from.x,
            y: to.y - from.y,
        };
        let length = delta.x.value().hypot(delta.y.value());
        if !length.is_finite() || length <= f32::EPSILON {
            continue;
        }
        let normal = ScreenPoint {
            x: -delta.y / length * half.value(),
            y: delta.x / length * half.value(),
        };
        push_quad([
            ScreenPoint {
                x: from.x + normal.x,
                y: from.y + normal.y,
            },
            ScreenPoint {
                x: to.x + normal.x,
                y: to.y + normal.y,
            },
            ScreenPoint {
                x: to.x - normal.x,
                y: to.y - normal.y,
            },
            ScreenPoint {
                x: from.x - normal.x,
                y: from.y - normal.y,
            },
        ]);
    }
    for point in &points {
        push_quad(square(*point));
    }
    let bounds = LogicalRect::new(min.x, min.y, max.x - min.x, max.y - min.y).outset(half / 2.0);
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

fn paint_circle(canvas: &mut Canvas, center: ScreenPoint, radius: LogicalPx, color: Color) {
    if radius <= px(0.0) {
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

fn push_quad(canvas: &mut Canvas, bounds: LogicalRect, fill: Color) {
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [px(0.0); 4],
        paint: QuadPaint::Solid { fill, border: None },
    });
}

fn push_clip(canvas: &mut Canvas, rect: LogicalRect) {
    let _ = canvas.push_clip(ClipRect {
        rect,
        corner_radius: [px(0.0); 4],
    });
}

fn pop_clip(canvas: &mut Canvas) {
    let _ = canvas.pop_clip();
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TextRecord {
    pub text: SmolStr,
    pub origin: LogicalPoint,
    pub font_size: LogicalPx,
    pub shaped_width: LogicalPx,
}

#[cfg(test)]
std::thread_local! {
    static TEXT_LOG: std::cell::RefCell<Vec<TextRecord>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn take_text_log() -> Vec<TextRecord> {
    TEXT_LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn reset_text_log() {
    TEXT_LOG.with(|log| log.borrow_mut().clear());
}

#[cfg(test)]
fn record_text(
    text: &str,
    x: LogicalPx,
    y: LogicalPx,
    font_size: LogicalPx,
    shaped_width: LogicalPx,
) {
    TEXT_LOG.with(|log| {
        log.borrow_mut().push(TextRecord {
            text: SmolStr::new(text),
            origin: LogicalPoint::new(x, y),
            font_size,
            shaped_width,
        });
    });
}
