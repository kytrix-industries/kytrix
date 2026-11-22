//! Painting one statistics chart.
//!
//! A frame is built from three parts, separated because they change at different
//! times:
//!
//! - [`measure`] shapes the axis labels and the legend, which is what the layout and
//!   the legend's placement are built from;
//! - [`prepare_*`] folds the window into the boxes and lines a frame draws, using the
//!   box the layout gave it. It runs only when that box or the window changed;
//! - [`paint`] turns those boxes into quads, those lines into a registered mesh, and
//!   shapes every label, plus the crosshair and the tooltip, which are the only parts
//!   that follow the pointer.
//!
//! Everything a label *says* was written when the snapshot arrived; this module places
//! text, it never formats it. The output is one engine [`Canvas`], because that is the
//! engine's only self-drawing entry point: a chart's labels move with its data, and a
//! tooltip that followed the pointer through a popover layer would escape the clipping
//! that keeps it inside the plot.
//!
//! # Lines are static geometry
//!
//! The engine has no path builder: a line is a registered mesh (`GeometryDesc`) drawn
//! with [`GeometryDraw`]. [`line_geometry`] builds that mesh — a two-pixel-wide ribbon,
//! one quad per segment and a square at every joint — and the chart registers it once
//! per prepared frame, releasing the previous one. The ribbon carries no
//! anti-aliasing distance (every vertex is marked as interior), so the line is drawn
//! with a hard edge rather than the feather the old path painter gave it; that is
//! registered as a W5a difference.

use base_ui::theme::{resolved::Theme, role::Role};
use bytes::Bytes;
use smol_str::SmolStr;
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

/// The most series one chart draws, and so the most legend and tooltip entries it can
/// have.
pub(crate) const MAX_SERIES: usize = 3;

/// The roles the two lines of a line chart are prepared and drawn in, in that order.
///
/// The roles are named once here because two passes need them: the one that strokes
/// the lines and the one that marks a hover. Naming them by index in both places would
/// leave a line and the marker on it free to be read as two different series.
const LINE_SERIES: [Series; 2] = [Series::Amount, Series::Value];

/// One point under the pointer, and how high the pointer was.
///
/// The height is kept rather than the whole position: the x a hover resolves to is the
/// reading's own, and the height is rounded to whole pixels before it is stored, so a
/// pointer that moved a fraction of a pixel is not a change worth a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub index: usize,
    pub cursor_y: f32,
}

/// One line of a tooltip's body: what it says, and the role it is read in.
///
/// The role is what ties a tooltip line to the legend's swatch for the same series —
/// naming it is what keeps the two one colour. A chart whose tooltip is not a legend
/// but a reading leaves it `None`, and every line of that tooltip is then drawn in the
/// tooltip's own foreground, the colour its title is already in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TooltipEntry {
    pub text: SmolStr,
    pub color: Option<Series>,
}

/// One tooltip, already written.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tooltip {
    pub title: SmolStr,
    pub entries: Box<[TooltipEntry]>,
}

/// The line and markers of one series.
///
/// A day that is not a reading is `None`, which breaks the line rather than drawing the
/// segment across the gap.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct LineGeometry {
    pub points: Option<Box<[Option<Point>]>>,
    pub markers: Box<[Rect]>,
}

/// The boxes and lines one frame draws, prepared from its layout.
#[derive(Default, Debug, PartialEq)]
pub(crate) enum Prepared {
    /// Nothing has been prepared yet, which is the state of a chart that has never been
    /// laid out.
    #[default]
    Nothing,
    Lines {
        series: [LineGeometry; 2],
        ticks: Box<[PositionedTimeTick]>,
    },
    Bars {
        bars: Box<[(Rect, Rect)]>,
        ticks: Box<[PositionedTimeTick]>,
    },
    Shares {
        bars: Box<[(Rect, Rect)]>,
        line: Option<Box<[Option<Point>]>>,
        markers: Box<[Rect]>,
        ticks: Box<[PositionedTimeTick]>,
    },
}

impl Prepared {
    /// How many points the frame was built from.
    pub(crate) fn point_count(&self) -> usize {
        match self {
            Self::Nothing => 0,
            Self::Lines { series, .. } => series[0].markers.len(),
            Self::Bars { bars, .. } => bars.len(),
            Self::Shares { markers, .. } => markers.len(),
        }
    }
}

/// The measured width of every legend entry.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LegendMeasurements {
    label_widths: [f32; MAX_SERIES],
    entry_widths: [f32; MAX_SERIES],
    pub content_width: f32,
}

/// What the axes and the legend need before a layout can be built.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct Measurements {
    pub left_axis_width: Option<f32>,
    pub right_axis_width: f32,
    pub legend: LegendMeasurements,
}

impl PartialEq for LegendMeasurements {
    fn eq(&self, other: &Self) -> bool {
        self.content_width == other.content_width
    }
}

/// Measures the legend and the axis labels, which is what the layout is built from.
///
/// Each axis is measured against everything drawn in it, its title included: a title
/// wider than its axis would be drawn off the edge of the chart and clipped away, so the
/// axis grows instead.
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

/// A title that is worth measuring: an empty one occupies nothing.
fn title_of(title: &str) -> Option<&str> {
    (!title.is_empty()).then_some(title)
}

/// The widest label of one axis, counting its title.
fn widest_label_width(
    text: &mut ChartTextCache,
    app: &mut App,
    scale: &AxisScale,
    title: Option<&str>,
    style: &TextStyle,
) -> f32 {
    scale
        .ticks
        .iter()
        .map(|tick| tick.label.as_str())
        .chain(title)
        .map(|label| text.width(app, label, style))
        .fold(0.0, f32::max)
}

/// Measures every legend entry, which is what decides whether the legend fits.
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
    measurements.content_width = measurements.entry_widths[..count].iter().sum::<f32>()
        + LEGEND_ENTRY_GAP * count.saturating_sub(1) as f32;
    measurements
}

/// Folds the window of an open-interest chart into lines and markers.
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
                points.iter().map(|point| {
                    // An interval the venue published no amount for breaks the line
                    // rather than being drawn as a value of zero.
                    point.quote_asset_volume.map_or(f64::NAN, quantity_as_f64)
                }),
                point_count,
                value.domain,
                layout.plot,
            ),
        ],
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

/// Folds the window of a taker-volume chart into boxes.
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

/// Folds the window of a ratio chart into boxes and a line.
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
        let center = Point::new(
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

/// The time labels of a window, written once.
pub(crate) fn time_labels(
    timestamps: impl Iterator<Item = i64>,
    label: impl Fn(i64) -> SmolStr,
) -> Box<[SmolStr]> {
    timestamps.map(label).collect()
}

/// Builds the line and markers of one series.
fn line_geometry(
    values: impl Iterator<Item = f64>,
    point_count: usize,
    domain: super::axes::ValueDomain,
    plot: Rect,
) -> LineGeometry {
    let mut markers = Vec::with_capacity(point_count);
    let mut points = Vec::with_capacity(point_count);
    for (index, value) in values.enumerate() {
        if !value.is_finite() {
            // A point that is not a reading breaks the line, so the segment across the
            // gap is not drawn as if the gap were data.
            points.push(None);
            continue;
        }
        let position = Point::new(
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

/// Places the time labels a window's width has room for.
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

/// The mesh of one series line: a ribbon of `width` through the points.
///
/// A run of consecutive readings becomes one quad per segment plus a square at every
/// joint, which is what keeps the ribbon continuous around a corner. A point that is
/// not a reading is a break rather than a zero, so the runs on either side are drawn as
/// separate lines — the segment across the gap is not drawn as if the gap were data.
///
/// Every vertex is marked as interior (a negative distance, which the engine's coverage
/// shader reads as full coverage), because this mesh is not anti-aliased: the engine's
/// `Ui` pipeline is a coverage-distance pipeline and a filled ribbon is its degenerate
/// case.
pub(crate) fn line_geometry_mesh(points: &[Option<Point>], width: f32) -> Option<GeometryDesc> {
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

    let mut run: Vec<Point> = Vec::with_capacity(points.len());
    for slot in points.iter().chain(std::iter::once(&None)) {
        let Some(point) = *slot else {
            if run.len() == 1 {
                // A run of one reading is a dot: a circle is not a quad, so it is drawn
                // as the square the joint of a longer run would have.
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

/// Which way a label hangs off the box it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// Everything one frame draws that is not the prepared window.
pub(crate) struct Frame<'a> {
    pub layout: ChartLayout,
    /// The y-axes the chart draws: `(left, right, share)`.
    ///
    /// The right one is not optional — every chart is read against a scale on its
    /// right — while a chart with a single series leaves the left empty.
    pub scales: (Option<&'a AxisScale>, &'a AxisScale),
    /// The titles of the axes: `(left, right)`; an empty one is not drawn.
    pub axis_titles: (&'a str, &'a str),
    pub legend: &'a [SeriesLabel],
    pub legend_measurements: LegendMeasurements,
}

/// Draws one frame of a chart into a canvas.
///
/// The canvas is built whole rather than patched: an engine canvas is a value, and a
/// chart whose box, data or pointer moved is a different value. The chart keeps the last
/// one it built and only rebuilds when its key or the pointer says so.
///
/// The second half of the answer says whether every label reached the canvas. A line
/// whose atlas lease died before it could be pushed is refused by the engine rather than
/// encoded from reused texture, so the frame that ran into it is missing that run: it is
/// not a frame the chart can keep (see [`ChartTextCache::has_stale`] for the other half
/// of the same guard).
pub(crate) fn paint(
    app: &mut App,
    theme: &Theme,
    text: &mut ChartTextCache,
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Option<Hover>,
    tooltip: Option<&Tooltip>,
    meshes: &[Option<GeometryId>; 2],
) -> (Canvas, bool) {
    let mut context = PaintContext {
        app,
        theme,
        text,
        whole: true,
    };
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
        corner_radius: [0.0; 4],
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
    let whole = context.whole;
    (canvas, whole)
}

/// Paints one frame with the shaped text and the theme it needs.
struct PaintContext<'a> {
    app: &'a mut App,
    theme: &'a Theme,
    text: &'a mut ChartTextCache,
    /// Whether every line this pass placed reached the canvas; see [`paint`].
    whole: bool,
}

impl PaintContext<'_> {
    /// Places one line of text inside `[x, x + width]`, at the top edge `y`.
    ///
    /// Alignment is resolved here rather than by the shaper: a label's box is its axis,
    /// and where inside that box it starts is what says which side of the plot it names.
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
        // A refused line is one whose atlas lease died between being shaped and being
        // pushed: its glyphs have been reused, so the run is left out rather than drawn
        // from stale texture, and the pass is not a whole frame any more.
        if !canvas.push_shaped_text(&line, Point::new(origin_x, y)) {
            self.whole = false;
        }
    }

    /// The width one line of `text` occupies in `style`.
    fn width(&mut self, text: &str, style: &TextStyle) -> f32 {
        self.text.width(self.app, text, style)
    }
}

/// Draws the two rules that bound the plot from above and below.
///
/// There is no background: the pane body behind the canvas is the background, so the
/// chart never paints over the body's rounded corners.
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

/// Draws the horizontal grid line of every tick of the axis the plot is read against.
///
/// One axis drives the grid, not two: two sets of lines a few pixels apart would read as
/// neither. The left axis is the one named first, so it is the one that draws them — and
/// a chart with only a right axis draws its grid from that.
fn paint_value_grid(canvas: &mut Canvas, frame: &Frame<'_>, color: Color) {
    let scale = frame.scales.0.unwrap_or(frame.scales.1);
    for tick in &scale.ticks {
        let y = value_y(tick.value, scale.domain, frame.layout.plot);
        push_quad(
            canvas,
            Rect::new(
                frame.layout.plot.origin.x,
                y,
                frame.layout.plot.size.width,
                1.0,
            ),
            color,
        );
    }
}

/// Draws both y-axes: the title above each, and a label per tick beside it.
fn paint_axes(canvas: &mut Canvas, frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let color = context.theme.role(Role::ChartAxis);
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, color);
    let layout = frame.layout;
    let title_y = layout.content.origin.y + 2.0;
    let inset = AXIS_TICK_TEXT_INSET;
    let (left_title, right_title) = frame.axis_titles;
    context.text(
        canvas,
        left_title,
        layout.left_axis.origin.x,
        (layout.left_axis.size.width - inset).max(0.0),
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
        layout.right_axis.origin.x + inset,
        (layout.right_axis.size.width - inset).max(0.0),
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

/// Which side of the plot a y-axis is on, which is what says where its labels go.
#[derive(Clone, Copy)]
enum AxisSide {
    Left,
    Right,
}

/// Draws one y-axis: a label per tick, beside the line that tick draws.
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

/// Draws the interval of every labelled position under the plot.
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
            layout.time_axis.origin.y + 4.0,
            Align::Center,
            &style,
        );
    }
}

/// Draws the legend: a swatch and a name per series, centred over the content.
fn paint_legend(canvas: &mut Canvas, frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let layout = frame.layout;
    let measurements = frame.legend_measurements;
    let text_color = context.theme.role(Role::FgMuted);
    let style = label_style(AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, text_color);
    let mut x = centered_legend_origin(layout, measurements.content_width);
    let y = layout.legend.origin.y + 8.0;
    for (index, entry) in frame.legend.iter().enumerate().take(MAX_SERIES) {
        let swatch = Rect::new(x + 3.0, y + 2.0, 8.0, 8.0);
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

/// The x the legend starts at: centred when it fits, inset from the left when it does
/// not.
fn centered_legend_origin(layout: ChartLayout, content_width: f32) -> f32 {
    let legend = layout.legend;
    if content_width <= legend.size.width {
        legend.origin.x + (legend.size.width - content_width) / 2.0
    } else {
        legend.origin.x + LEGEND_OVERFLOW_INSET
    }
}

/// Draws the series of the prepared frame.
///
/// The meshes arrive registered and are named by the canvas: the chart owns the geometry
/// store entries for as long as the canvas that draws them is the current one. A chart
/// whose series are not lines draws its boxes here.
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

/// Draws the dashed rule through the hovered interval.
fn paint_dashed_vertical(canvas: &mut Canvas, x: f32, plot: Rect, color: Color) {
    let mut y = plot.origin.y;
    while y < plot.bottom() {
        let height = CROSSHAIR_DASH.min(plot.bottom() - y);
        push_quad(canvas, Rect::new(x, y, 1.0, height), color);
        y += CROSSHAIR_PERIOD;
    }
}

/// Draws the square that says which point a tooltip belongs to, and the tooltip itself.
///
/// One marker per series, all at the hovered interval: a chart of two lines shows two
/// readings of the same moment, and marking only the one nearest the pointer would leave
/// the other reading unmarked on the very frame the tooltip names it.
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
        .fold(0.0f32, f32::max);
    let bounds = tooltip_bounds(
        Point::new(
            point_x(layout.plot, prepared.point_count(), hover.index),
            hover.cursor_y,
        ),
        style.content_size(content_width, 1 + tooltip.entries.len()),
        layout.plot,
        TOOLTIP_GAP,
    );
    paint_tooltip_surface(canvas, bounds, style, surface);
    let text_width = (bounds.size.width - style.padding * 2.0).max(0.0);
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

/// The marker of every series under the pointer, each naming the line it marks.
///
/// Every series is marked at the same x, because they are the readings of one interval;
/// each is marked at its own y, because each is read against its own axis. A chart whose
/// series are one shape — a stacked bar, a single line — has one marker, which is the
/// same answer with one element in it.
///
/// A paired-bar chart is marked by none: the top of a bar is both its reading and the
/// edge of the bar the pointer is over, so a square there would cover the very bar it
/// was meant to point at, and a hover already says which interval it is on with the rule
/// drawn through it.
pub(crate) fn hover_markers(
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Hover,
) -> Vec<(Series, Rect)> {
    let count = prepared.point_count();
    let x = point_x(frame.layout.plot, count, hover.index);
    let centers: Vec<(Series, f32)> = match prepared {
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
        .map(|(series, y)| (series, marker_bounds(Point::new(x, y), HOVER_MARKER_SIZE)))
        .collect()
}

fn paint_tooltip_surface(
    canvas: &mut Canvas,
    bounds: Rect,
    style: TooltipStyle,
    background: Color,
) {
    let radius = style
        .radius
        .max(0.0)
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

/// Pushes one solid quad.
fn push_quad(canvas: &mut Canvas, bounds: Rect, fill: Color) {
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid { fill, border: None },
    });
}
