use base_ui::theme::{resolved::Theme, role::Role};
use bytes::Bytes;
use ui_core::{
    app::App,
    math::{Point, Rect, Transform},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, GeometryDraw, Quad, QuadPaint},
    },
    text::TextStyle,
};

use super::{
    axes::{
        AXIS_TICK_TEXT_INSET, AxisScale, ChartLayout, PositionedTimeTick, axis_label_y,
        flow_bar_bounds, marker_bounds, point_x, position_time_ticks, tooltip_bounds, value_y,
    },
    data::{PreparedData, PreparedPoint},
    palette::{
        AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, CROSSHAIR_DASH, CROSSHAIR_PERIOD, TOOLTIP_GAP,
        TooltipStyle, flow, tooltip_style,
    },
    text::{ChartTextCache, label_style},
    units::RectExt,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub index: usize,
    pub cursor_y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Measurements {
    pub left_axis_width: f32,
    pub right_axis_width: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CacheKey {
    pub bounds: Rect,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PreparedBar {
    pub bounds: Rect,
    pub value: f64,
}

#[derive(Default, Debug, PartialEq)]
pub(crate) struct PreparedFrame {
    pub bars: Box<[PreparedBar]>,
    pub line: Option<Box<[Option<Point>]>>,
    pub marker: Option<Point>,
    pub time_ticks: Box<[PositionedTimeTick]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

pub(crate) fn measure(
    app: &mut App,
    data: &PreparedData,
    text: &mut ChartTextCache,
    theme: &Theme,
) -> Measurements {
    let style = label_style(
        AXIS_FONT_SIZE,
        AXIS_LINE_HEIGHT,
        theme.components().chart.axis,
    );
    Measurements {
        left_axis_width: widest_label_width(
            app,
            text,
            &style,
            &data.flow,
            Some(data.left_axis_title.as_str()),
        ),
        right_axis_width: data.net_assets.as_ref().map(|scale| {
            widest_label_width(
                app,
                text,
                &style,
                scale,
                Some(data.right_axis_title.as_str()),
            )
        }),
    }
}

fn widest_label_width(
    app: &mut App,
    text: &mut ChartTextCache,
    style: &TextStyle,
    scale: &AxisScale,
    title: Option<&str>,
) -> f32 {
    scale
        .ticks
        .iter()
        .map(|tick| tick.label.as_str())
        .chain(title.filter(|title| !title.is_empty()))
        .map(|label| text.width(app, label, style))
        .fold(0.0, f32::max)
}

pub(crate) fn prepare(data: &PreparedData, layout: ChartLayout) -> PreparedFrame {
    let point_count = data.points.len();
    let mut bars = Vec::with_capacity(point_count);
    bars.extend(
        data.points
            .iter()
            .enumerate()
            .filter(|(_, point)| point.flow.is_finite())
            .map(|(index, point)| PreparedBar {
                bounds: flow_bar_bounds(
                    layout.plot,
                    point_count,
                    index,
                    point.flow,
                    data.flow.domain,
                ),
                value: point.flow,
            }),
    );
    let (line, marker) = data.net_assets.as_ref().map_or((None, None), |scale| {
        if point_count == 1 {
            let point = &data.points[0];
            if point.net_assets.is_finite() {
                (
                    None,
                    Some(Point::new(
                        point_x(layout.plot, 1, 0),
                        value_y(point.net_assets, scale.domain, layout.plot),
                    )),
                )
            } else {
                (None, None)
            }
        } else {
            (
                Some(
                    data.points
                        .iter()
                        .enumerate()
                        .map(|(index, point)| {
                            point.net_assets.is_finite().then(|| {
                                Point::new(
                                    point_x(layout.plot, point_count, index),
                                    value_y(point.net_assets, scale.domain, layout.plot),
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                ),
                None,
            )
        }
    });
    PreparedFrame {
        bars: bars.into_boxed_slice(),
        line,
        marker,
        time_ticks: position_time_ticks(
            &data
                .points
                .iter()
                .map(|point| point.date_label.clone())
                .collect::<Vec<_>>(),
            layout,
        ),
    }
}

pub(crate) fn line_geometry(points: &[Option<Point>], width: f32) -> Option<GeometryDesc> {
    let half = (width / 2.0).max(0.5);
    let mut vertices = Vec::<[f32; 3]>::with_capacity(points.len() * 8);
    let mut indices = Vec::<u32>::with_capacity(points.len() * 12);
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

    let mut run: Vec<Point> = Vec::with_capacity(points.len());
    for slot in points.iter().chain(std::iter::once(&None)) {
        let Some(point) = *slot else {
            if run.len() == 1 {
                push_quad(square(run[0]));
            } else {
                for window in run.windows(2) {
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
                for point in &run {
                    push_quad(square(*point));
                }
            }
            run.clear();
            continue;
        };
        run.push(point);
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
        vertex_bytes: Bytes::from(vertex_bytes),
        index_bytes: Bytes::from(index_bytes),
        vertex_stride: GeometryPipeline::Ui.vertex_stride(),
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds,
        pipeline: GeometryPipeline::Ui,
    })
}

pub(crate) fn paint(
    app: &mut App,
    data: &PreparedData,
    prepared: &PreparedFrame,
    layout: ChartLayout,
    hover: Option<Hover>,
    theme: &Theme,
    text: &mut ChartTextCache,
    line: Option<(GeometryId, Color)>,
) -> (Canvas, bool) {
    let mut context = PaintContext {
        app,
        theme,
        text,
        whole: true,
    };
    let chart = theme.components().chart;
    let mut canvas = Canvas::new(layout.content);
    paint_plot_rules(&mut canvas, layout, chart.grid);
    paint_value_grid(&mut canvas, data, layout, chart.grid);
    paint_axes(&mut canvas, data, layout, &mut context);
    paint_time_axis(&mut canvas, &prepared.time_ticks, layout, &mut context);
    let _ = canvas.push_clip(ClipRect {
        rect: layout.plot,
        corner_radius: [0.0; 4],
    });
    paint_bars(&mut canvas, prepared, layout.plot, theme);
    let foreground = theme.role(Role::FgDefault);
    if let Some((id, color)) = line {
        canvas.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, color));
    } else if let Some(center) = prepared.marker {
        push_quad(&mut canvas, marker_bounds(center), foreground);
    }
    if let Some(hover) = hover.filter(|hover| hover.index < data.points.len()) {
        paint_crosshair(
            &mut canvas,
            hover.index,
            data.points.len(),
            layout.plot,
            chart.crosshair,
        );
    }
    let _ = canvas.pop_clip();
    if let Some(hover) = hover.filter(|hover| hover.index < data.points.len()) {
        paint_hover(&mut canvas, data, hover, layout, theme, &mut context);
    }
    let whole = context.whole;
    (canvas, whole)
}

struct PaintContext<'a> {
    app: &'a mut App,
    theme: &'a Theme,
    text: &'a mut ChartTextCache,
    whole: bool,
}

impl PaintContext<'_> {
    fn text(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: f32,
        width: f32,
        y: f32,
        align: Align,
        style: &TextStyle,
    ) {
        if width <= 0.0 {
            return;
        }
        let line = self.text.line(self.app, text, style);
        let line_width = line.size().width;
        let origin_x = match align {
            Align::Left => x,
            Align::Center => x + (width - line_width) / 2.0,
            Align::Right => x + width - line_width,
        };
        if !canvas.push_shaped_text(&line, Point::new(origin_x, y)) {
            self.whole = false;
        }
    }
}

fn paint_plot_rules(canvas: &mut Canvas, layout: ChartLayout, color: Color) {
    push_quad(
        canvas,
        Rect::new(
            layout.plot.origin.x,
            layout.plot.origin.y,
            layout.plot.size.width,
            1.0,
        ),
        color,
    );
    push_quad(
        canvas,
        Rect::new(
            layout.plot.origin.x,
            layout.plot.bottom() - 1.0,
            layout.plot.size.width,
            1.0,
        ),
        color,
    );
}

fn paint_value_grid(canvas: &mut Canvas, data: &PreparedData, layout: ChartLayout, color: Color) {
    for tick in &data.flow.ticks {
        let y = value_y(tick.value, data.flow.domain, layout.plot);
        push_quad(
            canvas,
            Rect::new(layout.plot.origin.x, y, layout.plot.size.width, 1.0),
            color,
        );
    }
}

fn paint_axes(
    canvas: &mut Canvas,
    data: &PreparedData,
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let color = context.theme.components().chart.axis;
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, color);
    let title_y = layout.content.origin.y + 2.0;
    let inset = AXIS_TICK_TEXT_INSET;
    context.text(
        canvas,
        data.left_axis_title.as_str(),
        layout.left_axis.origin.x,
        (layout.left_axis.size.width - inset).max(0.0),
        title_y,
        Align::Right,
        &style,
    );
    paint_value_axis(canvas, &data.flow, AxisSide::Left, layout, context, &style);
    if let Some(scale) = data.net_assets.as_ref() {
        context.text(
            canvas,
            data.right_axis_title.as_str(),
            layout.right_axis.origin.x + inset,
            (layout.right_axis.size.width - inset).max(0.0),
            title_y,
            Align::Left,
            &style,
        );
        paint_value_axis(canvas, scale, AxisSide::Right, layout, context, &style);
    }
}

#[derive(Clone, Copy)]
enum AxisSide {
    Left,
    Right,
}

fn paint_value_axis(
    canvas: &mut Canvas,
    scale: &AxisScale,
    side: AxisSide,
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
    style: &TextStyle,
) {
    let axis = match side {
        AxisSide::Left => layout.left_axis,
        AxisSide::Right => layout.right_axis,
    };
    let (align, origin_x) = match side {
        AxisSide::Left => (Align::Right, axis.origin.x),
        AxisSide::Right => (Align::Left, axis.origin.x + AXIS_TICK_TEXT_INSET),
    };
    let width = (axis.size.width - AXIS_TICK_TEXT_INSET).max(0.0);
    for tick in &scale.ticks {
        let y = axis_label_y(value_y(tick.value, scale.domain, layout.plot), axis);
        context.text(
            canvas,
            tick.label.as_str(),
            origin_x,
            width,
            y,
            align,
            style,
        );
    }
}

fn paint_time_axis(
    canvas: &mut Canvas,
    ticks: &[PositionedTimeTick],
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let style = label_style(
        AXIS_FONT_SIZE,
        AXIS_LINE_HEIGHT,
        context.theme.components().chart.axis,
    );
    for tick in ticks {
        context.text(
            canvas,
            tick.label.as_str(),
            tick.origin_x,
            tick.width,
            layout.time_axis.origin.y + 4.0,
            Align::Center,
            &style,
        );
    }
}

fn paint_bars(canvas: &mut Canvas, prepared: &PreparedFrame, plot: Rect, theme: &Theme) {
    let muted = theme.role(Role::MarketFlat);
    for bar in &prepared.bars {
        let color = flow(theme, bar.value, muted);
        let clipped = bar.bounds.intersect(plot);
        if clipped.is_empty() {
            continue;
        }
        push_quad(canvas, clipped, color);
    }
}

fn paint_crosshair(
    canvas: &mut Canvas,
    index: usize,
    point_count: usize,
    plot: Rect,
    color: Color,
) {
    let x = point_x(plot, point_count, index);
    let mut cursor_y = plot.origin.y;
    while cursor_y < plot.bottom() {
        let height = CROSSHAIR_DASH.min(plot.bottom() - cursor_y);
        push_quad(canvas, Rect::new(x, cursor_y, 1.0, height), color);
        cursor_y += CROSSHAIR_PERIOD;
    }
}

fn paint_hover(
    canvas: &mut Canvas,
    data: &PreparedData,
    hover: Hover,
    layout: ChartLayout,
    theme: &Theme,
    context: &mut PaintContext<'_>,
) {
    let point = &data.points[hover.index];
    let point_count = data.points.len();
    if let (Some(scale), true) = (data.net_assets.as_ref(), point.net_assets.is_finite()) {
        let center = Point::new(
            point_x(layout.plot, point_count, hover.index),
            value_y(point.net_assets, scale.domain, layout.plot),
        );
        push_quad(canvas, marker_bounds(center), theme.role(Role::FgDefault));
    }
    paint_tooltip(
        canvas,
        data,
        point,
        hover,
        point_count,
        layout,
        theme,
        context,
    );
}

fn paint_tooltip(
    canvas: &mut Canvas,
    data: &PreparedData,
    point: &PreparedPoint,
    hover: Hover,
    point_count: usize,
    layout: ChartLayout,
    theme: &Theme,
    context: &mut PaintContext<'_>,
) {
    let style = tooltip_style(theme.tokens());
    let lines = tooltip_lines(point, data.show_net_assets, theme);
    let line_styles = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|(_, color)| label_style(style.font_size, style.line_height, *color))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut content_width = 0.0f32;
    for (line, styles) in lines.iter().zip(&line_styles) {
        let width = line
            .iter()
            .zip(styles)
            .fold(0.0, |width, ((text, _), line_style)| {
                width + context.text.width(context.app, text, line_style)
            });
        content_width = content_width.max(width);
    }
    let bounds = tooltip_bounds(
        Point::new(
            point_x(layout.plot, point_count, hover.index),
            hover.cursor_y,
        ),
        style.content_size(content_width, lines.len()),
        layout.plot,
        TOOLTIP_GAP,
    );
    paint_tooltip_surface(canvas, bounds, style, theme);
    let text_width = (bounds.size.width - style.padding * 2.0).max(0.0);
    for (index, (line, styles)) in lines.iter().zip(&line_styles).enumerate() {
        let y = bounds.origin.y + style.padding + style.line_height * index as f32;
        let mut cursor_x = bounds.origin.x + style.padding;
        for ((text, _), line_style) in line.iter().zip(styles) {
            let width = context.text.width(context.app, text, line_style);
            context.text(
                canvas,
                text,
                cursor_x,
                text_width,
                y,
                Align::Left,
                line_style,
            );
            cursor_x += width;
        }
    }
}

pub(crate) fn tooltip_lines<'a>(
    point: &'a PreparedPoint,
    show_net_assets: bool,
    theme: &Theme,
) -> Vec<Vec<(&'a str, Color)>> {
    let foreground = theme.components().tooltip.foreground;
    let mut lines = vec![
        vec![(point.header_date.as_str(), foreground)],
        vec![
            (point.flow_tooltip_caption.as_str(), foreground),
            (
                point.flow_label.as_str(),
                flow(theme, point.flow, foreground),
            ),
        ],
    ];
    if show_net_assets {
        lines.push(vec![(point.net_assets_tooltip.as_str(), foreground)]);
    }
    lines
}

fn paint_tooltip_surface(canvas: &mut Canvas, bounds: Rect, style: TooltipStyle, theme: &Theme) {
    let radius = style
        .radius
        .max(0.0)
        .min(bounds.size.width / 2.0)
        .min(bounds.size.height / 2.0);
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [radius; 4],
        paint: QuadPaint::Solid {
            fill: theme.components().tooltip.background,
            border: None,
        },
    });
}

fn push_quad(canvas: &mut Canvas, bounds: Rect, fill: Color) {
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid { fill, border: None },
    });
}
