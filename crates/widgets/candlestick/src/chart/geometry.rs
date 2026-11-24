use market_participants_model::market::book::ListingCandlestick;
use smallvec::SmallVec;
use ui_core::math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px};

pub(crate) const TIME_AXIS_HEIGHT: LogicalPx = px(26.0);
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: LogicalPx = px(12.0);
pub(crate) const MIN_AXIS_WIDTH: LogicalPx = px(42.0);
pub(crate) const MIN_PLOT_WIDTH: LogicalPx = px(48.0);
pub(crate) const PLOT_SEPARATOR_HEIGHT: LogicalPx = px(1.0);
pub(crate) const SUB_PLOT_INLINE: usize = 4;
pub(crate) const MIN_PLOT_HEIGHT: LogicalPx = px(48.0);
pub(crate) const SUB_PLOT_HEIGHT_CEILING: LogicalPx = px(200.0);

#[derive(Clone, Copy, Debug)]
pub(crate) struct SubPlotLayout {
    pub plot: LogicalRect,
    pub axis: LogicalRect,
}

pub(crate) struct ChartLayout {
    pub price_plot: LogicalRect,
    pub price_axis: LogicalRect,
    pub separators: SmallVec<[LogicalRect; SUB_PLOT_INLINE]>,
    pub sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]>,
    pub time_axis: LogicalRect,
}

pub(crate) struct LayoutInputs {
    pub longest_label_width: LogicalPx,
    pub sub_plot_count: usize,
}

impl ChartLayout {
    pub(crate) fn new(bounds: LogicalRect, inputs: LayoutInputs) -> Self {
        let axis_width = axis_width(bounds.size.width, inputs.longest_label_width);
        let time_height = TIME_AXIS_HEIGHT.min(bounds.size.height * 0.24);
        let content_height = (bounds.size.height - time_height).max(px(0.0));
        let plot_width = (bounds.size.width - axis_width).max(px(1.0));

        let separator_height = PLOT_SEPARATOR_HEIGHT;
        let sub_seps = inputs.sub_plot_count as f32;
        let sub_room =
            (content_height - separator_height * sub_seps - MIN_PLOT_HEIGHT).max(px(0.0));
        let sub_wish = content_height * (sub_seps * 0.24).min(0.60);
        let sub_budget = sub_wish.min(sub_room);
        let per_sub = if inputs.sub_plot_count == 0 {
            LogicalPx::ZERO
        } else {
            (sub_budget / inputs.sub_plot_count as f32).min(SUB_PLOT_HEIGHT_CEILING)
        };
        let drawn = per_sub > LogicalPx::ZERO;
        let sub_total = if drawn {
            per_sub * sub_seps + separator_height * sub_seps
        } else {
            LogicalPx::ZERO
        };
        let price_height = content_height - sub_total;

        let price_plot = LogicalRect::from_origin_size(
            bounds.origin,
            LogicalSize::new(plot_width, price_height),
        );
        let mut separators: SmallVec<[LogicalRect; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut sub_plots: SmallVec<[SubPlotLayout; SUB_PLOT_INLINE]> = SmallVec::new();
        let mut y = price_plot.max().y;
        if drawn {
            for _ in 0..inputs.sub_plot_count {
                separators.push(LogicalRect::from_origin_size(
                    LogicalPoint::new(bounds.origin.x, y),
                    LogicalSize::new(plot_width + axis_width, separator_height),
                ));
                y += separator_height;
                let plot = LogicalRect::from_origin_size(
                    LogicalPoint::new(bounds.origin.x, y),
                    LogicalSize::new(plot_width, per_sub),
                );
                let axis = LogicalRect::from_origin_size(
                    LogicalPoint::new(plot.max().x, y),
                    LogicalSize::new(axis_width, per_sub),
                );
                sub_plots.push(SubPlotLayout { plot, axis });
                y += per_sub;
            }
        }
        let time_axis = LogicalRect::from_origin_size(
            LogicalPoint::new(bounds.origin.x, y),
            LogicalSize::new(plot_width, time_height),
        );

        Self {
            price_plot,
            price_axis: LogicalRect::from_origin_size(
                LogicalPoint::new(price_plot.max().x, price_plot.origin.y),
                LogicalSize::new(axis_width, price_plot.size.height),
            ),
            separators,
            sub_plots,
            time_axis,
        }
    }

    pub(crate) fn contains_data_point(&self, point: LogicalPoint) -> bool {
        self.price_plot.contains(point)
            || self
                .sub_plots
                .iter()
                .any(|sub_plot| sub_plot.plot.contains(point))
    }

    pub(crate) fn maximum_plot_width(&self) -> LogicalPx {
        let chart_width = self.price_plot.size.width + self.price_axis.size.width;
        (chart_width - axis_width(chart_width, LogicalPx::ZERO)).max(px(1.0))
    }

    pub(crate) fn data_bounds(&self) -> LogicalRect {
        self.sub_plots
            .iter()
            .fold(self.price_plot, |bounds, sub_plot| {
                LogicalRect::from_origin_size(
                    bounds.origin,
                    LogicalSize::new(bounds.size.width, sub_plot.plot.max().y - bounds.origin.y),
                )
            })
    }
}

fn axis_width(chart_width: LogicalPx, longest_label_width: LogicalPx) -> LogicalPx {
    let desired = (px(longest_label_width.value().ceil()) + AXIS_LABEL_HORIZONTAL_PADDING)
        .max(MIN_AXIS_WIDTH);
    let available = (chart_width - MIN_PLOT_WIDTH).max(LogicalPx::ZERO);
    desired.min(available)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CandleGeometry {
    first_x: LogicalPx,
    candle_width: LogicalPx,
    visible_len: usize,
}

impl CandleGeometry {
    pub(crate) fn new(plot: LogicalRect, visible_len: usize, candle_width: LogicalPx) -> Self {
        Self {
            first_x: plot.max().x - candle_width * visible_len as f32,
            candle_width,
            visible_len,
        }
    }

    pub(crate) fn center_x(self, slot: usize) -> LogicalPx {
        self.first_x + self.candle_width * (slot as f32 + 0.5)
    }

    pub(crate) fn body_width(self) -> LogicalPx {
        (self.candle_width * 0.68).max(px(1.0))
    }

    pub(crate) fn center_x_left_of_first(self, offset: usize) -> LogicalPx {
        self.first_x + self.candle_width * (0.5 - offset as f32)
    }

    pub(crate) fn slot_at(self, x: LogicalPx) -> Option<usize> {
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartViewport {
    pub candle_width: LogicalPx,
    pub right_timestamp: Option<i64>,
    pub cursor: Option<LogicalPoint>,
}

impl Default for ChartViewport {
    fn default() -> Self {
        Self {
            candle_width: px(9.0),
            right_timestamp: None,
            cursor: None,
        }
    }
}

pub(crate) fn visible_range(
    candles: &[ListingCandlestick],
    viewport: ChartViewport,
    plot_width: LogicalPx,
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
