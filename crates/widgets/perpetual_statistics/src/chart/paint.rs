use bytes::Bytes;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    math::{LogicalPoint, LogicalPx, LogicalRect, Transform, px},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, GeometryDraw, Quad, QuadPaint},
    },
    text::TextStyle,
};
use ui_kit::theme::{resolved::Theme, role::Role};

use super::{
    axes::{
        AXIS_TICK_TEXT_INSET, AxisScale, ChartLayout, PositionedTimeTick, axis_label_y,
        marker_bounds, paired_bar_bounds, point_x, position_time_ticks, ratio_bar_width,
        stacked_bar_bounds, time_tick_indices, tooltip_bounds, value_y,
    },
    palette::{
        AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, CROSSHAIR_DASH, CROSSHAIR_PERIOD, HOVER_MARKER_SIZE,
        LEGEND_ENTRY_GAP, LEGEND_OVERFLOW_INSET, LEGEND_SWATCH_GAP, LINE_MARKER_SIZE, Series,
        SeriesLabel, TOOLTIP_GAP, TooltipStyle, tooltip_style,
    },
    rect::RectExt,
    text::{ChartTextCache, label_style},
};
use crate::model::{
    LongShortRatioPoint, OpenInterestPoint, TakerVolumePoint, quantity_as_f64, ratio_as_f64,
    ratio_shares,
};

pub(crate) const MAX_SERIES: usize = 3;

const LINE_SERIES: [Series; 2] = [Series::Amount, Series::Value];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub index: usize,
    pub cursor_y: LogicalPx,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TooltipEntry {
    pub text: SmolStr,
    pub color: Option<Series>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tooltip {
    pub title: SmolStr,
    pub entries: Box<[TooltipEntry]>,
}

#[derive(Default, Debug, PartialEq)]
pub(crate) struct LineGeometry {
    pub points: Option<Box<[Option<LogicalPoint>]>>,
    pub markers: Box<[LogicalRect]>,
}

#[derive(Default, Debug, PartialEq)]
pub(crate) enum Prepared {
    #[default]
    Nothing,
    Lines {
        series: [LineGeometry; 2],
        ticks: Box<[PositionedTimeTick]>,
    },
    Bars {
        bars: Box<[(LogicalRect, LogicalRect)]>,
        ticks: Box<[PositionedTimeTick]>,
    },
    Shares {
        bars: Box<[(LogicalRect, LogicalRect)]>,
        line: Option<Box<[Option<LogicalPoint>]>>,
        markers: Box<[LogicalRect]>,
        ticks: Box<[PositionedTimeTick]>,
    },
}

impl Prepared {
    pub(crate) fn point_count(&self) -> usize {
        match self {
            Self::Nothing => 0,
            Self::Lines { series, .. } => series[0].markers.len(),
            Self::Bars { bars, .. } => bars.len(),
            Self::Shares { markers, .. } => markers.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LegendMeasurements {
    label_widths: [LogicalPx; MAX_SERIES],
    entry_widths: [LogicalPx; MAX_SERIES],
    pub content_width: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct Measurements {
    pub left_axis_width: Option<LogicalPx>,
    pub right_axis_width: LogicalPx,
    pub legend: LegendMeasurements,
}

impl PartialEq for LegendMeasurements {
    fn eq(&self, other: &Self) -> bool {
        self.content_width == other.content_width
    }
}

pub(crate) fn measure(
    app: &mut App,
    theme: &Theme,
    text: &mut ChartTextCache,
    axis_titles: (&str, &str),
    legend: &[SeriesLabel],
    scales: (Option<&AxisScale>, &AxisScale),
) -> Measurements {
    let axis_color = theme.role(Role::ChartAxis);
    let legend_color = theme.role(Role::FgMuted);
    let axis_style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, axis_color);
    let left_axis_width = scales
        .0
        .map(|scale| widest_label_width(text, app, scale, title_of(axis_titles.0), &axis_style));
    let right_axis_width =
        widest_label_width(text, app, scales.1, title_of(axis_titles.1), &axis_style);
    Measurements {
        left_axis_width,
        right_axis_width,
        legend: measure_legend(legend, legend_color, text, app),
    }
}

fn title_of(title: &str) -> Option<&str> {
    (!title.is_empty()).then_some(title)
}

fn widest_label_width(
    text: &mut ChartTextCache,
    app: &mut App,
    scale: &AxisScale,
    title: Option<&str>,
    style: &TextStyle,
) -> LogicalPx {
    scale
        .ticks
        .iter()
        .map(|tick| tick.label.as_str())
        .chain(title)
        .map(|label| text.width(app, label, style))
        .fold(LogicalPx::ZERO, LogicalPx::max)
}

fn measure_legend(
    legend: &[SeriesLabel],
    text_color: Color,
    text: &mut ChartTextCache,
    app: &mut App,
) -> LegendMeasurements {
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, text_color);
    let mut measurements = LegendMeasurements::default();
    let count = legend.len().min(MAX_SERIES);
    for (index, entry) in legend.iter().take(count).enumerate() {
        let label_width = text.width(app, &entry.label, &style);
        measurements.label_widths[index] = label_width;
        measurements.entry_widths[index] = label_width + LEGEND_SWATCH_GAP;
    }
    measurements.content_width = measurements.entry_widths[..count]
        .iter()
        .fold(LogicalPx::ZERO, |total, width| total + *width)
        + LEGEND_ENTRY_GAP * count.saturating_sub(1) as f32;
    measurements
}

pub(crate) fn prepare_open_interest(
    points: &[OpenInterestPoint],
    amount: &AxisScale,
    value: &AxisScale,
    time_labels: &[SmolStr],
    layout: ChartLayout,
) -> Prepared {
    let point_count = points.len();
    Prepared::Lines {
        series: [
            line_geometry(
                points
                    .iter()
                    .map(|point| quantity_as_f64(point.base_asset_volume)),
                point_count,
                amount.domain,
                layout.plot,
            ),
            line_geometry(
                points
                    .iter()
                    .map(|point| point.quote_asset_volume.map_or(f64::NAN, quantity_as_f64)),
                point_count,
                value.domain,
                layout.plot,
            ),
        ],
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

pub(crate) fn prepare_taker_volume(
    points: &[TakerVolumePoint],
    volume: &AxisScale,
    time_labels: &[SmolStr],
    layout: ChartLayout,
) -> Prepared {
    let point_count = points.len();
    let bars = points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            paired_bar_bounds(
                layout.plot,
                point_count,
                index,
                quantity_as_f64(point.buy_volume),
                quantity_as_f64(point.sell_volume),
                volume.domain,
            )
        })
        .collect::<Vec<_>>();
    Prepared::Bars {
        bars: bars.into_boxed_slice(),
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

pub(crate) fn prepare_ratio(
    points: &[LongShortRatioPoint],
    ratio: &AxisScale,
    time_labels: &[SmolStr],
    layout: ChartLayout,
) -> Prepared {
    let point_count = points.len();
    let width = ratio_bar_width(layout.plot, point_count);
    let mut bars = Vec::with_capacity(point_count);
    let mut markers = Vec::with_capacity(point_count);
    let mut line = Vec::with_capacity(point_count);
    for (index, bar) in points.iter().enumerate() {
        let Some((long_share, short_share)) = ratio_shares(bar.ratio) else {
            line.push(None);
            continue;
        };
        let center_x = point_x(layout.plot, point_count, index);
        bars.push(stacked_bar_bounds(
            center_x,
            width,
            long_share,
            short_share,
            layout.plot,
        ));
        let center = LogicalPoint::new(
            center_x,
            value_y(ratio_as_f64(bar.ratio), ratio.domain, layout.plot),
        );
        markers.push(marker_bounds(center, LINE_MARKER_SIZE));
        line.push(Some(center));
    }
    Prepared::Shares {
        bars: bars.into_boxed_slice(),
        line: (point_count > 1).then(|| line.into_boxed_slice()),
        markers: markers.into_boxed_slice(),
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

pub(crate) fn time_labels(
    timestamps: impl Iterator<Item = i64>,
    label: impl Fn(i64) -> SmolStr,
) -> Box<[SmolStr]> {
    timestamps.map(label).collect()
}

fn line_geometry(
    values: impl Iterator<Item = f64>,
    point_count: usize,
    domain: super::axes::ValueDomain,
    plot: LogicalRect,
) -> LineGeometry {
    let mut markers = Vec::with_capacity(point_count);
    let mut points = Vec::with_capacity(point_count);
    for (index, value) in values.enumerate() {
        if !value.is_finite() {
            points.push(None);
            continue;
        }
        let position = LogicalPoint::new(
            point_x(plot, point_count, index),
            value_y(value, domain, plot),
        );
        markers.push(marker_bounds(position, LINE_MARKER_SIZE));
        points.push(Some(position));
    }
    LineGeometry {
        points: (point_count > 1).then(|| points.into_boxed_slice()),
        markers: markers.into_boxed_slice(),
    }
}

fn time_ticks(
    labels: &[SmolStr],
    point_count: usize,
    layout: ChartLayout,
) -> Box<[PositionedTimeTick]> {
    position_time_ticks(
        time_tick_indices(point_count, layout.plot.size.width)
            .into_iter()
            .filter_map(|index| labels.get(index).map(|label| (index, label.clone()))),
        point_count,
        layout,
    )
}

pub(crate) fn line_geometry_mesh(
    points: &[Option<LogicalPoint>],
    width: LogicalPx,
) -> Option<GeometryDesc> {
    let half = (width / 2.0).max(px(0.5));
    let mut vertices = Vec::<[f32; 3]>::with_capacity(points.len() * 8);
    let mut indices = Vec::<u32>::with_capacity(points.len() * 12);
    let mut min = LogicalPoint::new(px(f32::INFINITY), px(f32::INFINITY));
    let mut max = LogicalPoint::new(px(f32::NEG_INFINITY), px(f32::NEG_INFINITY));
    let mut push_quad = |corners: [LogicalPoint; 4]| {
        let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
        for corner in corners {
            vertices.push([f32::from(corner.x), f32::from(corner.y), -1.0]);
            min = LogicalPoint::new(min.x.min(corner.x), min.y.min(corner.y));
            max = LogicalPoint::new(max.x.max(corner.x), max.y.max(corner.y));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    let square = |center: LogicalPoint| {
        [
            LogicalPoint::new(center.x - half, center.y - half),
            LogicalPoint::new(center.x + half, center.y - half),
            LogicalPoint::new(center.x + half, center.y + half),
            LogicalPoint::new(center.x - half, center.y + half),
        ]
    };

    let mut run: Vec<LogicalPoint> = Vec::with_capacity(points.len());
    for slot in points.iter().chain(std::iter::once(&None)) {
        let Some(point) = *slot else {
            if run.len() == 1 {
                push_quad(square(run[0]));
            } else {
                for window in run.windows(2) {
                    let (from, to) = (window[0], window[1]);
                    let delta = LogicalPoint::new(to.x - from.x, to.y - from.y);
                    let length = delta.x.value().hypot(delta.y.value());
                    if !length.is_finite() || length <= f32::EPSILON {
                        continue;
                    }
                    let normal = LogicalPoint::new(
                        -delta.y / length * half.value(),
                        delta.x / length * half.value(),
                    );
                    push_quad([
                        LogicalPoint::new(from.x + normal.x, from.y + normal.y),
                        LogicalPoint::new(to.x + normal.x, to.y + normal.y),
                        LogicalPoint::new(to.x - normal.x, to.y - normal.y),
                        LogicalPoint::new(from.x - normal.x, from.y - normal.y),
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
        vertex_bytes: Bytes::from(vertex_bytes),
        index_bytes: Bytes::from(index_bytes),
        vertex_stride: GeometryPipeline::Ui.vertex_stride(),
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds,
        pipeline: GeometryPipeline::Ui,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

pub(crate) struct Frame<'a> {
    pub layout: ChartLayout,
    pub scales: (Option<&'a AxisScale>, &'a AxisScale),
    pub axis_titles: (&'a str, &'a str),
    pub legend: &'a [SeriesLabel],
    pub legend_measurements: LegendMeasurements,
}

pub(crate) fn paint(
    app: &mut App,
    theme: &Theme,
    text: &mut ChartTextCache,
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Option<Hover>,
    tooltip: Option<&Tooltip>,
    meshes: &[Option<GeometryId>; 2],
) -> Canvas {
    let mut context = PaintContext { app, theme, text };
    let mut canvas = Canvas::new(frame.layout.content);
    paint_plot_rules(&mut canvas, frame.layout, theme.role(Role::ChartGrid));
    paint_value_grid(&mut canvas, frame, theme.role(Role::ChartGrid));
    paint_axes(&mut canvas, frame, &mut context);
    if let Prepared::Lines { ticks, .. }
    | Prepared::Bars { ticks, .. }
    | Prepared::Shares { ticks, .. } = prepared
    {
        paint_time_axis(&mut canvas, ticks, frame.layout, &mut context);
    }
    paint_legend(&mut canvas, frame, &mut context);
    let _ = canvas.push_clip(ClipRect {
        rect: frame.layout.plot,
        corner_radius: [px(0.0); 4],
    });
    paint_series(&mut canvas, prepared, meshes, theme);
    if let Some(hover) = hover {
        let x = point_x(frame.layout.plot, prepared.point_count(), hover.index);
        paint_dashed_vertical(
            &mut canvas,
            x,
            frame.layout.plot,
            theme.role(Role::ChartCrosshair),
        );
    }
    let _ = canvas.pop_clip();
    if let (Some(hover), Some(tooltip)) = (hover, tooltip) {
        paint_tooltip(&mut canvas, frame, prepared, hover, tooltip, &mut context);
    }
    canvas
}

struct PaintContext<'a> {
    app: &'a mut App,
    theme: &'a Theme,
    text: &'a mut ChartTextCache,
}

impl PaintContext<'_> {
    fn text(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: LogicalPx,
        width: LogicalPx,
        y: LogicalPx,
        align: Align,
        style: &TextStyle,
    ) {
        if width <= LogicalPx::ZERO {
            return;
        }
        let (content, line_width) = self.text.line(self.app, text, style);
        let origin_x = match align {
            Align::Left => x,
            Align::Center => x + (width - line_width) / 2.0,
            Align::Right => x + width - line_width,
        };
        let _ = canvas.push_text(content, LogicalPoint::new(origin_x, y));
    }

    fn width(&mut self, text: &str, style: &TextStyle) -> LogicalPx {
        self.text.width(self.app, text, style)
    }
}

fn paint_plot_rules(canvas: &mut Canvas, layout: ChartLayout, color: Color) {
    push_quad(
        canvas,
        LogicalRect::new(
            layout.plot.origin.x,
            layout.plot.origin.y,
            layout.plot.size.width,
            px(1.0),
        ),
        color,
    );
    push_quad(
        canvas,
        LogicalRect::new(
            layout.plot.origin.x,
            layout.plot.bottom() - px(1.0),
            layout.plot.size.width,
            px(1.0),
        ),
        color,
    );
}

fn paint_value_grid(canvas: &mut Canvas, frame: &Frame<'_>, color: Color) {
    let scale = frame.scales.0.unwrap_or(frame.scales.1);
    for tick in &scale.ticks {
        let y = value_y(tick.value, scale.domain, frame.layout.plot);
        push_quad(
            canvas,
            LogicalRect::new(
                frame.layout.plot.origin.x,
                y,
                frame.layout.plot.size.width,
                px(1.0),
            ),
            color,
        );
    }
}

fn paint_axes(canvas: &mut Canvas, frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let color = context.theme.role(Role::ChartAxis);
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, color);
    let layout = frame.layout;
    let title_y = layout.content.origin.y + px(2.0);
    let (left_title, right_title) = frame.axis_titles;
    context.text(
        canvas,
        left_title,
        layout.left_axis.origin.x,
        (layout.left_axis.size.width - AXIS_TICK_TEXT_INSET).max(px(0.0)),
        title_y,
        Align::Right,
        &style,
    );
    if let Some(scale) = frame.scales.0 {
        paint_value_axis(canvas, scale, AxisSide::Left, frame, context, &style);
    }
    context.text(
        canvas,
        right_title,
        layout.right_axis.origin.x + AXIS_TICK_TEXT_INSET,
        (layout.right_axis.size.width - AXIS_TICK_TEXT_INSET).max(px(0.0)),
        title_y,
        Align::Left,
        &style,
    );
    paint_value_axis(
        canvas,
        frame.scales.1,
        AxisSide::Right,
        frame,
        context,
        &style,
    );
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
    frame: &Frame<'_>,
    context: &mut PaintContext<'_>,
    style: &TextStyle,
) {
    let layout = frame.layout;
    let axis = match side {
        AxisSide::Left => layout.left_axis,
        AxisSide::Right => layout.right_axis,
    };
    let (align, origin_x) = match side {
        AxisSide::Left => (Align::Right, axis.origin.x),
        AxisSide::Right => (Align::Left, axis.origin.x + AXIS_TICK_TEXT_INSET),
    };
    let width = (axis.size.width - AXIS_TICK_TEXT_INSET).max(px(0.0));
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
        context.theme.role(Role::ChartAxis),
    );
    for tick in ticks {
        context.text(
            canvas,
            tick.label.as_str(),
            tick.origin_x,
            tick.width,
            layout.time_axis.origin.y + px(4.0),
            Align::Center,
            &style,
        );
    }
}

fn paint_legend(canvas: &mut Canvas, frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let layout = frame.layout;
    let measurements = frame.legend_measurements;
    let text_color = context.theme.role(Role::FgMuted);
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, text_color);
    let mut x = centered_legend_origin(layout, measurements.content_width);
    let y = layout.legend.origin.y + px(8.0);
    for (index, entry) in frame.legend.iter().enumerate().take(MAX_SERIES) {
        let swatch = LogicalRect::new(x + px(3.0), y + px(2.0), px(8.0), px(8.0));
        push_quad(canvas, swatch, entry.series.color(context.theme));
        context.text(
            canvas,
            entry.label.as_str(),
            x + LEGEND_SWATCH_GAP,
            measurements.label_widths[index],
            y,
            Align::Left,
            &style,
        );
        x += measurements.entry_widths[index] + LEGEND_ENTRY_GAP;
    }
}

fn centered_legend_origin(layout: ChartLayout, content_width: LogicalPx) -> LogicalPx {
    let legend = layout.legend;
    if content_width <= legend.size.width {
        legend.origin.x + (legend.size.width - content_width) / 2.0
    } else {
        legend.origin.x + LEGEND_OVERFLOW_INSET
    }
}

fn paint_series(
    canvas: &mut Canvas,
    prepared: &Prepared,
    meshes: &[Option<GeometryId>; 2],
    theme: &Theme,
) {
    match prepared {
        Prepared::Nothing => {}
        Prepared::Lines { series, .. } => {
            for (index, (geometry, role)) in series.iter().zip(LINE_SERIES).enumerate() {
                let color = role.color(theme);
                if let Some(id) = meshes.get(index).copied().flatten() {
                    canvas.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, color));
                }
                for marker in &geometry.markers {
                    push_quad(canvas, *marker, color);
                }
            }
        }
        Prepared::Bars { bars, .. } => {
            let buy_color = Series::Long.color(theme);
            let sell_color = Series::Short.color(theme);
            for (buy, sell) in bars {
                push_quad(canvas, *buy, buy_color);
                push_quad(canvas, *sell, sell_color);
            }
        }
        Prepared::Shares {
            bars,
            markers,
            line,
            ..
        } => {
            let long_color = Series::Long.color(theme);
            let short_color = Series::Short.color(theme);
            for (long, short) in bars {
                push_quad(canvas, *short, short_color);
                push_quad(canvas, *long, long_color);
            }
            let ratio = Series::Ratio.color(theme);
            if line.is_some()
                && let Some(Some(id)) = meshes.first()
            {
                canvas.push_geometry(GeometryDraw::full(*id, Transform::IDENTITY, ratio));
            }
            for marker in markers {
                push_quad(canvas, *marker, ratio);
            }
        }
    }
}

fn paint_dashed_vertical(canvas: &mut Canvas, x: LogicalPx, plot: LogicalRect, color: Color) {
    let bottom = plot.bottom();
    let mut y = plot.origin.y;
    while y < bottom {
        let height = CROSSHAIR_DASH.min(bottom - y);
        push_quad(canvas, LogicalRect::new(x, y, px(1.0), height), color);
        y += CROSSHAIR_PERIOD;
    }
}

fn paint_tooltip(
    canvas: &mut Canvas,
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Hover,
    tooltip: &Tooltip,
    context: &mut PaintContext<'_>,
) {
    let layout = frame.layout;
    let style = tooltip_style(context.theme.tokens());
    let foreground = context.theme.components().tooltip.foreground;
    let surface = context.theme.components().tooltip.background;
    for marker in hover_markers(frame, prepared, hover) {
        push_quad(canvas, marker.1, marker.0.color(context.theme));
    }
    let line_style = label_style(style.font_size, style.line_height, foreground);
    let content_width = std::iter::once(tooltip.title.as_str())
        .chain(tooltip.entries.iter().map(|entry| entry.text.as_str()))
        .map(|line| context.width(line, &line_style))
        .fold(LogicalPx::ZERO, LogicalPx::max);
    let bounds = tooltip_bounds(
        LogicalPoint::new(
            point_x(layout.plot, prepared.point_count(), hover.index),
            hover.cursor_y,
        ),
        style.content_size(content_width, 1 + tooltip.entries.len()),
        layout.plot,
        TOOLTIP_GAP,
    );
    paint_tooltip_surface(canvas, bounds, style, surface);
    let text_width = (bounds.size.width - style.padding * 2.0).max(px(0.0));
    context.text(
        canvas,
        tooltip.title.as_str(),
        bounds.origin.x + style.padding,
        text_width,
        bounds.origin.y + style.padding,
        Align::Left,
        &line_style,
    );
    for (index, entry) in tooltip.entries.iter().enumerate() {
        let color = entry
            .color
            .map_or(foreground, |series| series.color(context.theme));
        let entry_style = label_style(style.font_size, style.line_height, color);
        context.text(
            canvas,
            entry.text.as_str(),
            bounds.origin.x + style.padding,
            text_width,
            bounds.origin.y + style.padding + style.line_height * (index + 1) as f32,
            Align::Left,
            &entry_style,
        );
    }
}

pub(crate) fn hover_markers(
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Hover,
) -> Vec<(Series, LogicalRect)> {
    let count = prepared.point_count();
    let x = point_x(frame.layout.plot, count, hover.index);
    let centers: Vec<(Series, LogicalPx)> = match prepared {
        Prepared::Nothing | Prepared::Bars { .. } => Vec::new(),
        Prepared::Lines { series, .. } => series
            .iter()
            .zip(LINE_SERIES)
            .filter_map(|(geometry, series)| {
                geometry
                    .markers
                    .get(hover.index)
                    .map(|marker| (series, marker.origin.y + marker.size.height / 2.0))
            })
            .collect(),
        Prepared::Shares { markers, .. } => markers
            .get(hover.index)
            .map(|marker| vec![(Series::Ratio, marker.origin.y + marker.size.height / 2.0)])
            .unwrap_or_default(),
    };
    centers
        .into_iter()
        .map(|(series, y)| {
            (
                series,
                marker_bounds(LogicalPoint::new(x, y), HOVER_MARKER_SIZE),
            )
        })
        .collect()
}

fn paint_tooltip_surface(
    canvas: &mut Canvas,
    bounds: LogicalRect,
    style: TooltipStyle,
    background: Color,
) {
    let radius = style
        .radius
        .max(px(0.0))
        .min(bounds.size.width / 2.0)
        .min(bounds.size.height / 2.0);
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [radius; 4],
        paint: QuadPaint::Solid {
            fill: background,
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
