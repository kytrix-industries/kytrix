//! Painting one statistics chart.
//!
//! One frame is two passes, and the split is what makes a resize and a hover
//! cheap:
//!
//! - [`prepare`] folds the window into the pixel boxes and paths a frame draws,
//!   using the box the element was given. It runs only when that box changed.
//! - [`paint`] turns those boxes into quads and those paths into strokes, plus
//!   the crosshair and the tooltip, which are the only parts that follow the
//!   pointer.
//!
//! Everything a label *says* was written when the snapshot arrived; this module
//! places text, it never formats it.

use base_ui::style::{
    components::ChartColors,
    theme::{Theme, ThemeExt},
};
use gpui_kit::{
    App, Bounds, Corners, Hsla, Path, PathBuilder, Pixels, Point, SharedString, TextAlign, Window,
    fill, point as at, px, size,
};
use i18n::language::codes::LanguageCode;
use smol_str::SmolStr;

use super::{
    axes::{
        AXIS_TICK_TEXT_INSET, AxisScale, CHART_MIN_CONTENT_HEIGHT, CHART_MIN_CONTENT_WIDTH,
        ChartLayout, MINIMUM_PLOT_WIDTH, PositionedTimeTick, ValueDomain, axis_label_y,
        paired_bar_bounds, point_x, position_time_ticks, ratio_bar_width, stacked_bar_bounds,
        time_tick_indices, tooltip_bounds, value_y,
    },
    canvas::ChartState,
    palette::{
        AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, HOVER_MARKER_SIZE, LEGEND_ENTRY_GAP,
        LEGEND_OVERFLOW_INSET, LEGEND_SWATCH_GAP, LINE_MARKER_SIZE, LINE_WIDTH, Series,
        SeriesLabel, TOOLTIP_GAP, TooltipStyle, tooltip_style,
    },
    text::ChartTextCache,
};
use crate::model::{
    LongShortRatioPoint, OpenInterestPoint, TakerVolumePoint, quantity_as_f64, ratio_as_f64,
    ratio_shares,
};

/// The most series one chart draws, and so the most legend and tooltip entries
/// it can have.
pub(super) const MAX_SERIES: usize = 3;

/// The roles the two lines of a line chart are prepared and drawn in, in that
/// order.
///
/// The roles are named once here because two passes need them: the one that
/// strokes the lines and the one that marks a hover. Naming them by index in both
/// places would leave a line and the marker on it free to be read as two different
/// series.
const LINE_SERIES: [Series; 2] = [Series::Amount, Series::Value];

/// One point under the pointer, and where the pointer was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub index: usize,
    pub cursor: Point<Pixels>,
}

/// One marker a hover draws on the plot: the series it marks, and the box it is
/// drawn in.
///
/// The series travels with the box because a marker belongs to the line it sits
/// on rather than to the hover: it is that line's reading of the hovered interval,
/// so it is drawn in the colour the line and its legend entry are drawn in, and
/// not in the colour of the rule that crosses it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HoverMarker {
    pub series: Series,
    pub bounds: Bounds<Pixels>,
}

/// One line of a tooltip's body: what it says, and the role it is read in.
///
/// The role is what ties a tooltip line to the legend's swatch for the same
/// series — naming it is what keeps the two one colour. A chart whose tooltip is
/// not a legend but a reading leaves it `None`, and every line of that tooltip is
/// then drawn in the tooltip's own foreground, the colour its title is already in.
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
#[derive(Default)]
pub(crate) struct LineGeometry {
    path: Option<Path<Pixels>>,
    markers: Box<[Bounds<Pixels>]>,
    /// Whether the markers are far enough apart to be drawn as one layer.
    markers_are_disjoint: bool,
}

/// The boxes and paths one frame draws, prepared from its layout.
#[derive(Default)]
pub(crate) enum Prepared {
    /// Nothing has been prepared yet, which is the state of a chart that has never
    /// been laid out.
    #[default]
    Nothing,
    Lines {
        series: [LineGeometry; 2],
        ticks: Box<[PositionedTimeTick]>,
    },
    Bars {
        bars: Box<[(Bounds<Pixels>, Bounds<Pixels>)]>,
        bars_are_disjoint: bool,
        ticks: Box<[PositionedTimeTick]>,
    },
    Shares {
        bars: Box<[(Bounds<Pixels>, Bounds<Pixels>)]>,
        paths: Box<[Path<Pixels>]>,
        markers: Box<[Bounds<Pixels>]>,
        markers_are_disjoint: bool,
        ticks: Box<[PositionedTimeTick]>,
    },
}

/// The measured width of every legend entry.
#[derive(Clone, Copy, Default)]
pub(crate) struct LegendMeasurements {
    label_widths: [Pixels; MAX_SERIES],
    entry_widths: [Pixels; MAX_SERIES],
    content_width: Pixels,
}

/// What the axes and the legend need before a layout can be built.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct Measurements {
    pub left_axis_width: Option<Pixels>,
    pub right_axis_width: Pixels,
    pub legend: LegendMeasurements,
}

impl PartialEq for LegendMeasurements {
    fn eq(&self, other: &Self) -> bool {
        self.content_width == other.content_width
    }
}

impl std::fmt::Debug for LegendMeasurements {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LegendMeasurements")
            .field("content_width", &self.content_width)
            .finish_non_exhaustive()
    }
}

/// Everything one frame of a chart draws.
pub(crate) struct Frame<'a> {
    pub layout: ChartLayout,
    /// The y-axes the chart draws: `(left, right, share)`.
    ///
    /// The right one is not optional — every chart is read against a scale on its
    /// right — while a chart with a single series leaves the left empty.
    pub scales: (Option<&'a AxisScale>, &'a AxisScale, Option<&'a AxisScale>),
    /// The titles of the axes: `(left, right)`.
    pub axis_titles: (&'a Option<SharedString>, &'a SharedString),
    pub legend: &'a [SeriesLabel],
    pub legend_measurements: LegendMeasurements,
}

/// The two things a paint pass needs that only a frame can hand over.
///
/// Text is shaped through the cache rather than through the window, and the window
/// cannot be borrowed while a shaped line is being told about its own
/// accessibility, so both travel together.
pub(crate) struct PaintContext<'a> {
    pub text_cache: &'a mut ChartTextCache,
    pub window: &'a mut Window,
    pub app: &'a mut App,
}

impl PaintContext<'_> {
    /// The active style, which is where every colour a frame paints comes from.
    ///
    /// The application is what owns it, so a colour is taken out of it as an owned
    /// [`Hsla`] before the context is borrowed again to paint with.
    fn theme(&self) -> &Theme {
        self.app.theme()
    }

    /// Draws one line of text, shaped through the cache.
    fn text(
        &mut self,
        text: impl Into<SharedString>,
        origin: Point<Pixels>,
        color: Hsla,
        align: TextAlign,
        width: Pixels,
        font_size: Pixels,
        line_height: Pixels,
    ) {
        if width <= Pixels::ZERO {
            return;
        }
        let window = &*self.window;
        let line = self.text_cache.line(text.into(), font_size, color, window);
        let _ = line.paint(
            origin,
            line_height,
            align,
            Some(width),
            self.window,
            self.app,
        );
    }

    /// The width one axis-sized label will draw at.
    fn axis_text_width(&mut self, text: &str, color: Hsla) -> Pixels {
        let window = &*self.window;
        self.text_cache
            .width(text, px(AXIS_FONT_SIZE), color, window)
    }

    /// The width one tooltip line will draw at.
    fn tooltip_text_width(&mut self, text: &str, color: Hsla, font_size: Pixels) -> Pixels {
        let window = &*self.window;
        self.text_cache.width(text, font_size, color, window)
    }
}

/// The part of a chart's own state that a prepared frame depends on beyond its
/// box: the window it is showing, and whether it is labelled at the instrument's
/// ticks or in a compact form.
///
/// The revision alone would do for the data, but not for the labels: a window
/// whose points are the same is still drawn differently once the instrument
/// precision it was waiting for arrives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameKey {
    /// The box the element was given, which every pixel the frame computes is
    /// relative to.
    pub bounds: Bounds<Pixels>,
    revision: u64,
    precise: bool,
}

impl FrameKey {
    pub(super) const fn new(bounds: Bounds<Pixels>, revision: u64, precise: bool) -> Self {
        Self {
            bounds,
            revision,
            precise,
        }
    }
}

/// The inputs one prepared frame was built from.
///
/// Everything that *preparing* a frame reads has to be in here, or a change to it
/// would keep serving the frame from before the change. A colour is not one of
/// them: nothing the preparation keeps — the measured labels, the boxes, the paths
/// — is coloured, and every colour a frame paints with is read from the active
/// style as it draws.
#[derive(PartialEq)]
pub(crate) struct CacheKey {
    pub bounds: Bounds<Pixels>,
    pub locale: LanguageCode,
    pub frame: FrameKey,
}

/// Measures the legend and the axis labels, which is what the layout is built
/// from.
pub(super) fn measure(
    axis_titles: (&Option<SharedString>, &SharedString),
    legend: &[SeriesLabel],
    scales: (Option<&AxisScale>, &AxisScale, Option<&AxisScale>),
    context: &mut PaintContext<'_>,
) -> Measurements {
    let (axis, legend_text) = {
        let theme = context.theme();
        (theme.components.chart.axis.hsla, theme.roles.fg.muted.hsla)
    };
    let left_axis_width = scales
        .0
        .map(|scale| widest_label_width(scale, axis_titles.0.as_deref(), axis, context));
    let right_axis_width =
        widest_label_width(scales.1, Some(axis_titles.1.as_ref()), axis, context);
    Measurements {
        legend: measure_legend(legend, legend_text, context),
        left_axis_width,
        right_axis_width,
    }
}

/// The widest label of one axis, counting its title.
fn widest_label_width(
    scale: &AxisScale,
    title: Option<&str>,
    color: Hsla,
    context: &mut PaintContext<'_>,
) -> Pixels {
    scale
        .ticks
        .iter()
        .map(|tick| tick.label.as_str())
        .chain(title.filter(|title| !title.is_empty()))
        .map(|label| context.axis_text_width(label, color))
        .fold(Pixels::ZERO, Pixels::max)
}

/// Measures every legend entry, which is what decides whether the legend fits.
fn measure_legend(
    legend: &[SeriesLabel],
    text_color: Hsla,
    context: &mut PaintContext<'_>,
) -> LegendMeasurements {
    let mut measurements = LegendMeasurements::default();
    let count = legend.len().min(MAX_SERIES);
    for (index, entry) in legend.iter().take(count).enumerate() {
        let label_width = context.axis_text_width(&entry.label, text_color);
        measurements.label_widths[index] = label_width;
        measurements.entry_widths[index] = label_width + px(LEGEND_SWATCH_GAP);
    }
    measurements.content_width = measurements.entry_widths[..count]
        .iter()
        .copied()
        .sum::<Pixels>()
        + px(LEGEND_ENTRY_GAP) * count.saturating_sub(1) as f32;
    measurements
}

/// The content box one chart needs, which is the box its pane scrolls to.
///
/// It is a function of the labels and nothing else: the widest label either axis
/// can be asked to show, and the legend those labels sit beside. The box the last
/// frame was laid out in is deliberately not part of it — a minimum that took the
/// pane's own size into it could only ever grow, and a pane that shrank would keep
/// drawing at the width it had when it was last enlarged.
pub(super) fn content_min_size(measurements: Measurements) -> gpui_kit::Size<Pixels> {
    let left = measurements.left_axis_width.unwrap_or(Pixels::ZERO);
    gpui_kit::size(
        (MINIMUM_PLOT_WIDTH + left + measurements.right_axis_width)
            .max(measurements.legend.content_width + px(LEGEND_OVERFLOW_INSET * 2.0))
            .max(CHART_MIN_CONTENT_WIDTH),
        CHART_MIN_CONTENT_HEIGHT,
    )
}

/// Folds the window of an open-interest chart into boxes and paths.
pub(super) fn prepare_open_interest(
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
pub(super) fn prepare_taker_volume(
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
        bars_are_disjoint: paired_bars_are_disjoint(&bars),
        bars: bars.into_boxed_slice(),
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

/// Folds the window of a ratio chart into boxes and paths.
pub(super) fn prepare_ratio(
    points: &[LongShortRatioPoint],
    ratio: &AxisScale,
    time_labels: &[SmolStr],
    layout: ChartLayout,
) -> Prepared {
    let point_count = points.len();
    let width = ratio_bar_width(layout.plot, point_count);
    let mut bars = Vec::with_capacity(point_count);
    let mut markers = Vec::with_capacity(point_count);
    for (index, bar) in points.iter().enumerate() {
        let Some((long_share, short_share)) = ratio_shares(bar.ratio) else {
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
        markers.push(marker_bounds(
            at(
                center_x,
                value_y(ratio_as_f64(bar.ratio), ratio.domain, layout.plot),
            ),
            LINE_MARKER_SIZE,
        ));
    }
    Prepared::Shares {
        markers_are_disjoint: markers_are_disjoint(&markers),
        paths: ratio_paths(points, ratio.domain, layout.plot),
        bars: bars.into_boxed_slice(),
        markers: markers.into_boxed_slice(),
        ticks: time_ticks(time_labels, point_count, layout),
    }
}

/// The time labels of a window, written once.
pub(super) fn time_labels(
    timestamps: impl Iterator<Item = i64>,
    label: impl Fn(i64) -> SmolStr,
) -> Box<[SmolStr]> {
    timestamps.map(label).collect()
}

/// The content mask of one plot, which is what keeps a line inside its own chart.
pub(super) fn plot_mask(layout: ChartLayout) -> gpui_kit::ContentMask<Pixels> {
    gpui_kit::ContentMask {
        bounds: layout.plot,
    }
}

/// Draws the frame a chart prepared last.
///
/// The state arrives as a whole rather than as a borrowed [`Frame`] because a
/// frame needs both halves of it at once — the prepared boxes, which are read, and
/// the shaped-text cache, which is written through — and they live in the same
/// `RefCell`. Reading them here, in one place, is what keeps a frame from having to
/// clone either.
pub(super) fn paint(
    layout: ChartLayout,
    scales: (Option<&AxisScale>, &AxisScale, Option<&AxisScale>),
    axis_titles: (&Option<SharedString>, &SharedString),
    legend: &[SeriesLabel],
    hover: Option<Hover>,
    tooltip: Option<&Tooltip>,
    state: &ChartState,
    window: &mut Window,
    app: &mut App,
) {
    let legend_measurements = state.cache.borrow().measurements.legend;
    let frame = Frame {
        layout,
        scales,
        axis_titles,
        legend,
        legend_measurements,
    };
    let frame = &frame;
    let prepared = std::cell::Ref::map(state.cache.borrow(), |cache| &cache.prepared);
    let mut context = PaintContext {
        text_cache: &mut state.text.borrow_mut(),
        window,
        app,
    };
    let context = &mut context;
    paint_frame(frame, &prepared, hover, tooltip, context);
}

/// Draws one prepared frame, with every borrow already resolved.
fn paint_frame(
    frame: &Frame<'_>,
    prepared: &Prepared,
    hover: Option<Hover>,
    tooltip: Option<&Tooltip>,
    context: &mut PaintContext<'_>,
) {
    let layout = frame.layout;
    paint_plot_rules(layout, context);
    paint_value_grid(frame, context);
    paint_axes(frame, context);
    match prepared {
        Prepared::Nothing => {}
        Prepared::Lines { ticks, .. }
        | Prepared::Bars { ticks, .. }
        | Prepared::Shares { ticks, .. } => paint_time_axis(ticks, layout, context),
    }
    paint_legend(frame, context);
    // The chart's colours are taken as owned values before the window is borrowed
    // again: the series a frame draws are prepared as geometry, so their colours
    // are resolved here, on the pass that paints them.
    let colors = context.theme().components.chart;
    let content = context.theme().roles.fg.default.hsla;
    let crosshair = context.theme().components.chart.crosshair.hsla;
    context
        .window
        .with_content_mask(Some(plot_mask(layout)), |window| {
            paint_series(frame, prepared, colors, content, window);
            if let Some(hover) = hover {
                let x = point_x(layout.plot, point_count(prepared), hover.index);
                paint_dashed_vertical(x, layout.plot, crosshair, window);
            }
        });
    if let (Some(hover), Some(tooltip)) = (hover, tooltip) {
        paint_tooltip(hover, tooltip, prepared, frame, context);
    }
}

/// How many points a prepared frame was built from.
fn point_count(prepared: &Prepared) -> usize {
    match prepared {
        Prepared::Nothing => 0,
        Prepared::Lines { series, .. } => series[0].markers.len(),
        Prepared::Bars { bars, .. } => bars.len(),
        Prepared::Shares { markers, .. } => markers.len(),
    }
}

fn paint_series(
    frame: &Frame<'_>,
    prepared: &Prepared,
    colors: ChartColors,
    content: Hsla,
    window: &mut Window,
) {
    match prepared {
        Prepared::Nothing => {}
        Prepared::Lines { series, .. } => {
            for (geometry, series) in series.iter().zip(LINE_SERIES) {
                paint_line(geometry, series.color(&colors, content), frame, window);
            }
        }
        Prepared::Bars {
            bars,
            bars_are_disjoint,
            ..
        } => paint_paired_bars(
            bars,
            *bars_are_disjoint,
            frame,
            Series::Long.color(&colors, content),
            Series::Short.color(&colors, content),
            window,
        ),
        Prepared::Shares {
            bars,
            paths,
            markers,
            markers_are_disjoint,
            ..
        } => {
            // Each side of a share bar is drawn in its own role colour, and the
            // ratio over it in the role the legend and the tooltip name it by — so
            // the swatch beside the name and the line under it are one colour.
            paint_shares(
                bars,
                Series::Long.color(&colors, content),
                Series::Short.color(&colors, content),
                window,
            );
            let ratio = Series::Ratio.color(&colors, content);
            paint_paths(paths, ratio, frame.layout.plot, window);
            paint_markers(
                markers,
                *markers_are_disjoint,
                ratio,
                frame.layout.plot,
                window,
            );
        }
    }
}

/// Draws the square that says which point a tooltip belongs to, and the tooltip
/// itself.
///
/// One marker per series, all at the hovered interval: a chart of two lines shows
/// two readings of the same moment, and marking only the one nearest the pointer
/// would leave the other reading unmarked on the very frame the tooltip names it.
fn paint_tooltip(
    hover: Hover,
    tooltip: &Tooltip,
    prepared: &Prepared,
    frame: &Frame<'_>,
    context: &mut PaintContext<'_>,
) {
    let layout = frame.layout;
    let style = tooltip_style(context.window);
    // A tooltip is the window's own surface rather than a colour of the chart's,
    // so it is drawn in the two colours the framework draws its own tooltip in.
    // The marker squares are the part of it that sits on the plot, and each sits on
    // the line it marks — so each is drawn in that line's own colour, the one its
    // legend entry is drawn in, and the crosshair through it stays the chart's
    // content colour. An entry that names a series is drawn in that series' colour,
    // so a tooltip line and the legend's swatch for it stay one colour. An entry
    // that names none reads in the tooltip's own foreground, like the title above
    // it.
    let content = context.theme().roles.fg.default.hsla;
    let tooltip_colors = context.theme().components.tooltip;
    let foreground = tooltip_colors.foreground.hsla;
    let surface = tooltip_colors.background.hsla;
    let colors = context.theme().components.chart;
    for marker in hover_markers(prepared, hover, frame) {
        context
            .window
            .paint_quad(fill(marker.bounds, marker.series.color(&colors, content)));
    }
    let content_width = std::iter::once(tooltip.title.as_str())
        .chain(tooltip.entries.iter().map(|entry| entry.text.as_str()))
        .map(|line| context.tooltip_text_width(line, foreground, style.font_size))
        .fold(Pixels::ZERO, Pixels::max);
    let bounds = tooltip_bounds(
        at(
            point_x(layout.plot, point_count(prepared), hover.index),
            hover.cursor.y,
        ),
        style.content_size(content_width, 1 + tooltip.entries.len()),
        layout.plot,
        px(TOOLTIP_GAP),
    );
    paint_tooltip_surface(bounds, style, surface, context.window);
    let text_width = (bounds.size.width - style.padding * 2.0).max(Pixels::ZERO);
    context.text(
        tooltip.title.to_string(),
        bounds.origin + at(style.padding, style.padding),
        foreground,
        TextAlign::Left,
        text_width,
        style.font_size,
        style.line_height,
    );
    for (index, entry) in tooltip.entries.iter().enumerate() {
        let color = entry
            .color
            .map_or(foreground, |series| series.color(&colors, foreground));
        context.text(
            entry.text.to_string(),
            bounds.origin
                + at(
                    style.padding,
                    style.padding + style.line_height * (index + 1) as f32,
                ),
            color,
            TextAlign::Left,
            text_width,
            style.font_size,
            style.line_height,
        );
    }
}

/// The marker of every series under the pointer, each naming the line it marks.
///
/// Every series is marked at the same x, because they are the readings of one
/// interval; each is marked at its own y, because each is read against its own
/// axis. A chart whose series are one shape — a stacked bar, a single line — has
/// one marker, which is the same answer with one element in it.
///
/// A paired-bar chart is marked by none: the top of a bar is both its reading and
/// the edge of the bar the pointer is over, so a square there would cover the very
/// bar it was meant to point at, and a hover already says which interval it is on
/// with the rule drawn through it.
pub(super) fn hover_markers(
    prepared: &Prepared,
    hover: Hover,
    frame: &Frame<'_>,
) -> Vec<HoverMarker> {
    let count = point_count(prepared);
    let x = point_x(frame.layout.plot, count, hover.index);
    let centers: Vec<(Series, Pixels)> = match prepared {
        Prepared::Nothing | Prepared::Bars { .. } => Vec::new(),
        Prepared::Lines { series, .. } => series
            .iter()
            .zip(LINE_SERIES)
            .filter_map(|(geometry, series)| {
                geometry
                    .markers
                    .get(hover.index)
                    .map(|marker| (series, marker.center().y))
            })
            .collect(),
        Prepared::Shares { markers, .. } => markers
            .get(hover.index)
            .map(|marker| vec![(Series::Ratio, marker.center().y)])
            .unwrap_or_default(),
    };
    centers
        .into_iter()
        .map(|(series, y)| HoverMarker {
            series,
            bounds: marker_bounds(at(x, y), HOVER_MARKER_SIZE),
        })
        .collect()
}

/// Draws the two rules that bound the plot from above and below.
///
/// There is no background: the pane body behind the canvas is the background, so
/// the chart never paints over the body's rounded corners. A fill here would also
/// be invisible whenever it matched the body, and visibly wrong whenever it did
/// not — a chart must not paint a colour the window already owns.
fn paint_plot_rules(layout: ChartLayout, context: &mut PaintContext<'_>) {
    let top = Bounds::new(layout.plot.origin, size(layout.plot.size.width, px(1.0)));
    let bottom = Bounds::new(
        at(layout.plot.origin.x, layout.plot.bottom() - px(1.0)),
        size(layout.plot.size.width, px(1.0)),
    );
    let grid = context.theme().components.chart.grid.hsla;
    context.window.paint_quad(fill(top, grid));
    context.window.paint_quad(fill(bottom, grid));
}

fn paint_axes(frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let layout = frame.layout;
    let muted = context.theme().components.chart.axis.hsla;
    let (left_title, right_title) = frame.axis_titles;
    if let Some(title) = left_title.as_ref().filter(|title| !title.is_empty()) {
        context.text(
            title.to_string(),
            at(layout.left_axis.origin.x, layout.content.origin.y + px(2.0)),
            muted,
            TextAlign::Right,
            (layout.left_axis.size.width - px(AXIS_TICK_TEXT_INSET)).max(Pixels::ZERO),
            px(AXIS_FONT_SIZE),
            px(AXIS_LINE_HEIGHT),
        );
    }
    if !right_title.is_empty() {
        context.text(
            right_title.to_string(),
            at(
                layout.right_axis.origin.x + px(AXIS_TICK_TEXT_INSET),
                layout.content.origin.y + px(2.0),
            ),
            muted,
            TextAlign::Left,
            (layout.right_axis.size.width - px(AXIS_TICK_TEXT_INSET)).max(Pixels::ZERO),
            px(AXIS_FONT_SIZE),
            px(AXIS_LINE_HEIGHT),
        );
    }
    paint_value_axis(frame.scales.0, AxisSide::Left, frame, context);
    paint_value_axis(Some(frame.scales.1), AxisSide::Right, frame, context);
}

/// Which side of the plot a y-axis is on, which is what says where its labels go.
#[derive(Clone, Copy)]
enum AxisSide {
    Left,
    Right,
}

/// Draws one y-axis: a label per tick, beside the line that tick draws.
///
/// The two axes of a chart share the plot's rows but not their values, so each is
/// labelled from its own scale. A chart with only a right axis — a ratio, whose
/// shares are whole by definition — passes `None` on the left and gets no labels
/// there.
fn paint_value_axis(
    scale: Option<&AxisScale>,
    side: AxisSide,
    frame: &Frame<'_>,
    context: &mut PaintContext<'_>,
) {
    let Some(scale) = scale else {
        return;
    };
    let layout = frame.layout;
    let axis = match side {
        AxisSide::Left => layout.left_axis,
        AxisSide::Right => layout.right_axis,
    };
    let (align, origin_x) = match side {
        AxisSide::Left => (TextAlign::Right, axis.origin.x),
        AxisSide::Right => (TextAlign::Left, axis.origin.x + px(AXIS_TICK_TEXT_INSET)),
    };
    let muted = context.theme().components.chart.axis.hsla;
    for tick in &scale.ticks {
        let y = axis_label_y(value_y(tick.value, scale.domain, layout.plot), axis);
        context.text(
            tick.label.to_string(),
            at(origin_x, y),
            muted,
            align,
            (axis.size.width - px(AXIS_TICK_TEXT_INSET)).max(Pixels::ZERO),
            px(AXIS_FONT_SIZE),
            px(AXIS_LINE_HEIGHT),
        );
    }
}

/// Draws the horizontal grid line of every tick of the axis the plot is read
/// against.
///
/// One axis drives the grid, not two: two sets of lines a few pixels apart would
/// read as neither. The left axis is the one named first, so it is the one that
/// draws them — and a chart with only a right axis draws its grid from that.
fn paint_value_grid(frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let layout = frame.layout;
    let scale = frame.scales.0.unwrap_or(frame.scales.1);
    let grid = context.theme().components.chart.grid.hsla;
    context.window.paint_layer(layout.plot, |window| {
        for tick in &scale.ticks {
            let y = value_y(tick.value, scale.domain, layout.plot);
            window.paint_quad(fill(
                Bounds::new(
                    at(layout.plot.origin.x, y),
                    size(layout.plot.size.width, px(1.0)),
                ),
                grid,
            ));
        }
    });
}

fn paint_time_axis(
    ticks: &[PositionedTimeTick],
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let muted = context.theme().components.chart.axis.hsla;
    for tick in ticks {
        context.text(
            tick.label.to_string(),
            at(tick.origin_x, layout.time_axis.origin.y + px(4.0)),
            muted,
            TextAlign::Center,
            tick.width,
            px(AXIS_FONT_SIZE),
            px(AXIS_LINE_HEIGHT),
        );
    }
}

fn paint_legend(frame: &Frame<'_>, context: &mut PaintContext<'_>) {
    let layout = frame.layout;
    let measurements = frame.legend_measurements;
    let (colors, content, text_color) = {
        let theme = context.theme();
        (
            theme.components.chart,
            theme.roles.fg.default.hsla,
            theme.roles.fg.muted.hsla,
        )
    };
    let mut x = centered_legend_origin(layout, measurements.content_width);
    let y = layout.legend.origin.y + px(8.0);
    for (index, entry) in frame.legend.iter().enumerate().take(MAX_SERIES) {
        let swatch = Bounds::new(at(x + px(3.0), y + px(2.0)), size(px(8.0), px(8.0)));
        context
            .window
            .paint_quad(fill(swatch, entry.series.color(&colors, content)));
        context.text(
            entry.label.to_string(),
            at(x + px(LEGEND_SWATCH_GAP), y),
            text_color,
            TextAlign::Left,
            measurements.label_widths[index],
            px(AXIS_FONT_SIZE),
            px(AXIS_LINE_HEIGHT),
        );
        x += measurements.entry_widths[index] + px(LEGEND_ENTRY_GAP);
    }
}

fn centered_legend_origin(layout: ChartLayout, content_width: Pixels) -> Pixels {
    let legend = layout.legend;
    if content_width <= legend.size.width {
        legend.origin.x + (legend.size.width - content_width) / 2.0
    } else {
        legend.origin.x + px(LEGEND_OVERFLOW_INSET)
    }
}

fn paint_line(geometry: &LineGeometry, color: Hsla, frame: &Frame<'_>, window: &mut Window) {
    if let Some(path) = geometry.path.clone() {
        window.paint_path(path, color);
    }
    paint_markers(
        &geometry.markers,
        geometry.markers_are_disjoint,
        color,
        frame.layout.plot,
        window,
    );
}

fn paint_markers(
    markers: &[Bounds<Pixels>],
    disjoint: bool,
    color: Hsla,
    plot: Bounds<Pixels>,
    window: &mut Window,
) {
    let paint = |window: &mut Window| {
        for &marker in markers {
            window.paint_quad(fill(marker, color));
        }
    };
    if disjoint {
        window.paint_layer(plot, paint);
    } else {
        paint(window);
    }
}

fn paint_paths(paths: &[Path<Pixels>], color: Hsla, plot: Bounds<Pixels>, window: &mut Window) {
    window.paint_layer(plot, |window| {
        for path in paths.iter().cloned() {
            window.paint_path(path, color);
        }
    });
}

/// Draws one stacked ratio bar per interval: the short side from the ceiling and
/// the long side from the floor, each in its own role colour.
fn paint_shares(
    bars: &[(Bounds<Pixels>, Bounds<Pixels>)],
    long_color: Hsla,
    short_color: Hsla,
    window: &mut Window,
) {
    for (long, short) in bars {
        window.paint_quad(fill(*short, short_color));
        window.paint_quad(fill(*long, long_color));
    }
}

fn paint_paired_bars(
    bars: &[(Bounds<Pixels>, Bounds<Pixels>)],
    disjoint: bool,
    frame: &Frame<'_>,
    buy_color: Hsla,
    sell_color: Hsla,
    window: &mut Window,
) {
    let paint = |window: &mut Window| {
        for (buy, sell) in bars {
            window.paint_quad(fill(*buy, buy_color));
            window.paint_quad(fill(*sell, sell_color));
        }
    };
    if disjoint {
        window.paint_layer(frame.layout.plot, paint);
    } else {
        paint(window);
    }
}

/// Builds the path and markers of one line series.
fn line_geometry(
    values: impl Iterator<Item = f64>,
    point_count: usize,
    domain: ValueDomain,
    plot: Bounds<Pixels>,
) -> LineGeometry {
    let mut markers = Vec::with_capacity(point_count);
    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
    let mut has_path = false;
    for (index, value) in values.enumerate() {
        if !value.is_finite() {
            // A point that is not a reading breaks the line, so the segment
            // across the gap is not drawn as if the gap were data.
            has_path = false;
            continue;
        }
        let position = at(
            point_x(plot, point_count, index),
            value_y(value, domain, plot),
        );
        markers.push(marker_bounds(position, LINE_MARKER_SIZE));
        if has_path {
            builder.line_to(position);
        } else {
            builder.move_to(position);
            has_path = true;
        }
    }
    LineGeometry {
        path: (point_count > 1).then(|| builder.build().ok()).flatten(),
        markers_are_disjoint: markers_are_disjoint(&markers),
        markers: markers.into_boxed_slice(),
    }
}

/// The paths of the ratio line, one per run of valid points.
fn ratio_paths(
    points: &[LongShortRatioPoint],
    domain: ValueDomain,
    plot: Bounds<Pixels>,
) -> Box<[Path<Pixels>]> {
    let mut paths = Vec::with_capacity(points.len() / 2);
    let mut builder: Option<PathBuilder> = None;
    let mut run_len = 0;
    for (index, bar) in points.iter().enumerate() {
        if ratio_shares(bar.ratio).is_none() {
            push_path(&mut builder, &mut run_len, &mut paths);
            continue;
        }
        let position = at(
            point_x(plot, points.len(), index),
            value_y(ratio_as_f64(bar.ratio), domain, plot),
        );
        if let Some(builder) = builder.as_mut() {
            builder.line_to(position);
        } else {
            let mut path = PathBuilder::stroke(px(LINE_WIDTH));
            path.move_to(position);
            builder = Some(path);
        }
        run_len += 1;
    }
    push_path(&mut builder, &mut run_len, &mut paths);
    paths.into_boxed_slice()
}

/// Finishes one run of the ratio line, if it is long enough to be a line.
fn push_path(
    builder: &mut Option<PathBuilder>,
    run_len: &mut usize,
    paths: &mut Vec<Path<Pixels>>,
) {
    let Some(builder) = builder.take() else {
        return;
    };
    if *run_len >= 2
        && let Ok(path) = builder.build()
    {
        paths.push(path);
    }
    *run_len = 0;
}

fn marker_bounds(center: Point<Pixels>, side: f32) -> Bounds<Pixels> {
    let radius = px(side / 2.0);
    Bounds::new(
        center - at(radius, radius),
        size(radius * 2.0, radius * 2.0),
    )
}

fn markers_are_disjoint(markers: &[Bounds<Pixels>]) -> bool {
    horizontal_bounds_are_disjoint(
        markers
            .iter()
            .map(|bounds| (bounds.origin.x, bounds.right())),
    )
}

fn paired_bars_are_disjoint(bars: &[(Bounds<Pixels>, Bounds<Pixels>)]) -> bool {
    let mut previous_right = None;
    for (buy, sell) in bars {
        if previous_right.is_some_and(|right: Pixels| buy.origin.x < right) {
            return false;
        }
        previous_right = Some(sell.right());
    }
    true
}

fn horizontal_bounds_are_disjoint(bounds: impl Iterator<Item = (Pixels, Pixels)>) -> bool {
    let mut previous_right: Option<Pixels> = None;
    for (left, right) in bounds {
        if previous_right.is_some_and(|previous| left < previous) {
            return false;
        }
        previous_right = Some(previous_right.map_or(right, |previous: Pixels| previous.max(right)));
    }
    true
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

fn paint_dashed_vertical(x: Pixels, plot: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    // Dash segments are disjoint, so one layer replaces one ordering-tree
    // insertion per segment.
    window.paint_layer(plot, |window| {
        let mut y = plot.origin.y;
        while y < plot.bottom() {
            let height = px(super::palette::CROSSHAIR_DASH).min(plot.bottom() - y);
            window.paint_quad(fill(Bounds::new(at(x, y), size(px(1.0), height)), color));
            y += px(super::palette::CROSSHAIR_PERIOD);
        }
    });
}

fn paint_tooltip_surface(
    bounds: Bounds<Pixels>,
    style: TooltipStyle,
    background: Hsla,
    window: &mut Window,
) {
    let corner_radii = Corners::all(style.radius).clamp_radii_for_quad_size(bounds.size);
    window.paint_quad(fill(bounds, background).corner_radii(corner_radii));
}
