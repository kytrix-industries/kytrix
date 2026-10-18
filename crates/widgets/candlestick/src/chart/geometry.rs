//! Where everything on the chart goes.
//!
//! The chart is five boxes computed from one: a price plot, a volume plot under
//! it, a y-axis beside each of them, and a time axis below. Nothing here paints;
//! the boxes are derived from the element's own bounds, so a resize needs no
//! state and the same inputs always produce the same layout.
//!
//! The y-axis width is not fixed. It has to fit the widest price or volume label
//! the visible candles can produce, and widening the axis takes width away from
//! the plot, which changes which candles are visible, which can change the
//! labels. [`ChartLayout::resolve`] iterates that to a fixed point instead of
//! guessing.

use gpui_kit::{Bounds, Pixels, Point, point, px, size};
use market_participants_model::market::book::ListingCandlestick;

/// Height of the time axis.
pub(crate) const TIME_AXIS_HEIGHT: f32 = 26.0;
/// Horizontal padding a label needs inside an axis.
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: f32 = 12.0;
/// Narrowest y-axis, used when the labels are short.
pub(crate) const MIN_AXIS_WIDTH: f32 = 42.0;
/// Narrowest plot, which a long label may not squeeze further.
pub(crate) const MIN_PLOT_WIDTH: f32 = 48.0;
/// Height of the gap between the price plot and the volume plot.
pub(crate) const PLOT_SEPARATOR_HEIGHT: f32 = 1.0;
/// Share of the plotting area given to volume.
pub(crate) const VOLUME_PLOT_RATIO: f32 = 0.24;

/// Where each part of the chart sits inside the element it was given.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChartLayout {
    pub price_plot: Bounds<Pixels>,
    pub volume_plot: Bounds<Pixels>,
    pub price_axis: Bounds<Pixels>,
    pub volume_axis: Bounds<Pixels>,
    pub time_axis: Bounds<Pixels>,
}

impl ChartLayout {
    /// Places the five boxes inside `bounds`, reserving `longest_label_width`
    /// for the y-axes.
    pub(crate) fn new(bounds: Bounds<Pixels>, longest_label_width: Pixels) -> Self {
        let axis_width = axis_width(bounds.size.width, longest_label_width);
        let time_height = px(TIME_AXIS_HEIGHT).min(bounds.size.height * 0.24);
        let plot_width = (bounds.size.width - axis_width).max(px(1.0));
        let content_height = (bounds.size.height - time_height).max(px(2.0));
        let separator_height = px(PLOT_SEPARATOR_HEIGHT).min(content_height * 0.1);
        let plots_height = (content_height - separator_height).max(px(2.0));
        let volume_height = (plots_height * VOLUME_PLOT_RATIO)
            .max(px(1.0))
            .min(plots_height - px(1.0));
        let price_height = plots_height - volume_height;
        let price_plot = Bounds::new(bounds.origin, size(plot_width, price_height));
        let volume_plot = Bounds::new(
            point(bounds.origin.x, price_plot.bottom() + separator_height),
            size(plot_width, volume_height),
        );
        let price_axis = Bounds::new(
            point(price_plot.right(), price_plot.origin.y),
            size(axis_width, price_plot.size.height),
        );
        let volume_axis = Bounds::new(
            point(volume_plot.right(), volume_plot.origin.y),
            size(axis_width, volume_plot.size.height),
        );
        let time_axis = Bounds::new(
            point(bounds.origin.x, volume_plot.bottom()),
            size(plot_width, time_height),
        );
        Self {
            price_plot,
            volume_plot,
            price_axis,
            volume_axis,
            time_axis,
        }
    }

    /// Whether a pointer position is over data rather than over an axis.
    pub(crate) fn contains_data_point(&self, point: &Point<Pixels>) -> bool {
        self.price_plot.contains(point) || self.volume_plot.contains(point)
    }

    /// The plot width the chart would have if the axis shrank to its minimum.
    ///
    /// Prefetching uses this to ask for history slightly too early rather than
    /// too late: the axis is at its widest when the labels are longest, so this
    /// is the widest the visible range can be.
    pub(crate) fn maximum_plot_width(self) -> Pixels {
        let chart_width = self.price_plot.size.width + self.price_axis.size.width;
        (chart_width - axis_width(chart_width, px(0.0))).max(px(1.0))
    }

    /// Both plots and the separator between them, as one box.
    pub(crate) fn data_bounds(self) -> Bounds<Pixels> {
        Bounds::new(
            self.price_plot.origin,
            size(
                self.price_plot.size.width,
                self.volume_plot.bottom() - self.price_plot.origin.y,
            ),
        )
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

#[cfg(test)]
mod tests {
    use gpui_kit::{Bounds, point, px, size};
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::venue::PriceBasis,
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };

    use super::*;

    const CHART: Bounds<Pixels> = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(800.0), px(500.0)),
    };

    fn candle(timestamp: i64) -> ListingCandlestick {
        let value = Price::from(1_u64);
        let price = MarketPrice {
            value,
            quote_asset: test_support::currency("USD"),
            basis: PriceBasis::PerAssetUnit,
        };
        ListingCandlestick {
            listing: test_support::listing_key(&test_support::spot_route(), "BTCUSDT"),
            open: price.clone(),
            high: price.clone(),
            low: price.clone(),
            close: price,
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::Contracts,
            }),
            traded_notional: None,
            timestamp,
        }
    }

    #[test]
    fn layout_should_place_volume_plot_below_price_plot() {
        let layout = ChartLayout::new(CHART, px(0.0));

        assert_eq!(
            layout.volume_plot.origin.y,
            layout.price_plot.bottom() + px(PLOT_SEPARATOR_HEIGHT)
        );
    }

    #[test]
    fn layout_should_place_time_axis_below_volume_plot() {
        let layout = ChartLayout::new(CHART, px(0.0));

        assert_eq!(layout.time_axis.origin.y, layout.volume_plot.bottom());
    }

    #[test]
    fn layout_should_align_each_y_axis_with_its_plot() {
        let layout = ChartLayout::new(CHART, px(0.0));

        assert_eq!(
            (
                layout.price_axis.size.height,
                layout.volume_axis.size.height
            ),
            (
                layout.price_plot.size.height,
                layout.volume_plot.size.height
            )
        );
    }

    #[test]
    fn layout_should_use_minimum_axis_width_for_short_labels() {
        let layout = ChartLayout::new(CHART, px(8.0));

        assert_eq!(layout.price_axis.size.width, px(MIN_AXIS_WIDTH));
    }

    #[test]
    fn layout_should_expand_axis_for_long_labels() {
        let layout = ChartLayout::new(CHART, px(80.0));

        assert_eq!(layout.price_axis.size.width, px(92.0));
    }

    #[test]
    fn layout_should_preserve_minimum_plot_width_when_label_is_too_wide() {
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(80.0))),
            px(200.0),
        );

        assert_eq!(layout.price_plot.size.width, px(MIN_PLOT_WIDTH));
    }

    #[test]
    fn layout_should_not_cap_fitting_labels_to_a_chart_ratio() {
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(300.0))),
            px(160.0),
        );

        assert_eq!(layout.price_axis.size.width, px(172.0));
    }

    #[test]
    fn layout_should_share_width_between_price_and_volume_axes() {
        let layout = ChartLayout::new(CHART, px(80.0));

        assert_eq!(layout.price_axis.size.width, layout.volume_axis.size.width);
    }

    #[test]
    fn maximum_plot_width_should_assume_the_minimum_axis_width() {
        let layout = ChartLayout::new(CHART, px(80.0));

        assert_eq!(layout.maximum_plot_width(), px(758.0));
    }

    #[test]
    fn hit_testing_should_accept_both_plots_and_reject_the_axes() {
        let layout = ChartLayout::new(CHART, px(0.0));

        assert!(layout.contains_data_point(&layout.price_plot.center()));
        assert!(layout.contains_data_point(&layout.volume_plot.center()));
        assert!(!layout.contains_data_point(&layout.price_axis.center()));
        assert!(!layout.contains_data_point(&layout.time_axis.center()));
    }

    #[test]
    fn candle_geometry_should_share_centers_between_price_and_volume_plots() {
        let price_plot = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(300.0)));
        let volume_plot = Bounds::new(point(px(10.0), px(321.0)), size(px(400.0), px(100.0)));
        let price_geometry = CandleGeometry::new(price_plot, 20, 10.0);
        let volume_geometry = CandleGeometry::new(volume_plot, 20, 10.0);

        assert_eq!(price_geometry.center_x(7), volume_geometry.center_x(7));
    }

    #[test]
    fn candle_geometry_should_share_body_width_between_price_and_volume_plots() {
        let price_plot = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(300.0)));
        let volume_plot = Bounds::new(point(px(10.0), px(321.0)), size(px(400.0), px(100.0)));
        let price_geometry = CandleGeometry::new(price_plot, 20, 10.0);
        let volume_geometry = CandleGeometry::new(volume_plot, 20, 10.0);

        assert_eq!(price_geometry.body_width(), volume_geometry.body_width());
    }

    #[test]
    fn candle_geometry_should_anchor_the_newest_candle_to_the_right_edge() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 10, 10.0);

        assert_eq!(geometry.center_x(9), px(95.0));
    }

    #[test]
    fn candle_geometry_should_clamp_a_slot_outside_the_visible_range() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 10, 10.0);

        assert_eq!(geometry.slot_at(px(-50.0)), Some(0));
        assert_eq!(geometry.slot_at(px(500.0)), Some(9));
        assert_eq!(CandleGeometry::new(plot, 0, 10.0).slot_at(px(1.0)), None);
    }

    #[test]
    fn visible_range_should_follow_timestamp_anchor() {
        let candles = (1..=10).map(candle).collect::<Vec<_>>();
        let viewport = ChartViewport {
            candle_width: 10.0,
            right_timestamp: Some(6),
            cursor: None,
        };

        assert_eq!(visible_range(&candles, viewport, px(30.0)), 2..6);
    }

    #[test]
    fn visible_range_should_show_the_newest_candles_when_following_the_live_edge() {
        let candles = (1..=10).map(candle).collect::<Vec<_>>();
        let viewport = ChartViewport {
            candle_width: 10.0,
            right_timestamp: None,
            cursor: None,
        };

        assert_eq!(visible_range(&candles, viewport, px(30.0)), 6..10);
    }

    #[test]
    fn visible_range_should_be_empty_without_candles() {
        let viewport = ChartViewport::default();

        assert_eq!(visible_range(&[], viewport, px(300.0)), 0..0);
    }
}
