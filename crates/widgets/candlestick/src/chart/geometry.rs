//! Where everything on the chart goes.
//!
//! The chart is one price plot and up to [`SUB_PLOT_INLINE`] sub plots under it,
//! each with its own y-axis beside it, plus a time axis below all of them. Every
//! box is derived from the element's own bounds, so a resize needs no state and
//! the same inputs always produce the same layout.
//!
//! The y-axis width is not fixed. It has to fit the widest label any of the axes
//! can produce, and widening the axis takes width away from the plot, which
//! changes which candles are visible, which can change the labels. The painter
//! resolves that once per frame: it measures the candidate labels of the widest
//! plot the box allows (the axis at its narrowest), and lays the final boxes out
//! with the measured width ([`crate::chart::paint`] documents the reasoning). No
//! label measured on that wider plot can be narrower than one measured on the
//! narrower final plot, so the axis always fits what is drawn in it.
//!
//! Sub plots share the y-axis width with the price plot. That is what makes
//! [`CandleGeometry`] place the same slot at the same x in every plot: the plot
//! widths are equal, so the same `slot` and `candle_width` produce the same
//! `center_x`.

use market_participants_model::market::book::ListingCandlestick;
use smallvec::SmallVec;
use ui_core::math::{Point, Rect, Size};

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
    pub plot: Rect,
    pub axis: Rect,
}

/// Where each part of the chart sits inside the element it was given.
///
/// `separators` has exactly as many entries as `sub_plots`, and the i-th one sits
/// directly above the i-th sub plot. There is no separator between the last sub
/// plot and the time axis: the time axis's top edge is drawn from the layout.
pub(crate) struct ChartLayout {
    pub price_plot: Rect,
    pub price_axis: Rect,
    pub separators: SmallVec<[Rect; SUB_PLOT_INLINE]>,
    pub sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]>,
    pub time_axis: Rect,
}

/// What the layout needs from outside: the widest axis label and how many sub
/// plots are enabled.
pub(crate) struct LayoutInputs {
    /// The widest label across **all** y-axes, price and sub plots alike.
    pub longest_label_width: f32,
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
    pub(crate) fn new(bounds: Rect, inputs: LayoutInputs) -> Self {
        let axis_width = axis_width(bounds.size.width, inputs.longest_label_width);
        let time_height = TIME_AXIS_HEIGHT.min(bounds.size.height * 0.24);
        let content_height = (bounds.size.height - time_height).max(0.0);
        let plot_width = (bounds.size.width - axis_width).max(1.0);

        let separator_height = PLOT_SEPARATOR_HEIGHT;
        let sub_seps = inputs.sub_plot_count as f32;
        let sub_room = (content_height - separator_height * sub_seps - MIN_PLOT_HEIGHT).max(0.0);
        // Each sub plot wishes for 24% of the content, the group for 60% at most,
        // and never more than the price plot's floor leaves over.
        let sub_wish = content_height * (sub_seps * 0.24).min(0.60);
        let sub_budget = sub_wish.min(sub_room);
        let per_sub = if inputs.sub_plot_count == 0 {
            0.0
        } else {
            (sub_budget / inputs.sub_plot_count as f32).min(SUB_PLOT_HEIGHT_CEILING)
        };
        // A sub plot that gets no height is not drawn at all: zero-height boxes
        // would make hit testing and paint order depend on how `Rect::contains`
        // treats an empty rectangle.
        let drawn = per_sub > 0.0;
        let sub_total = if drawn {
            per_sub * sub_seps + separator_height * sub_seps
        } else {
            0.0
        };
        let price_height = content_height - sub_total;

        let price_plot = Rect::from_origin_size(bounds.origin, Size::new(plot_width, price_height));
        let mut separators: SmallVec<[Rect; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut y = price_plot.max().y;
        if drawn {
            for _ in 0..inputs.sub_plot_count {
                separators.push(Rect::from_origin_size(
                    Point::new(bounds.origin.x, y),
                    Size::new(plot_width + axis_width, separator_height),
                ));
                y += separator_height;
                let plot = Rect::from_origin_size(
                    Point::new(bounds.origin.x, y),
                    Size::new(plot_width, per_sub),
                );
                let axis = Rect::from_origin_size(
                    Point::new(plot.max().x, y),
                    Size::new(axis_width, per_sub),
                );
                sub_plots.push(SubPlotLayout { plot, axis });
                y += per_sub;
            }
        }
        let time_axis = Rect::from_origin_size(
            Point::new(bounds.origin.x, y),
            Size::new(plot_width, time_height),
        );

        Self {
            price_plot,
            price_axis: Rect::from_origin_size(
                Point::new(price_plot.max().x, price_plot.origin.y),
                Size::new(axis_width, price_plot.size.height),
            ),
            separators,
            sub_plots,
            time_axis,
        }
    }

    /// Whether a pointer position is over data rather than over an axis.
    pub(crate) fn contains_data_point(&self, point: Point) -> bool {
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
    pub(crate) fn maximum_plot_width(&self) -> f32 {
        let chart_width = self.price_plot.size.width + self.price_axis.size.width;
        (chart_width - axis_width(chart_width, 0.0)).max(1.0)
    }

    /// Every plotting area as one box: the price plot and all sub plots, without
    /// the y-axes, the separators or the time axis.
    ///
    /// The vertical grid lines and the crosshair use it so that one line spans
    /// every plot; they must all agree on the same box, which is why this is
    /// derived here rather than assembled by each caller.
    pub(crate) fn data_bounds(&self) -> Rect {
        self.sub_plots
            .iter()
            .fold(self.price_plot, |bounds, sub_plot| {
                Rect::from_origin_size(
                    bounds.origin,
                    Size::new(bounds.size.width, sub_plot.plot.max().y - bounds.origin.y),
                )
            })
    }
}

fn axis_width(chart_width: f32, longest_label_width: f32) -> f32 {
    let desired = (longest_label_width.ceil() + AXIS_LABEL_HORIZONTAL_PADDING).max(MIN_AXIS_WIDTH);
    let available = (chart_width - MIN_PLOT_WIDTH).max(0.0);
    desired.min(available)
}

/// Which candles a viewport is showing, and where each one sits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CandleGeometry {
    first_x: f32,
    candle_width: f32,
    visible_len: usize,
}

impl CandleGeometry {
    /// Places `visible_len` candles of `candle_width` against the right edge of
    /// `plot`, which is where the newest candle always belongs.
    pub(crate) fn new(plot: Rect, visible_len: usize, candle_width: f32) -> Self {
        Self {
            first_x: plot.max().x - visible_len as f32 * candle_width,
            candle_width,
            visible_len,
        }
    }

    /// The x coordinate at the middle of the candle in `slot`.
    pub(crate) fn center_x(self, slot: usize) -> f32 {
        self.first_x + (slot as f32 + 0.5) * self.candle_width
    }

    /// The width of a candle body, which is narrower than its slot.
    pub(crate) fn body_width(self) -> f32 {
        (self.candle_width * 0.68).max(1.0)
    }

    /// The x coordinate at the middle of the candle `offset` slots left of the
    /// first visible one.
    ///
    /// An indicator line draws one candle outside the left edge so the segment
    /// entering the plot has the right height. That slot is not part of
    /// `visible_len`, so [`CandleGeometry::center_x`] cannot express it.
    pub(crate) fn center_x_left_of_first(self, offset: usize) -> f32 {
        self.first_x + (0.5 - offset as f32) * self.candle_width
    }

    /// The visible slot under an x coordinate, clamped to the visible range.
    pub(crate) fn slot_at(self, x: f32) -> Option<usize> {
        if self.visible_len == 0 {
            return None;
        }
        Some(
            ((x - self.first_x) / self.candle_width)
                .floor()
                .clamp(0.0, self.visible_len.saturating_sub(1) as f32) as usize,
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
    pub cursor: Option<Point>,
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
    plot_width: f32,
) -> std::ops::Range<usize> {
    if candles.is_empty() {
        return 0..0;
    }
    let visible_count = ((plot_width / viewport.candle_width).ceil() as usize)
        .saturating_add(1)
        .max(2);
    let end = viewport.right_timestamp.map_or(candles.len(), |timestamp| {
        candles.partition_point(|candle| candle.timestamp <= timestamp)
    });
    let end = end.clamp(1, candles.len());
    end.saturating_sub(visible_count)..end
}
