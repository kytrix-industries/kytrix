//! Where everything on the chart goes.
//!
//! The chart is one price plot and up to [`SUB_PLOT_INLINE`] sub plots under it,
//! each with its own y-axis beside it, plus a time axis below all of them. Every
//! box is derived from the element's own bounds, so a resize needs no state and
//! the same inputs always produce the same layout.
//!
//! The y-axis width is not fixed. It has to fit the widest label any of the axes
//! can produce, and widening the axis takes width away from the plot, which
//! changes which candles are visible, which can change the labels. [`ChartLayout::new`]
//! is called in a loop by the paint side to reach that fixed point instead of
//! guessing.
//!
//! Sub plots share the y-axis width with the price plot. That is what makes
//! [`CandleGeometry`] place the same slot at the same x in every plot: the plot
//! widths are equal, so the same `slot` and `candle_width` produce the same
//! `center_x`.

use gpui_kit::{Bounds, Pixels, Point, point, px, size};
use market_participants_model::market::book::ListingCandlestick;
use smallvec::SmallVec;

/// Height of the time axis.
pub(crate) const TIME_AXIS_HEIGHT: f32 = 26.0;
/// Horizontal padding a label needs inside an axis.
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: f32 = 12.0;
/// Narrowest y-axis, used when the labels are short.
pub(crate) const MIN_AXIS_WIDTH: f32 = 42.0;
/// Narrowest plot, which a long label may not squeeze further.
pub(crate) const MIN_PLOT_WIDTH: f32 = 48.0;
/// Height of the gap above each sub plot.
pub(crate) const PLOT_SEPARATOR_HEIGHT: f32 = 1.0;
/// How many sub plots the layout stores inline.
///
/// It matches the number of sub-plot indicators in the catalogue; the catalogue
/// test pins that, so adding a fifth one without raising this fails a test
/// instead of silently moving the boxes to the heap.
pub(crate) const SUB_PLOT_INLINE: usize = 4;
/// Height the price plot keeps before any sub plot may take space.
pub(crate) const MIN_PLOT_HEIGHT: f32 = 48.0;
/// Tallest a sub plot may become, so one of them cannot take half the pane.
pub(crate) const SUB_PLOT_HEIGHT_CEILING: f32 = 200.0;

/// One sub plot: its plotting area and the y-axis beside it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubPlotLayout {
    pub plot: Bounds<Pixels>,
    pub axis: Bounds<Pixels>,
}

/// Where each part of the chart sits inside the element it was given.
///
/// `separators` has exactly as many entries as `sub_plots`, and the i-th one sits
/// directly above the i-th sub plot. There is no separator between the last sub
/// plot and the time axis: the time axis's top edge is drawn from the layout.
pub(crate) struct ChartLayout {
    pub price_plot: Bounds<Pixels>,
    pub price_axis: Bounds<Pixels>,
    pub separators: SmallVec<[Bounds<Pixels>; SUB_PLOT_INLINE]>,
    pub sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]>,
    pub time_axis: Bounds<Pixels>,
}

/// What the layout needs from outside: the widest axis label and how many sub
/// plots are enabled.
pub(crate) struct LayoutInputs {
    /// The widest label across **all** y-axes, price and sub plots alike.
    pub longest_label_width: Pixels,
    /// The number of enabled sub plots, `0..=SUB_PLOT_INLINE`.
    pub sub_plot_count: usize,
}

impl ChartLayout {
    /// Places the price plot, its sub plots and the axes inside `bounds`.
    ///
    /// The price plot is served first: sub plots only get what is left after
    /// [`MIN_PLOT_HEIGHT`], and when even that is not enough they are left out
    /// entirely — separators included — rather than taking a pixel from the price
    /// plot.
    pub(crate) fn new(bounds: Bounds<Pixels>, inputs: LayoutInputs) -> Self {
        let axis_width = axis_width(bounds.size.width, inputs.longest_label_width);
        let time_height = px(TIME_AXIS_HEIGHT).min(bounds.size.height * 0.24);
        let content_height = (bounds.size.height - time_height).max(px(0.0));
        let plot_width = (bounds.size.width - axis_width).max(px(1.0));

        let separator_height = px(PLOT_SEPARATOR_HEIGHT);
        let sub_seps = inputs.sub_plot_count as f32;
        let sub_room =
            (content_height - separator_height * sub_seps - px(MIN_PLOT_HEIGHT)).max(px(0.0));
        // Each sub plot wishes for 24% of the content, the group for 60% at most,
        // and never more than the price plot's floor leaves over.
        let sub_wish = content_height * (sub_seps * 0.24).min(0.60);
        let sub_budget = sub_wish.min(sub_room);
        let per_sub = if inputs.sub_plot_count == 0 {
            px(0.0)
        } else {
            (sub_budget / inputs.sub_plot_count as f32).min(px(SUB_PLOT_HEIGHT_CEILING))
        };
        // A sub plot that gets no height is not drawn at all: zero-height boxes
        // would make hit testing and paint order depend on how `Bounds::contains`
        // treats an empty rectangle.
        let drawn = per_sub > px(0.0);
        let sub_total = if drawn {
            per_sub * sub_seps + separator_height * sub_seps
        } else {
            px(0.0)
        };
        let price_height = content_height - sub_total;

        let price_plot = Bounds::new(bounds.origin, size(plot_width, price_height));
        let mut separators: SmallVec<[Bounds<Pixels>; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut y = price_plot.bottom();
        if drawn {
            for _ in 0..inputs.sub_plot_count {
                separators.push(Bounds::new(
                    point(bounds.origin.x, y),
                    size(plot_width + axis_width, separator_height),
                ));
                y += separator_height;
                let plot = Bounds::new(point(bounds.origin.x, y), size(plot_width, per_sub));
                let axis = Bounds::new(point(plot.right(), y), size(axis_width, per_sub));
                sub_plots.push(SubPlotLayout { plot, axis });
                y += per_sub;
            }
        }
        let time_axis = Bounds::new(point(bounds.origin.x, y), size(plot_width, time_height));

        Self {
            price_plot,
            price_axis: Bounds::new(
                point(price_plot.right(), price_plot.origin.y),
                size(axis_width, price_plot.size.height),
            ),
            separators,
            sub_plots,
            time_axis,
        }
    }

    /// Whether a pointer position is over data rather than over an axis.
    pub(crate) fn contains_data_point(&self, point: &Point<Pixels>) -> bool {
        self.price_plot.contains(point)
            || self
                .sub_plots
                .iter()
                .any(|sub_plot| sub_plot.plot.contains(point))
    }

    /// The plot width the chart would have if the axis shrank to its minimum.
    ///
    /// Prefetching uses this to ask for history slightly too early rather than
    /// too late: the axis is at its widest when the labels are longest, so this
    /// is the widest the visible range can be.
    pub(crate) fn maximum_plot_width(&self) -> Pixels {
        let chart_width = self.price_plot.size.width + self.price_axis.size.width;
        (chart_width - axis_width(chart_width, px(0.0))).max(px(1.0))
    }

    /// Every plotting area as one box: the price plot and all sub plots, without
    /// the y-axes, the separators or the time axis.
    ///
    /// The vertical grid lines and the crosshair use it so that one line spans
    /// every plot; they must all agree on the same box, which is why this is
    /// derived here rather than assembled by each caller.
    pub(crate) fn data_bounds(&self) -> Bounds<Pixels> {
        self.sub_plots
            .iter()
            .fold(self.price_plot, |bounds, sub_plot| {
                Bounds::new(
                    bounds.origin,
                    size(bounds.size.width, sub_plot.plot.bottom() - bounds.origin.y),
                )
            })
    }
}

fn axis_width(chart_width: Pixels, longest_label_width: Pixels) -> Pixels {
    let desired =
        (longest_label_width.ceil() + px(AXIS_LABEL_HORIZONTAL_PADDING)).max(px(MIN_AXIS_WIDTH));
    let available = (chart_width - px(MIN_PLOT_WIDTH)).max(px(0.0));
    desired.min(available)
}

/// Which candles a viewport is showing, and where each one sits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CandleGeometry {
    first_x: Pixels,
    candle_width: f32,
    visible_len: usize,
}

impl CandleGeometry {
    /// Places `visible_len` candles of `candle_width` against the right edge of
    /// `plot`, which is where the newest candle always belongs.
    pub(crate) fn new(plot: Bounds<Pixels>, visible_len: usize, candle_width: f32) -> Self {
        Self {
            first_x: plot.right() - px(visible_len as f32 * candle_width),
            candle_width,
            visible_len,
        }
    }

    /// The x coordinate at the middle of the candle in `slot`.
    pub(crate) fn center_x(self, slot: usize) -> Pixels {
        self.first_x + px((slot as f32 + 0.5) * self.candle_width)
    }

    /// The width of a candle body, which is narrower than its slot.
    pub(crate) fn body_width(self) -> Pixels {
        px((self.candle_width * 0.68).max(1.0))
    }

    /// The x coordinate at the middle of the candle `offset` slots left of the
    /// first visible one.
    ///
    /// An indicator line draws one candle outside the left edge so the segment
    /// entering the plot has the right height. That slot is not part of
    /// `visible_len`, so [`CandleGeometry::center_x`] cannot express it.
    pub(crate) fn center_x_left_of_first(self, offset: usize) -> Pixels {
        self.first_x + px((0.5 - offset as f32) * self.candle_width)
    }

    /// The visible slot under an x coordinate, clamped to the visible range.
    pub(crate) fn slot_at(self, x: Pixels) -> Option<usize> {
        if self.visible_len == 0 {
            return None;
        }
        Some(
            ((f32::from(x - self.first_x) / self.candle_width).floor() as isize)
                .clamp(0, self.visible_len.saturating_sub(1) as isize) as usize,
        )
    }
}

/// Where the chart is looking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartViewport {
    pub candle_width: f32,
    /// The timestamp of the rightmost visible candle, or `None` to follow the
    /// newest one.
    ///
    /// Following is expressed as absence rather than as "the latest timestamp"
    /// so that it survives a new candle arriving: a chart that stored the newest
    /// timestamp it knew would stop following the moment it saw one.
    pub right_timestamp: Option<i64>,
    /// The pointer position, when it is over the chart.
    pub cursor: Option<Point<Pixels>>,
}

impl Default for ChartViewport {
    fn default() -> Self {
        Self {
            candle_width: 9.0,
            right_timestamp: None,
            cursor: None,
        }
    }
}

/// The candles a viewport can show in `plot_width`.
pub(crate) fn visible_range(
    candles: &[ListingCandlestick],
    viewport: ChartViewport,
    plot_width: Pixels,
) -> std::ops::Range<usize> {
    if candles.is_empty() {
        return 0..0;
    }
    let visible_count = ((f32::from(plot_width) / viewport.candle_width).ceil() as usize)
        .saturating_add(1)
        .max(2);
    let end = viewport.right_timestamp.map_or(candles.len(), |timestamp| {
        candles.partition_point(|candle| candle.timestamp <= timestamp)
    });
    let end = end.clamp(1, candles.len());
    end.saturating_sub(visible_count)..end
}
