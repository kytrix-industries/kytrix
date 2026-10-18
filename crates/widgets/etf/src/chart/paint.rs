//! Painting one ETF chart.
//!
//! One frame is two passes, and the split is what makes a resize and a hover cheap:
//!
//! - [`prepare`] folds the window into the pixel boxes and paths a frame draws,
//!   using the box the element was given. It runs only when that box changed or a
//!   new snapshot arrived.
//! - [`paint`] turns those boxes into quads and those paths into strokes, plus the
//!   crosshair and the tooltip, which are the only parts that follow the pointer.
//!
//! Everything a label *says* was written when the snapshot arrived; this module
//! places text, it never formats it.

use base_ui::style::theme::{Theme, ThemeExt};
use gpui_kit::{
    App, Bounds, ContentMask, Corners, Hsla, Path, PathBuilder, Pixels, Point, SharedString,
    TextAlign, Window, fill, point as at, px, size,
};

use super::{
    axes::{
        AXIS_TICK_TEXT_INSET, AxisScale, ChartLayout, PositionedTimeTick, axis_label_y,
        flow_bar_bounds, marker_bounds, point_x, position_time_ticks, tooltip_bounds, value_y,
    },
    canvas::ChartState,
    data::{PreparedData, PreparedPoint},
    palette::{
        AXIS_FONT_SIZE, AXIS_LINE_HEIGHT, CROSSHAIR_DASH, CROSSHAIR_PERIOD, LINE_WIDTH,
        TOOLTIP_GAP, TooltipStyle, flow, tooltip_style,
    },
    text::ChartTextCache,
};

/// One point under the pointer, and how high the pointer was.
///
/// The height is kept rather than the whole position: the x a hover resolves to is
/// the reading's own, and the height is rounded to whole pixels before it is stored,
/// so a pointer that moved a fraction of a pixel is not a change worth a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub index: usize,
    pub cursor_y: Pixels,
}

/// What the axes need before a layout can be built.
///
/// Nothing is known before the first frame — no text has been shaped, so no label
/// has a width — and that is spelled out rather than guessed at: the layout then
/// falls back to its own minimum axis width for that one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Measurements {
    pub left_axis_width: Pixels,
    /// Absent while a single fund is shown, which is what keeps its width for the
    /// plot.
    pub right_axis_width: Option<Pixels>,
}

impl Default for Measurements {
    fn default() -> Self {
        Self {
            left_axis_width: Pixels::ZERO,
            right_axis_width: None,
        }
    }
}

/// The inputs one prepared frame was built from.
///
/// Everything a frame reads has to be in here, or a change to it would keep serving
/// the frame from before the change: the box it was laid out in, and the revision of
/// the resolved window — a new snapshot, another asset, another fund or another
/// window all rebuild the data and move the revision with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CacheKey {
    pub bounds: Bounds<Pixels>,
    pub revision: u64,
}

/// One flow bar, already placed.
#[derive(Clone, Debug)]
pub(crate) struct PreparedBar {
    pub bounds: Bounds<Pixels>,
    pub value: f64,
}

/// The boxes and paths one frame draws, prepared from its layout.
#[derive(Default)]
pub(crate) struct PreparedFrame {
    pub bars: Box<[PreparedBar]>,
    /// Whether the bars are far enough apart to be drawn as one layer.
    pub bars_are_disjoint: bool,
    /// The net-asset line, when the window has more than one day to draw it over.
    pub line: Option<Path<Pixels>>,
    /// The single reading a window of one day has, which is a point rather than a
    /// line.
    pub marker: Option<Point<Pixels>>,
    pub time_ticks: Box<[PositionedTimeTick]>,
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
    /// Draws one line of text, shaped through the cache.
    fn text(
        &mut self,
        text: &str,
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
        let line = self
            .text_cache
            .line(SharedString::new(text), font_size, color, window);
        let _ = line.paint(
            origin,
            line_height,
            align,
            Some(width),
            self.window,
            self.app,
        );
    }

    /// The width one axis-sized label draws at.
    fn axis_text_width(&mut self, text: &str, color: Hsla) -> Pixels {
        let window = &*self.window;
        self.text_cache.width(text, AXIS_FONT_SIZE, color, window)
    }

    /// The width one tooltip line draws at.
    fn tooltip_text_width(&mut self, text: &str, color: Hsla, font_size: Pixels) -> Pixels {
        let window = &*self.window;
        self.text_cache.width(text, font_size, color, window)
    }
}

/// Measures the axis labels, which is what the layout is built from.
///
/// Each axis is measured against everything drawn in it, its title included: a title
/// wider than its axis would be drawn off the edge of the chart and clipped away, so
/// the axis grows instead and the pane scrolls to the content that makes.
pub(crate) fn measure(data: &PreparedData, context: &mut PaintContext<'_>) -> Measurements {
    let muted = context.app.theme().components.chart.axis.hsla;
    Measurements {
        left_axis_width: widest_label_width(
            &data.flow,
            Some(data.left_axis_title.as_ref()),
            muted,
            context,
        ),
        right_axis_width: data.net_assets.as_ref().map(|scale| {
            widest_label_width(scale, Some(data.right_axis_title.as_ref()), muted, context)
        }),
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

/// Folds the resolved window into the boxes and paths one frame draws.
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
                    Some(at(
                        point_x(layout.plot, 1, 0),
                        value_y(point.net_assets, scale.domain, layout.plot),
                    )),
                )
            } else {
                (None, None)
            }
        } else {
            (
                build_line_path(
                    data.points.iter().map(|point| point.net_assets),
                    point_count,
                    scale.domain,
                    layout.plot,
                ),
                None,
            )
        }
    });
    PreparedFrame {
        bars_are_disjoint: bars_are_disjoint(&bars),
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

/// Builds the path of the net-asset line.
fn build_line_path(
    values: impl Iterator<Item = f64>,
    point_count: usize,
    domain: super::axes::ValueDomain,
    plot: Bounds<Pixels>,
) -> Option<Path<Pixels>> {
    if point_count < 2 {
        return None;
    }
    let mut path = PathBuilder::stroke(LINE_WIDTH);
    let mut started = false;
    for (index, value) in values.enumerate() {
        if !value.is_finite() {
            // A day that is not a reading breaks the line, so the segment across
            // the gap is not drawn as if the gap were data.
            started = false;
            continue;
        }
        let position = at(
            point_x(plot, point_count, index),
            value_y(value, domain, plot),
        );
        if started {
            path.line_to(position);
        } else {
            path.move_to(position);
            started = true;
        }
    }
    path.build().ok()
}

/// Whether every bar has a column of its own, which is what lets a frame draw them
/// as one layer instead of one insertion each.
fn bars_are_disjoint(bars: &[PreparedBar]) -> bool {
    let mut previous_right: Option<Pixels> = None;
    for bar in bars {
        if previous_right.is_some_and(|right| bar.bounds.origin.x < right) {
            return false;
        }
        previous_right = Some(previous_right.map_or(bar.bounds.right(), |right: Pixels| {
            right.max(bar.bounds.right())
        }));
    }
    true
}

/// Draws the frame a chart prepared last.
///
/// The state arrives whole rather than as a borrowed frame because a frame needs
/// both halves of it at once — the prepared boxes, which are read, and the shaped
/// text cache, which is written through — and they live in the same `RefCell`.
/// Reading them here, in one place, is what keeps a frame from having to clone
/// either.
pub(crate) fn paint(
    data: &PreparedData,
    layout: ChartLayout,
    hover: Option<Hover>,
    state: &ChartState,
    window: &mut Window,
    app: &mut App,
) {
    let prepared = std::cell::Ref::map(state.cache.borrow(), |cache| &cache.prepared);
    let mut context = PaintContext {
        text_cache: &mut state.text.borrow_mut(),
        window,
        app,
    };
    let context = &mut context;
    paint_plot_rules(
        layout,
        context.app.theme().components.chart.grid.hsla,
        context.window,
    );
    paint_value_grid(
        data,
        layout,
        context.app.theme().components.chart.grid.hsla,
        context.window,
    );
    paint_axes(data, layout, context);
    paint_time_axis(&prepared.time_ticks, layout, context);
    context.window.with_content_mask(
        Some(ContentMask {
            bounds: layout.plot,
        }),
        |window| {
            paint_bars(&prepared, context.app.theme(), layout.plot, window);
            let foreground = context.app.theme().roles.fg.default.hsla;
            if let Some(path) = prepared.line.clone() {
                window.paint_path(path, foreground);
            } else if let Some(center) = prepared.marker {
                window.paint_quad(fill(marker_bounds(center), foreground));
            }
            if let Some(hover) = hover.filter(|hover| hover.index < data.points.len()) {
                paint_crosshair(
                    hover.index,
                    data.points.len(),
                    layout.plot,
                    context.app.theme().components.chart.crosshair.hsla,
                    window,
                );
            }
        },
    );
    if let Some(hover) = hover.filter(|hover| hover.index < data.points.len()) {
        paint_hover(data, hover, layout, context);
    }
}

/// Draws the two rules that bound the plot from above and below.
///
/// There is no background: the pane body behind the canvas is the background, so the
/// chart never paints over the body's rounded corners.
fn paint_plot_rules(layout: ChartLayout, color: Hsla, window: &mut Window) {
    let top = Bounds::new(layout.plot.origin, size(layout.plot.size.width, px(1.0)));
    let bottom = Bounds::new(
        at(layout.plot.origin.x, layout.plot.bottom() - px(1.0)),
        size(layout.plot.size.width, px(1.0)),
    );
    window.paint_quad(fill(top, color));
    window.paint_quad(fill(bottom, color));
}

/// Draws the horizontal grid line of every tick of the flow axis.
///
/// One axis drives the grid, not two: the flow axis is the one the bars are read
/// against, and a second set of lines a few pixels away would read as neither.
fn paint_value_grid(data: &PreparedData, layout: ChartLayout, color: Hsla, window: &mut Window) {
    window.paint_layer(layout.plot, |window| {
        for tick in &data.flow.ticks {
            let y = value_y(tick.value, data.flow.domain, layout.plot);
            window.paint_quad(fill(
                Bounds::new(
                    at(layout.plot.origin.x, y),
                    size(layout.plot.size.width, px(1.0)),
                ),
                color,
            ));
        }
    });
}

/// Draws both y-axes: the title above each, and a label per tick beside it.
///
/// Each title sits over its own axis at the top of the chart, and the title is where
/// the unit comes from: the numbers an axis carries are bare, so what they count and
/// what they are counted in are written once, above the axis they belong to.
fn paint_axes(data: &PreparedData, layout: ChartLayout, context: &mut PaintContext<'_>) {
    let color = context.app.theme().components.chart.axis.hsla;
    let title_y = layout.content.origin.y + px(2.0);
    let inset = px(AXIS_TICK_TEXT_INSET);
    // The flow axis is the one every chart of a flow has, whatever is selected.
    context.text(
        data.left_axis_title.as_ref(),
        at(layout.left_axis.origin.x, title_y),
        color,
        TextAlign::Right,
        (layout.left_axis.size.width - inset).max(Pixels::ZERO),
        AXIS_FONT_SIZE,
        AXIS_LINE_HEIGHT,
    );
    paint_value_axis(&data.flow, AxisSide::Left, layout, context);
    // The net-asset axis is the one only the whole complex has: a single fund has a
    // flow but no assets of its own to report.
    if let Some(scale) = data.net_assets.as_ref() {
        context.text(
            data.right_axis_title.as_ref(),
            at(layout.right_axis.origin.x + inset, title_y),
            color,
            TextAlign::Left,
            (layout.right_axis.size.width - inset).max(Pixels::ZERO),
            AXIS_FONT_SIZE,
            AXIS_LINE_HEIGHT,
        );
        paint_value_axis(scale, AxisSide::Right, layout, context);
    }
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
/// labelled from its own scale: the flow is read against the left axis and the net
/// assets against the right, and a reader who mixed them up would be off by
/// whatever the two happen to differ by.
fn paint_value_axis(
    scale: &AxisScale,
    side: AxisSide,
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let axis = match side {
        AxisSide::Left => layout.left_axis,
        AxisSide::Right => layout.right_axis,
    };
    let (align, origin_x) = match side {
        AxisSide::Left => (TextAlign::Right, axis.origin.x),
        AxisSide::Right => (TextAlign::Left, axis.origin.x + px(AXIS_TICK_TEXT_INSET)),
    };
    let width = (axis.size.width - px(AXIS_TICK_TEXT_INSET)).max(Pixels::ZERO);
    for tick in &scale.ticks {
        let y = axis_label_y(value_y(tick.value, scale.domain, layout.plot), axis);
        context.text(
            tick.label.as_str(),
            at(origin_x, y),
            context.app.theme().components.chart.axis.hsla,
            align,
            width,
            AXIS_FONT_SIZE,
            AXIS_LINE_HEIGHT,
        );
    }
}

/// Draws the date of every labelled position under the plot.
///
/// Every label is already placed inside the time axis by the layout, so nothing
/// here needs a mask: a date that hung over the edge of the chart would be clipped
/// by the pane, and the placement is what keeps that from happening.
fn paint_time_axis(
    ticks: &[PositionedTimeTick],
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    for tick in ticks {
        context.text(
            tick.label.as_str(),
            at(tick.origin_x, layout.time_axis.origin.y + px(4.0)),
            context.app.theme().components.chart.axis.hsla,
            TextAlign::Center,
            tick.width,
            AXIS_FONT_SIZE,
            AXIS_LINE_HEIGHT,
        );
    }
}

/// Draws every bar of the window.
///
/// Bars are a presentation layer: later bars may cover earlier ones when slots are
/// dense. When they do not touch, one layer holds the whole series and the scene
/// builder can batch it; when they do, each is drawn in turn so that the later day
/// is the one on top, which is the order the series is read in.
fn paint_bars(prepared: &PreparedFrame, theme: &Theme, plot: Bounds<Pixels>, window: &mut Window) {
    if prepared.bars.is_empty() {
        return;
    }
    let paint = |window: &mut Window| {
        let muted = theme.roles.market.flat.hsla;
        for bar in &prepared.bars {
            window.paint_quad(fill(bar.bounds, flow(theme, bar.value, muted)));
        }
    };
    if prepared.bars_are_disjoint {
        window.paint_layer(plot, paint);
    } else {
        paint(window);
    }
}

/// Draws the crosshair: a dashed line down the hovered day.
///
/// The line stands where the reading does, and nowhere else: a hover is about the day
/// under the pointer, so a line that also followed the pointer's own height would move
/// with the hand rather than with the data. The pointer's height is still what the
/// tooltip hangs from, which is the one thing about it a reader needs.
fn paint_crosshair(
    index: usize,
    point_count: usize,
    plot: Bounds<Pixels>,
    color: Hsla,
    window: &mut Window,
) {
    let x = point_x(plot, point_count, index);
    // Dash segments are disjoint, so one layer replaces one ordering-tree insertion
    // per segment.
    window.paint_layer(plot, |window| {
        let mut cursor_y = plot.origin.y;
        while cursor_y < plot.bottom() {
            let height = CROSSHAIR_DASH.min(plot.bottom() - cursor_y);
            window.paint_quad(fill(
                Bounds::new(at(x, cursor_y), size(px(1.0), height)),
                color,
            ));
            cursor_y += CROSSHAIR_PERIOD;
        }
    });
}

/// Draws the square at the hovered reading and the tooltip that names it.
///
/// The marker sits on the net-asset line, which is the series the hover follows;
/// the flow of the same day is the bar the crosshair already stands on.
fn paint_hover(
    data: &PreparedData,
    hover: Hover,
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let point = &data.points[hover.index];
    let point_count = data.points.len();
    if let (Some(scale), true) = (data.net_assets.as_ref(), point.net_assets.is_finite()) {
        let center = at(
            point_x(layout.plot, point_count, hover.index),
            value_y(point.net_assets, scale.domain, layout.plot),
        );
        context.window.paint_quad(fill(
            marker_bounds(center),
            context.app.theme().roles.fg.default.hsla,
        ));
    }
    paint_tooltip(
        point,
        data.show_net_assets,
        hover,
        point_count,
        layout,
        context,
    );
}

/// Draws the tooltip of one day: the date, its flow, and what the funds hold.
fn paint_tooltip(
    point: &PreparedPoint,
    show_net_assets: bool,
    hover: Hover,
    point_count: usize,
    layout: ChartLayout,
    context: &mut PaintContext<'_>,
) {
    let style = tooltip_style(context.window);
    let lines = tooltip_lines(point, show_net_assets, context.app.theme());
    let content_width = lines
        .iter()
        .map(|line| {
            line.iter().fold(Pixels::ZERO, |width, (text, color)| {
                width + context.tooltip_text_width(text, *color, style.font_size)
            })
        })
        .fold(Pixels::ZERO, Pixels::max);
    let bounds = tooltip_bounds(
        at(
            point_x(layout.plot, point_count, hover.index),
            hover.cursor_y,
        ),
        style.content_size(content_width, lines.len()),
        layout.plot,
        TOOLTIP_GAP,
    );
    paint_tooltip_surface(bounds, style, context);
    let text_width = (bounds.size.width - style.padding * 2.0).max(Pixels::ZERO);
    for (index, line) in lines.iter().enumerate() {
        let y = bounds.origin.y + style.padding + style.line_height * index as f32;
        let mut cursor_x = bounds.origin.x + style.padding;
        for (text, color) in line.iter() {
            context.text(
                text,
                at(cursor_x, y),
                *color,
                TextAlign::Left,
                text_width,
                style.font_size,
                style.line_height,
            );
            // The next run of the line starts where this one ended, which is the only
            // placement a line drawn in more than one colour can use: the two runs are
            // one reading, not two columns.
            cursor_x += context.tooltip_text_width(text, *color, style.font_size);
        }
    }
}

/// The lines one tooltip shows, and the colour each run of a line is drawn in.
///
/// A tooltip is the window's own surface rather than a colour of the chart's, so
/// it is drawn in the two colours the framework draws its own tooltip in. Only a
/// day's flow is more than one colour: what the day did is read in the tooltip's own
/// text colour, and the amount it moved keeps the colour the bar under it is drawn
/// in.
pub(crate) fn tooltip_lines<'a>(
    point: &'a PreparedPoint,
    show_net_assets: bool,
    theme: &Theme,
) -> Vec<Vec<(&'a str, Hsla)>> {
    let foreground = theme.components.tooltip.foreground.hsla;
    let mut lines = vec![
        vec![(point.header_date.as_ref(), foreground)],
        vec![
            (point.flow_tooltip_caption.as_ref(), foreground),
            (
                point.flow_label.as_ref(),
                flow(theme, point.flow, foreground),
            ),
        ],
    ];
    if show_net_assets {
        lines.push(vec![(point.net_assets_tooltip.as_ref(), foreground)]);
    }
    lines
}

fn paint_tooltip_surface(
    bounds: Bounds<Pixels>,
    style: TooltipStyle,
    context: &mut PaintContext<'_>,
) {
    let corner_radii = Corners::all(style.radius).clamp_radii_for_quad_size(bounds.size);
    context.window.paint_quad(
        fill(
            bounds,
            context.app.theme().components.tooltip.background.hsla,
        )
        .corner_radii(corner_radii),
    );
}
